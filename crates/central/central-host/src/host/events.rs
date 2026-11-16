//! 中枢事件广播（R1 从 app.rs 拆出）：后台工作状态变化的前端通知入口。
//! R5 消息卡片：授权请求事件（UI 是投影，方案 §3.6/§5.6）。
use serde::Serialize;

/// 授权请求数据（`central-approval-required` 事件载荷）。
///
/// 派发被 Workspace 授权拒绝时广播；前端渲染授权卡片，用户真实操作
/// 后经 `central_approve_workspace` 签发票据（模型不能自证同意）。
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalRequiredEvent {
    /// 待授权工作目录。
    pub cwd: String,
    /// 任务文本（= 派发时的任务指纹：授权与重派一致才放行，防偷换）。
    pub task: String,
    pub work_id: String,
}

/// 通知前端后台工作状态已变化（状态条刷新）。
pub fn emit_central_work_updated<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    use tauri::Emitter;
    let _ = app.emit("central-work-updated", ());
}

/// emit 的 state 侧入口（经注入的 app_handle 广播；测试环境未注入时
/// 静默跳过——host 集成测试因此无需 Wry 句柄）。
pub fn emit_central_work_updated_state(state: &crate::CentralHostState) {
    if let Some(handle) = state.app_handle_read() {
        emit_central_work_updated(&handle);
    }
}

/// 授权请求广播（R5 卡片）：派发被授权拒绝时通知前端渲染授权卡片。
pub fn emit_approval_required(state: &crate::CentralHostState, event: ApprovalRequiredEvent) {
    if let Some(handle) = state.app_handle_read() {
        use tauri::Emitter;
        let _ = handle.emit("central-approval-required", event);
    }
}

/// 宿主系统注入的渲染标记（R5 P1-3 修复）：中枢宿主注入的消息
/// （回报/授权/派发说明/激活摘要）不得伪装为用户消息（方案 §6.2）。
///
/// 前端按 `plugin = "central-host" && view = "system-note"` 识别，
/// 渲染为系统提示块（非用户气泡）。`kind` 供细分样式：
/// report / approval / dispatch-note / activation / recovery。
pub fn system_note_render(kind: &str) -> tiangong_types::MessageRender {
    tiangong_types::MessageRender {
        plugin: "central-host".to_string(),
        view: "system-note".to_string(),
        data: serde_json::json!({ "kind": kind }),
    }
}
