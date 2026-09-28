use serde::{Deserialize, Serialize};

use crate::recall::{RecallAnchors, RecallRequest, RecallResponse};
use crate::{Ack, Empty, MemoryOperation};

pub const CONFIG_GET_OPERATION: &str = "ui.memory.config.get";
pub const CONFIG_SET_OPERATION: &str = "ui.memory.config.set";
/// 真实请求在线端点：Embedding 返回向量维度，Rerank / LLM 校验连通性。
pub const CONFIG_PROBE_OPERATION: &str = "ui.memory.config.probe";
pub const LIST_NODES_OPERATION: &str = "list_nodes";
pub const COUNT_NODES_OPERATION: &str = "count_nodes";
pub const LIST_RELATIONS_OPERATION: &str = "list_relations";
pub const LIST_RELATIONS_BATCH_OPERATION: &str = "list_relations_batch";
pub const UPSERT_MANUAL_MEMORY_OPERATION: &str = "upsert_manual_memory";
pub const SET_NODE_STATUS_OPERATION: &str = "set_node_status";
pub const UPSERT_RELATION_OPERATION: &str = "upsert_relation";
pub const DELETE_RELATION_OPERATION: &str = "delete_relation";

/// 页面 / CLI 配置视图（与 sidecar 侧 `MemoryConfigSelection` 结构一致）。
///
/// 密钥不回传：读取时只给 `has_api_key`，保存时 `api_key` 为空表示保留原值。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemorySelection {
    /// 本地内置模型档位：low | mid | high。
    #[serde(default)]
    pub local_tier: String,
    #[serde(default)]
    pub llm: MemoryLlmSelection,
    #[serde(default)]
    pub embedding: MemoryComponentSelection,
    #[serde(default)]
    pub rerank: MemoryComponentSelection,
    #[serde(default = "default_vector_mode")]
    pub vector_mode: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryLlmSelection {
    /// models_ref（从模型列表选择，key 为空时跟随 lite → chat）| remote
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<MemoryRemoteSelection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryComponentSelection {
    /// disabled | builtin | remote
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<MemoryRemoteSelection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryRemoteSelection {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<usize>,
    /// 只写：非空时替换密钥。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// 只读：当前是否已保存密钥。
    #[serde(default)]
    pub has_api_key: bool,
}

/// LLM 快速选择候选（仅 chat 能力模型与 chat/lite 路由）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryUiModel {
    pub key: String,
    pub provider: String,
    pub model: String,
    pub capabilities: Vec<String>,
    /// 候选类型：route（路由槽位）| model（注册表模型）。
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryBootstrap {
    pub config: MemorySelection,
    pub models: Vec<MemoryUiModel>,
    /// 当前默认 LLM 解析结果说明（如 "lite · step-mini"），未配置为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_llm: Option<String>,
    #[serde(default)]
    pub disabled: bool,
    /// 内置本地模型（各档位 embedding / rerank）状态。
    #[serde(default)]
    pub local_models: Vec<MemoryLocalModel>,
}

/// 内置本地模型状态。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryLocalModel {
    /// low | mid | high
    pub tier: String,
    /// embedding | rerank
    pub kind: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<usize>,
    /// 总字节数。
    pub size: u64,
    /// not_downloaded | downloading | loading | ready | installed | failed
    pub state: String,
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct GetConfig;

impl MemoryOperation for GetConfig {
    const NAME: &'static str = CONFIG_GET_OPERATION;
    type Request = Empty;
    type Response = MemoryBootstrap;
}

pub struct SetConfig;

impl MemoryOperation for SetConfig {
    const NAME: &'static str = CONFIG_SET_OPERATION;
    type Request = MemorySelection;
    type Response = Ack;
}

/// 在线端点探测请求。`api_key` 为空时使用已保存配置中对应组件的密钥。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProbeRequest {
    /// llm | embedding | rerank
    pub component: String,
    pub remote: MemoryRemoteSelection,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProbeResponse {
    pub ok: bool,
    /// Embedding 探测到的向量维度。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<usize>,
    #[serde(default)]
    pub message: String,
}

pub struct ProbeConfig;

impl MemoryOperation for ProbeConfig {
    const NAME: &'static str = CONFIG_PROBE_OPERATION;
    type Request = ProbeRequest;
    type Response = ProbeResponse;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Episode,
    Entity,
    Decision,
    Evidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCognitiveType {
    #[default]
    Factual,
    UserPreference,
    UserHabit,
    Skill,
    ProjectStructure,
    ArchitectureDecision,
    ProblemIncident,
    DomainKnowledge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScopeType {
    Global,
    Workspace,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    #[default]
    Active,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRelationKind {
    #[default]
    RelatedTo,
    DependsOn,
    Supports,
    Contradicts,
    Supersedes,
    CausedBy,
    BelongsTo,
    LearnedFrom,
    ValidatedBy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNode {
    pub id: String,
    pub kind: MemoryKind,
    #[serde(default)]
    pub memory_type: MemoryCognitiveType,
    pub scope_type: MemoryScopeType,
    pub scope_id: Option<String>,
    pub title: String,
    pub summary: String,
    pub keywords: Vec<String>,
    pub importance: f32,
    pub confidence: f32,
    pub status: MemoryStatus,
    pub source: Option<String>,
    pub usage_count: i64,
    pub last_used_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManualMemoryDraft {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub memory_type: MemoryCognitiveType,
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub importance: f32,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryListQuery {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub status: Option<MemoryStatus>,
    #[serde(default)]
    pub created_after: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRelation {
    pub id: String,
    pub from_node_id: String,
    pub to_node_id: String,
    pub relation_kind: MemoryRelationKind,
    pub weight: f32,
    pub note: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryRelationDraft {
    #[serde(default)]
    pub id: Option<String>,
    pub from_node_id: String,
    pub to_node_id: String,
    #[serde(default)]
    pub relation_kind: MemoryRelationKind,
    #[serde(default)]
    pub weight: f32,
    #[serde(default)]
    pub note: Option<String>,
}

macro_rules! operation {
    ($name:ident, $operation:expr, $request:ty, $response:ty) => {
        pub struct $name;
        impl MemoryOperation for $name {
            const NAME: &'static str = $operation;
            type Request = $request;
            type Response = $response;
        }
    };
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListNodesRequest {
    pub query: MemoryListQuery,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodesResponse {
    #[serde(default)]
    pub items: Vec<MemoryNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CountNodesRequest {
    pub query: MemoryListQuery,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeCountResponse {
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRelationsRequest {
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRelationsBatchRequest {
    pub node_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RelationsResponse {
    #[serde(default)]
    pub items: Vec<MemoryRelation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertManualMemoryRequest {
    pub draft: ManualMemoryDraft,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResponse {
    pub item: MemoryNode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetNodeStatusRequest {
    pub node_id: String,
    pub status: MemoryStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertRelationRequest {
    pub draft: MemoryRelationDraft,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationResponse {
    pub item: MemoryRelation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteRelationRequest {
    pub relation_id: String,
}

operation!(
    ListNodes,
    LIST_NODES_OPERATION,
    ListNodesRequest,
    NodesResponse
);
operation!(
    CountNodes,
    COUNT_NODES_OPERATION,
    CountNodesRequest,
    NodeCountResponse
);
operation!(
    ListRelations,
    LIST_RELATIONS_OPERATION,
    ListRelationsRequest,
    RelationsResponse
);
operation!(
    ListRelationsBatch,
    LIST_RELATIONS_BATCH_OPERATION,
    ListRelationsBatchRequest,
    RelationsResponse
);
operation!(
    UpsertManualMemory,
    UPSERT_MANUAL_MEMORY_OPERATION,
    UpsertManualMemoryRequest,
    NodeResponse
);
operation!(
    SetNodeStatus,
    SET_NODE_STATUS_OPERATION,
    SetNodeStatusRequest,
    Ack
);
operation!(
    UpsertRelation,
    UPSERT_RELATION_OPERATION,
    UpsertRelationRequest,
    RelationResponse
);
operation!(
    DeleteRelation,
    DELETE_RELATION_OPERATION,
    DeleteRelationRequest,
    Ack
);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum UiRequest {
    ListNodes {
        query: MemoryListQuery,
    },
    CountNodes {
        query: MemoryListQuery,
    },
    ListRelations {
        node_id: String,
    },
    ListRelationsBatch {
        node_ids: Vec<String>,
    },
    UpsertManualMemory {
        draft: ManualMemoryDraft,
    },
    SetNodeStatus {
        node_id: String,
        status: MemoryStatus,
    },
    UpsertRelation {
        draft: MemoryRelationDraft,
    },
    DeleteRelation {
        relation_id: String,
    },
    Recall {
        anchors: RecallAnchors,
        limit: usize,
    },
}

impl From<RecallRequest> for UiRequest {
    fn from(request: RecallRequest) -> Self {
        Self::Recall {
            anchors: request.anchors,
            limit: request.limit,
        }
    }
}

pub type UiRecallResponse = RecallResponse;

fn default_vector_mode() -> String {
    "auto".to_string()
}
