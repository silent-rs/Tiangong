//! Memory 系统客户端句柄
//!
//! 可任意 Clone 跨线程/任务使用，通过 mpsc channel 与 MemoryActor 通信。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::command::{InjectionLevel, MemoryCommand};
use crate::ipc::IpcClient;
use crate::ipc::protocol::{IpcRequest, MemoryIpcRequestPayload, MemoryIpcResponsePayload};
use crate::options::MemoryOptions;
use crate::types::{
    EnhancedTurnResult, Episode, ExpandedMemory, ManualMemoryDraft, MemoryCandidate,
    MemoryListQuery, MemoryNode, MemoryRecallRequest, MemoryRecallResponse, MemoryRelation,
    MemoryRelationDraft, MemoryStatus, RecallAnchors, RecallHit, RecallSufficiency,
    RuntimeRecallContext, TurnResult,
};

/// Memory 系统的客户端句柄，可任意 Clone 跨线程使用
#[derive(Clone)]
pub struct MemoryHandle {
    inner: Arc<HandleInner>,
}

enum HandleInner {
    Local { tx: mpsc::Sender<MemoryCommand> },
    Remote { client: Mutex<IpcClient> },
}

impl MemoryHandle {
    pub(crate) fn new(tx: mpsc::Sender<MemoryCommand>) -> Self {
        Self {
            inner: Arc::new(HandleInner::Local { tx }),
        }
    }

    pub async fn connect_tcp(service: &str) -> Result<Self> {
        let client = IpcClient::connect(service).await?;
        Ok(Self {
            inner: Arc::new(HandleInner::Remote {
                client: Mutex::new(client),
            }),
        })
    }

