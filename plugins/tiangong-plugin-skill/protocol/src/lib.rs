//! Skill 插件私有业务协议。
//!
//! 本 crate 只定义 WASM 与 sidecar 共同使用的操作、请求和响应，不包含 IPC、
//! 进程、文件系统或 Wasmtime 依赖。可同时编译为本机与 `wasm32-wasip2`。
//!
//! sidecar 内部使用带 `Instant`/`SystemTime`/`PathBuf` 的运行时结构（不可序列化），
//! 在响应边界转换为这里的可序列化协议结构。

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const PLUGIN_ID: &str = "skill";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SKILL_PROTOCOL_VERSION: u32 = 1;

/// 工具名常量（与工具规格、handle_tool 路由对齐）。
pub const TOOL_GET_SKILL_DETAIL: &str = "get_skill_detail";

/// 一个类型化 Skill 业务操作。
///
/// 每个操作由零字段 marker struct 实现，提供操作名常量与关联的请求/响应类型。
/// WASM 端通过 `sidecar_client::invoke::<O>()` 泛型调用，以 `NAME` 作为 operation、
/// 序列化 `Request`、反序列化 `Response`。
pub trait SkillOperation {
    const NAME: &'static str;
    type Request: Serialize;
    type Response: DeserializeOwned;
}

// ── 操作名常量 ────────────────────────────────────────────────

pub const LIST_SKILLS_OPERATION: &str = "list_skills";
/// 设置页的结构化详情操作（与工具名 `get_skill_detail` 分离：工具直连返回
/// ToolOutcome 形状，管理页需要结构化 SkillDetailResponse）。
pub const SKILL_DETAIL_OPERATION: &str = "skill_detail";
/// 工具直连操作名（与清单 tools 声明一致，返回 ToolOutcome 形状）。
pub const TOOL_GET_SKILL_DETAIL_OPERATION: &str = TOOL_GET_SKILL_DETAIL;
pub const REMOVE_SKILL_OPERATION: &str = "remove_skill";
pub const SET_SKILL_ENABLED_OPERATION: &str = "set_skill_enabled";
pub const REFRESH_SKILLS_OPERATION: &str = "refresh_skills";
pub const GET_SKILL_ENV_OPERATION: &str = "get_skill_env";
pub const SET_SKILL_ENV_OPERATION: &str = "set_skill_env";
pub const GET_SKILL_SUMMARY_OPERATION: &str = "get_skill_summary";
pub const INIT_SKILL_OPERATION: &str = "init_skill";
pub const UPDATE_SKILL_MD_OPERATION: &str = "update_skill_md";
pub const REVEAL_SKILL_DIR_OPERATION: &str = "reveal_skill_dir";

/// @ 提及候选查询（manifest mention.candidates_op → sidecar，含插件前缀）。
pub const MENTION_CANDIDATES_OPERATION: &str = "skill.mention_candidates";

// ── marker 类型 ───────────────────────────────────────────────

pub struct ListSkills;
pub struct GetSkillDetail;
pub struct RemoveSkill;
pub struct SetSkillEnabled;
pub struct RefreshSkills;
pub struct GetSkillEnv;
pub struct SetSkillEnv;
pub struct GetSkillSummary;
pub struct InitSkill;
pub struct UpdateSkillMd;
pub struct RevealSkillDir;

impl SkillOperation for ListSkills {
    const NAME: &'static str = LIST_SKILLS_OPERATION;
    type Request = Empty;
    type Response = ListSkillsResponse;
}

impl SkillOperation for GetSkillDetail {
    const NAME: &'static str = SKILL_DETAIL_OPERATION;
    type Request = GetSkillDetailRequest;
    type Response = SkillDetailResponse;
}

impl SkillOperation for RemoveSkill {
    const NAME: &'static str = REMOVE_SKILL_OPERATION;
    type Request = RemoveSkillRequest;
    type Response = RemoveSkillResponse;
}

impl SkillOperation for SetSkillEnabled {
    const NAME: &'static str = SET_SKILL_ENABLED_OPERATION;
    type Request = SetSkillEnabledRequest;
    type Response = MessageResponse;
}

impl SkillOperation for RefreshSkills {
    const NAME: &'static str = REFRESH_SKILLS_OPERATION;
    type Request = Empty;
    type Response = MessageResponse;
}

impl SkillOperation for GetSkillEnv {
    const NAME: &'static str = GET_SKILL_ENV_OPERATION;
    type Request = GetSkillEnvRequest;
    type Response = GetSkillEnvResponse;
}

impl SkillOperation for SetSkillEnv {
    const NAME: &'static str = SET_SKILL_ENV_OPERATION;
    type Request = SetSkillEnvRequest;
    type Response = Empty;
}

impl SkillOperation for GetSkillSummary {
    const NAME: &'static str = GET_SKILL_SUMMARY_OPERATION;
    type Request = Empty;
    type Response = SkillSummaryResponse;
}

