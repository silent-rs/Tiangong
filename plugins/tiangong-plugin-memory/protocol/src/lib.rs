//! Memory 插件私有业务协议。
//!
//! 本 crate 只定义 WASM 与 sidecar 共同使用的操作、请求和响应，不包含 IPC、
//! 进程、文件系统、模型或 Wasmtime 依赖。

use serde::Serialize;
use serde::de::DeserializeOwned;

pub mod control;
pub mod error;
pub mod injection;
pub mod recall;
pub mod rumination;
pub mod ui;

pub const PLUGIN_ID: &str = "memory";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MEMORY_PROTOCOL_VERSION: u32 = 1;
/// 宿主清单直连的对外工具名（operation = 工具名，返回 ToolOutcome 形状）。
pub const TOOL_RECALL_MEMORY: &str = "recall_memory";
/// 宿主生命周期钩子转发操作（payload = PluginSession 快照 JSON；
/// session_ready / turn_started 仅含会话身份）。
pub const LIFECYCLE_SESSION_READY_OPERATION: &str = "lifecycle.on_session_ready";
pub const LIFECYCLE_TURN_STARTED_OPERATION: &str = "lifecycle.on_turn_started";
pub const LIFECYCLE_TURN_FINISHED_OPERATION: &str = "lifecycle.on_turn_finished";
pub const LIFECYCLE_SESSION_ENDED_OPERATION: &str = "lifecycle.on_session_ended";

/// 一个类型化 Memory 业务操作。
pub trait MemoryOperation {
    const NAME: &'static str;
    type Request: Serialize;
    type Response: DeserializeOwned;
}

#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct Empty {}

#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct Ack {
    #[serde(default)]
    pub kind: Option<String>,
}
