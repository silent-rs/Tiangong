//! 通用记忆服务层：MCP 与 HTTP 守护模式共用的输入、输出与操作。
//!
//! 天工内通过 WASM 生命周期钩子自动接入；外部 Agent 通过本层显式调用
//! 回忆、写入、上报轮次与结束会话，二者最终走同一个 Memory Leader。

use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tiangong_memory::election::ManagedMemory;
use tiangong_memory::external::{ExternalTurn, normalize_workspace_id};
use tiangong_memory::{
    ManualMemoryDraft, MemoryCognitiveType, MemoryHandle, MemoryListQuery, MemoryNode,
    MemoryRecallRequest, MemoryRecallResponse, MemoryStatus, RecallAnchors, RecallHit,
};

const DEFAULT_LIST_LIMIT: usize = 20;
const MAX_LIST_LIMIT: usize = 100;
const DEFAULT_SEARCH_LIMIT: usize = 8;
const MAX_SEARCH_LIMIT: usize = 30;

/// 服务错误：区分调用方输入问题、Memory 已禁用与内部错误。
#[derive(Debug)]
pub enum ServiceError {
    Invalid(String),
    Disabled,
    Internal(anyhow::Error),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "{message}"),
            Self::Disabled => write!(f, "Memory 已禁用，请先执行 enable 或在配置页启用"),
            Self::Internal(error) => write!(f, "{error:#}"),
        }
    }
}

impl From<anyhow::Error> for ServiceError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

pub type ServiceResult<T> = Result<T, ServiceError>;

