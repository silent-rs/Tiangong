//! 模式协调器（R4，方案 §4.2）：中枢/普通模式切换的宿主级状态机。
//!
//! POC 的模式切换完全由前端模块级变量（`preCentralContext`）与 Zustand
//! 状态承载——方案明确指出这不可靠：重启恢复、恢复失败、快速反向切换
//! 都没有宿主事实。本协调器把模式状态与普通上下文快照收归宿主：
//!
//! ```text
//! Normal → EnterRequested → CentralActive
//! CentralActive → ExitRequested → Normal
//! 异常：Enter 失败回 Normal；Exit 恢复失败回 CentralActive（可重试）
//! ```
//!
//! - **可重入**：CentralActive 下重复 enter 幂等；Enter 期间的新请求合并；
//! - **快照归还**：exit 返回进入时保存的普通上下文快照，前端据此恢复；
//!   无快照的 exit（异常路径）返回 None，前端走「原会话不可恢复」分支
//!   （方案 §4.4：不静默跳到无关会话）；
//! - **偏好持久化**：`central/mode-preference.json` 记录上次停留模式，
//!   重启后前端按偏好恢复（§4.1）；
//! - 后台 Worker 不受模式切换影响（§4.3/§4.4：只改视图，不动生命周期）。
//!
//! 前端仍保留自己的临时 UI 状态（动画进度等），但事实源在此。

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// 模式状态（Z7 两态化：EnterRequested/ExitRequested 为死变体——
/// request_enter/exit 同步完成，从未置位过渡态；方案 §4.2 的完整
/// 异步状态机待前端转场异步化时再引入，偏差已记录于附录 A.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeState {
    /// 普通模式（默认）。
    Normal,
    /// 中枢模式激活。
    CentralActive,
}

impl ModeState {
    pub fn as_str(self) -> &'static str {
        match self {
            ModeState::Normal => "normal",
            ModeState::CentralActive => "central_active",
        }
    }
}

/// 普通模式上下文快照（前端 `preCentralContext` 的宿主对应物）。
///
/// 进入中枢前由前端上报保存；退出中枢时原样归还。字段为 POC 前端
/// 实际维护的两项（活跃会话 / 新对话占位）；输入缓存与面板状态仍属
/// 前端临时 UI 状态（§6.3），随快照的 active_session_id 恢复。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalContextSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_conversation_id: Option<String>,
    /// 快照保存时刻（本地时间文本）。
    pub saved_at: String,
}

/// 模式偏好（重启恢复依据，§4.1）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModePreference {
    /// 上次停留模式（"central" / "normal"）。
    pub last_mode: String,
    pub updated_at: String,
}

/// 宿主模式协调器（挂 [`crate::CentralHostState`]）。
#[derive(Default)]
pub struct ModeCoordinator {
    inner: Mutex<ModeCoordinatorInner>,
}

#[derive(Default)]
struct ModeCoordinatorInner {
    state: Option<ModeState>,
    normal_context: Option<NormalContextSnapshot>,
}

fn now_text() -> String {
    chrono::Local::now().naive_local().to_string()
}

/// 偏好持久化路径：`{storage_root}/central/mode-preference.json`。
fn preference_path(storage_root: &Path) -> std::path::PathBuf {
    storage_root.join("central").join("mode-preference.json")
}

/// 读取上次停留偏好（无记录默认 normal）。
pub fn load_mode_preference(storage_root: &Path) -> ModePreference {
    std::fs::read_to_string(preference_path(storage_root))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(ModePreference {
            last_mode: "normal".to_string(),
            updated_at: String::new(),
        })
}

fn save_mode_preference(storage_root: &Path, last_mode: &str) {
    let preference = ModePreference {
        last_mode: last_mode.to_string(),
        updated_at: now_text(),
    };
    let path = preference_path(storage_root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&preference) {
        // 复用持久层事务写（随机后缀 + 写锁 + fsync）
        let _ = central_state::persistence::atomic_write_bytes(&path, text.as_bytes());
    }
}