impl SkillOperation for InitSkill {
    const NAME: &'static str = INIT_SKILL_OPERATION;
    type Request = InitSkillRequest;
    type Response = InitSkillResult;
}

impl SkillOperation for UpdateSkillMd {
    const NAME: &'static str = UPDATE_SKILL_MD_OPERATION;
    type Request = UpdateSkillMdRequest;
    type Response = Empty;
}

impl SkillOperation for RevealSkillDir {
    const NAME: &'static str = REVEAL_SKILL_DIR_OPERATION;
    type Request = RevealSkillDirRequest;
    type Response = Empty;
}

// ── 通用结构 ──────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Empty {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageResponse {
    #[serde(default)]
    pub message: String,
}

// ── skill.toml / 配置类型（与 sidecar 内 SkillManifest 对齐，纯可序列化）──

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillSourceConfig {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillMcpRequirementConfig {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub source: String,
    #[serde(default, alias = "pkg")]
    pub package: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillPermissionConfig {
    #[serde(default)]
    pub fs_read: Vec<String>,
    #[serde(default)]
    pub fs_write: Vec<String>,
    #[serde(default)]
    pub cmd_exec: Vec<String>,
    #[serde(default)]
    pub net: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillManifestRequires {
    #[serde(default)]
    pub mcp: Vec<SkillMcpRequirementConfig>,
    #[serde(default)]
    pub env: Vec<String>,
}

/// skill.toml 清单（与磁盘格式对齐）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillManifest {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_entry")]
    pub entry: String,
    #[serde(default = "default_available")]
    pub available: bool,
    #[serde(default)]
    pub source: SkillSourceConfig,
    #[serde(default)]
    pub requires: SkillManifestRequires,
    #[serde(default)]
    pub permissions: SkillPermissionConfig,
}

impl Default for SkillManifest {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            version: String::new(),
            description: String::new(),
            entry: default_entry(),
            available: default_available(),
            source: SkillSourceConfig::default(),
            requires: SkillManifestRequires::default(),
            permissions: SkillPermissionConfig::default(),
        }
    }
}

fn default_entry() -> String {
    "index.js".to_string()
}

fn default_available() -> bool {
    true
}

// ── 已安装 Skill 摘要（设置页列表 + Agent prompt 用）──

/// 已安装 Skill 的轻量配置（只读 skill.toml，不含 SKILL.md 全文）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstalledSkillConfig {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub entry: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub installed_at: String,
    #[serde(default)]
    pub managed_mcp_servers: Vec<String>,
    #[serde(default)]
    pub source: SkillSourceConfig,
    #[serde(default)]
    pub requires_mcp: Vec<SkillMcpRequirementConfig>,
    #[serde(default)]
    pub permissions: SkillPermissionConfig,
}

/// 单个 Skill 详情（含 SKILL.md 全文）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillDetail {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub entry: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub readme: String,
}

/// prompt 段落注入用的 skill 摘要项（更精简）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillSummaryItem {
    pub id: String,
    pub name: String,
    pub description: String,
}

// ── 请求/响应类型 ─────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetSkillDetailRequest {
    pub id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoveSkillRequest {
    pub id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetSkillEnabledRequest {
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetSkillEnvRequest {
    pub id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetSkillEnvResponse {
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetSkillEnvRequest {
    pub id: String,
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListSkillsResponse {
    pub skills: Vec<InstalledSkillConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillDetailResponse {
    pub detail: SkillDetail,
}

/// remove_skill 返回：消息 + 需入口层清理的孤儿 MCP server 名。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoveSkillResponse {
    #[serde(default)]
    pub message: String,
    /// 不再被任何已安装 skill 引用的托管 MCP server 名（`skill::<id>::<mcp_id>`）。
    /// 入口层据此从 agent_config.mcp.servers 移除并 rebuild runtime。
    #[serde(default)]
    pub orphan_mcp_servers: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillSummaryResponse {
    /// enabled skill 的摘要（供 prompt 段落注入）。
    pub items: Vec<SkillSummaryItem>,
    /// skills 存储根目录（供 prompt 声明允许文件操作目录）。
    #[serde(default)]
    pub storage_root: String,
}

// ── skill init（脚手架生成）──

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InitSkillRequest {
    /// 目标目录路径（绝对路径）。
    pub path: String,
    /// skill 显示名（可选）。
    #[serde(default)]
    pub name: Option<String>,
    /// skill id（可选）。
    #[serde(default)]
    pub id: Option<String>,
    /// 覆盖已有文件。
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InitSkillResult {
    /// 生成的 skill 目录。
    pub dir: String,
    pub skill_id: String,
    pub skill_name: String,
}

// ── 编辑 SKILL.md / 打开目录 ──

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateSkillMdRequest {
    pub id: String,
    /// 新的 SKILL.md 全文。
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RevealSkillDirRequest {
    pub id: String,
}
