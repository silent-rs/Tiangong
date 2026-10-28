//! MCP 插件私有业务协议。
//!
//! 本 crate 只定义 WASM 与 sidecar 共同使用的操作、请求和响应，不包含 IPC、
//! 进程、文件系统、rmcp 或 Wasmtime 依赖。可同时编译为本机与 `wasm32-wasip2`。

use serde::Serialize;
use serde::de::DeserializeOwned;

pub mod capability;
pub mod config;
pub mod env;
pub mod error;
pub mod management;
pub mod query;
pub mod tool;

pub const PLUGIN_ID: &str = "mcp";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MCP_PROTOCOL_VERSION: u32 = 1;
/// @提及候选操作（v2 `mention.candidates_op` 声明）：响应为
/// `{ entry: { hint, mark? }, candidates: [ { value, label, kind, hint, mark? } ] }`。
pub const MCP_MENTION_CANDIDATES_OPERATION: &str = "mcp.mention_candidates";

/// 一个类型化 MCP 业务操作。
///
/// 每个操作由零字段 marker struct 实现，提供操作名常量与关联的请求/响应类型。
/// WASM 端通过 `sidecar_client::invoke::<O>()` 泛型调用，以 `NAME` 作为 operation、
/// 序列化 `Request`、反序列化 `Response`。
pub trait McpOperation {
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

/// 通用字符串响应（管理 CRUD 操作的返回值，如「MCP server 已注册：xxx」）。
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct MessageResponse {
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct NameFilterRequest {
    /// 可选的名称过滤；为空表示全部。
    #[serde(default)]
    pub filter: Option<String>,
}
