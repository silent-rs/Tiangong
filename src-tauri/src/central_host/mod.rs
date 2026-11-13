//! 中枢宿主层（重构方案 §5.1 `central-host` 的落位，R1 从 `app.rs`
//! 按职责拆分）：执行路由、Core 生命周期、派发执行器、终态回报、
//! 事件广播与重启恢复。app.rs 只保留应用装配与非中枢职责。

pub mod authorization;
pub(crate) mod central_events;
mod core_lifecycle;
pub(crate) mod dispatch;
pub(crate) mod events;
mod execution_router;
pub mod mode_commands;
pub mod mode_coordinator;
pub(crate) mod recovery;
mod reports;
pub mod work_view;

pub(crate) use events::emit_central_work_updated;
pub use execution_router::ExecutionRouter;
pub(crate) use recovery::inject_central_activation_summary;
pub use recovery::recover_central_work_state;

/// 从 AppHandle 取 TiangongApp（派发执行器/插件的共享状态入口）。
pub(crate) fn state_of<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> &crate::app::TiangongApp {
    use tauri::Manager;
    app.state::<crate::app::TiangongApp>().inner()
}
