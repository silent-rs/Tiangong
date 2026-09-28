//! 天工 Memory 系统 — 基础类型定义
//!
//! 定义 Memory 系统所有层次共用的数据结构。

use serde::{Deserialize, Serialize};
use std::path::Path;

/// 记忆节点类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Episode,
    Entity,
    Decision,
    Evidence,
}

/// 认知层面的记忆分类，用于决定记忆整理、召回和图关系展开策略。
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

/// 记忆节点之间的图关系类型。
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

/// 记忆范围类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScopeType {
    Global,
    Workspace,
    Session,
}

/// 记忆状态
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    #[default]
    Active,
    Archived,
}

/// 记忆节点元数据（对应 memory_nodes 表）
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

/// 手动新增或调整记忆的输入。
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

/// 手动记忆列表过滤条件。
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

/// 记忆图关系，近似图数据库中的边。
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

/// 手动新增或调整记忆关系的输入。
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

/// Episode 结果状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOutcome {
    Success,
    PartialSuccess,
    Failed,
    Abandoned,
}

/// 事件记忆（Episode）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: String,
    pub session_id: String,
    #[serde(default)]
    pub memory_type: MemoryCognitiveType,
    pub title: String,
    pub summary: String,
    pub outcome: EpisodeOutcome,
    pub keywords: Vec<String>,
    pub tool_calls: Vec<String>,
    pub importance: f32,
    pub created_at: String,
}

impl Episode {
    /// 创建新 Episode
    pub fn new(
        session_id: String,
        title: String,
        summary: String,
        outcome: EpisodeOutcome,
        keywords: Vec<String>,
        tool_calls: Vec<String>,
        importance: f32,
    ) -> Self {
        Self {
            id: scru128::new().to_string(),
            session_id,
            memory_type: MemoryCognitiveType::Factual,
            title,
            summary,
            outcome,
            keywords,
            tool_calls,
            importance,
            created_at: chrono::Local::now().naive_local().to_string(),
        }
    }

    /// 设置认知分类，保留 `new` 的兼容签名。
    pub fn with_memory_type(mut self, memory_type: MemoryCognitiveType) -> Self {
        self.memory_type = memory_type;
        self
    }
}

/// 实体类型
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Project,
    Repository,
    Server,
    Skill,
    Provider,
    Document,
    Module,
}

/// 实体记忆（Entity）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub name: String,
    pub entity_type: EntityType,
    pub description: String,
    pub file_path: Option<String>,
    pub related_episodes: Vec<String>,
    pub importance: f32,
    pub created_at: String,
    pub updated_at: String,
}

/// 决策记忆（Decision）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub title: String,
    pub context: String,
    pub alternatives: Vec<String>,
    pub chosen: String,
    pub reasons: Vec<String>,
    pub episode_ids: Vec<String>,
    pub created_at: String,
}

/// Profile 记忆条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileEntry {
    pub key: String,
    pub value: String,
    pub updated_at: String,
}

/// 检索策略（由 Core/LLM 决定，传入 Memory 执行层）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchStrategy {
    /// 跳过记忆检索（查询不需要历史记忆）
    Skip,
    /// 纯关键词检索（BM25）
    Keyword,
    /// 纯语义检索（向量）
    Semantic,
    /// 混合检索：semantic_ratio 为语义权重占比（0.0~1.0）
    Hybrid { semantic_ratio: f64 },
}

impl Default for SearchStrategy {
    fn default() -> Self {
        SearchStrategy::Hybrid {
            semantic_ratio: 0.5,
        }
    }
}

/// 回忆执行深度。
///
/// `recall_memory` 只是外部刺激入口；真正的深度由 Memory 在初始回忆后基于命中结果决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallDepth {
    /// 不需要回忆。
    Skip,
    /// 初始回忆已足够，直接快速整理。
    Simple,
    /// 常规召回与 Depth2 展开。
    #[default]
    Normal,
    /// 初始回忆不足，已执行后续深挖查询。
    Deep,
}

/// 召回锚点（Phase C 实现）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecallAnchors {
    pub keywords: Vec<String>,
    pub query: String,
    /// 检索策略（由 Core 传入，为 None 时 Memory 内部自行判断）
    #[serde(default)]
    pub strategy: Option<SearchStrategy>,
}

/// 召回命中项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallHit {
    pub node_id: String,
    pub title: String,
    pub summary: String,
    pub score: f64,
    pub kind: MemoryKind,
    pub importance: f64,
    pub depth1_loaded: bool,
}

/// 回忆进度回调：参数为当前阶段描述（如 "规划检索策略" / "检索中"）。
pub type MemoryRecallProgress = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// Tool 化回忆请求。
///
/// Core 只提供当前请求和最近语境；检索规划、去重和结果整理由 Memory 内部完成。
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct MemoryRecallRequest {
    pub query: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub expected: Vec<String>,
    #[serde(default)]
    pub context: Vec<String>,
    #[serde(default)]
    pub limit: usize,
    /// 进度回调：每进入一个检索阶段触发一次（如 "规划检索策略" / "检索中"）。
    ///
    /// 闭包不可序列化，跨 actor/IPC 边界会被 `#[serde(skip)]` 丢弃（自然降级为不发进度）。
    #[serde(skip)]
    pub progress: Option<MemoryRecallProgress>,
}

impl std::fmt::Debug for MemoryRecallRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryRecallRequest")
            .field("query", &self.query)
            .field("reason", &self.reason)
            .field("expected", &self.expected)
            .field("context", &self.context)
            .field("limit", &self.limit)
            .field(
                "progress",
                &self.progress.as_ref().map(|_| "<progress callback>"),
            )
            .finish()
    }
}

