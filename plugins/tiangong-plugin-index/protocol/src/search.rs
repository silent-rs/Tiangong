//! 检索工具链路操作（index_search / search_code）。

use serde::{Deserialize, Serialize};

use crate::{IndexHit, IndexOperation, IndexScope, SessionIndexHit};

pub const INDEX_SEARCH_OPERATION: &str = "index.search";
pub const SEARCH_CODE_OPERATION: &str = "index.search_code";
/// `@` 提及文件候选：输入框补全用，只查 path 字段（不查内容）。
/// 操作名与清单 `mention.candidates_op` 一致（`index.mention_candidates`）。
pub const MENTION_CANDIDATES_OPERATION: &str = "index.mention_candidates";

/// `index_search` 工具请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexSearchRequest {
    /// 搜索关键词。
    pub query: String,
    /// 搜索范围。
    #[serde(default)]
    pub scope: IndexScope,
    /// 最多返回条数。
    #[serde(default)]
    pub limit: usize,
    /// 当前会话工作目录（workspace 范围必填）。
    #[serde(default)]
    pub workspace: Option<String>,
    /// 当前会话 ID（session 范围必填）。
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `index_search` 工具响应：聚合 workspace + session 命中。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexSearchResponse {
    pub workspace_hits: Vec<IndexHit>,
    pub session_hits: Vec<SessionIndexHit>,
    /// 后台扫描进行中时为 true（workspace 部分降级提示）。
    #[serde(default)]
    pub scanning: bool,
}

pub struct IndexSearch;
impl IndexOperation for IndexSearch {
    const NAME: &'static str = INDEX_SEARCH_OPERATION;
    type Request = IndexSearchRequest;
    type Response = IndexSearchResponse;
}

/// `search_code` 工具请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchCodeRequest {
    /// 检索文本或正则模式。
    pub pattern: String,
    /// 目标目录或文件路径，默认当前目录。
    #[serde(default)]
    pub path: Option<String>,
    /// 当前会话工作目录（current_dir 注入）。
    #[serde(default)]
    pub workspace: Option<String>,
    /// 是否完全信任模式（放宽工作区外路径校验）。
    #[serde(default)]
    pub full_trust: bool,
}

/// `search_code` 工具响应：保留与 core `ToolResult` 同构字段，便于 sidecar 直接构造。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchCodeResponse {
    pub ok: bool,
    pub summary: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i64,
}

pub struct SearchCode;
impl IndexOperation for SearchCode {
    const NAME: &'static str = SEARCH_CODE_OPERATION;
    type Request = SearchCodeRequest;
    type Response = SearchCodeResponse;
}

/// `@` 提及文件候选请求。
///
/// 与 {@link IndexSearchRequest} 分开：mention 只查 path 字段（用户要的是
/// 「指向某个文件」，不是搜内容），scope 固定 Workspace、limit 更严，且查询
/// 词可含空格（按词 AND 匹配）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MentionFilesRequest {
    /// 查询词（用户在 `@` 之后输入的内容，可含空格）。
    #[serde(default)]
    pub query: String,
    /// 返回数量上限。
    #[serde(default)]
    pub limit: usize,
    /// 工作区根目录（由宿主按会话上下文注入）。
    #[serde(default)]
    pub workspace: Option<String>,
}

/// 单个文件候选。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MentionFileCandidate {
    /// 相对工作区的路径（统一 `/` 分隔，跨平台可原样往返）。
    pub relative_path: String,
    /// 文件名（展示用）。
    pub file_name: String,
}

/// 文件候选响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MentionFilesResponse {
    pub candidates: Vec<MentionFileCandidate>,
    /// 后台扫描进行中为 true：此时索引不完整，候选可能缺失。
    #[serde(default)]
    pub scanning: bool,
}

pub struct MentionFiles;
impl IndexOperation for MentionFiles {
    const NAME: &'static str = MENTION_CANDIDATES_OPERATION;
    type Request = MentionFilesRequest;
    type Response = MentionFilesResponse;
}
