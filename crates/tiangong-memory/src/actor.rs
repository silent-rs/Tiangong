//! Memory Actor — 独立 tokio task 运行时
//!
//! 所有 Memory 读写均串行经过 Actor，天然消除并发冲突。
//! 外部通过 MemoryHandle 的 mpsc channel 发送命令。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tiangong_llm::{EmbeddingProvider, RerankProvider};
use tokio::sync::mpsc;

use crate::command::MemoryCommand;
use crate::handle::MemoryHandle;
use crate::options::MemoryOptions;
use crate::recall_context;
use crate::rumination;
use crate::store::MemoryStore;
use crate::types::{EnhancedTurnResult, MemoryCandidate};

/// 反刍后台任务：Actor 主循环只做「读快照 → 入队」与「落库」两类快操作，
/// LLM 提炼与本地评估全部在 worker 上并发执行——Actor 内联长任务会阻塞
/// micro 反刍的入队确认，进而拉长 WASM 收尾的实例锁持有窗口。
enum RuminationJob {
    EnhancedMicro {
        turn_result: Box<EnhancedTurnResult>,
        model: Option<tiangong_llm::LlmEndpointConfig>,
    },
    Meso(Box<rumination::MesoJob>),
    Meta(Box<rumination::MetaJob>),
}

/// 反刍 worker 的 LLM 提炼并发上限：保守值，避免高频多轮对话时打爆
/// 模型端点限流；积压由有界队列（64）兜底。
const RUMINATION_CONCURRENCY: usize = 2;

/// Memory Actor（独立运行时）
pub(crate) struct MemoryActor {
    rx: mpsc::Receiver<MemoryCommand>,
    store: MemoryStore,
    options: MemoryOptions,
    pending_candidates: Vec<MemoryCandidate>,
    rumination_tx: mpsc::Sender<RuminationJob>,
    /// 回送本地模型加载结果用（弱引用，不阻止通道关闭）。
    self_tx: mpsc::WeakSender<MemoryCommand>,
    /// 配置代号：每次（重新）配置递增，丢弃过期的本地模型加载结果。
    generation: Arc<AtomicU64>,
    /// 当前本地模型下载任务的取消令牌：重新配置时取消上一轮，避免旧档位
    /// 继续占用带宽和下载锁。
    local_model_cancel: Option<crate::local_model::CancelToken>,
}

impl MemoryActor {
    fn new(
        rx: mpsc::Receiver<MemoryCommand>,
        store: MemoryStore,
        options: MemoryOptions,
        rumination_tx: mpsc::Sender<RuminationJob>,
        self_tx: mpsc::WeakSender<MemoryCommand>,
    ) -> Self {
        Self {
            rx,
            store,
            options,
            pending_candidates: Vec::new(),
            rumination_tx,
            self_tx,
            generation: Arc::new(AtomicU64::new(0)),
            local_model_cancel: None,
        }
    }

    /// 启动 Actor 消息循环
    pub(crate) async fn run(mut self) {
        self.apply_recall_options(false).await;
        tracing::info!("Memory Actor 已启动");
        let shutdown_reply = loop {
            // 有待回填向量时先把下一批交给后台算 embedding：主循环只做
            // SQLite 取批与向量落库，端点慢或超时都不会拖住命令处理。
            let next = if let Some(due) = self.store.backfill_due_in() {
                if due.is_zero() {
                    if let Some(batch) = self.store.take_backfill_batch() {
                        self.spawn_backfill_batch(batch);
                    }
                    continue;
                }
                match tokio::time::timeout(due, self.rx.recv()).await {
                    Ok(message) => message,
                    Err(_) => continue,
                }
            } else {
                self.rx.recv().await
            };
            match next {
                Some(MemoryCommand::Shutdown { reply }) => {
                    tracing::info!("Memory Actor 收到 Shutdown 命令，正在关闭");
                    break Some(reply);
                }
                Some(cmd) => self.handle(cmd).await,
                None => {
                    tracing::info!("Memory Actor 通道已关闭，退出");
                    break None;
                }
            }
        };
        drop(self);
        if let Some(reply) = shutdown_reply {
            let _ = reply.send(());
        }
        tracing::info!("Memory Actor 已关闭");
    }

