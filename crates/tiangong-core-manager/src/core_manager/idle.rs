//! 空闲 Core 实例的生命周期管理。
//!
//! Core 实例在 turn 结束后并不销毁，而是留在 registry 中等待复用
//! （会话真相源在磁盘，重建只是多一次插件装配）。但桌面 / Server 长时间
//! 运行时，打开过的会话会不断累积成常驻实例（内存、插件 sidecar 等后台
//! 资源随实例存续）。
//!
//! 本模块提供空闲回收（idle retire）：
//! - 每个会话记录最后活动时刻：`ensure_core` 命中/新建、成功投递、turn
//!   运行中都会刷新；
//! - 后台 reaper 周期扫描 registry，对「空闲超过保留时长的 Core」执行
//!   `retire_core(false)`——不打断运行中的 turn（忙即跳过），空闲时无
//!   在途 turn，cancel 无意义；
//! - **豁免**：当前激活会话（桌面上点击选中的那一个）不参与空闲回收。
//!   激活会话始终只有一个，用单值槽位维护——设置新激活即自动解除对
//!   旧激活的豁免（旧会话落回常规回收），host 只需在激活会话变化时
//!   同步槽位。
//!
//! 保留时长是程序内固定常量 [`IDLE_RETENTION`]（暂定 1 小时），不做运行时
//! 配置。
//!
//! 销毁只释放实例：会话数据始终在磁盘上，下一次交互经 `ensure_core`
//! 按需重建，行为与首次打开一致。

use std::time::{Duration, Instant};

use crate::CoreManager;

/// 空闲保留时长：turn 结束 / 最后交互后 Core 实例至少保留这么久，
/// 达到即销毁（激活中的会话除外，见 pin 豁免）。
///
/// 暂定 1 小时（当前需求）；需要调整时直接修改此常量。
pub(crate) const IDLE_RETENTION: Duration = Duration::from_secs(60 * 60);

/// reaper 扫描周期。粒度只影响销毁延迟（最多晚一个周期），1 分钟足够。
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

impl CoreManager {
    /// 设置空闲回收豁免的会话（当前激活会话）。
    ///
    /// 激活会话始终只有一个：本方法直接替换槽位，原豁免会话（如有）自动
    /// 落回常规空闲回收，host 无需显式解除。
    pub fn set_exempt_session(&self, session_id: &str) {
        *self
            .exempt_session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(session_id.to_string());
    }

    /// 清除空闲回收豁免（会话被删除 / 不再激活时调用）。
    ///
    /// 仅当豁免槽位恰好是该会话时清除，避免误清后续设置的新激活会话。幂等。
    pub fn clear_exempt_session(&self, session_id: &str) {
        let mut exempt = self
            .exempt_session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if exempt.as_deref() == Some(session_id) {
            *exempt = None;
        }
    }

    /// 会话是否为当前空闲回收豁免的激活会话。
    pub fn is_exempt_session(&self, session_id: &str) -> bool {
        self.exempt_session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_deref()
            == Some(session_id)
    }

