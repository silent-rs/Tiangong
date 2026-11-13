//! 中枢事件广播（R1 从 app.rs 拆出）：后台工作状态变化的前端通知入口。

use crate::app::TiangongApp;

/// 通知前端后台工作状态已变化（状态条刷新）。
pub(crate) fn emit_central_work_updated<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    use tauri::Emitter;
    let _ = app.emit("central-work-updated", ());
}

/// emit 的 state 侧入口：经注入的 app_handle 广播（测试环境未注入时
/// 静默跳过——host 集成测试因此无需 Wry 句柄）。
pub(crate) fn emit_central_work_updated_state(state: &TiangongApp) {
    if let Some(handle) = state.app_handle_read() {
        emit_central_work_updated(&handle);
    }
}

/// emit_central_work_updated 的跨模块入口（commands 复用）。
pub(crate) fn emit_central_work_updated_pub(app: &tauri::AppHandle) {
    emit_central_work_updated(app);
}