impl ModeCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ModeCoordinatorInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 当前状态（未初始化视为 Normal）。
    pub fn state(&self) -> ModeState {
        self.lock().state.unwrap_or(ModeState::Normal)
    }

    /// 请求进入中枢：保存普通上下文快照，状态切到 CentralActive。
    ///
    /// 幂等：已 CentralActive 时更新快照（快速切换合并）不报错。
    pub fn request_enter(&self, snapshot: NormalContextSnapshot) -> ModeState {
        let mut inner = self.lock();
        let current = inner.state.unwrap_or(ModeState::Normal);
        match current {
            ModeState::Normal => {
                inner.normal_context = Some(snapshot);
                inner.state = Some(ModeState::CentralActive);
                tracing::info!(
                    from = current.as_str(),
                    "模式切换：进入中枢（普通上下文快照已保存）"
                );
            }
            // 中枢中重入（请求合并）：仅刷新快照
            ModeState::CentralActive => {
                inner.normal_context = Some(snapshot);
                inner.state = Some(ModeState::CentralActive);
                tracing::info!("模式切换：中枢激活期间重复进入请求已合并");
            }
        }
        ModeState::CentralActive
    }

    /// 请求退出中枢：归还进入时保存的快照。
    ///
    /// 返回 None 表示没有可用快照（异常进入路径）——调用方（前端）必须
    /// 走「原会话不可恢复」显式分支，不得静默跳到无关会话（§4.4）。
    pub fn request_exit(&self) -> Option<NormalContextSnapshot> {
        let mut inner = self.lock();
        let current = inner.state.unwrap_or(ModeState::Normal);
        match current {
            ModeState::CentralActive => {
                inner.state = Some(ModeState::Normal);
                let snapshot = inner.normal_context.take();
                tracing::info!(
                    from = current.as_str(),
                    has_snapshot = snapshot.is_some(),
                    "模式切换：退出中枢（快照归还）"
                );
                snapshot
            }
            ModeState::Normal => {
                tracing::warn!("模式切换：Normal 下收到退出请求（忽略）");
                None
            }
        }
    }

    /// 记录进入偏好（持久化，重启恢复依据）。
    pub fn persist_enter_preference(&self, storage_root: &Path) {
        save_mode_preference(storage_root, "central");
    }

    /// 记录退出偏好。
    pub fn persist_exit_preference(&self, storage_root: &Path) {
        save_mode_preference(storage_root, "normal");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Z7/Z8 行为驱动（取代自证式矩阵测试）：request_enter/exit 的
    /// 真实转移行为——重入合并、快照无损往返、异常退出分支。
    #[test]
    fn enter_exit_transitions_are_driven_by_real_calls() {
        let coordinator = ModeCoordinator::new();
        assert_eq!(coordinator.state(), ModeState::Normal);
        coordinator.request_enter(NormalContextSnapshot {
            active_session_id: Some("s-1".to_string()),
            new_conversation_id: None,
            saved_at: "2026-11-13 12:00:00".to_string(),
        });
        assert_eq!(coordinator.state(), ModeState::CentralActive);
        // 重入合并：仍 CentralActive、快照取最新
        coordinator.request_enter(NormalContextSnapshot {
            active_session_id: Some("s-2".to_string()),
            new_conversation_id: None,
            saved_at: "2026-11-13 12:01:00".to_string(),
        });
        assert_eq!(coordinator.state(), ModeState::CentralActive);
        let restored = coordinator.request_exit().expect("快照归还");
        assert_eq!(restored.active_session_id.as_deref(), Some("s-2"));
        assert_eq!(coordinator.state(), ModeState::Normal);
        // Normal 下再退出：无操作、无快照
        assert!(coordinator.request_exit().is_none());
        assert_eq!(coordinator.state(), ModeState::Normal);
    }

    /// 进入/退出往返：快照原样归还；再次退出（Normal）返回 None。
    #[test]
    fn enter_exit_round_trips_snapshot() {
        let coordinator = ModeCoordinator::new();
        assert_eq!(coordinator.state(), ModeState::Normal);
        let snapshot = NormalContextSnapshot {
            active_session_id: Some("s-abc".to_string()),
            new_conversation_id: None,
            saved_at: "2026-11-13 12:00:00".to_string(),
        };
        coordinator.request_enter(snapshot.clone());
        assert_eq!(coordinator.state(), ModeState::CentralActive);
        let restored = coordinator.request_exit().expect("快照应归还");
        assert_eq!(restored, snapshot, "普通上下文无损往返");
        assert_eq!(coordinator.state(), ModeState::Normal);
        assert!(
            coordinator.request_exit().is_none(),
            "Normal 下退出返回 None"
        );
    }

    /// 异常路径：无快照的退出（异常进入）返回 None——前端走显式恢复分支。
    #[test]
    fn exit_without_snapshot_returns_none() {
        let coordinator = ModeCoordinator::new();
        // 模拟异常进入（未走 request_enter 保存快照——如状态被外部置位）
        {
            let mut inner = coordinator.lock();
            inner.state = Some(ModeState::CentralActive);
        }
        let snapshot = coordinator.request_exit();
        assert!(
            snapshot.is_none(),
            "无快照退出必须返回 None（不得虚构恢复点）"
        );
        assert_eq!(coordinator.state(), ModeState::Normal);
    }

    /// 重入合并：CentralActive 下重复 enter 刷新快照不报错。
    #[test]
    fn reentrant_enter_merges_snapshot() {
        let coordinator = ModeCoordinator::new();
        coordinator.request_enter(NormalContextSnapshot {
            active_session_id: Some("s-1".to_string()),
            new_conversation_id: None,
            saved_at: now_text(),
        });
        // 快速反向切换：再次进入（用户退出后立即重进）
        coordinator.request_exit();
        let second = NormalContextSnapshot {
            active_session_id: Some("s-2".to_string()),
            new_conversation_id: Some("new-conv".to_string()),
            saved_at: now_text(),
        };
        coordinator.request_enter(second.clone());
        assert_eq!(coordinator.state(), ModeState::CentralActive);
        assert_eq!(coordinator.request_exit(), Some(second), "快照取最新");
    }

    /// 偏好持久化往返（重启恢复依据）。
    #[test]
    fn mode_preference_round_trips() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            load_mode_preference(root.path()).last_mode,
            "normal",
            "无记录默认 normal"
        );
        save_mode_preference(root.path(), "central");
        assert_eq!(load_mode_preference(root.path()).last_mode, "central");
    }
}
