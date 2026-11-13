//! 中枢模式切换命令（R4）：前端消费的宿主模式事实源。
//!
//! 前端切换流程（方案 §4.3 / §4.4）：
//! - 进入：先 `central_mode_enter` 上报普通上下文快照（宿主保存、状态
//!   切 CentralActive、偏好落盘），再执行视图切换与激活问候；
//! - 退出：`central_mode_exit` 归还快照（None = 异常进入，前端必须走
//!   「原会话不可恢复」显式分支，不静默跳会话）；
//! - 对账/重启恢复：`central_mode_state` 返回当前状态与上次停留偏好。
//!
//! 前端可保留动画等临时 UI 状态，但模式事实以本组命令为准（§6.3）。

use serde_json::json;
use tauri::{AppHandle, Manager};

use crate::app::TiangongApp;
use crate::central_host::mode_coordinator::NormalContextSnapshot;

/// 进入中枢：保存普通上下文快照，状态切 CentralActive（幂等，重入合并）。
#[tauri::command]
pub async fn central_mode_enter(
    app: AppHandle,
    active_session_id: Option<String>,
    new_conversation_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let state = app.state::<TiangongApp>();
    let snapshot = NormalContextSnapshot {
        active_session_id,
        new_conversation_id,
        saved_at: tiangong_central::now_text(),
    };
    let mode = state.mode_coordinator.request_enter(snapshot);
    state
        .mode_coordinator
        .persist_enter_preference(state.core_manager.storage_root());
    Ok(json!({ "mode": mode.as_str() }))
}

/// 退出中枢：归还普通上下文快照。
///
/// `snapshot == null` 表示无可用恢复点（异常进入路径）——前端必须显示
/// 「原会话不可恢复」并提供「打开新普通对话 / 查看会话列表」选项。
#[tauri::command]
pub async fn central_mode_exit(app: AppHandle) -> Result<serde_json::Value, String> {
    let state = app.state::<TiangongApp>();
    let snapshot = state.mode_coordinator.request_exit();
    state
        .mode_coordinator
        .persist_exit_preference(state.core_manager.storage_root());
    Ok(json!({
        "mode": "normal",
        "snapshot": snapshot,
    }))
}

/// 模式状态查询（前端对账与重启恢复依据）。
#[tauri::command]
pub async fn central_mode_state(app: AppHandle) -> Result<serde_json::Value, String> {
    let state = app.state::<TiangongApp>();
    let mode = state.mode_coordinator.state();
    let preference = crate::central_host::mode_coordinator::load_mode_preference(
        state.core_manager.storage_root(),
    );
    Ok(json!({
        "mode": mode.as_str(),
        "last_mode": preference.last_mode,
    }))
}