    async fn handle(&mut self, cmd: MemoryCommand) {
        match cmd {
            MemoryCommand::LoadInjection {
                session_id,
                workspace_id,
                reply,
            } => {
                let result = self
                    .store
                    .load_injection(&session_id, workspace_id.as_deref());
                let _ = reply.send(result);
            }

            MemoryCommand::WriteEpisode {
                episode,
                workspace_id,
            } => {
                if let Err(e) = self
                    .store
                    .write_episode(episode, workspace_id.as_deref())
                    .await
                {
                    tracing::warn!("Memory 写入 Episode 失败: {}", e);
                }
            }

            MemoryCommand::UpdateInjection {
                level,
                target_id,
                content,
            } => {
                if let Err(e) = self.store.update_injection(level, &target_id, &content) {
                    tracing::warn!("Memory 更新 Injection 失败: {}", e);
                }
            }

            MemoryCommand::UpsertManualMemory { draft, reply } => {
                let result = if draft.id.as_deref().is_some_and(|id| !id.trim().is_empty()) {
                    self.store.update_manual_memory(draft).await
                } else {
                    self.store.upsert_manual_memory(draft).await
                }
                .map_err(|err| err.to_string());
                let _ = reply.send(result);
            }

            MemoryCommand::SetNodeStatus {
                node_id,
                status,
                reply,
            } => {
                let result = self
                    .store
                    .set_node_status(&node_id, status)
                    .await
                    .map_err(|err| err.to_string());
                let _ = reply.send(result);
            }

            MemoryCommand::UpsertRelation { draft, reply } => {
                let result = self
                    .store
                    .upsert_relation(draft)
                    .map_err(|err| err.to_string());
                let _ = reply.send(result);
            }

            MemoryCommand::DeleteRelation { relation_id, reply } => {
                let result = self
                    .store
                    .delete_relation(&relation_id)
                    .map_err(|err| err.to_string());
                let _ = reply.send(result);
            }

            // Phase C：双引擎召回（Tantivy BM25 + LanceDB 语义）
            MemoryCommand::Recall {
                anchors,
                limit,
                reply,
            } => {
                let hits = self.store.recall_async(&anchors, limit).await;
                let _ = reply.send(hits);
            }

            MemoryCommand::RecallContext { request, reply } => {
                let response = recall_context::recall_context(
                    &self.store,
                    self.options.model.as_ref(),
                    request,
                )
                .await;
                let _ = reply.send(response);
            }

            MemoryCommand::RoughRecall { context, reply } => {
                let limit = context.policy.rough_limit.clamp(1, 10);

                // 首条消息的运行时回忆：优先使用 LLM 规划检索策略
                let hits = if let Some(strategy) = recall_context::plan_runtime_recall(
                    &context.query,
                    &context.current_context,
                    self.options.model.as_ref(),
                )
                .await
                {
                    match strategy {
                        recall_context::RuntimeRecallStrategy::Recent => {
                            let recent = self.store.recent_memory_hits(limit);
                            tracing::debug!(
                                query = %context.query,
                                recent_count = recent.len(),
                                "Memory 运行时粗回忆使用 LLM 规划的最近记忆策略"
                            );
                            recent
                        }
                        recall_context::RuntimeRecallStrategy::Keyword { search_terms } => {
                            let expanded_query = search_terms.join(" ");
                            let hits = self.store.rough_recall(&expanded_query, limit);
                            tracing::debug!(
                                query = %context.query,
                                expanded_query = %expanded_query,
                                expanded_count = hits.len(),
                                "Memory 运行时粗回忆使用 LLM 规划的扩展关键词策略"
                            );
                            if hits.is_empty() {
                                self.store.recent_memory_hits(limit)
                            } else {
                                hits
                            }
                        }
                    }
                } else {
                    // 无 LLM 或规划失败，直接 BM25
                    let hits = self.store.rough_recall(&context.query, limit);
                    tracing::debug!(
                        query = %context.query,
                        trigger = ?context.trigger,
                        hit_count = hits.len(),
                        "Memory 运行时粗回忆 BM25 fallback"
                    );
                    hits
                };

                let _ = reply.send(hits);
            }

            MemoryCommand::EvaluateRecallSufficiency {
                context,
                rough_hits,
                reply,
            } => {
                let result = recall_context::evaluate_recall_sufficiency(
                    &context,
                    &rough_hits,
                    self.options.model.as_ref(),
                )
                .await;
                tracing::debug!(
                    query = %context.query,
                    sufficient = result.sufficient,
                    should_upgrade_to_hybrid = result.should_upgrade_to_hybrid,
                    "Memory 运行时召回充分性评估完成"
                );
                let _ = reply.send(result);
            }

            MemoryCommand::LoadDepth2 { node_ids, reply } => {
                let items = self.store.load_depth2(&node_ids);
                let _ = reply.send(items);
            }

            MemoryCommand::ListNodes { query, reply } => {
                let items = self.store.list_nodes(&query);
                let _ = reply.send(items);
            }

            MemoryCommand::CountNodes { query, reply } => {
                let count = self.store.count_nodes(&query);
                let _ = reply.send(count);
            }

            MemoryCommand::ListRelations { node_id, reply } => {
                let items = self.store.list_relations(&node_id);
                let _ = reply.send(items);
            }

            MemoryCommand::ListRelationsBatch { node_ids, reply } => {
                let items = self.store.list_relations_batch(&node_ids);
                let _ = reply.send(items);
            }

            // Phase B：Micro 反刍
            MemoryCommand::SubmitCandidate { candidate } => {
                self.pending_candidates.push(candidate);
            }

            MemoryCommand::RunMicroRumination { turn_result } => {
                // 使用 turn 携带的 workspace_id 作为 scope 标记
                let wid = turn_result.workspace_id.as_deref();
                if let Err(e) = rumination::process_micro(
                    &mut self.store,
                    &turn_result,
                    wid,
                    self.options.model.as_ref(),
                )
                .await
                {
                    tracing::warn!("Micro 反刍失败: {}", e);
                }
            }

            MemoryCommand::EnqueueEnhancedMicroRumination { turn_result, reply } => {
                let started = std::time::Instant::now();
                let mut enhanced = *turn_result;
                let pending = std::mem::take(&mut self.pending_candidates);
                enhanced.memory_candidates.extend(pending.iter().cloned());
                let session_id = enhanced.session_id.clone();
                let turn_id = enhanced.turn_id.clone();
                let job = RuminationJob::EnhancedMicro {
                    turn_result: Box::new(enhanced),
                    model: self.options.model.clone(),
                };
                let result = self.rumination_tx.try_send(job).map_err(|error| {
                    self.pending_candidates = pending;
                    format!("Memory 反刍队列不可用: {error}")
                });
                match &result {
                    Ok(()) => tracing::debug!(
                        %session_id,
                        %turn_id,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "Memory 增强反刍已入队"
                    ),
                    Err(error) => tracing::warn!(
                        %session_id,
                        %turn_id,
                        %error,
                        "Memory 增强反刍入队失败"
                    ),
                }
                let _ = reply.send(result);
            }
            MemoryCommand::ApplyEnhancedMicroRumination {
                turn_result,
                extraction,
            } => {
                let started = std::time::Instant::now();
                let enhanced = *turn_result;
                let session_id = enhanced.session_id.clone();
                let turn_id = enhanced.turn_id.clone();
                let wid = enhanced.workspace_id.as_deref();
                match rumination::apply_enhanced_micro(&mut self.store, &enhanced, &extraction, wid)
                    .await
                {
                    Ok(()) => tracing::info!(
                        %session_id,
                        %turn_id,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "Memory 增强反刍写入完成"
                    ),
                    Err(error) => tracing::warn!(
                        %session_id,
                        %turn_id,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        %error,
                        "Memory 增强反刍写入失败"
                    ),
                }
            }

            // Phase C 实现：读快照后投后台 worker 提炼，Actor 不内联 LLM。
            MemoryCommand::RunMesoRumination {
                session_id,
                workspace_id,
            } => {
                let model = self.options.model.clone();
                match rumination::prepare_meso_job(&self.store, &workspace_id, model) {
                    Some(job) => {
                        if let Err(error) = self
                            .rumination_tx
                            .try_send(RuminationJob::Meso(Box::new(job)))
                        {
                            tracing::warn!(
                                %session_id,
                                %workspace_id,
                                %error,
                                "Meso 反刍入队失败，本次整理丢弃"
                            );
                        }
                    }
                    None => tracing::debug!(
                        %session_id,
                        %workspace_id,
                        "Meso 反刍：无可用 Episode，跳过"
                    ),
                }
            }

            // Phase D 实现：读节点快照后投后台 worker 评估，Actor 不内联文件/URL 检查。
            MemoryCommand::RunMetaRumination => {
                let job = rumination::prepare_meta_job(&self.store);
                if let Err(error) = self
                    .rumination_tx
                    .try_send(RuminationJob::Meta(Box::new(job)))
                {
                    tracing::warn!(%error, "Meta 反刍入队失败，本次整理丢弃");
                }
            }

            MemoryCommand::ApplyMesoRumination { outcome } => {
                let workspace_id = outcome.workspace_id.clone();
                if let Err(error) = rumination::apply_meso(&mut self.store, *outcome).await {
                    tracing::warn!(%workspace_id, %error, "Meso 反刍写入失败");
                }
            }

            MemoryCommand::ApplyMetaRumination { outcome } => {
                rumination::apply_meta(&mut self.store, *outcome).await;
            }

            MemoryCommand::BackfillVectorsReady {
                generation,
                nodes,
                vectors,
            } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    tracing::debug!("Memory 丢弃过期的向量回填结果");
                    return;
                }
                self.store.apply_backfill_vectors(nodes, vectors).await;
            }

