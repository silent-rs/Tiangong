#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod app;
pub mod commands;
mod config_handoff;
mod core_factory;
mod embedded_server;
#[cfg(target_os = "macos")]
pub mod inactive_hover;
pub mod plugin_instances;
pub mod session_input;
mod session_ops;
mod state_ops;
pub mod view;
pub mod web_config;
pub mod webview_host;

pub use app::{TiangongApp, ToolInjection};
