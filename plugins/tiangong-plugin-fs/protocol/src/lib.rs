//! Fs 插件私有业务协议。
//!
//! 本 crate 只定义 WASM 与 sidecar 共同使用的操作、请求和响应，不包含 IPC、
//! 进程、文件系统、锁表或 Wasmtime 依赖。可同时编译为本机与 `wasm32-wasip2`。
//!
//! 沙箱预留：`FsAccessContext` 把"当前会话的访问能力"显式建模成请求字段
//! （当前只有 `full_trust` + `workspace`），未来细化权限（如 allowed_roots、
//! deny_patterns）只需扩展该结构并 bump business-protocol，不动 WIT/wasm 桥接。

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub mod tools;

pub const PLUGIN_ID: &str = "fs";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const FS_PROTOCOL_VERSION: u32 = 1;

/// 工具名常量（与工具规格、handle_tool 路由对齐）。
///
/// 对模型只暴露 3 个工具：读类操作经 `file_read` 的 `action` 分派，写类
/// 操作经 `file_write` 的 `action` 分派，`current_time` 单独保留。
pub const TOOL_FILE_READ: &str = "file_read";
pub const TOOL_FILE_WRITE: &str = "file_write";
pub const TOOL_CURRENT_TIME: &str = "current_time";

/// `file_read` 的 action 取值。
pub const READ_ACTION_LIST: &str = "list";
pub const READ_ACTION_TREE: &str = "tree";
pub const READ_ACTION_READ: &str = "read";
pub const READ_ACTION_IMAGE: &str = "image";
pub const READ_ACTIONS: &[&str] = &[
    READ_ACTION_LIST,
    READ_ACTION_TREE,
    READ_ACTION_READ,
    READ_ACTION_IMAGE,
];

/// `file_write` 的 action 取值。
pub const WRITE_ACTION_WRITE: &str = "write";
pub const WRITE_ACTION_REPLACE: &str = "replace";
pub const WRITE_ACTION_PATCH: &str = "patch";
pub const WRITE_ACTIONS: &[&str] = &[WRITE_ACTION_WRITE, WRITE_ACTION_REPLACE, WRITE_ACTION_PATCH];

/// 一个类型化 Fs 业务操作。
///
/// 每个操作由零字段 marker struct 实现，提供操作名常量与关联的请求/响应类型。
/// WASM 端通过 `sidecar_client::invoke::<O>()` 泛型调用，以 `NAME` 作为 operation、
/// 序列化 `Request`、反序列化 `Response`。
pub trait FsOperation {
    const NAME: &'static str;
    type Request: Serialize;
    type Response: DeserializeOwned;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ack {}

/// 当前会话的访问能力（沙箱预留点 B）。
///
/// 当前仅承载 `full_trust` 与 `workspace`；未来引入沙箱（路径白名单、
/// deny 列表等）时，扩展本结构并 bump business-protocol 即可，WIT/wasm 不动。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsAccessContext {
    /// 当前会话工作目录（路径解析的 base，download/写盘基准）。
    #[serde(default)]
    pub workspace: Option<String>,
    /// 是否完全信任模式（放宽读路径越界校验，写仍受限）。
    #[serde(default)]
    pub full_trust: bool,
}