            MemoryCommand::LocalModelsReady {
                generation,
                embedding,
                rerank,
            } => {
                if generation != self.generation.load(Ordering::SeqCst) {
                    tracing::debug!("Memory 丢弃过期的本地模型加载结果");
                    return;
                }
                let (remote_embedding, remote_rerank) = remote_providers(&self.options);
                self.store
                    .reconfigure_recall_engine(
                        remote_embedding.or(embedding),
                        remote_rerank.or(rerank),
                        self.options.vector_mode,
                    )
                    .await;
                tracing::info!("Memory 内置本地模型已接入召回");
            }

            MemoryCommand::Reconfigure { options, reply } => {
                let result = self
                    .reconfigure(*options)
                    .await
                    .map_err(|err| err.to_string());
                let _ = reply.send(result);
            }

            // Shutdown 在外层 loop 处理
            MemoryCommand::Shutdown { .. } => {}
        }
    }

    async fn reconfigure(&mut self, options: MemoryOptions) -> anyhow::Result<()> {
        self.options = options;
        self.apply_recall_options(true).await;
        tracing::info!("Memory Actor 配置热更新完成");
        Ok(())
    }

    /// 把一批回填节点的 embedding 计算交给后台任务，算完经命令通道回送。
    ///
    /// 计算完成前 `in_flight` 为真，主循环不会再取新批次，天然限流为单批在途。
    fn spawn_backfill_batch(&self, batch: crate::store::BackfillBatch) {
        let Some(tx) = self.self_tx.upgrade() else {
            return;
        };
        let generation = self.generation.load(Ordering::SeqCst);
        let crate::store::BackfillBatch {
            nodes,
            texts,
            embedding,
        } = batch;
        tokio::task::spawn_local(async move {
            let vectors = embedding
                .embed(texts)
                .await
                .map_err(|err| format!("{err:#}"));
            let _ = tx
                .send(MemoryCommand::BackfillVectorsReady {
                    generation,
                    nodes,
                    vectors,
                })
                .await;
        });
    }

    /// 按当前选项启用召回增强层：在线组件立即生效；内置本地模型在独立线程
    /// 下载并加载，完成后经 `LocalModelsReady` 回到 Actor 再整体启用。
    async fn apply_recall_options(&mut self, reset: bool) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        // 先取消上一轮下载：换档位后旧模型不再需要，让它尽快释放下载锁。
        if let Some(cancel) = self.local_model_cancel.take() {
            cancel.cancel();
        }
        let (embedding, rerank) = remote_providers(&self.options);
        if reset {
            self.store
                .reconfigure_recall_engine(embedding, rerank, self.options.vector_mode)
                .await;
        } else {
            self.store
                .try_enable_recall_engine(embedding, rerank, self.options.vector_mode)
                .await;
        }

        let local_embedding = self
            .options
            .local_embedding
            .filter(|_| self.options.embedding.is_none());
        let local_rerank = self
            .options
            .local_rerank
            .filter(|_| self.options.rerank.is_none());
        if local_embedding.is_none() && local_rerank.is_none() {
            return;
        }
        let cancel = crate::local_model::CancelToken::default();
        match spawn_local_model_loader(
            self.self_tx.clone(),
            self.generation.clone(),
            generation,
            local_embedding,
            local_rerank,
            cancel.clone(),
        ) {
            Ok(()) => self.local_model_cancel = Some(cancel),
            Err(error) => {
                tracing::warn!("Memory 启动内置模型加载线程失败，保持降级召回: {error}")
            }
        }
    }
}

