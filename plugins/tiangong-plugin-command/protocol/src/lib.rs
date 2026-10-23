//! Command 插件私有业务协议。
//!
//! 本 crate 只定义宿主封套与 sidecar 共同使用的操作、请求和响应，不包含 IPC、
//! 进程、子进程或文件系统依赖。
//!
//! `CommandAccessContext` 只携带当前会话的权威工作区。目录、环境和系统能力
//! 边界由 Runtime 与 Launcher 统一实施，业务协议不再复制命令白名单策略。

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub mod exec;

pub const PLUGIN_ID: &str = "command";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const COMMAND_PROTOCOL_VERSION: u32 = 1;

/// 工具名常量（与工具规格、handle_tool 路由对齐）。
pub const TOOL_RUN_COMMAND: &str = "run_command";
pub const TOOL_RUN_SHELL: &str = "run_shell";

/// 一个类型化 Command 业务操作。
///
/// 每个操作由零字段 marker struct 实现，提供操作名常量与关联的请求/响应类型。
/// v2 清单直连下 `NAME` 即工具名（sidecar dispatch 同口径）。
pub trait CommandOperation {
    const NAME: &'static str;
    type Request: Serialize;
    type Response: DeserializeOwned;
}

/// 当前会话的命令执行上下文。
///
/// 这里只携带 cwd 解析所需的权威工作区；访问能力由宿主沙箱策略决定。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandAccessContext {
    /// 当前会话工作目录（cwd 解析的 base）。
    #[serde(default)]
    pub workspace: Option<String>,
}
