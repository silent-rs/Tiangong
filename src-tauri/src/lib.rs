#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod app;
pub mod central;
pub use central_host;
pub mod commands;
mod config_handoff;
mod core_factory;
mod embedded_server;
#[cfg(target_os = "macos")]
pub mod inactive_hover;
pub mod plugin_instances;
pub mod remote;
pub mod session_input;
mod session_ops;
#[doc(hidden)]
pub mod state_ops;
pub mod updater_fallback;
pub mod view;
pub mod web_config;
pub mod webview_host;

pub use app::{TiangongApp, ToolInjection};

/// 集成测试入口（R7 测试集中）：中枢集成测试经此取宿主。
#[doc(hidden)]
pub fn state_of_for_test<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> &app::TiangongApp {
    use tauri::Manager;
    app.state::<app::TiangongApp>().inner()
}