/// 召回增强组件（embedding, rerank）。
type RecallProviders = (
    Option<Arc<dyn EmbeddingProvider>>,
    Option<Arc<dyn RerankProvider>>,
);

/// 按在线端点配置构造 provider；构造失败仅告警（该组件按未配置处理）。
fn remote_providers(options: &MemoryOptions) -> RecallProviders {
    let embedding = options.embedding.as_ref().and_then(|config| {
        tiangong_llm::embedding_provider_from_config(config)
            .inspect_err(|err| {
                tracing::warn!("Memory embedding provider 初始化失败，跳过向量层: {err}")
            })
            .ok()
    });
    let rerank = options.rerank.as_ref().and_then(|config| {
        tiangong_llm::rerank_provider_from_config(config)
            .inspect_err(|err| {
                tracing::warn!("Memory rerank provider 初始化失败，跳过模型精排: {err}")
            })
            .ok()
    });
    (embedding, rerank)
}

/// 本地模型加载失败后的重试间隔（秒），用尽后停止，等待下次配置变更。
const LOCAL_MODEL_RETRY_SECS: [u64; 5] = [30, 60, 120, 300, 600];
/// 下载锁被其他进程占用时的等待间隔（秒）：不算失败，一直等到锁释放。
const LOCAL_MODEL_BUSY_WAIT_SECS: u64 = 15;

