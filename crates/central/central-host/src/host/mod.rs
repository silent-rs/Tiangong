//! 中枢宿主层模块（原 `src-tauri/src/central_host`，R6 抽离为独立 crate）。
pub mod activity;
pub mod authorization;
pub mod central_events;
pub mod core_lifecycle;
pub mod dispatch;
pub mod events;
pub mod execution_router;
pub mod mode_commands;
pub mod mode_coordinator;
pub mod recovery;
pub mod reports;
pub mod work_view;

pub use events::emit_central_work_updated;
pub use execution_router::ExecutionRouter;
pub use recovery::inject_central_activation_summary;
pub use recovery::recover_central_work_state;
