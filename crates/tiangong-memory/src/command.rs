//! Memory Actor 消息协议定义

use tokio::sync::oneshot;

use crate::options::MemoryOptions;
use crate::types::{
    EnhancedTurnResult, Episode, ExpandedMemory, ManualMemoryDraft, MemoryCandidate,
    MemoryListQuery, MemoryNode, MemoryRecallRequest, MemoryRecallResponse, MemoryRelation,
    MemoryRelationDraft, MemoryStatus, RecallAnchors, RecallHit, RecallSufficiency,
    RuntimeRecallContext, TurnResult,
};

/// 注入级别
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum InjectionLevel {
    Profile,
    Workspace,
    Session,
}

/// Memory Actor 接收的命令
pub enum MemoryCommand {
    // ── 查询类（需要响应）──
    LoadInjection {
        session_id: String,
        workspace_id: Option<String>,
        reply: oneshot::Sender<Vec<String>>,
    },
    Recall {
        anchors: RecallAnchors,
        limit: usize,
        reply: oneshot::Sender<Vec<RecallHit>>,
    },
    RecallContext {
        request: MemoryRecallRequest,
        reply: oneshot::Sender<MemoryRecallResponse>,
    },
    RoughRecall {
        context: RuntimeRecallContext,
        reply: oneshot::Sender<Vec<RecallHit>>,
    },
    EvaluateRecallSufficiency {
        context: RuntimeRecallContext,
        rough_hits: Vec<RecallHit>,
        reply: oneshot::Sender<RecallSufficiency>,
    },
    LoadDepth2 {
        node_ids: Vec<String>,
        reply: oneshot::Sender<Vec<ExpandedMemory>>,
    },
    ListNodes {
        query: MemoryListQuery,
        reply: oneshot::Sender<Vec<MemoryNode>>,
    },
    CountNodes {
        query: MemoryListQuery,
        reply: oneshot::Sender<usize>,
    },
    ListRelations {
        node_id: String,
        reply: oneshot::Sender<Vec<MemoryRelation>>,
    },
    /// 批量查询多个节点的关联关系（修复 N+1 性能问题）
    ListRelationsBatch {
        node_ids: Vec<String>,
        reply: oneshot::Sender<Vec<MemoryRelation>>,
    },

    // ── 写入类（fire-and-forget）──
    WriteEpisode {
        episode: Episode,
        /// 显式 workspace_id，为 None 时由 Actor 自身 workspace_id 兜底
        workspace_id: Option<String>,
    },
    UpdateInjection {
        level: InjectionLevel,
        target_id: String,
        content: String,
    },
    UpsertManualMemory {
        draft: ManualMemoryDraft,
        reply: oneshot::Sender<Result<MemoryNode, String>>,
    },
    SetNodeStatus {
        node_id: String,
        status: MemoryStatus,
        reply: oneshot::Sender<Result<(), String>>,
    },
    UpsertRelation {
        draft: MemoryRelationDraft,
        reply: oneshot::Sender<Result<MemoryRelation, String>>,
    },
    DeleteRelation {
        relation_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },

    // ── 反刍类（fire-and-forget）──
    SubmitCandidate {
        candidate: MemoryCandidate,
    },
    RunMicroRumination {
        turn_result: Box<TurnResult>,
    },
    /// 将增强版 Micro 反刍快速投递到独立 worker。
    EnqueueEnhancedMicroRumination {
        turn_result: Box<EnhancedTurnResult>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// 提交后台 worker 已完成模型提取的增强版 Micro 结果。
    ApplyEnhancedMicroRumination {
        turn_result: Box<EnhancedTurnResult>,
        extraction: Box<crate::types::ExtractionOutput>,
    },
    RunMesoRumination {
        session_id: String,
        workspace_id: String,
    },
    RunMetaRumination,
    /// 提交后台 worker 已完成 LLM 提炼的 Meso 结果（Actor 串行落库）。
    ApplyMesoRumination {
        outcome: Box<crate::rumination::MesoOutcome>,
    },
    /// 提交后台 worker 已完成评估的 Meta 结果（Actor 串行归档）。
    ApplyMetaRumination {
        outcome: Box<crate::rumination::MetaOutcome>,
    },

    // ── 生命周期 ──
    Reconfigure {
        options: Box<MemoryOptions>,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    /// 向量回填批次的 embedding 已在后台算完（内部命令）；`generation` 过期时丢弃。
    ///
    /// embedding 可能是远程端点，单次请求可达数十秒；放在 Actor 主循环里同步
    /// 等待会让召回、写入和重配置一起排队，所以只把向量写入留在 Actor。
    BackfillVectorsReady {
        generation: u64,
        nodes: Vec<crate::types::MemoryNode>,
        vectors: Result<Vec<Vec<f32>>, String>,
    },
    /// 内置本地模型后台加载完成（内部命令）；`generation` 过期时丢弃。
    LocalModelsReady {
        generation: u64,
        embedding: Option<std::sync::Arc<dyn tiangong_llm::EmbeddingProvider>>,
        rerank: Option<std::sync::Arc<dyn tiangong_llm::RerankProvider>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}