/// 在独立线程下载并加载内置模型：下载与 ONNX 初始化耗时且含阻塞 IO，
/// 放在 Actor 的单线程 runtime 外执行，避免阻塞记忆读写。
fn spawn_local_model_loader(
    actor_tx: mpsc::WeakSender<MemoryCommand>,
    current: Arc<AtomicU64>,
    generation: u64,
    embedding_tier: Option<crate::config::MemoryLocalTier>,
    rerank_tier: Option<crate::config::MemoryLocalTier>,
    cancel: crate::local_model::CancelToken,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("tiangong-memory-local-model".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::warn!("Memory 内置模型加载 runtime 构建失败: {error}");
                    return;
                }
            };
            let stale = || current.load(Ordering::SeqCst) != generation;
            let mut embedding = None;
            let mut rerank = None;
            let mut attempt = 0;
            loop {
                if stale() {
                    return;
                }
                // 本轮是否只遇到「锁被占用 / 已取消」这类暂时中断：不计入失败
                // 退避次数，否则等别的进程下载完时本轮已经放弃了。
                let mut transient = false;
                if let Some(tier) = embedding_tier.filter(|_| embedding.is_none()) {
                    match runtime.block_on(crate::local_model::load_embedding(tier, &cancel)) {
                        Ok(provider) => embedding = Some(provider),
                        Err(error) => {
                            if crate::local_model::is_transient(&error) {
                                transient = true;
                                tracing::debug!("Memory 内置 Embedding 暂时不可用: {error:#}");
                            } else {
                                tracing::warn!("Memory 内置 Embedding 暂不可用: {error:#}");
                            }
                        }
                    }
                }
                if stale() {
                    return;
                }
                if let Some(tier) = rerank_tier.filter(|_| rerank.is_none()) {
                    match runtime.block_on(crate::local_model::load_rerank(tier, &cancel)) {
                        Ok(provider) => rerank = Some(provider),
                        Err(error) => {
                            if crate::local_model::is_transient(&error) {
                                transient = true;
                                tracing::debug!("Memory 内置 Rerank 暂时不可用: {error:#}");
                            } else {
                                tracing::warn!("Memory 内置 Rerank 暂不可用: {error:#}");
                            }
                        }
                    }
                }
                let done = (embedding_tier.is_none() || embedding.is_some())
                    && (rerank_tier.is_none() || rerank.is_some());
                // 部分就绪也先接入，失败的组件继续重试。
                if embedding.is_some() || rerank.is_some() {
                    let Some(tx) = actor_tx.upgrade() else {
                        return;
                    };
                    if stale()
                        || tx
                            .blocking_send(MemoryCommand::LocalModelsReady {
                                generation,
                                embedding: embedding.clone(),
                                rerank: rerank.clone(),
                            })
                            .is_err()
                    {
                        return;
                    }
                }
                if done {
                    return;
                }
                // 只有真正的故障才推进退避序列；锁竞争按固定短间隔等待。
                let delay = if transient {
                    &LOCAL_MODEL_BUSY_WAIT_SECS
                } else {
                    let Some(delay) = LOCAL_MODEL_RETRY_SECS.get(attempt) else {
                        tracing::warn!("Memory 内置模型多次加载失败，保持降级召回，修改配置后重试");
                        return;
                    };
                    attempt += 1;
                    delay
                };
                // 分段休眠，配置变更后能尽快退出。
                for _ in 0..*delay {
                    if stale() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        })
        .map(|_| ())
}