/// Tool 化回忆响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryRecallResponse {
    pub content: String,
    #[serde(default)]
    pub hits: Vec<RecallHit>,
    #[serde(default)]
    pub used_llm: bool,
    #[serde(default)]
    pub recall_depth: RecallDepth,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deep_queries: Vec<String>,
    /// 回忆过程中产生的 LLM token 消耗（anchor 规划 + 结果整理）。
    #[serde(default)]
    pub usage: tiangong_llm::TokenUsageData,
}

/// 运行时召回策略。
///
/// 先用纯搜索粗回忆；只有粗回忆不足时，调用方才升级到混合检索或
/// Tool 化上下文回忆。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRecallPolicy {
    #[serde(default = "default_rough_recall_limit")]
    pub rough_limit: usize,
    #[serde(default = "default_deep_recall_limit")]
    pub deep_limit: usize,
    #[serde(default = "default_true")]
    pub enable_hybrid_on_demand: bool,
    #[serde(default = "default_true")]
    pub enable_rerecall: bool,
}

impl Default for RuntimeRecallPolicy {
    fn default() -> Self {
        Self {
            rough_limit: default_rough_recall_limit(),
            deep_limit: default_deep_recall_limit(),
            enable_hybrid_on_demand: true,
            enable_rerecall: true,
        }
    }
}

/// 运行时召回上下文。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeRecallContext {
    pub query: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub next_action: Option<String>,
    #[serde(default)]
    pub current_context: Vec<String>,
    #[serde(default)]
    pub policy: RuntimeRecallPolicy,
}

/// 粗回忆是否足够支撑当前操作。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecallSufficiency {
    pub sufficient: bool,
    pub reason: String,
    #[serde(default)]
    pub missing: Vec<String>,
    #[serde(default)]
    pub next_query: Option<String>,
    #[serde(default)]
    pub should_upgrade_to_hybrid: bool,
}

fn default_rough_recall_limit() -> usize {
    5
}

fn default_deep_recall_limit() -> usize {
    10
}

fn default_true() -> bool {
    true
}

/// 工具执行后产生的记忆候选类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCandidateKind {
    Episode,
    Entity,
    Decision,
    Evidence,
    UserPreference,
}

/// 工具执行后的轻量记忆候选（无 LLM 调用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCandidate {
    pub tool_name: String,
    pub step_index: usize,
    pub hint: String,
    #[serde(default)]
    pub suggested_kinds: Vec<MemoryCandidateKind>,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub result_summary: Option<String>,
    pub success: bool,
    /// 工具来源分类（如 "memory_recall"、"file_operation"），供 Memory 分析层判断。
    #[serde(default)]
    pub tool_source: Option<String>,
    /// 标记是否为召回的已有记忆（不应作为新记忆写入）。
    #[serde(default)]
    pub is_recalled_context: bool,
}

/// 轮次中的对话消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnMessage {
    pub role: String,
    pub content: String,
}

/// 轮次执行状态。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    /// 轮次正常完成。
    #[default]
    Completed,
    /// 轮次被用户取消。
    Cancelled,
    /// 轮次因错误结束。
    Failed,
}

/// 扩展的轮次结果，在 TurnResult 基础上增加候选列表和对话消息。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnhancedTurnResult {
    pub session_id: String,
    pub turn_id: String,
    pub had_tool_calls: bool,
    /// 轮次执行状态。
    #[serde(default)]
    pub turn_status: TurnStatus,
    #[serde(default)]
    pub user_input: String,
    pub summary: String,
    #[serde(default)]
    pub tool_calls: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<TurnArtifact>,
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub memory_candidates: Vec<MemoryCandidate>,
    #[serde(default)]
    pub turn_messages: Vec<TurnMessage>,
}

/// 多类型记忆提取输出。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionOutput {
    #[serde(default)]
    pub episodes: Vec<Episode>,
    #[serde(default)]
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub decisions: Vec<Decision>,
    #[serde(default)]
    pub evidences: Vec<Evidence>,
}

/// 证据记忆，记录具体产物（文件路径、URL、工具结果摘要等）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub source_tool: Option<String>,
}

/// 向量索引点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorPoint {
    pub node_id: String,
    pub title: String,
    pub summary: String,
    pub kind: MemoryKind,
    pub importance: f64,
    pub vector: Vec<f32>,
}

/// 展开的记忆节点（Phase C 实现）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandedMemory {
    pub node_id: String,
    pub full_content: String,
}

/// Turn 中可被长期记忆保存的结构化产物类型。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnArtifactKind {
    Media,
    File,
    #[default]
    ToolResult,
}

/// Turn 中可被长期记忆保存的结构化产物。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnArtifact {
    pub kind: TurnArtifactKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Turn 执行结果（Phase B 实现）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnResult {
    pub session_id: String,
    pub turn_id: String,
    pub had_tool_calls: bool,
    #[serde(default)]
    pub user_input: String,
    pub summary: String,
    #[serde(default)]
    pub tool_calls: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<TurnArtifact>,
    /// 当前工作区 ID（显式携带，避免 Actor 固化到启动时工作区）
    pub workspace_id: Option<String>,
}

/// 根据工作区路径生成 workspace_id（SHA-256 前 16 字符）
pub fn workspace_id_from_path(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    hex::encode(&digest[..8]) // 16 字符
}