    /// 记录会话的交互时刻（ensure / 成功投递 / turn 运行时刷新）。
    pub(crate) fn touch_activity(&self, session_id: &str) {
        self.last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.to_string(), Instant::now());
    }

    /// 清除会话的活动记录（实例已移出 registry 时调用，防表无限增长）。
    pub(crate) fn clear_activity(&self, session_id: &str) {
        self.last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
    }

    /// 会话是否空闲超过保留时长。
    ///
    /// 调用方（reaper）须先持有该会话的 creation_lock 再判定：与
    /// `ensure_core` 的创建区间互斥后，锁内看到的「刚交互」必然已刷新
    /// 活动时刻，不会出现用户刚发起会话就被回收的竞态。
    fn idle_past_retention(&self, session_id: &str) -> bool {
        if self
            .registry()
            .get(session_id)
            .is_some_and(|core| core.is_busy())
        {
            return false;
        }
        self.last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .is_some_and(|last| last.elapsed() >= IDLE_RETENTION)
    }

    /// 扫描并销毁空闲超时的 Core，返回本轮销毁的 session_id。
    ///
    /// 每个候选都经 creation_lock 复核后再销毁；复核不通过的（期间发生
    /// 交互、turn 开始、被标记豁免）留待下轮。销毁失败只告警，下轮重试。
    pub(crate) async fn sweep_idle_cores(&self) -> Vec<String> {
        let now = Instant::now();
        // 候选收集：registry 与 activity 两把锁内完成，随后立即释放，
        // 不与 retire 的锁序（creation_lock → registry）交叉。
        let candidates: Vec<String> = {
            let registry = self.registry();
            let mut activity = self
                .last_activity
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let exempt = self
                .exempt_session
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut candidates = Vec::new();
            for (session_id, core) in registry.iter() {
                if exempt.as_deref() == Some(session_id.as_str()) {
                    // 激活中的会话不回收。
                    continue;
                }
                if core.is_busy() {
                    // turn 运行中视为持续交互：从当下重新起算空闲计时，
                    // turn 结束后的首轮扫描才开始累计。
                    activity.insert(session_id.clone(), now);
                    continue;
                }
                let Some(last) = activity.get(session_id).copied() else {
                    // 无活动记录（防御外部直插 registry 的路径）：
                    // 从当下起算，本轮不销毁。
                    activity.insert(session_id.clone(), now);
                    continue;
                };
                if now.duration_since(last) >= IDLE_RETENTION {
                    candidates.push(session_id.clone());
                }
            }
            candidates
        };

        let mut retired = Vec::new();
        for session_id in candidates {
            let creation_lock = self.creation_lock(&session_id);
            let _creation_guard = creation_lock.lock_owned().await;
            // 锁内复核：候选收集后可能被设为激活或发生交互，均不销毁。
            if self.is_exempt_session(&session_id) || !self.idle_past_retention(&session_id) {
                continue;
            }
            // 空闲路径传 false：无在途 turn，无需投递 Cancel。
            match self.retire_core_locked(&session_id, false).await {
                Ok(()) => {
                    self.clear_activity(&session_id);
                    tracing::info!(session_id = %session_id, "会话空闲超时，已销毁 Core 实例");
                    retired.push(session_id);
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        %error,
                        "销毁空闲 Core 实例失败，待下轮扫描重试"
                    );
                }
            }
        }
        retired
    }

    /// 启动后台空闲回收任务。
    ///
    /// 跑在进程级共享 runtime 上（与 turn task 相同），随进程退出终止；
    /// registry 为空时每轮只做一次空扫描，无实质开销。
    pub(crate) fn spawn_idle_reaper(&self) {
        let manager = self.clone();
        tiangong_core::shared_runtime::shared_runtime().spawn(async move {
            loop {
                tokio::time::sleep(SWEEP_INTERVAL).await;
                manager.sweep_idle_cores().await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tiangong_core::config::core::{CoreConfig, CoreConfigProvider};
    use tiangong_core::core::Plugin;
    use tiangong_core::permission::TrustMode;
    use tiangong_core::session::Session;

    use crate::CoreManager;

    fn seed_models(dir: &tempfile::TempDir) {
        let config = serde_json::json!({
            "providers": {
                "p": {
                    "base_url": "http://127.0.0.1:1",
                    "api_key": "test-key",
                    "timeout_ms": 60000,
                    "protocol": "openai_chat_completions",
                    "headers": {}
                }
            },
            "models": {
                "model-a": {
                    "provider": "p",
                    "model": "model-a",
                    "capabilities": ["chat"]
                }
            },
            "routing": { "chat": "model-a" }
        });
        std::fs::write(
            dir.path().join("models.json"),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();
    }

    fn seed_session(dir: &tempfile::TempDir, id: &str) {
        let mut session = Session::new("空闲回收测试");
        session.id = id.to_string();
        session.bind_storage_root(dir.path().to_path_buf());
        session.try_persist_to_disk().unwrap();
    }

    async fn make_manager_with_core(dir: &tempfile::TempDir, id: &str) -> CoreManager {
        seed_models(dir);
        seed_session(dir, id);
        let manager = CoreManager::new(
            CoreConfigProvider::new(CoreConfig::default()),
            dir.path().to_path_buf(),
        );
        let (stream_tx, _stream_rx) = std::sync::mpsc::channel();
        manager
            .ensure_core(
                id,
                CoreConfig::builder()
                    .with_trust_mode(TrustMode::FullTrust)
                    .build(),
                "/tmp".to_string(),
                None,
                stream_tx,
                Vec::<Arc<dyn Plugin>>::new,
            )
            .await
            .expect("ensure_core 失败");
        manager
    }

    /// 把会话的活动时刻回拨到指定时长之前（模拟已闲置）。
    fn rewind_activity(manager: &CoreManager, session_id: &str, elapsed: Duration) {
        let mut activity = manager
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        activity.insert(
            session_id.to_string(),
            Instant::now()
                .checked_sub(elapsed)
                .expect("回拨时长超出 Instant 表示范围"),
        );
    }

    #[tokio::test]
    async fn 空闲超过保留时长的实例被销毁() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager_with_core(&dir, "idle-expired").await;
        assert!(manager.has_live_core("idle-expired"));

        rewind_activity(&manager, "idle-expired", Duration::from_secs(3601));
        let retired = manager.sweep_idle_cores().await;

        assert_eq!(retired, vec!["idle-expired"]);
        assert!(!manager.has_live_core("idle-expired"));
        // 活动记录随之清理
        assert!(
            !manager
                .last_activity
                .lock()
                .unwrap()
                .contains_key("idle-expired")
        );
    }

    #[tokio::test]
    async fn 未超时的实例保留() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager_with_core(&dir, "idle-fresh").await;

        rewind_activity(&manager, "idle-fresh", Duration::from_secs(600));
        let retired = manager.sweep_idle_cores().await;

        assert!(retired.is_empty());
        assert!(manager.has_live_core("idle-fresh"));
    }

    #[tokio::test]
    async fn 最近交互的实例不被销毁() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager_with_core(&dir, "idle-touched").await;

        // 回拨到超时后，再模拟一次交互刷新计时，随后扫描不应销毁。
        rewind_activity(&manager, "idle-touched", Duration::from_secs(7200));
        manager.touch_activity("idle-touched");
        let retired = manager.sweep_idle_cores().await;

        assert!(retired.is_empty());
        assert!(manager.has_live_core("idle-touched"));
    }

    #[tokio::test]
    async fn 无活动记录的实例从当下起算() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager_with_core(&dir, "idle-unknown").await;

        // 清掉活动记录模拟外部直插 registry 的路径：首轮只补记时刻，不销毁。
        manager.clear_activity("idle-unknown");
        let retired = manager.sweep_idle_cores().await;

        assert!(retired.is_empty());
        assert!(manager.has_live_core("idle-unknown"));
        assert!(
            manager
                .last_activity
                .lock()
                .unwrap()
                .contains_key("idle-unknown")
        );
    }

    #[tokio::test]
    async fn 豁免中的实例即使空闲超时也不销毁() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager_with_core(&dir, "exempt-active").await;

        manager.set_exempt_session("exempt-active");
        assert!(manager.is_exempt_session("exempt-active"));

        // 回拨远超保留时长：激活（豁免）中的实例不应被销毁。
        rewind_activity(&manager, "exempt-active", Duration::from_secs(7200));
        let retired = manager.sweep_idle_cores().await;

        assert!(retired.is_empty());
        assert!(manager.has_live_core("exempt-active"));
    }

    #[tokio::test]
    async fn 激活切换后旧会话落回常规回收() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager_with_core(&dir, "exempt-old").await;

        manager.set_exempt_session("exempt-old");
        rewind_activity(&manager, "exempt-old", Duration::from_secs(7200));
        assert!(manager.sweep_idle_cores().await.is_empty());

        // 切换激活：新会话进入豁免槽位，旧会话自动解除豁免并被回收。
        manager.set_exempt_session("exempt-new");
        assert!(!manager.is_exempt_session("exempt-old"));
        assert!(manager.is_exempt_session("exempt-new"));
        assert_eq!(manager.sweep_idle_cores().await, vec!["exempt-old"]);
        assert!(!manager.has_live_core("exempt-old"));
    }

    #[tokio::test]
    async fn 清除豁免只影响槽位中的会话() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager_with_core(&dir, "exempt-clear").await;

        manager.set_exempt_session("exempt-clear");
        // 清除其他会话不应影响豁免槽位。
        manager.clear_exempt_session("exempt-other");
        assert!(manager.is_exempt_session("exempt-clear"));

        rewind_activity(&manager, "exempt-clear", Duration::from_secs(7200));
        assert!(manager.sweep_idle_cores().await.is_empty());

        // 清除豁免后按空闲规则回收。
        manager.clear_exempt_session("exempt-clear");
        assert!(!manager.is_exempt_session("exempt-clear"));
        assert_eq!(manager.sweep_idle_cores().await, vec!["exempt-clear"]);
        assert!(!manager.has_live_core("exempt-clear"));
    }
}