/// 启动 Memory 系统，返回 MemoryHandle（同步接口）
///
/// 内部在独立线程 + current_thread runtime + LocalSet 中运行 Actor，
/// 不要求 MemoryActor 或 MemoryStore 实现 Send/Sync。
pub fn start_memory() -> anyhow::Result<MemoryHandle> {
    start_memory_with_options(MemoryOptions::new())
}
///
/// 上层可通过 `tiangong-config` 读取配置文件，再将解析后的 embedding
/// 端点传入这里。Memory 自身不负责重复解析全局配置文件。
pub fn start_memory_with_options(options: MemoryOptions) -> anyhow::Result<MemoryHandle> {
    let (handle, _join) = spawn_memory_actor(options)?;
    Ok(handle)
}

/// 启动 Memory Actor 线程，返回 handle 与 actor 线程的 JoinHandle。
///
/// 调用方负责在合适的时机 join 返回的 JoinHandle，以保证 actor 线程内的
/// native 资源（Tantivy mmap、SQLite 连接等）在进程退出前完成析构，避免
/// 与进程级全局静态析构产生竞态（Linux 偶发 SIGSEGV）。不需要显式 join
/// 的调用方（如生产路径）可直接丢弃返回的 JoinHandle，actor 线程会在
/// handle 全部释放后随通道关闭自行退出。
/// 反刍 worker：单线程驱动，LLM 提炼经信号量限流并发执行（上限
/// [`RUMINATION_CONCURRENCY`]）。提取乱序完成，但结果统一发回 Actor 命令
/// 通道**串行落库**——乱序写库会触发 Session/Workspace Injection 覆盖与
/// Episode 去重竞态。
fn spawn_rumination_worker(
    mut rx: mpsc::Receiver<RuminationJob>,
    actor_tx: mpsc::WeakSender<MemoryCommand>,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("tiangong-memory-rumination".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Memory 反刍 runtime 构建失败");
            runtime.block_on(async move {
                let semaphore =
                    std::sync::Arc::new(tokio::sync::Semaphore::new(RUMINATION_CONCURRENCY));
                let mut in_flight: tokio::task::JoinSet<()> = Default::default();
                while let Some(job) = rx.recv().await {
                    // 手满时先等空位再取新任务（有界队列兜底背压）。
                    let Ok(permit) = semaphore.clone().acquire_owned().await else {
                        break;
                    };
                    let actor_tx = actor_tx.clone();
                    in_flight.spawn(async move {
                        let _permit = permit;
                        dispatch_rumination_job(actor_tx, job).await;
                    });
                    // 回收已完成任务，防 JoinSet 无界增长。
                    while in_flight.try_join_next().is_some() {}
                }
                // 通道关闭：等在途提取全部提交回 Actor 后退出。
                while in_flight.join_next().await.is_some() {}
            });
        })
        .map_err(Into::into)
}

