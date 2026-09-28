//! IPC 帧协议定义（TCP loopback + JSON Lines）

use serde::{Deserialize, Serialize};

use crate::command::InjectionLevel;
use crate::types::{
    EnhancedTurnResult, Episode, ExpandedMemory, ManualMemoryDraft, MemoryCandidate,
    MemoryListQuery, MemoryNode, MemoryRecallRequest, MemoryRecallResponse, MemoryRelation,
    MemoryRelationDraft, MemoryStatus, RecallAnchors, RecallHit, RecallSufficiency,
    RuntimeRecallContext, TurnResult,
};
pub use tiangong_plugin_runtime::protocol::{
    IpcAuth, IpcEndpoint, IpcFrame, IpcRequest, IpcResponse,
};

/// Memory IPC 请求载荷
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum MemoryIpcRequestPayload {
    LoadInjection {
        session_id: String,
        workspace_id: Option<String>,
    },
    Recall {
        anchors: RecallAnchors,
        limit: usize,
    },
    RecallContext {
        request: MemoryRecallRequest,
    },
    RoughRecall {
        context: RuntimeRecallContext,
    },
    EvaluateRecallSufficiency {
        context: RuntimeRecallContext,
        rough_hits: Vec<RecallHit>,
    },
    LoadDepth2 {
        node_ids: Vec<String>,
    },
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
    WriteEpisode {
        episode: Episode,
        workspace_id: Option<String>,
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
    Reconfigure {
        config: crate::MemoryConfig,
    },
    UpdateInjection {
        level: InjectionLevel,
        target_id: String,
        content: String,
    },
    RunMicroRumination {
        turn_result: TurnResult,
    },
    SubmitCandidate {
        candidate: MemoryCandidate,
    },
    RunEnhancedMicroRumination {
        turn_result: EnhancedTurnResult,
    },
    RunMesoRumination {
        session_id: String,
        workspace_id: String,
    },
    RunMetaRumination,
    Shutdown,
}

/// Memory IPC 响应载荷
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryIpcResponsePayload {
    Ack,
    Injection { items: Vec<String> },
    Recall { hits: Vec<RecallHit> },
    RecallContext { response: MemoryRecallResponse },
    RecallSufficiency { result: RecallSufficiency },
    Depth2 { items: Vec<ExpandedMemory> },
    Nodes { items: Vec<MemoryNode> },
    NodeCount { count: usize },
    Node { item: MemoryNode },
    Relations { items: Vec<MemoryRelation> },
    Relation { item: MemoryRelation },
}