/// 解析调用方 JSON 参数；失败视为输入错误。
pub fn parse_input<T: for<'de> Deserialize<'de>>(value: serde_json::Value) -> ServiceResult<T> {
    serde_json::from_value(value)
        .map_err(|error| ServiceError::Invalid(format!("参数无效：{error}")))
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecallInput {
    pub query: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub expected: Vec<String>,
    #[serde(default)]
    pub context: Vec<String>,
    #[serde(default)]
    pub limit: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchInput {
    pub query: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchOutput {
    pub hits: Vec<RecallHit>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RememberInput {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub memory_type: MemoryCognitiveType,
    #[serde(default)]
    pub importance: Option<f32>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordTurnOutput {
    pub accepted: bool,
    pub turn_id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EndSessionInput {
    pub session_id: String,
    pub workspace: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListInput {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    /// `active` | `archived`，缺省全部。
    #[serde(default)]
    pub status: Option<MemoryStatus>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListOutput {
    pub total: usize,
    pub items: Vec<MemoryNode>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ForgetInput {
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ack {
    pub ok: bool,
}

/// 记忆服务：每次调用时从 `ManagedMemory` 取当前句柄，
/// 以便 Follower 在 Leader 退出后自动接替时拿到新句柄。
#[derive(Clone)]
pub struct MemoryService {
    managed: Arc<ManagedMemory>,
}

impl MemoryService {
    pub fn new(managed: Arc<ManagedMemory>) -> Self {
        Self { managed }
    }

    pub fn handle(&self) -> MemoryHandle {
        self.managed.handle()
    }

    fn ensure_enabled() -> ServiceResult<()> {
        if tiangong_memory::is_memory_disabled() {
            return Err(ServiceError::Disabled);
        }
        Ok(())
    }

    /// 深度回忆：规划检索、召回并整理成可直接阅读的上下文。
    pub async fn recall(&self, input: RecallInput) -> ServiceResult<MemoryRecallResponse> {
        Self::ensure_enabled()?;
        let query = input.query.trim().to_string();
        if query.is_empty() {
            return Err(ServiceError::Invalid("query 不能为空".to_string()));
        }
        let request = MemoryRecallRequest {
            query,
            reason: input.reason,
            expected: input.expected,
            context: input.context,
            limit: input.limit,
            progress: None,
        };
        Ok(self.handle().recall_context(request).await)
    }

    /// 快速检索：不调用 LLM，直接返回命中节点。
    pub async fn search(&self, input: SearchInput) -> ServiceResult<SearchOutput> {
        Self::ensure_enabled()?;
        let query = input.query.trim().to_string();
        if query.is_empty() && input.keywords.is_empty() {
            return Err(ServiceError::Invalid(
                "query 与 keywords 不能同时为空".to_string(),
            ));
        }
        let limit = match input.limit {
            0 => DEFAULT_SEARCH_LIMIT,
            limit => limit.min(MAX_SEARCH_LIMIT),
        };
        let anchors = RecallAnchors {
            keywords: input.keywords,
            query,
            strategy: None,
        };
        Ok(SearchOutput {
            hits: self.handle().recall(anchors, limit).await,
        })
    }

    /// 显式写入一条记忆。
    pub async fn remember(&self, input: RememberInput) -> ServiceResult<MemoryNode> {
        Self::ensure_enabled()?;
        let title = input.title.trim().to_string();
        let summary = input.summary.trim().to_string();
        if title.is_empty() || summary.is_empty() {
            return Err(ServiceError::Invalid(
                "title 与 summary 不能为空".to_string(),
            ));
        }
        let draft = ManualMemoryDraft {
            id: None,
            memory_type: input.memory_type,
            title,
            summary,
            keywords: input
                .keywords
                .into_iter()
                .map(|keyword| keyword.trim().to_string())
                .filter(|keyword| !keyword.is_empty())
                .collect(),
            importance: input.importance.unwrap_or(0.0).clamp(0.0, 1.0),
            workspace_id: normalize_workspace_id(input.workspace.as_deref()),
            session_id: input.session_id.filter(|id| !id.trim().is_empty()),
        };
        Ok(self
            .handle()
            .upsert_manual_memory(draft)
            .await
            .context("写入记忆失败")?)
    }

    /// 上报一轮对话，异步提取记忆（与天工 on_turn_finished 同一链路）。
    pub async fn record_turn(&self, turn: ExternalTurn) -> ServiceResult<RecordTurnOutput> {
        Self::ensure_enabled()?;
        let enhanced = turn
            .into_enhanced()
            .map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let turn_id = enhanced.turn_id.clone();
        self.handle()
            .run_enhanced_micro_rumination(enhanced)
            .await
            .context("提交轮次反刍失败")?;
        Ok(RecordTurnOutput {
            accepted: true,
            turn_id,
        })
    }

    /// 结束会话，触发工作区级整理（与天工 on_session_ended 同一链路）。
    pub async fn end_session(&self, input: EndSessionInput) -> ServiceResult<Ack> {
        Self::ensure_enabled()?;
        let session_id = input.session_id.trim().to_string();
        let workspace = normalize_workspace_id(Some(&input.workspace))
            .ok_or_else(|| ServiceError::Invalid("workspace 不能为空".to_string()))?;
        if session_id.is_empty() {
            return Err(ServiceError::Invalid("session_id 不能为空".to_string()));
        }
        self.handle().run_meso_rumination(session_id, workspace);
        Ok(Ack { ok: true })
    }

    pub async fn list(&self, input: ListInput) -> ServiceResult<ListOutput> {
        Self::ensure_enabled()?;
        let limit = match input.limit {
            0 => DEFAULT_LIST_LIMIT,
            limit => limit.min(MAX_LIST_LIMIT),
        };
        let query = MemoryListQuery {
            workspace_id: normalize_workspace_id(input.workspace.as_deref()),
            query: input.query.filter(|query| !query.trim().is_empty()),
            status: input.status,
            created_after: None,
            offset: input.offset,
            limit,
        };
        let handle = self.handle();
        let total = handle
            .count_nodes(MemoryListQuery {
                offset: 0,
                limit: 0,
                ..query.clone()
            })
            .await;
        let items = handle.list_nodes(query).await;
        Ok(ListOutput { total, items })
    }

    /// 归档（软删除）一条记忆，之后不再参与召回。
    pub async fn forget(&self, input: ForgetInput) -> ServiceResult<Ack> {
        Self::ensure_enabled()?;
        let node_id = input.node_id.trim().to_string();
        if node_id.is_empty() {
            return Err(ServiceError::Invalid("node_id 不能为空".to_string()));
        }
        self.handle()
            .set_node_status(node_id, MemoryStatus::Archived)
            .await
            .context("归档记忆失败")?;
        Ok(Ack { ok: true })
    }

    /// 当前配置与模型状态（禁用状态下也可查询）。
    pub async fn status(&self) -> ServiceResult<serde_json::Value> {
        self.dispatch(
            tiangong_plugin_memory_protocol::control::STATUS_OPERATION,
            serde_json::json!({}),
        )
        .await
    }

    /// 直接转发插件协议操作（配置页复用天工页面时使用）。
    pub async fn dispatch(
        &self,
        operation: &str,
        payload: serde_json::Value,
    ) -> ServiceResult<serde_json::Value> {
        let request = tiangong_plugin_runtime::protocol::Request::new(operation, payload);
        let response =
            tiangong_memory::ipc::dispatch_checked_plugin_request(self.handle(), request).await;
        if response.success {
            Ok(response.payload.unwrap_or(serde_json::Value::Null))
        } else {
            Err(ServiceError::Internal(anyhow::anyhow!(
                response
                    .error_message
                    .unwrap_or_else(|| format!("Memory 操作 {operation} 失败"))
            )))
        }
    }
}
