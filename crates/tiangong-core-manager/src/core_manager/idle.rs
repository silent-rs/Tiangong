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

use tiangong_core::core::TiangongCore;

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

    /// 原子地确认存活并刷新活动时刻。
    ///
    /// 检查与刷新在同一把 registry 锁内完成，与空闲回收的原子摘除互斥：
    /// 返回 `true` 时活动已刷新（该实例至少在保留时长内不会被回收，
    /// 后续投递安全）；返回 `false` 表示实例已被回收或不存在，且**不会**
    /// 写入孤儿活动记录，调用方应走 `ensure_core` 重建。
    ///
    /// 供 host 的复用快速路径使用，取代分别调用 `has_live_core` +
    /// `touch_activity` 的两步组合（两步之间实例可能被回收）。
    pub fn touch_if_live(&self, session_id: &str) -> bool {
        let registry = self.registry();
        if !registry.contains_key(session_id) {
            return false;
        }
        self.touch_activity(session_id);
        true
    }

    /// 记录会话的交互时刻（ensure / 成功投递 / turn 运行时刷新）。
    ///
    /// host 在「即将使用该会话」的复用检查点应改用 [`Self::touch_if_live`]；
    /// 本方法直接写入，仅供已知实例存活的内部路径调用。
    pub fn touch_activity(&self, session_id: &str) {
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

    /// 原子地复核并摘除空闲超期的 Core。
    ///
    /// busy 复核与 remove 在同一把 registry 锁内完成：投递路径（持同一把
    /// 锁调用 `TiangongCore::deliver`，turn task 在其返回前已注册）只能
    /// 在本操作**之前**完成——此时 busy 复核失败、放弃摘除——或在本操作
    /// **之后**进行——此时 registry 已无该 Core、投递失败并由投递方重建。
    /// 因此不存在「复核通过后、移除前新 turn 启动，随后被 `shutdown_join`
    /// 的 `cancel_and_join` 取消」的窗口。
    fn take_idle_core(&self, session_id: &str) -> Option<TiangongCore> {
        let mut registry = self.registry();
        if !registry.get(session_id).is_some_and(|core| !core.is_busy()) {
            return None;
        }
        let expired = self
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .is_some_and(|last| last.elapsed() >= IDLE_RETENTION);
        if !expired {
            return None;
        }
        registry.remove(session_id)
    }

    /// 扫描并销毁空闲超时的 Core，返回本轮销毁的 session_id。
    ///
    /// 候选仅作粗筛；销毁经 [`Self::take_idle_core`] 原子复核摘除（期间
    /// 发生交互、turn 开始、被设为激活的实例都不会被移除）。销毁失败只
    /// 告警，下轮重试。
    pub(crate) async fn sweep_idle_cores(&self) -> Vec<String> {
        let now = Instant::now();
        // 候选收集：registry 与 activity 两把锁内完成，随后立即释放，
        // 不与销毁阶段的锁序（registry → activity）交叉。
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
            // 持 creation_lock 覆盖「原子摘除 → 旧 Core 关闭完成」整个区间：
            // `ensure_core` 的重建在同一把锁上等待，旧 Core 的
            // `shutdown_join → cancel_and_join` 按 session_id 操作全局 turn
            // 注册表（无代际校验），若允许重建抢先插入，新 Core 的 turn 会
            // 被旧实例的关闭路径误杀。
            let creation_lock = self.creation_lock(&session_id);
            let _creation_guard = creation_lock.lock_owned().await;
            // 锁内复核：候选收集后可能被设为激活会话，跳过。
            if self.is_exempt_session(&session_id) {
                continue;
            }
            let Some(core) = self.take_idle_core(&session_id) else {
                continue;
            };
            // Core 已摘出 registry：新 turn 无法再经投递路径落到它身上；
            // creation_lock 同时阻止本会话重建，直到关闭完成。
            let sid = session_id.clone();
            match tokio::task::spawn_blocking(move || core.shutdown_join()).await {
                Ok(Ok(())) => {
                    self.clear_activity(&session_id);
                    tracing::info!(session_id = %sid, "会话空闲超时，已销毁 Core 实例");
                    retired.push(session_id);
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        session_id = %sid,
                        %error,
                        "销毁空闲 Core 实例失败，待下轮扫描重试"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %sid,
                        %error,
                        "等待空闲 Core 关闭失败，待下轮扫描重试"
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

    // ===== 回收与首条消息并发的回归测试 =====

    /// 投递先完成（turn 已注册）：原子摘除必须跳过该实例。
    ///
    /// `deliver_to_core_if_live` 在 registry 锁内同步执行投递（turn task
    /// 注册先于返回）并刷新活动时刻；随后 sweep 的 `take_idle_core` 无论
    /// 先命中 busy 复核还是活动复核，都不会移除实例，刚接受的轮次不会被
    /// 回收关闭。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 投递启动轮次后的实例不被回收() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager_with_core(&dir, "race-delivered").await;

        // 空闲已超期（模拟空闲边缘时刻），随后用户消息投递成功。
        rewind_activity(&manager, "race-delivered", Duration::from_secs(7200));
        assert!(
            manager.deliver_to_core_if_live(
                "race-delivered",
                tiangong_core::agent_input::AgentInputKind::prepared_with_id(
                    "msg-race".to_string(),
                    vec![tiangong_types::ContentBlock::text("你好")],
                ),
            ),
            "投递应成功（turn 已启动）"
        );

        let retired = manager.sweep_idle_cores().await;
        assert!(retired.is_empty(), "刚接受轮次的实例不得回收：{retired:?}");
        assert!(manager.has_live_core("race-delivered"));
    }

    /// 回收先移除：投递失败但经 ensure_core 重建后可恢复，不丢消息路径。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 回收先移除时投递失败但可重建() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager_with_core(&dir, "race-retired").await;

        rewind_activity(&manager, "race-retired", Duration::from_secs(7200));
        assert_eq!(manager.sweep_idle_cores().await, vec!["race-retired"]);
        assert!(!manager.has_live_core("race-retired"));

        // 回收后的首条消息：投递找不到实例，host 收到明确失败。
        assert!(
            !manager.deliver_to_core_if_live(
                "race-retired",
                tiangong_core::agent_input::AgentInputKind::prepared_with_id(
                    "msg-after-retire".to_string(),
                    vec![tiangong_types::ContentBlock::text("回收后的消息")],
                ),
            ),
            "Core 已被回收，投递应失败"
        );

        // host 经既有 ensure_core 语义重建后，同一条消息投递成功。
        let (stream_tx, _stream_rx) = std::sync::mpsc::channel();
        manager
            .ensure_core(
                "race-retired",
                CoreConfig::builder()
                    .with_trust_mode(TrustMode::FullTrust)
                    .build(),
                "/tmp".to_string(),
                None,
                stream_tx,
                Vec::<Arc<dyn Plugin>>::new,
            )
            .await
            .expect("重建 Core 失败");
        assert!(
            manager.deliver_to_core_if_live(
                "race-retired",
                tiangong_core::agent_input::AgentInputKind::prepared_with_id(
                    "msg-after-retire".to_string(),
                    vec![tiangong_types::ContentBlock::text("回收后的消息")],
                ),
            ),
            "重建后投递应成功"
        );
        assert!(manager.has_live_core("race-retired"));
    }

    /// P1 回归：旧实例「摘除 → 关闭完成」全程持有 creation_lock，
    /// 期间发起的重建必须等到关闭完成后才能创建新实例并启动 turn，
    /// 旧实例的 `cancel_and_join` 不得取消新 turn。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 旧实例关闭期间重建的轮次不受影响() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager_with_core(&dir, "race-close").await;
        rewind_activity(&manager, "race-close", Duration::from_secs(7200));

        // 模拟 reaper 的完整锁区间：creation_lock 覆盖「摘除 → 关闭」。
        let reaper_manager = manager.clone();
        let (detached_tx, detached_rx) = tokio::sync::oneshot::channel::<()>();
        let reaper = tokio::spawn(async move {
            let creation_lock = reaper_manager.creation_lock("race-close");
            let _guard = creation_lock.lock_owned().await;
            let Some(core) = reaper_manager.take_idle_core("race-close") else {
                panic!("空闲超期实例应可摘除");
            };
            detached_tx.send(()).expect("通知主线程失败");
            tokio::task::spawn_blocking(move || core.shutdown_join())
                .await
                .expect("join 任务异常")
                .expect("关闭旧实例失败");
        });

        // 旧实例已摘除、关闭进行中：重建在 creation_lock 上等待。
        detached_rx.await.unwrap();
        let (stream_tx, _stream_rx) = std::sync::mpsc::channel();
        manager
            .ensure_core(
                "race-close",
                CoreConfig::builder()
                    .with_trust_mode(TrustMode::FullTrust)
                    .build(),
                "/tmp".to_string(),
                None,
                stream_tx,
                Vec::<Arc<dyn Plugin>>::new,
            )
            .await
            .expect("重建失败");
        assert!(
            manager.deliver_to_core_if_live(
                "race-close",
                tiangong_core::agent_input::AgentInputKind::prepared_with_id(
                    "msg-after-rebuild".to_string(),
                    vec![tiangong_types::ContentBlock::text("重建后的消息")],
                ),
            ),
            "重建后投递应成功"
        );
        reaper.await.unwrap();

        // 旧实例关闭已完成的瞬间新 turn 仍在运行 ⟹ 未被其取消：
        // `cancel_and_join` 按 session_id 取任务并等待其结束，若它取消过
        // 新 turn，reaper 返回时 is_busy 必然已是 false。
        if !manager.is_core_busy("race-close") {
            // turn 已自行收敛（端点不可达 → 正常失败）：终态必须是失败，
            // 而不是被旧实例关闭路径取消。
            let session = manager.load_session("race-close").expect("会话应存在");
            let last_status = session.messages.last().and_then(|m| m.turn_status());
            assert_ne!(
                last_status,
                Some(tiangong_types::TurnStatus::Cancelled),
                "新 turn 不得被旧实例的关闭路径取消"
            );
        }

        // 收尾：主动取消可能仍在运行的 turn（nextest 每测试独立进程，
        // 此处尽力收敛只为日志干净，不影响断言）。
        let _ = manager.cancel_core("race-close");
        for _ in 0..200 {
            if !manager.is_core_busy("race-close") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(manager.has_live_core("race-close"), "新实例应存活");
    }

    /// P2 回归：确认存活与刷新活动是原子操作，回收后不写入孤儿记录。
    #[tokio::test]
    async fn 确认存活并刷新活动是原子操作() {
        let dir = tempfile::tempdir().unwrap();
        let manager = make_manager_with_core(&dir, "touch-live").await;

        // 存活时确认并刷新（保留时长内 sweep 不回收）。
        assert!(manager.touch_if_live("touch-live"));
        rewind_activity(&manager, "touch-live", Duration::from_secs(7200));

        // 摘除后确认失败，且不为已回收会话写入活动记录。
        assert_eq!(manager.sweep_idle_cores().await, vec!["touch-live"]);
        assert!(!manager.touch_if_live("touch-live"));
        assert!(
            !manager
                .last_activity
                .lock()
                .unwrap()
                .contains_key("touch-live"),
            "不应为已回收会话写入孤儿活动记录"
        );
    }
}
