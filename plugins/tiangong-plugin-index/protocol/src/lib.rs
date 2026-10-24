//! Index 插件私有业务协议。
//!
//! 本 crate 只定义 WASM 与 sidecar 共同使用的操作、请求和响应，不包含 IPC、
//! 进程、文件系统、tantivy 或 Wasmtime 依赖。可同时编译为本机与 `wasm32-wasip2`。

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub mod lifecycle;
pub mod management;
pub mod search;

pub const PLUGIN_ID: &str = "index";

/// 宿主清单直连工具名（operation = 工具名，返回 ToolOutcome 形状）。
/// 宿主生命周期钩子转发操作（payload = PluginSession 快照 JSON）。
pub const LIFECYCLE_SESSION_READY_OPERATION: &str = "lifecycle.on_session_ready";
pub const LIFECYCLE_TURN_STARTED_OPERATION: &str = "lifecycle.on_turn_started";
pub const LIFECYCLE_TURN_FINISHED_OPERATION: &str = "lifecycle.on_turn_finished";
pub const LIFECYCLE_SESSION_ENDED_OPERATION: &str = "lifecycle.on_session_ended";
/// 动态提及候选查询（mention.candidates_op；载荷 MentionQuery JSON）。
pub const MENTION_CANDIDATES_OPERATION: &str = "index.mention_candidates";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const INDEX_PROTOCOL_VERSION: u32 = 1;

/// 工具名常量（与工具规格、handle_tool 路由对齐）。
pub const TOOL_INDEX_SEARCH: &str = "index_search";
pub const TOOL_SEARCH_CODE: &str = "search_code";

/// 一个类型化 Index 业务操作。
///
/// 每个操作由零字段 marker struct 实现，提供操作名常量与关联的请求/响应类型。
/// WASM 端通过 `sidecar_client::invoke::<O>()` 泛型调用，以 `NAME` 作为 operation、
/// 序列化 `Request`、反序列化 `Response`。
pub trait IndexOperation {
    const NAME: &'static str;
    type Request: Serialize;
    type Response: DeserializeOwned;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Empty {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ack {}

/// 索引搜索范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexScope {
    Workspace,
    Session,
    #[default]
    All,
}

/// 工作区文件索引命中。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexHit {
    pub path: String,
    pub language: String,
    pub scope: IndexScope,
}

/// 对话历史索引命中。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionIndexHit {
    pub turn_id: String,
    pub role: String,
    pub content: String,
}

/// 单轮对话数据（写入会话索引用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnData {
    pub turn_id: String,
    pub workspace_id: String,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub entity_names: Vec<String>,
}

/// 工作区索引信息（GUI 管理 / list 用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceIndexInfo {
    pub id: String,
    pub root: String,
    pub entry_count: usize,
    pub updated_at: String,
}