    /// 判断两个 handle 是否指向同一个底层 handle 实例。
    ///
    /// 主要用于 registry 生命周期测试和诊断：同一 workspace 的重复获取应返回
    /// 同一个 handle clone；不同 workspace 应创建不同 handle。
    pub fn is_same_handle(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// 异步热更新 Memory Actor 配置。
    pub async fn reconfigure(&self, options: MemoryOptions) -> Result<()> {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = std::sync::mpsc::channel();
                tx.send(MemoryCommand::Reconfigure {
                    options: Box::new(options),
                    reply: reply_tx,
                })
                .await
                .with_context(|| "发送 Memory 热更新命令失败")?;
                tokio::task::spawn_blocking(move || {
                    reply_rx
                        .recv_timeout(Duration::from_secs(30))
                        .with_context(|| "等待 Memory 热更新结果超时或失败")?
                        .map_err(|err| anyhow!(err))
                })
                .await
                .with_context(|| "等待 Memory 热更新任务失败")?
            }
            HandleInner::Remote { .. } => {
                let config = crate::MemoryConfig::from_options(&options);
                match self
                    .send_remote_request(MemoryIpcRequestPayload::Reconfigure { config })
                    .await?
                {
                    MemoryIpcResponsePayload::Ack => Ok(()),
                    other => Err(anyhow!(
                        "Memory IPC reconfigure 返回了非预期响应: {other:?}"
                    )),
                }
            }
        }
    }

    /// 加载注入上下文（查询，等待响应）
    pub async fn load_injection(
        &self,
        session_id: &str,
        workspace_id: Option<&str>,
    ) -> Vec<String> {
        match self.inner.as_ref() {
            HandleInner::Local { .. } => self.load_injection_local(session_id, workspace_id).await,
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::LoadInjection {
                        session_id: session_id.to_string(),
                        workspace_id: workspace_id.map(String::from),
                    })
                    .await
                {
                    Ok(MemoryIpcResponsePayload::Injection { items }) => items,
                    Ok(other) => {
                        tracing::warn!("Memory IPC load_injection 返回了非预期响应: {:?}", other);
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::warn!("Memory IPC load_injection 失败: {}", e);
                        Vec::new()
                    }
                }
            }
        }
    }

    /// 执行粗召回（查询，等待响应）
    pub async fn recall(&self, anchors: RecallAnchors, limit: usize) -> Vec<RecallHit> {
        match self.inner.as_ref() {
            HandleInner::Local { .. } => self.recall_local(anchors, limit).await,
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::Recall { anchors, limit })
                    .await
                {
                    Ok(MemoryIpcResponsePayload::Recall { hits }) => hits,
                    Ok(other) => {
                        tracing::warn!("Memory IPC recall 返回了非预期响应: {:?}", other);
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::warn!("Memory IPC recall 失败: {}", e);
                        Vec::new()
                    }
                }
            }
        }
    }

    /// 执行 Tool 化上下文回忆，由 Memory 内部完成检索规划和结果整理。
    pub async fn recall_context(&self, request: MemoryRecallRequest) -> MemoryRecallResponse {
        match self.inner.as_ref() {
            HandleInner::Local { .. } => self.recall_context_local(request).await,
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::RecallContext { request })
                    .await
                {
                    Ok(MemoryIpcResponsePayload::RecallContext { response }) => response,
                    Ok(other) => {
                        tracing::warn!("Memory IPC recall_context 返回了非预期响应: {:?}", other);
                        MemoryRecallResponse::default()
                    }
                    Err(e) => {
                        tracing::warn!("Memory IPC recall_context 失败: {}", e);
                        MemoryRecallResponse::default()
                    }
                }
            }
        }
    }

    /// 通用请求：按 IPC payload 分发到对应的 memory 操作。
    ///
    /// 供 WASM 桥接层使用——WASM 组件经 host import 传入 method + payload JSON，
    /// host 反序列化为 [`MemoryIpcRequestPayload`] 后调本方法，由统一的
    /// dispatcher（`handle_memory_request`）转发到具体能力。
    /// Local/Remote 透明：Local 经 dispatcher 直接调本进程 actor，Remote 经 IPC 发到 sidecar。
    pub async fn ipc_request(
        &self,
        payload: MemoryIpcRequestPayload,
    ) -> anyhow::Result<MemoryIpcResponsePayload> {
        crate::ipc::handle_memory_request(self.clone(), payload).await
    }

    /// 运行时粗回忆：只使用本地全文搜索，避免触发 embedding/rerank/Memory LLM。
    pub async fn rough_recall(&self, context: RuntimeRecallContext) -> Vec<RecallHit> {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(MemoryCommand::RoughRecall {
                        context,
                        reply: reply_tx,
                    })
                    .await
                    .is_err()
                {
                    tracing::warn!("Memory rough_recall 发送失败");
                    return Vec::new();
                }
                reply_rx.await.unwrap_or_default()
            }
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::RoughRecall { context })
                    .await
                {
                    Ok(MemoryIpcResponsePayload::Recall { hits }) => hits,
                    Ok(other) => {
                        tracing::warn!("Memory IPC rough_recall 返回了非预期响应: {:?}", other);
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::warn!("Memory IPC rough_recall 失败: {}", e);
                        Vec::new()
                    }
                }
            }
        }
    }

    /// 评估粗回忆是否足够支撑当前运行时操作。
    pub async fn evaluate_recall_sufficiency(
        &self,
        context: RuntimeRecallContext,
        rough_hits: Vec<RecallHit>,
    ) -> RecallSufficiency {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(MemoryCommand::EvaluateRecallSufficiency {
                        context,
                        rough_hits,
                        reply: reply_tx,
                    })
                    .await
                    .is_err()
                {
                    tracing::warn!("Memory evaluate_recall_sufficiency 发送失败");
                    return RecallSufficiency::default();
                }
                reply_rx.await.unwrap_or_default()
            }
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::EvaluateRecallSufficiency {
                        context,
                        rough_hits,
                    })
                    .await
                {
                    Ok(MemoryIpcResponsePayload::RecallSufficiency { result }) => result,
                    Ok(other) => {
                        tracing::warn!(
                            "Memory IPC evaluate_recall_sufficiency 返回了非预期响应: {:?}",
                            other
                        );
                        RecallSufficiency::default()
                    }
                    Err(e) => {
                        tracing::warn!("Memory IPC evaluate_recall_sufficiency 失败: {}", e);
                        RecallSufficiency::default()
                    }
                }
            }
        }
    }

    /// 提交记忆候选（fire-and-forget）。
    ///
    /// 工具执行完成后调用，候选累积在 Actor 内部，轮次结束时由
    /// `run_enhanced_micro_rumination` 统一提炼。
    pub fn submit_memory_candidate(&self, candidate: MemoryCandidate) {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                if let Err(e) = tx.try_send(MemoryCommand::SubmitCandidate { candidate }) {
                    tracing::warn!("Memory submit_memory_candidate 发送失败: {}", e);
                }
            }
            HandleInner::Remote { .. } => {
                self.dispatch_remote_request(
                    MemoryIpcRequestPayload::SubmitCandidate { candidate },
                    "submit_memory_candidate",
                );
            }
        }
    }

    /// 将增强版 Micro 反刍快速投递到独立 worker。
    ///
    /// 返回时只保证任务已进入有界队列，不等待模型提取和最终写入。
    pub async fn run_enhanced_micro_rumination(
        &self,
        turn_result: EnhancedTurnResult,
    ) -> Result<()> {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                tx.send(MemoryCommand::EnqueueEnhancedMicroRumination {
                    turn_result: Box::new(turn_result),
                    reply: reply_tx,
                })
                .await
                .with_context(|| "发送 Memory 增强反刍入队命令失败")?;
                reply_rx
                    .await
                    .with_context(|| "等待 Memory 增强反刍入队确认失败")?
                    .map_err(|error| anyhow!(error))
            }
            HandleInner::Remote { .. } => match self
                .send_remote_request(MemoryIpcRequestPayload::RunEnhancedMicroRumination {
                    turn_result,
                })
                .await?
            {
                MemoryIpcResponsePayload::Ack => Ok(()),
                other => Err(anyhow!(
                    "Memory IPC run_enhanced_micro_rumination 返回了非预期响应: {other:?}"
                )),
            },
        }
    }

    /// 写入 Episode（fire-and-forget）
    ///
    /// `workspace_id` 显式携带，为 `None` 时由 Actor 内部值兜底。
    pub fn write_episode(&self, episode: Episode, workspace_id: Option<String>) {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                if let Err(e) = tx.try_send(MemoryCommand::WriteEpisode {
                    episode,
                    workspace_id,
                }) {
                    tracing::warn!("Memory write_episode 发送失败: {}", e);
                }
            }
            HandleInner::Remote { .. } => {
                self.dispatch_remote_request(
                    MemoryIpcRequestPayload::WriteEpisode {
                        episode,
                        workspace_id,
                    },
                    "write_episode",
                );
            }
        }
    }

    /// 更新注入文件（fire-and-forget）
    pub fn update_injection(&self, level: InjectionLevel, target_id: String, content: String) {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                if let Err(e) = tx.try_send(MemoryCommand::UpdateInjection {
                    level,
                    target_id,
                    content,
                }) {
                    tracing::warn!("Memory update_injection 发送失败: {}", e);
                }
            }
            HandleInner::Remote { .. } => {
                self.dispatch_remote_request(
                    MemoryIpcRequestPayload::UpdateInjection {
                        level,
                        target_id,
                        content,
                    },
                    "update_injection",
                );
            }
        }
    }

    /// 列出记忆节点（查询，等待响应）。
    pub async fn list_nodes(&self, query: MemoryListQuery) -> Vec<MemoryNode> {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(MemoryCommand::ListNodes {
                        query,
                        reply: reply_tx,
                    })
                    .await
                    .is_err()
                {
                    tracing::warn!("Memory list_nodes 发送失败");
                    return Vec::new();
                }
                reply_rx.await.unwrap_or_default()
            }
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::ListNodes { query })
                    .await
                {
                    Ok(MemoryIpcResponsePayload::Nodes { items }) => items,
                    Ok(other) => {
                        tracing::warn!("Memory IPC list_nodes 返回了非预期响应: {:?}", other);
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::warn!("Memory IPC list_nodes 失败: {}", e);
                        Vec::new()
                    }
                }
            }
        }
    }

    /// 统计记忆节点数量（查询，等待响应）。
    pub async fn count_nodes(&self, query: MemoryListQuery) -> usize {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(MemoryCommand::CountNodes {
                        query,
                        reply: reply_tx,
                    })
                    .await
                    .is_err()
                {
                    tracing::warn!("Memory count_nodes 发送失败");
                    return 0;
                }
                reply_rx.await.unwrap_or_default()
            }
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::CountNodes { query })
                    .await
                {
                    Ok(MemoryIpcResponsePayload::NodeCount { count }) => count,
                    Ok(other) => {
                        tracing::warn!("Memory IPC count_nodes 返回了非预期响应: {:?}", other);
                        0
                    }
                    Err(e) => {
                        tracing::warn!("Memory IPC count_nodes 失败: {}", e);
                        0
                    }
                }
            }
        }
    }

    /// 手动新增或调整记忆。
    pub async fn upsert_manual_memory(&self, draft: ManualMemoryDraft) -> Result<MemoryNode> {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                tx.send(MemoryCommand::UpsertManualMemory {
                    draft,
                    reply: reply_tx,
                })
                .await
                .with_context(|| "发送 Memory 手动记忆写入命令失败")?;
                reply_rx
                    .await
                    .with_context(|| "等待 Memory 手动记忆写入结果失败")?
                    .map_err(|err| anyhow!(err))
            }
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::UpsertManualMemory { draft })
                    .await?
                {
                    MemoryIpcResponsePayload::Node { item } => Ok(item),
                    other => Err(anyhow!("Memory IPC upsert 返回了非预期响应: {other:?}")),
                }
            }
        }
    }

    /// 手动设置记忆状态。
    pub async fn set_node_status(&self, node_id: String, status: MemoryStatus) -> Result<()> {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                tx.send(MemoryCommand::SetNodeStatus {
                    node_id,
                    status,
                    reply: reply_tx,
                })
                .await
                .with_context(|| "发送 Memory 状态更新命令失败")?;
                reply_rx
                    .await
                    .with_context(|| "等待 Memory 状态更新结果失败")?
                    .map_err(|err| anyhow!(err))
            }
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::SetNodeStatus { node_id, status })
                    .await?
                {
                    MemoryIpcResponsePayload::Ack => Ok(()),
                    other => Err(anyhow!(
                        "Memory IPC set_node_status 返回了非预期响应: {other:?}"
                    )),
                }
            }
        }
    }

    /// 列出某个记忆节点的图关系。
    pub async fn list_relations(&self, node_id: String) -> Vec<MemoryRelation> {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(MemoryCommand::ListRelations {
                        node_id,
                        reply: reply_tx,
                    })
                    .await
                    .is_err()
                {
                    tracing::warn!("Memory list_relations 发送失败");
                    return Vec::new();
                }
                reply_rx.await.unwrap_or_default()
            }
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::ListRelations { node_id })
                    .await
                {
                    Ok(MemoryIpcResponsePayload::Relations { items }) => items,
                    Ok(other) => {
                        tracing::warn!("Memory IPC list_relations 返回了非预期响应: {:?}", other);
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::warn!("Memory IPC list_relations 失败: {}", e);
                        Vec::new()
                    }
                }
            }
        }
    }

    /// 批量列出多个记忆节点的关联关系（去重，修复 N+1 性能问题）。
    pub async fn list_relations_batch(&self, node_ids: Vec<String>) -> Vec<MemoryRelation> {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(MemoryCommand::ListRelationsBatch {
                        node_ids,
                        reply: reply_tx,
                    })
                    .await
                    .is_err()
                {
                    tracing::warn!("Memory list_relations_batch 发送失败");
                    return Vec::new();
                }
                reply_rx.await.unwrap_or_default()
            }
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::ListRelationsBatch { node_ids })
                    .await
                {
                    Ok(MemoryIpcResponsePayload::Relations { items }) => items,
                    Ok(other) => {
                        tracing::warn!(
                            "Memory IPC list_relations_batch 返回了非预期响应: {:?}",
                            other
                        );
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::warn!("Memory IPC list_relations_batch 失败: {}", e);
                        Vec::new()
                    }
                }
            }
        }
    }

    /// 新增或调整记忆图关系。
    pub async fn upsert_relation(&self, draft: MemoryRelationDraft) -> Result<MemoryRelation> {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                tx.send(MemoryCommand::UpsertRelation {
                    draft,
                    reply: reply_tx,
                })
                .await
                .with_context(|| "发送 Memory 关系写入命令失败")?;
                reply_rx
                    .await
                    .with_context(|| "等待 Memory 关系写入结果失败")?
                    .map_err(|err| anyhow!(err))
            }
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::UpsertRelation { draft })
                    .await?
                {
                    MemoryIpcResponsePayload::Relation { item } => Ok(item),
                    other => Err(anyhow!(
                        "Memory IPC upsert_relation 返回了非预期响应: {other:?}"
                    )),
                }
            }
        }
    }

    /// 删除记忆图关系。
    pub async fn delete_relation(&self, relation_id: String) -> Result<()> {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                tx.send(MemoryCommand::DeleteRelation {
                    relation_id,
                    reply: reply_tx,
                })
                .await
                .with_context(|| "发送 Memory 关系删除命令失败")?;
                reply_rx
                    .await
                    .with_context(|| "等待 Memory 关系删除结果失败")?
                    .map_err(|err| anyhow!(err))
            }
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::DeleteRelation { relation_id })
                    .await?
                {
                    MemoryIpcResponsePayload::Ack => Ok(()),
                    other => Err(anyhow!(
                        "Memory IPC delete_relation 返回了非预期响应: {other:?}"
                    )),
                }
            }
        }
    }

    /// 触发 Micro 反刍（fire-and-forget）
    pub fn run_micro_rumination(&self, turn_result: TurnResult) {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                if let Err(e) = tx.try_send(MemoryCommand::RunMicroRumination {
                    turn_result: Box::new(turn_result),
                }) {
                    tracing::warn!("Memory run_micro_rumination 发送失败: {}", e);
                }
            }
            HandleInner::Remote { .. } => {
                self.dispatch_remote_request(
                    MemoryIpcRequestPayload::RunMicroRumination { turn_result },
                    "run_micro_rumination",
                );
            }
        }
    }

    /// 触发 Micro 反刍（同步版，适用于 std::thread 中的 blocking_send）
    ///
    /// 在非 async 上下文（如 TiangongCore 工作线程）中使用。
    pub fn run_micro_rumination_blocking(&self, turn_result: TurnResult) {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                if let Err(e) = tx.blocking_send(MemoryCommand::RunMicroRumination {
                    turn_result: Box::new(turn_result),
                }) {
                    tracing::warn!("Memory run_micro_rumination_blocking 发送失败: {}", e);
                }
            }
            HandleInner::Remote { .. } => {
                if let Err(e) =
                    self.block_on_remote_request(MemoryIpcRequestPayload::RunMicroRumination {
                        turn_result,
                    })
                {
                    tracing::warn!("Memory run_micro_rumination_blocking 远端发送失败: {}", e);
                }
            }
        }
    }

    /// 执行粗召回（同步版，适用于 std::thread 中使用）
    ///
    /// 在非 async 上下文（如 TiangongCore 工作线程）中使用。
    pub fn recall_blocking(&self, anchors: RecallAnchors, limit: usize) -> Vec<RecallHit> {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let cmd = MemoryCommand::Recall {
                    anchors,
                    limit,
                    reply: reply_tx,
                };
                if tx.blocking_send(cmd).is_err() {
                    tracing::warn!("Memory Actor 已关闭，返回空召回");
                    return Vec::new();
                }
                reply_rx.blocking_recv().unwrap_or_default()
            }
            HandleInner::Remote { .. } => match self
                .block_on_remote_request(MemoryIpcRequestPayload::Recall { anchors, limit })
            {
                Ok(MemoryIpcResponsePayload::Recall { hits }) => hits,
                Ok(other) => {
                    tracing::warn!("Memory IPC recall_blocking 返回了非预期响应: {:?}", other);
                    Vec::new()
                }
                Err(e) => {
                    tracing::warn!("Memory IPC recall_blocking 失败: {}", e);
                    Vec::new()
                }
            },
        }
    }

    /// Tool 化上下文回忆（同步版，适用于 std::thread 中使用）。
    pub fn recall_context_blocking(&self, request: MemoryRecallRequest) -> MemoryRecallResponse {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let cmd = MemoryCommand::RecallContext {
                    request,
                    reply: reply_tx,
                };
                if tx.blocking_send(cmd).is_err() {
                    tracing::warn!("Memory Actor 已关闭，返回空上下文回忆");
                    return MemoryRecallResponse::default();
                }
                reply_rx.blocking_recv().unwrap_or_default()
            }
            HandleInner::Remote { .. } => match self
                .block_on_remote_request(MemoryIpcRequestPayload::RecallContext { request })
            {
                Ok(MemoryIpcResponsePayload::RecallContext { response }) => response,
                Ok(other) => {
                    tracing::warn!(
                        "Memory IPC recall_context_blocking 返回了非预期响应: {:?}",
                        other
                    );
                    MemoryRecallResponse::default()
                }
                Err(e) => {
                    tracing::warn!("Memory IPC recall_context_blocking 失败: {}", e);
                    MemoryRecallResponse::default()
                }
            },
        }
    }

    /// 运行时粗回忆（同步版）。
    pub fn rough_recall_blocking(&self, context: RuntimeRecallContext) -> Vec<RecallHit> {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .blocking_send(MemoryCommand::RoughRecall {
                        context,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    tracing::warn!("Memory rough_recall_blocking 发送失败");
                    return Vec::new();
                }
                reply_rx.blocking_recv().unwrap_or_default()
            }
            HandleInner::Remote { .. } => match self
                .block_on_remote_request(MemoryIpcRequestPayload::RoughRecall { context })
            {
                Ok(MemoryIpcResponsePayload::Recall { hits }) => hits,
                Ok(other) => {
                    tracing::warn!(
                        "Memory IPC rough_recall_blocking 返回了非预期响应: {:?}",
                        other
                    );
                    Vec::new()
                }
                Err(e) => {
                    tracing::warn!("Memory IPC rough_recall_blocking 失败: {}", e);
                    Vec::new()
                }
            },
        }
    }

    /// 召回充分性评估（同步版）。
    pub fn evaluate_recall_sufficiency_blocking(
        &self,
        context: RuntimeRecallContext,
        rough_hits: Vec<RecallHit>,
    ) -> RecallSufficiency {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .blocking_send(MemoryCommand::EvaluateRecallSufficiency {
                        context,
                        rough_hits,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    tracing::warn!("Memory evaluate_recall_sufficiency_blocking 发送失败");
                    return RecallSufficiency::default();
                }
                reply_rx.blocking_recv().unwrap_or_default()
            }
            HandleInner::Remote { .. } => match self.block_on_remote_request(
                MemoryIpcRequestPayload::EvaluateRecallSufficiency {
                    context,
                    rough_hits,
                },
            ) {
                Ok(MemoryIpcResponsePayload::RecallSufficiency { result }) => result,
                Ok(other) => {
                    tracing::warn!(
                        "Memory IPC evaluate_recall_sufficiency_blocking 返回了非预期响应: {:?}",
                        other
                    );
                    RecallSufficiency::default()
                }
                Err(e) => {
                    tracing::warn!(
                        "Memory IPC evaluate_recall_sufficiency_blocking 失败: {}",
                        e
                    );
                    RecallSufficiency::default()
                }
            },
        }
    }

    /// 加载二跳展开内容（查询，等待响应）
    pub async fn load_depth2(&self, node_ids: Vec<String>) -> Vec<ExpandedMemory> {
        match self.inner.as_ref() {
            HandleInner::Local { .. } => self.load_depth2_local(node_ids).await,
            HandleInner::Remote { .. } => {
                match self
                    .send_remote_request(MemoryIpcRequestPayload::LoadDepth2 { node_ids })
                    .await
                {
                    Ok(MemoryIpcResponsePayload::Depth2 { items }) => items,
                    Ok(other) => {
                        tracing::warn!("Memory IPC load_depth2 返回了非预期响应: {:?}", other);
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::warn!("Memory IPC load_depth2 失败: {}", e);
                        Vec::new()
                    }
                }
            }
        }
    }

    /// 触发 Meso 反刍（fire-and-forget）
    pub fn run_meso_rumination(&self, session_id: String, workspace_id: String) {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                if let Err(e) = tx.try_send(MemoryCommand::RunMesoRumination {
                    session_id,
                    workspace_id,
                }) {
                    tracing::warn!("Memory run_meso_rumination 发送失败: {}", e);
                }
            }
            HandleInner::Remote { .. } => {
                self.dispatch_remote_request(
                    MemoryIpcRequestPayload::RunMesoRumination {
                        session_id,
                        workspace_id,
                    },
                    "run_meso_rumination",
                );
            }
        }
    }

    /// 触发 Meta 反刍（fire-and-forget）
    pub fn run_meta_rumination(&self) {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                if let Err(e) = tx.try_send(MemoryCommand::RunMetaRumination) {
                    tracing::warn!("Memory run_meta_rumination 发送失败: {}", e);
                }
            }
            HandleInner::Remote { .. } => {
                self.dispatch_remote_request(
                    MemoryIpcRequestPayload::RunMetaRumination,
                    "run_meta_rumination",
                );
            }
        }
    }

    /// 优雅关闭
    pub async fn shutdown(&self) {
        match self.inner.as_ref() {
            HandleInner::Local { tx } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(MemoryCommand::Shutdown { reply: reply_tx })
                    .await
                    .is_ok()
                {
                    let _ = reply_rx.await;
                }
            }
            HandleInner::Remote { .. } => {
                let _ = self
                    .send_remote_request(MemoryIpcRequestPayload::Shutdown)
                    .await;
            }
        }
    }

    async fn load_injection_local(
        &self,
        session_id: &str,
        workspace_id: Option<&str>,
    ) -> Vec<String> {
        let HandleInner::Local { tx } = self.inner.as_ref() else {
            return Vec::new();
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let cmd = MemoryCommand::LoadInjection {
            session_id: session_id.to_string(),
            workspace_id: workspace_id.map(String::from),
            reply: reply_tx,
        };
        if tx.send(cmd).await.is_err() {
            tracing::warn!("Memory Actor 已关闭，返回空注入");
            return Vec::new();
        }
        reply_rx.await.unwrap_or_default()
    }

    async fn recall_local(&self, anchors: RecallAnchors, limit: usize) -> Vec<RecallHit> {
        let HandleInner::Local { tx } = self.inner.as_ref() else {
            return Vec::new();
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let cmd = MemoryCommand::Recall {
            anchors,
            limit,
            reply: reply_tx,
        };
        if tx.send(cmd).await.is_err() {
            tracing::warn!("Memory Actor 已关闭，返回空召回");
            return Vec::new();
        }
        reply_rx.await.unwrap_or_default()
    }

    async fn recall_context_local(&self, request: MemoryRecallRequest) -> MemoryRecallResponse {
        let HandleInner::Local { tx } = self.inner.as_ref() else {
            return MemoryRecallResponse::default();
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let cmd = MemoryCommand::RecallContext {
            request,
            reply: reply_tx,
        };
        if tx.send(cmd).await.is_err() {
            tracing::warn!("Memory Actor 已关闭，返回空上下文回忆");
            return MemoryRecallResponse::default();
        }
        reply_rx.await.unwrap_or_default()
    }

    async fn load_depth2_local(&self, node_ids: Vec<String>) -> Vec<ExpandedMemory> {
        let HandleInner::Local { tx } = self.inner.as_ref() else {
            return Vec::new();
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let cmd = MemoryCommand::LoadDepth2 {
            node_ids,
            reply: reply_tx,
        };
        if tx.send(cmd).await.is_err() {
            tracing::warn!("Memory Actor 已关闭，返回空 depth2");
            return Vec::new();
        }
        reply_rx.await.unwrap_or_default()
    }

    async fn send_remote_request(
        &self,
        payload: MemoryIpcRequestPayload,
    ) -> Result<MemoryIpcResponsePayload> {
        let HandleInner::Remote { client } = self.inner.as_ref() else {
            return Err(anyhow!("当前 MemoryHandle 不是 Remote 模式"));
        };
        let mut client = client.lock().await;
        client
            .send_request(IpcRequest {
                request_id: scru128::new().to_string(),
                payload: serde_json::to_value(payload)
                    .with_context(|| "序列化 Memory IPC 请求载荷失败")?,
            })
            .await?;
        let response = client.read_response().await?;
        serde_json::from_value(response.payload).with_context(|| "解析 Memory IPC 响应载荷失败")
    }

    fn block_on_remote_request(
        &self,
        payload: MemoryIpcRequestPayload,
    ) -> Result<MemoryIpcResponsePayload> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .with_context(|| "构建临时 tokio runtime 失败")?;
        rt.block_on(self.send_remote_request(payload))
    }

    fn dispatch_remote_request(&self, payload: MemoryIpcRequestPayload, action: &'static str) {
        let handle = self.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(err) = handle.send_remote_request(payload).await {
                    tracing::warn!("Memory {action} 远端发送失败: {}", err);
                }
            });
            return;
        }

        let spawn_result = std::thread::Builder::new()
            .name(format!("memory-remote-{action}"))
            .spawn(move || {
                if let Err(err) = handle.block_on_remote_request(payload) {
                    tracing::warn!("Memory {action} 远端发送失败: {}", err);
                }
            });
        if let Err(err) = spawn_result {
            tracing::warn!("Memory {action} 启动远端发送线程失败: {}", err);
        }
    }
}