/// 单个反刍任务的「提取 → 回投 Actor」链：提取在 worker 上执行（LLM/
/// 本地评估），落库由 Actor 串行完成。
async fn dispatch_rumination_job(actor_tx: mpsc::WeakSender<MemoryCommand>, job: RuminationJob) {
    match job {
        RuminationJob::EnhancedMicro { turn_result, model } => {
            let started = std::time::Instant::now();
            let session_id = turn_result.session_id.clone();
            let turn_id = turn_result.turn_id.clone();
            let extraction = rumination::extract_enhanced_micro(&turn_result, model.as_ref()).await;
            tracing::info!(
                %session_id,
                %turn_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                episode_count = extraction.episodes.len(),
                entity_count = extraction.entities.len(),
                decision_count = extraction.decisions.len(),
                evidence_count = extraction.evidences.len(),
                "Memory 增强反刍提取完成"
            );
            if send_to_actor(
                &actor_tx,
                MemoryCommand::ApplyEnhancedMicroRumination {
                    turn_result,
                    extraction: Box::new(extraction),
                },
            )
            .await
            {
                return;
            }
        }
        RuminationJob::Meso(job) => {
            let outcome = rumination::extract_meso(*job).await;
            if send_to_actor(
                &actor_tx,
                MemoryCommand::ApplyMesoRumination {
                    outcome: Box::new(outcome),
                },
            )
            .await
            {
                return;
            }
        }
        RuminationJob::Meta(job) => {
            let outcome = rumination::evaluate_meta(*job);
            if send_to_actor(
                &actor_tx,
                MemoryCommand::ApplyMetaRumination {
                    outcome: Box::new(outcome),
                },
            )
            .await
            {
                return;
            }
        }
    }
    tracing::debug!("Memory Actor 已关闭，反刍结果丢弃");
}

/// 回投 Actor；返回 false 表示 Actor 已关闭。
async fn send_to_actor(actor_tx: &mpsc::WeakSender<MemoryCommand>, command: MemoryCommand) -> bool {
    match actor_tx.upgrade() {
        Some(tx) => tx.send(command).await.is_ok(),
        None => false,
    }
}

pub(crate) fn spawn_memory_actor(
    options: MemoryOptions,
) -> anyhow::Result<(MemoryHandle, std::thread::JoinHandle<()>)> {
    let (tx, rx) = mpsc::channel(256);
    let (rumination_tx, rumination_rx) = mpsc::channel(64);

    let store = MemoryStore::open().map_err(|e| {
        tracing::error!("Memory Store 初始化失败: {}", e);
        e
    })?;

    let actor = MemoryActor::new(rx, store, options.clone(), rumination_tx, tx.downgrade());
    let _rumination_join = spawn_rumination_worker(rumination_rx, tx.downgrade())?;

    let join_handle = std::thread::Builder::new()
        .name("tiangong-memory-actor".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Memory Actor runtime 构建失败");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, actor.run());
        })
        .expect("Memory Actor 线程创建失败");

    Ok((MemoryHandle::new(tx), join_handle))
}
