//! Subagent 插件私有业务协议。
//!
//! 定义持久 Agent（身份、激活、任务、运行、Hook）的数据结构与操作约定，
//! 供 sidecar、插件 UI 与后续宿主接入共用。本 crate 只包含可序列化类型，
//! 不依赖 IPC、进程或文件系统，可同时编译为本机与 `wasm32-wasip2`。

pub mod config;
pub mod hooks;
pub mod ops;
pub mod render;
pub mod state;

pub use config::{AdapterCapabilities, AgentConfig, BackendKind, WorkspacePolicy};
pub use hooks::{HookEvent, HookEventType};
pub use state::{ActivationRecord, AgentEventRecord, RunKind, RunRecord, RunStatus, TaskRecord};

/// 插件 ID（与 plugin.json 一致）。
pub const PLUGIN_ID: &str = "subagent";
/// 插件版本（与 plugin.json 一致，由 xtask 校验）。
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
/// 业务协议版本（与 protocol crate metadata 一致）。
pub const SUBAGENT_PROTOCOL_VERSION: u32 = 1;

/// sidecar → 插件 UI 的通知通道（状态变化时推送，UI 订阅 `sidecar.*` 事件）。
pub const NOTIFICATION_CHANNEL: &str = "subagent.event";

pub use ops::{MENTION_CANDIDATES, SESSION_TURN_FINISHED, TOOL_OPERATIONS};
