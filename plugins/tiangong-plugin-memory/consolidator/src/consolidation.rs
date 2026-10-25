//! 记忆深度整理引擎。
//!
//! 整理分三个阶段（`--stage` 可单独执行，缺省按顺序全部执行）：
//!
//! - **dedup**：同一范围内摘要完全一致的记忆直接归一（不调用 LLM），见 [`crate::stages`]；
//! - **distill**：工具日志由 LLM 结合证据路径与同时间段上下文提炼为经历，见 [`crate::stages`]；
//! - **similar**：本模块的相似召回 + LLM 统一判定（排除工具日志），流程如下。
//!
//! 试运行时后面的阶段会跳过前面阶段模拟删除的记忆。
//!
//! similar 阶段：
//!
//! 1. 获取全部活跃记忆；
//! 2. **相似召回**：按阈值找出每条记忆的全部相似记忆（读向量库时精确计算
//!    全量余弦相似度；否则经 Memory 召回 + 字符二元组相似度）。阈值只决定
//!    「哪些记忆值得交给 LLM 看」，不决定是否合并；
//! 3. **LLM 统一判定**：逐条以未处理记忆为种子，把它与**全部**达到阈值、尚未
//!    一起判定过的相似记忆分批（`batch_size`）交给 LLM。LLM 在一批中自行识别
//!    哪些记忆真正描述同一件事（可给出多个合并组）、哪些需要删除、哪些需要
//!    重新分类、以及新的关联；未被点名的记忆保持原样；
//! 4. 合并后主记忆继承组员的相似记忆，继续参与后续判定；同一对记忆只送审
//!    一次，避免重复调用；
//! 5. 规则识别的低质量记忆直接删除（向量与索引一并移除，不再参与召回）。
//!
//! 本 crate 经 `tiangong-memory` 的公开 API（[`MemoryHandle`]）读写记忆；
//! 物理删除使用 Memory 插件提供的 [`MemoryHandle::delete_node`] 能力。
//! 接入方式与 sidecar 通用模式一致（竞争 Leader，天工运行时以 Follower
//! 经 IPC 转发）。

use std::collections::{HashMap, HashSet, VecDeque};

use futures_util::StreamExt as _;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
use tiangong_llm::{EmbeddingProvider, LlmEndpointConfig, complete_text_with_usage};

use crate::journal::Journal;
pub use crate::stages::{
    ConsolidationStage, DEFAULT_DEDUP_MIN_SUMMARY_CHARS, DEFAULT_DISTILL_BATCH_SIZE,
    DEFAULT_DISTILL_WINDOW_MINUTES,
};
use crate::stages::{is_tool_log, run_dedup, run_distill, run_entity};
use crate::vector_store::StoredVectors;
use tiangong_memory::{
    ManualMemoryDraft, MemoryCognitiveType, MemoryHandle, MemoryListQuery, MemoryNode,
    MemoryRelation, MemoryRelationDraft, MemoryRelationKind, MemoryStatus, RecallAnchors,
    SearchStrategy,
};

/// 默认相似度阈值：余弦相似度 / 字符二元组 Jaccard 达到该值即作为候选送 LLM 判定。
pub const DEFAULT_SIMILARITY_THRESHOLD: f64 = 0.90;
/// 单次 LLM 判定最多包含的记忆数（含种子）。
pub const DEFAULT_BATCH_SIZE: usize = 20;
/// 单次分页拉取的记忆数。
const PAGE_LIMIT: usize = 500;
/// 召回路径单批扫描上限（逐条召回代价高，分批 bounds 工作量）。
const MAX_SCAN_NODES: usize = 5_000;
/// 向量路径单次扫描上限（读向量库本地算相似度，一批覆盖全库）。
const MAX_SCAN_NODES_VECTOR: usize = 50_000;
/// 召回路径每条记忆的召回条数（再按阈值过滤）。
const LEXICAL_RECALL_LIMIT: usize = 50;
/// 单次 embedding 请求的文本数。
const EMBEDDING_BATCH: usize = 32;
/// 摘要短于该字符数（且重要性也低）视为低质量。
pub const DEFAULT_MIN_SUMMARY_CHARS: usize = 12;
/// 重要性与使用率双低、超过该天数未更新，视为可遗忘的低质量记忆。
pub const DEFAULT_STALE_DAYS: i64 = 60;
/// 低质量判定的重要性下限。
pub const DEFAULT_IMPORTANCE_FLOOR: f64 = 0.35;
/// LLM 单次判定输出的 token 上限。
///
/// 一批可能给出多个合并组（每组含合并后的摘要），留足余量。推理型模型须
/// 关闭思考（见 CLI 对火山方舟的协议切换），否则推理会吃满额度、JSON 被截断。
const LLM_MAX_TOKENS: u32 = 8192;
/// 送给 LLM 的单条摘要最大字符数（控制单批提示长度）。
const PROMPT_SUMMARY_CHARS: usize = 800;
/// 默认 LLM 并发请求数。
pub const DEFAULT_CONCURRENCY: usize = 4;
/// LLM 请求失败后的默认重试次数（含首次共尝试 N 次）。
pub const DEFAULT_LLM_RETRY_ATTEMPTS: usize = 3;
/// LLM 请求失败后的默认重试间隔。
pub const DEFAULT_LLM_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(3);
/// 报告里保留的处理明细条数上限。
const MAX_REPORT_ACTIONS: usize = 200;

/// 整理进度（结构化，供 CLI 打印与设置页展示）。
///
/// `done/total` 为当前阶段的进度（`total` 为 0 表示总量未知）；`removed` /
/// `created` 为当前阶段累计删除 / 新建的记忆数；`llm_calls` 为本次整理累计的
/// LLM 调用次数（部分事件无法取得时为 0，展示方应取最大值）。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Progress {
    /// 阶段：prepare | vectors | dedup | entity | distill | similar。
    pub stage: &'static str,
    /// 面向用户的说明（CLI 原样打印）。
    pub message: String,
    pub done: usize,
    pub total: usize,
    pub removed: usize,
    pub created: usize,
    pub llm_calls: usize,
}
impl Progress {
    pub fn new(stage: &'static str, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
            ..Default::default()
        }
    }
    pub fn count(mut self, done: usize, total: usize) -> Self {
        self.done = done;
        self.total = total;
        self
    }
    pub fn removed(mut self, removed: usize) -> Self {
        self.removed = removed;
        self
    }
    pub fn created(mut self, created: usize) -> Self {
        self.created = created;
        self
    }
    pub fn llm_calls(mut self, llm_calls: usize) -> Self {
        self.llm_calls = llm_calls;
        self
    }
}
/// 整理进度回调。
pub type ConsolidationProgress<'a> = &'a (dyn Fn(&Progress) + Send + Sync);
/// 发送一条进度（未设置回调时忽略）。
pub(crate) fn emit(progress: Option<ConsolidationProgress<'_>>, event: impl FnOnce() -> Progress) {
    if let Some(callback) = progress {
        callback(&event());
    }
}

/// 记忆句柄提供者：每次调用返回当前可用的 [`MemoryHandle`]。
///
/// Leader 退出时 follower 监控会自动接替并替换内部句柄；整理跨越多轮 LLM
/// 调用，长连接可能中途失效，因此读写都经此获取最新句柄，写失败时刷新重试。
pub type HandleProvider<'a> = &'a (dyn Fn() -> MemoryHandle + Send + Sync);

/// 单个写操作失败后的重试次数（Leader 接替需要时间）。
const WRITE_RETRY_ATTEMPTS: usize = 3;
/// 重试前的等待时间（给 follower 监控留出接替窗口）。
const WRITE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(3);

/// 记忆深度整理参数。
#[derive(Clone)]
pub struct ConsolidationOptions {
    /// 相似召回阈值（0~1）：达到即送 LLM 判定，越高送审越少。
    pub similarity_threshold: f64,
    /// 单次 LLM 判定最多包含的记忆数（含种子，至少 2）。
    pub batch_size: usize,
    /// Memory LLM；为 `None` 时只合并文本完全相同的记忆。
    pub llm: Option<LlmEndpointConfig>,
    /// Embedding 提供者；为 `None` 时走「召回候选 + 文本相似度」。
    pub embedding: Option<Arc<dyn EmbeddingProvider>>,
    /// 试运行：只报告不落库。
    pub dry_run: bool,
    /// 是否遗忘低质量记忆。
    pub forget_low_quality: bool,
    /// 低质量判定：摘要最短字符数。
    pub min_summary_chars: usize,
    /// 低质量判定：超过该天数未更新且从未被使用。
    pub stale_days: i64,
    /// 低质量判定：重要性下限。
    pub importance_floor: f64,
    /// 最多以多少条记忆为种子处理（0 表示全部）。种子按相似记忆数从多到少
    /// 排列，没有相似记忆的排在最后。
    pub max_nodes: usize,
    /// 最多调用多少次 LLM（0 表示不限），用于试运行控制成本。
    pub max_llm_calls: usize,
    /// 跳过最新的 N 条记忆再开始扫描（配合扫描上限分批处理全库）。
    pub offset: usize,
    /// 最多扫描多少批（0 表示直到全库整理完成）。
    pub max_pages: usize,
    /// 向量库里已算好的向量（读取即用，不重新向量化）。
    pub stored_vectors: Option<std::sync::Arc<StoredVectors>>,
    /// LLM 请求失败后的重试次数（含首次共尝试 N 次）。
    pub llm_retry_attempts: usize,
    /// LLM 请求失败后的重试间隔。
    pub llm_retry_delay: std::time::Duration,
    /// 要执行的阶段（按 dedup → distill → similar 顺序执行；空表示全部）。
    pub stages: Vec<ConsolidationStage>,
    /// dedup：摘要短于该字符数的记忆不参与归一。
    pub dedup_min_summary_chars: usize,
    /// distill：相邻工具日志超过该间隔（分钟）即切分时间段。
    pub distill_window_minutes: i64,
    /// distill：单次 LLM 调用最多包含的工具日志数。
    pub distill_batch_size: usize,
    /// 执行记录（审计日志）：删除/改写/关联变更/LLM 判定逐条落盘。
    pub journal: Option<Arc<Journal>>,
    /// 停止请求（Ctrl+C）：在当前批次落库完成后停止，不留下半完成的合并。
    pub stop: Arc<std::sync::atomic::AtomicBool>,
    /// LLM 并发请求数（至少 1）。判定并发进行，落库仍按顺序串行。
    pub concurrency: usize,
}

impl Default for ConsolidationOptions {
    fn default() -> Self {
        Self {
            similarity_threshold: DEFAULT_SIMILARITY_THRESHOLD,
            batch_size: DEFAULT_BATCH_SIZE,
            llm: None,
            embedding: None,
            dry_run: false,
            forget_low_quality: true,
            min_summary_chars: DEFAULT_MIN_SUMMARY_CHARS,
            stale_days: DEFAULT_STALE_DAYS,
            importance_floor: DEFAULT_IMPORTANCE_FLOOR,
            max_nodes: 0,
            max_llm_calls: 0,
            offset: 0,
            max_pages: 0,
            stored_vectors: None,
            llm_retry_attempts: DEFAULT_LLM_RETRY_ATTEMPTS,
            llm_retry_delay: DEFAULT_LLM_RETRY_DELAY,
            stages: Vec::new(),
            dedup_min_summary_chars: DEFAULT_DEDUP_MIN_SUMMARY_CHARS,
            distill_window_minutes: DEFAULT_DISTILL_WINDOW_MINUTES,
            distill_batch_size: DEFAULT_DISTILL_BATCH_SIZE,
            journal: None,
            stop: Arc::default(),
            concurrency: DEFAULT_CONCURRENCY,
        }
    }
}

impl ConsolidationOptions {
    /// 是否已请求停止。
    pub(crate) fn stop_requested(&self) -> bool {
        self.stop.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 追加一条执行记录（未配置记录时忽略）；写入失败返回错误，调用方应中断。
    pub(crate) fn record(&self, event: &str, fields: serde_json::Value) -> anyhow::Result<()> {
        match self.journal.as_deref() {
            Some(journal) => {
                let mut fields = fields;
                if let serde_json::Value::Object(map) = &mut fields {
                    map.insert("dry_run".to_string(), self.dry_run.into());
                }
                journal.record(event, fields)
            }
            None => Ok(()),
        }
    }
}

/// 记录完整快照后物理删除一条记忆（试运行只记录）。
///
/// 快照含节点全部字段、当前全部关联与证据路径，可据此追溯或人工恢复；
/// 记录写入失败时不删除。
pub(crate) async fn delete_with_journal(
    provider: HandleProvider<'_>,
    options: &ConsolidationOptions,
    node: &MemoryNode,
    stage: &str,
    reason: &str,
    merged_into: Option<&str>,
    known_relations: Option<&[MemoryRelation]>,
) -> anyhow::Result<()> {
    if options.journal.is_some() {
        // 合并时关联会先改挂到主记忆再删除组员：调用方传入改挂前读取的关联，
        // 快照才是删除前的原貌。
        let relations: Vec<MemoryRelation> = match known_relations {
            Some(known) => known
                .iter()
                .filter(|relation| {
                    relation.from_node_id == node.id || relation.to_node_id == node.id
                })
                .cloned()
                .collect(),
            None => provider().list_relations(node.id.clone()).await,
        };
        let evidence_path = if node.kind == tiangong_memory::MemoryKind::Evidence {
            provider()
                .evidence_paths(vec![node.id.clone()])
                .await
                .ok()
                .and_then(|items| items.into_iter().next())
                .map(|item| item.path)
        } else {
            None
        };
        options.record(
            "delete_node",
            serde_json::json!({
                "stage": stage,
                "reason": reason,
                "merged_into": merged_into,
                "node": node,
                "evidence_path": evidence_path,
                "relations": relations,
            }),
        )?;
    }
    if !options.dry_run {
        retry_write(provider, |handle| {
            let node_id = node.id.clone();
            async move { handle.delete_node(node_id).await }
        })
        .await?;
    }
    Ok(())
}

/// 记录并写入一条记忆的改写或新建（试运行只记录）。`before` 为 `None` 表示新建。
pub(crate) async fn upsert_with_journal(
    provider: HandleProvider<'_>,
    options: &ConsolidationOptions,
    stage: &str,
    before: Option<&MemoryNode>,
    draft: ManualMemoryDraft,
    reason: &str,
) -> anyhow::Result<Option<MemoryNode>> {
    let event = if before.is_some() {
        "update_node"
    } else {
        "create_node"
    };
    let written = if options.dry_run {
        None
    } else {
        Some(
            retry_write(provider, |handle| {
                let draft = draft.clone();
                async move { handle.upsert_manual_memory(draft).await }
            })
            .await?,
        )
    };
    // 新建记忆的 ID 只有写入后才知道，因此在写入后记录（改写不涉及删除，
    // 失败时内容仍在库中）。
    options.record(
        event,
        serde_json::json!({
            "stage": stage,
            "reason": reason,
            "before": before,
            "after": draft,
            "id": written.as_ref().map(|node| node.id.clone()),
        }),
    )?;
    Ok(written)
}

impl ConsolidationOptions {
    /// 是否执行某个阶段（未指定阶段时执行全部）。
    pub fn runs(&self, stage: ConsolidationStage) -> bool {
        self.stages.is_empty() || self.stages.contains(&stage)
    }
}

/// 整理结果报告。
#[derive(Debug, Clone, Default)]
pub struct ConsolidationReport {
    /// 扫描到的活跃记忆数。
    pub scanned: usize,
    /// 分批扫描的批次数（全库超过单批上限时 > 1）。
    pub pages: usize,
    /// 作为种子处理过的记忆条数。
    pub processed: usize,
    /// 未作为种子处理、且仍存活的剩余条数（受 `max_nodes` 影响）。
    pub remaining: usize,
    /// 达到阈值的相似记忆对数（送审候选规模）。
    pub candidate_pairs: usize,
    /// 拥有至少一条相似记忆的记忆条数。
    pub nodes_with_candidates: usize,
    /// 单条记忆的最大相似记忆数。
    pub max_candidates: usize,
    /// 相似聚类统计（相似关系连通的记忆簇）。
    pub clusters: ClusterStats,
    /// 送 LLM 判定的记忆条次（同一记忆可出现在多批中）。
    pub reviewed: usize,
    /// 发生合并的组数。
    pub merged_groups: usize,
    /// 被合并删除的记忆条数。
    pub merged_away: usize,
    /// 被重写（归一/重分类）的主记忆条数。
    pub updated: usize,
    /// 其中认知分类发生变化的条数。
    pub reclassified: usize,
    /// 被删除的低质量记忆条数（规则 + LLM）。
    pub forgotten: usize,
    /// 机械重指向重建的关联数。
    pub relations_relinked: usize,
    /// LLM 新建的关联数。
    pub relations_created: usize,
    /// LLM 调用次数。
    pub llm_calls: usize,
    /// 是否使用了 LLM。
    pub used_llm: bool,
    /// 使用的检索后端。
    pub retrieval_backend: &'static str,
    /// 向量库中暂无向量的记忆条数（未参与相似比较）。
    pub without_vectors: usize,
    /// 是否试运行。
    pub dry_run: bool,
    /// 是否因达到 LLM 调用上限而提前停止。
    pub llm_budget_exhausted: bool,
    /// 致命中断原因（LLM 连续失败等）：整理被中断，后续批次不再执行。
    pub fatal_error: Option<String>,
    /// 人类可读的处理明细（限量；单条可含多行组员明细）。
    pub actions: Vec<String>,
    /// 处理过程中的错误（不中断整理）。
    pub errors: Vec<String>,
    /// 实际执行的阶段。
    pub stages: Vec<ConsolidationStage>,
    /// dedup：归一的组数。
    pub dedup_groups: usize,
    /// dedup：删除的重复记忆数。
    pub dedup_removed: usize,
    /// dedup：标题/重要性被改写的主记忆数。
    pub dedup_updated: usize,
    /// dedup：改挂到主记忆的关联数。
    pub dedup_relinked: usize,
    /// distill：工具日志条数。
    pub distill_logs: usize,
    /// distill：计划批数。
    pub distill_batches_total: usize,
    /// distill：已完成批数。
    pub distill_batches: usize,
    /// distill：提炼出的新经历数。
    pub distill_created: usize,
    /// distill：删除的日志数。
    pub distill_deleted: usize,
    /// distill：已判定后保留的日志数。
    pub distill_kept: usize,
    /// distill：新建或改挂的关联数。
    pub distill_relations: usize,
    /// distill：是否因达到 LLM 调用上限提前停止。
    pub distill_budget_exhausted: bool,
    /// similar 阶段开始前已发生的 LLM 调用数（各阶段分别计算调用上限）。
    pub similar_llm_base: usize,
    /// similar：排除的工具日志条数。
    pub similar_excluded_logs: usize,
    /// 是否因手动停止（Ctrl+C）而提前结束。
    pub stopped: bool,
    /// entity：名称重复的实体名数。
    pub entity_names: usize,
    /// entity：涉及的实体条数。
    pub entity_members: usize,
    /// entity：完成的 LLM 判定批数。
    pub entity_batches: usize,
    /// entity：预计 LLM 判定批数（进度分母）。
    pub entity_batches_total: usize,
    /// entity：合并组数。
    pub entity_groups: usize,
    /// entity：删除的实体数（合并 + 无信息量）。
    pub entity_removed: usize,
    /// entity：改挂的关联数。
    pub entity_relinked: usize,
    /// entity：是否因达到 LLM 调用上限提前停止。
    pub entity_budget_exhausted: bool,
    /// entity：进行中的 LLM 请求数（并发时控制调用上限，内部使用）。
    pub entity_inflight: usize,
}

impl ConsolidationReport {
    /// 渲染为面向用户的文本报告。
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "记忆深度整理{}",
            if self.dry_run {
                "（试运行，未落库）"
            } else if self.fatal_error.is_some() {
                "中断"
            } else if self.stopped {
                "已手动停止（已完成的批次均已落库，可直接重新运行继续）"
            } else {
                "完成"
            }
        );
        if let Some(fatal) = &self.fatal_error {
            let _ = writeln!(out, "- 致命中断：{fatal}");
        }
        let stages: Vec<&str> = self.stages.iter().map(|stage| stage.key()).collect();
        let _ = writeln!(out, "- 执行阶段：{}", stages.join(" → "));
        if self.stages.contains(&ConsolidationStage::Dedup) {
            let _ = writeln!(
                out,
                "- [dedup] 摘要完全一致：归一 {} 组，删除重复 {} 条，改写主记忆 {} 条，改挂关联 {} 条",
                self.dedup_groups, self.dedup_removed, self.dedup_updated, self.dedup_relinked
            );
        }
        if self.stages.contains(&ConsolidationStage::Entity) {
            let _ = writeln!(
                out,
                "- [entity] 同名实体（跨工作区）：{} 个名称涉及 {} 条，LLM 判定 {} 批：合并 {} 组，删除 {} 条，改挂关联 {} 条{}",
                self.entity_names,
                self.entity_members,
                self.entity_batches,
                self.entity_groups,
                self.entity_removed,
                self.entity_relinked,
                if self.entity_budget_exhausted {
                    "（已达 LLM 调用上限，提前停止）"
                } else {
                    ""
                }
            );
        }
        if self.stages.contains(&ConsolidationStage::Distill) {
            let _ = writeln!(
                out,
                "- [distill] 工具日志 {} 条，完成 {}/{} 批：提炼新经历 {} 条，删除日志 {} 条，保留 {} 条，关联 {} 条{}",
                self.distill_logs,
                self.distill_batches,
                self.distill_batches_total,
                self.distill_created,
                self.distill_deleted,
                self.distill_kept,
                self.distill_relations,
                if self.distill_budget_exhausted {
                    "（已达 LLM 调用上限，提前停止）"
                } else {
                    ""
                }
            );
        }
        if !self.stages.contains(&ConsolidationStage::Similar) {
            let _ = writeln!(out, "- LLM 调用合计：{} 次", self.llm_calls);
            self.render_details(&mut out);
            return out;
        }
        if self.similar_excluded_logs > 0 {
            let _ = writeln!(
                out,
                "- [similar] 已排除工具日志 {} 条（由 distill 阶段处理）",
                self.similar_excluded_logs
            );
        }
        let _ = writeln!(
            out,
            "- 检索后端：{}；扫描活跃记忆：{}（{} 批）",
            self.retrieval_backend, self.scanned, self.pages
        );
        if self.without_vectors > 0 {
            let _ = writeln!(
                out,
                "- 其中 {} 条暂无向量（未参与相似比较）",
                self.without_vectors
            );
        }
        let _ = writeln!(
            out,
            "- 相似候选：{} 对，涉及 {} 条记忆（单条最多 {} 条相似）",
            self.candidate_pairs, self.nodes_with_candidates, self.max_candidates
        );
        if self.clusters.count > 0 {
            let _ = writeln!(
                out,
                "- 相似簇：{} 个，最大簇 {} 条；相似记忆数分布：{}",
                self.clusters.count,
                self.clusters.largest,
                self.clusters.degree_histogram()
            );
            let _ = writeln!(
                out,
                "- LLM 判定次数估算：约 {} ~ {} 次（下限按簇内全部合并、上限按全部保留估算，每批 {} 条）",
                self.clusters.min_calls, self.clusters.max_calls, self.clusters.batch_size
            );
            if !self.clusters.densest.is_empty() {
                let _ = writeln!(out, "- 相似记忆最多的记忆：");
                for (title, degree, id) in &self.clusters.densest {
                    let _ = writeln!(out, "    {degree:>5} 条相似  {id}「{title}」");
                }
            }
        }
        let _ = writeln!(
            out,
            "- 已处理种子：{}（剩余未处理：{}）；LLM 判定 {} 次，送审 {} 条次{}",
            self.processed,
            self.remaining,
            self.llm_calls,
            self.reviewed,
            if self.llm_budget_exhausted {
                "（已达 LLM 调用上限，提前停止）"
            } else {
                ""
            }
        );
        let _ = writeln!(
            out,
            "- 合并组数：{}，合并删除：{}，重写归一：{}（重分类 {}）",
            self.merged_groups, self.merged_away, self.updated, self.reclassified
        );
        let _ = writeln!(out, "- 删除低质量记忆：{}", self.forgotten);
        let _ = writeln!(
            out,
            "- 关联链：重指向重建 {}，LLM 新建 {}",
            self.relations_relinked, self.relations_created
        );
        self.render_details(&mut out);
        out
    }

    fn render_details(&self, out: &mut String) {
        if !self.actions.is_empty() {
            let _ = writeln!(out, "处理明细：");
            for action in self.actions.iter().take(MAX_REPORT_ACTIONS) {
                let _ = writeln!(out, "  - {action}");
            }
            if self.actions.len() > MAX_REPORT_ACTIONS {
                let _ = writeln!(
                    out,
                    "  - ……其余 {} 条明细已省略",
                    self.actions.len() - MAX_REPORT_ACTIONS
                );
            }
        }
        if !self.errors.is_empty() {
            let _ = writeln!(out, "错误（不影响已完成的整理）：");
            for error in &self.errors {
                let _ = writeln!(out, "  - {error}");
            }
        }
    }
}

/// 执行一次记忆深度整理：按 dedup → distill → similar 顺序执行所选阶段。
///
/// 试运行时后面的阶段会跳过前面阶段「模拟删除」的记忆，统计与真实执行一致。
pub async fn run_consolidation(
    provider: HandleProvider<'_>,
    options: &ConsolidationOptions,
    progress: Option<ConsolidationProgress<'_>>,
) -> ConsolidationReport {
    let mut report = ConsolidationReport {
        dry_run: options.dry_run,
        stages: ConsolidationStage::ALL
            .into_iter()
            .filter(|stage| options.runs(*stage))
            .collect(),
        ..Default::default()
    };
    let mut removed: HashSet<String> = HashSet::new();
    if options.runs(ConsolidationStage::Dedup)
        || options.runs(ConsolidationStage::Entity)
        || options.runs(ConsolidationStage::Distill)
    {
        // 前置阶段面向全库（不分批、不受 offset 影响）。
        let nodes = collect_active_nodes(provider, 0, usize::MAX).await;
        emit(progress, || {
            Progress::new(
                "prepare",
                format!("前置阶段：读取到 {} 条活跃记忆", nodes.len()),
            )
        });
        if options.runs(ConsolidationStage::Dedup)
            && let Err(err) = run_dedup(
                provider,
                &nodes,
                options,
                &mut removed,
                progress,
                &mut report,
            )
            .await
        {
            fail(&mut report, "dedup", err);
            return report;
        }
        if report.stopped {
            return report;
        }
        if options.runs(ConsolidationStage::Entity)
            && let Err(err) = run_entity(
                provider,
                &nodes,
                options,
                &mut removed,
                progress,
                &mut report,
            )
            .await
        {
            fail(&mut report, "entity", err);
            return report;
        }
        if report.stopped {
            return report;
        }
        if options.runs(ConsolidationStage::Distill)
            && let Err(err) = run_distill(
                provider,
                &nodes,
                options,
                &mut removed,
                progress,
                &mut report,
            )
            .await
        {
            fail(&mut report, "distill", err);
            return report;
        }
    }
    if report.stopped {
        return report;
    }
    if options.runs(ConsolidationStage::Similar) {
        report.similar_llm_base = report.llm_calls;
        run_similar(provider, options, &removed, progress, &mut report).await;
    }
    report
}

/// similar 阶段：相似召回 + LLM 判定（排除工具日志与前置阶段已删除的记忆）。
async fn run_similar(
    provider: HandleProvider<'_>,
    options: &ConsolidationOptions,
    removed: &HashSet<String>,
    progress: Option<ConsolidationProgress<'_>>,
    report: &mut ConsolidationReport,
) {
    // 全库分批：每批扫描上限内的活跃记忆，整理完再取下一批（跳过已整理的
    // 最新若干条），直到某批不足上限即全库整理完成。`offset` 用于从指定位置
    // 开始（断点续跑），`max_nodes` 约束单批种子条数。
    let stored = options.stored_vectors.clone();
    // 向量路径（读向量库 / 本地 embedding）一批覆盖全库；召回路径分批 bounds。
    let scan_cap = if stored.is_some() || options.embedding.is_some() {
        MAX_SCAN_NODES_VECTOR
    } else {
        MAX_SCAN_NODES
    };
    let mut skip = options.offset;
    loop {
        let fetched = collect_active_nodes(provider, skip, scan_cap).await;
        if fetched.is_empty() {
            break;
        }
        let page = fetched.len();
        // 工具日志交给 distill 阶段；前置阶段已删除（或试运行中模拟删除）的记忆跳过。
        let nodes: Vec<MemoryNode> = fetched
            .into_iter()
            .filter(|node| !removed.contains(&node.id))
            .collect();
        let without_removed = nodes.len();
        let nodes: Vec<MemoryNode> = nodes
            .into_iter()
            .filter(|node| !is_tool_log(node))
            .collect();
        report.similar_excluded_logs += without_removed - nodes.len();
        report.pages += 1;
        report.scanned += nodes.len();
        emit(progress, || {
            Progress::new(
                "similar",
                format!(
                    "第 {} 批：扫描到 {} 条活跃记忆（累计跳过最新 {} 条）",
                    report.pages, page, skip
                ),
            )
            .llm_calls(report.llm_calls)
        });
        if let Some(stored) = stored.as_deref() {
            report.without_vectors += nodes
                .iter()
                .filter(|node| stored.get(&node.id).is_none())
                .count();
        }
        consolidate_page(
            provider,
            report.pages,
            nodes,
            stored.as_deref(),
            options,
            progress,
            report,
        )
        .await;
        skip += page;
        if report.fatal_error.is_some() || report.llm_budget_exhausted || report.stopped {
            // 致命错误（LLM 连续失败等）或达到 LLM 调用上限：停止后续批次。
            break;
        }
        if page < PAGE_LIMIT {
            break;
        }
        if options.max_pages > 0 && report.pages >= options.max_pages {
            emit(progress, || {
                Progress::new(
                    "similar",
                    format!(
                        "已达单次批次上限（{} 批），剩余记忆可用 --offset {} 继续",
                        options.max_pages, skip
                    ),
                )
                .llm_calls(report.llm_calls)
            });
            break;
        }
    }
}

/// 相似聚类统计：帮助判断阈值是否合适、预估 LLM 判定次数。
#[derive(Debug, Clone, Default)]
pub struct ClusterStats {
    /// 相似簇数（至少两条记忆、经相似关系连通）。
    pub count: usize,
    /// 最大簇的记忆数。
    pub largest: usize,
    /// 相似记忆数分布：1、2~5、6~20、21~100、101~500、>500 条。
    pub degree_buckets: [usize; 6],
    /// 簇内全部合并时的最少 LLM 判定次数（每批合并 batch_size-1 条）。
    pub min_calls: usize,
    /// 全部保留时的最多 LLM 判定次数（每个种子的相似记忆分批送审）。
    pub max_calls: usize,
    /// 估算所用的单批上限。
    pub batch_size: usize,
    /// 相似记忆最多的若干条记忆：(标题, 相似数, ID)。
    pub densest: Vec<(String, usize, String)>,
}

impl ClusterStats {
    fn compute(nodes: &[MemoryNode], neighbors: &[Vec<(u32, f32)>], batch_size: usize) -> Self {
        let len = neighbors.len();
        let mut parent: Vec<usize> = (0..len).collect();
        fn find(parent: &mut [usize], mut node: usize) -> usize {
            while parent[node] != node {
                parent[node] = parent[parent[node]];
                node = parent[node];
            }
            node
        }
        for (left, list) in neighbors.iter().enumerate() {
            for (right, _) in list {
                let a = find(&mut parent, left);
                let b = find(&mut parent, *right as usize);
                if a != b {
                    parent[a] = b;
                }
            }
        }
        let mut sizes: HashMap<usize, usize> = HashMap::new();
        for (position, list) in neighbors.iter().enumerate() {
            if !list.is_empty() {
                *sizes.entry(find(&mut parent, position)).or_default() += 1;
            }
        }
        let per_batch = batch_size.saturating_sub(1).max(1);
        let mut stats = Self {
            count: sizes.len(),
            largest: sizes.values().copied().max().unwrap_or_default(),
            min_calls: sizes
                .values()
                .map(|size| (size - 1).div_ceil(per_batch))
                .sum(),
            batch_size,
            ..Default::default()
        };
        // 全部保留时：每个种子最多把自己的全部相似记忆分批送审一次（已与其他
        // 种子一起判定过的会被跳过，实际更少），故上界为各记忆 ⌈相似数/每批⌉ 之和。
        // 该上界不小于簇数，与「全部合并」的下界保持有序。
        stats.max_calls = neighbors
            .iter()
            .map(|list| list.len().div_ceil(per_batch))
            .sum();
        for list in neighbors.iter().filter(|list| !list.is_empty()) {
            let bucket = match list.len() {
                1 => 0,
                2..=5 => 1,
                6..=20 => 2,
                21..=100 => 3,
                101..=500 => 4,
                _ => 5,
            };
            stats.degree_buckets[bucket] += 1;
        }
        let mut dense: Vec<usize> = (0..len).filter(|p| !neighbors[*p].is_empty()).collect();
        dense.sort_by_key(|position| std::cmp::Reverse(neighbors[*position].len()));
        stats.densest = dense
            .into_iter()
            .take(10)
            .map(|position| {
                (
                    one_line(&nodes[position].title, 40),
                    neighbors[position].len(),
                    nodes[position].id.clone(),
                )
            })
            .collect();
        stats
    }

    fn degree_histogram(&self) -> String {
        const LABELS: [&str; 6] = ["1", "2~5", "6~20", "21~100", "101~500", ">500"];
        LABELS
            .iter()
            .zip(self.degree_buckets)
            .map(|(label, count)| format!("{label} 条:{count}"))
            .collect::<Vec<_>>()
            .join("，")
    }
}

/// 一批记忆的整理状态：节点快照（合并后就地更新）、相似图与处理进度。
struct PageState {
    nodes: Vec<MemoryNode>,
    /// 相似邻接表：位置 → (邻居位置, 相似度)。
    neighbors: Vec<Vec<(u32, f32)>>,
    /// 仍存活（未被合并删除 / 遗忘）。
    alive: Vec<bool>,
    /// 已作为种子处理过。
    visited: Vec<bool>,
    /// 已在同一批中交给 LLM 判定过的记忆对（小位置在前）。
    judged: HashSet<(u32, u32)>,
    /// 本批已删除（合并/遗忘）的记忆数。
    removed: usize,
}

impl PageState {
    fn new(nodes: Vec<MemoryNode>, neighbors: Vec<Vec<(u32, f32)>>) -> Self {
        let len = nodes.len();
        Self {
            nodes,
            neighbors,
            alive: vec![true; len],
            visited: vec![false; len],
            judged: HashSet::new(),
            removed: 0,
        }
    }

    /// 标记一条记忆已删除（合并或遗忘）。
    fn remove(&mut self, position: usize) {
        if std::mem::replace(&mut self.alive[position], false) {
            self.removed += 1;
        }
    }

    /// 当前仍存活的记忆数。
    fn alive_count(&self) -> usize {
        self.nodes.len() - self.removed
    }

    fn pair(left: usize, right: usize) -> (u32, u32) {
        let (low, high) = if left < right {
            (left, right)
        } else {
            (right, left)
        };
        (low as u32, high as u32)
    }

    /// 种子尚未与之一起判定过的存活相似记忆（按相似度降序）。
    fn pending(&self, seed: usize) -> Vec<(usize, f32)> {
        let mut pending: Vec<(usize, f32)> = self.neighbors[seed]
            .iter()
            .map(|(other, score)| (*other as usize, *score))
            .filter(|(other, _)| {
                self.alive[*other] && !self.judged.contains(&Self::pair(seed, *other))
            })
            .collect();
        pending.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        pending
    }

    /// 标记一批记忆两两判定过。
    fn mark_judged(&mut self, batch: &[usize]) {
        for (offset, left) in batch.iter().enumerate() {
            for right in &batch[offset + 1..] {
                self.judged.insert(Self::pair(*left, *right));
            }
        }
    }

    /// 被合并的记忆把相似关系交给主记忆：返回主记忆是否获得了尚未判定的新候选。
    fn inherit(&mut self, from: usize, into: usize) -> bool {
        let inherited = std::mem::take(&mut self.neighbors[from]);
        let mut gained = false;
        for (other, score) in inherited {
            let other = other as usize;
            if other == into || !self.alive[other] {
                continue;
            }
            upsert_neighbor(&mut self.neighbors[into], other, score);
            upsert_neighbor(&mut self.neighbors[other], into, score);
            if !self.judged.contains(&Self::pair(into, other)) {
                gained = true;
            }
        }
        gained
    }
}

/// 写入或抬高邻接表中的相似度。
fn upsert_neighbor(list: &mut Vec<(u32, f32)>, other: usize, score: f32) {
    match list.iter_mut().find(|(id, _)| *id as usize == other) {
        Some(entry) => entry.1 = entry.1.max(score),
        None => list.push((other as u32, score)),
    }
}

/// 整理一批记忆：相似召回 → 逐个种子分批交 LLM 判定。
async fn consolidate_page(
    provider: HandleProvider<'_>,
    page_no: usize,
    nodes: Vec<MemoryNode>,
    stored: Option<&StoredVectors>,
    options: &ConsolidationOptions,
    progress: Option<ConsolidationProgress<'_>>,
    report: &mut ConsolidationReport,
) {
    let neighbors = match build_neighbors(provider, &nodes, stored, options, progress).await {
        Ok((neighbors, backend)) => {
            report.retrieval_backend = backend;
            neighbors
        }
        Err(err) => {
            report
                .errors
                .push(format!("相似检索失败，本批仅执行低质量遗忘：{err:#}"));
            report.retrieval_backend = "unavailable";
            vec![Vec::new(); nodes.len()]
        }
    };
    let pairs: usize = neighbors.iter().map(Vec::len).sum::<usize>() / 2;
    report.candidate_pairs += pairs;
    report.nodes_with_candidates += neighbors.iter().filter(|list| !list.is_empty()).count();
    report.max_candidates = report
        .max_candidates
        .max(neighbors.iter().map(Vec::len).max().unwrap_or_default());
    emit(progress, || {
        Progress::new(
            "similar",
            format!(
                "第 {page_no} 批：相似候选 {pairs} 对（阈值 {:.2}），开始逐条判定",
                options.similarity_threshold
            ),
        )
        .count(0, nodes.len())
        .llm_calls(report.llm_calls)
    });
    report.clusters = ClusterStats::compute(&nodes, &neighbors, options.batch_size.max(2));
    let total = nodes.len();
    // 种子顺序：相似记忆多的优先（同数时保持原顺序，即更新时间从新到旧），
    // 没有相似记忆的排在最后（只做低质量检查）。`--max-nodes` 因此优先覆盖
    // 真正需要 LLM 判定的记忆。
    let mut order: Vec<usize> = (0..total).collect();
    order.sort_by_key(|position| std::cmp::Reverse(neighbors[*position].len()));
    let mut state = PageState::new(nodes, neighbors);
    let mut queue: VecDeque<usize> = order.into_iter().collect();
    let mut handled = 0;
    let batch_size = options.batch_size.max(2);
    let concurrency = options.concurrency.max(1);
    // 并发调度：同时在途的批次两两不共享记忆（busy），LLM 判定并发进行；
    // 结果按返回顺序逐批落库（只有一个写入者，落库期间状态不被其他批次修改）。
    // 在途批次的记忆不会被其他批次合并或删除，送审时的快照即落库时的状态。
    let mut busy: HashSet<usize> = HashSet::new();
    // 相似候选全部在途、暂时无法组批的种子：有批次落库后重新排队。
    let mut deferred: Vec<usize> = Vec::new();
    let mut inflight = futures_util::stream::FuturesUnordered::new();
    let mut launched = 0usize;
    let mut aborted = false;
    loop {
        // 1) 补满并发窗口。
        while !aborted
            && !report.stopped
            && !report.llm_budget_exhausted
            && inflight.len() < concurrency
        {
            let Some(seed) = queue.pop_front() else {
                break;
            };
            if options.stop_requested() {
                report.stopped = true;
                break;
            }
            if !state.alive[seed] {
                continue;
            }
            if busy.contains(&seed) {
                deferred.push(seed);
                continue;
            }
            // 达到种子上限后整体停止（含排队等待继续判定的主记忆）。
            if options.max_nodes > 0 && handled >= options.max_nodes && !state.visited[seed] {
                queue.clear();
                break;
            }
            if !state.visited[seed] {
                state.visited[seed] = true;
                handled += 1;
                report.processed += 1;
                // 分母为当前存活数：被合并/删除的记忆不再处理，分母随之减少。
                emit(progress, || {
                    Progress::new(
                        "similar",
                        format!(
                            "第 {} 批整理中 [{}/{}]（已删除 {} 条）{}",
                            page_no,
                            handled,
                            state.alive_count(),
                            state.removed,
                            one_line(&state.nodes[seed].title, 40)
                        ),
                    )
                    .count(handled, state.alive_count())
                    .removed(state.removed)
                    .llm_calls(report.llm_calls)
                });
                // 规则低质量遗忘（不调用 LLM，直接落库）。
                if options.forget_low_quality
                    && let Some(reason) = rule_low_quality(&state.nodes[seed], options)
                {
                    let node = state.nodes[seed].clone();
                    if let Err(err) = delete_with_journal(
                        provider, options, &node, "similar", &reason, None, None,
                    )
                    .await
                    {
                        fail(report, &node.id, err);
                        aborted = true;
                        break;
                    }
                    state.remove(seed);
                    report.forgotten += 1;
                    report.actions.push(format!(
                        "删除低质量记忆「{}」（{}）{}",
                        one_line(&state.nodes[seed].title, 40),
                        reason,
                        dry_run_tag(options)
                    ));
                    continue;
                }
            }
            // 种子与尚未一起判定过、且不在途的相似记忆组批。
            let all_pending = state.pending(seed);
            if all_pending.is_empty() {
                continue;
            }
            let pending: Vec<(usize, f32)> = all_pending
                .into_iter()
                .filter(|(other, _)| !busy.contains(other))
                .collect();
            if pending.is_empty() {
                deferred.push(seed);
                continue;
            }
            if options.max_llm_calls > 0 && launched >= options.max_llm_calls {
                report.llm_budget_exhausted = true;
                queue.push_front(seed);
                break;
            }
            let left = pending.len();
            let mut batch = vec![seed];
            batch.extend(pending.iter().take(batch_size - 1).map(|(other, _)| *other));
            let scores: HashMap<usize, f32> = pending.into_iter().collect();
            emit(progress, || {
                Progress::new(
                    "similar",
                    format!(
                        "  LLM 判定：种子 + {} 条相似记忆（该种子剩余候选 {}，并发 {}）",
                        batch.len() - 1,
                        left,
                        inflight.len() + 1
                    ),
                )
                .count(handled, state.alive_count())
                .removed(state.removed)
                .llm_calls(report.llm_calls)
            });
            state.mark_judged(&batch);
            busy.extend(batch.iter().copied());
            launched += 1;
            let members: Vec<MemoryNode> = batch
                .iter()
                .map(|position| state.nodes[*position].clone())
                .collect();
            let rule = options.llm.is_none().then(|| rule_decision(&state, &batch));
            inflight.push(async move {
                let ids: Vec<String> = members.iter().map(|node| node.id.clone()).collect();
                let relations = provider().list_relations_batch(ids).await;
                let result = match (options.llm.as_ref(), rule) {
                    (Some(model), _) => {
                        let refs: Vec<&MemoryNode> = members.iter().collect();
                        let similarity: Vec<Option<f32>> = batch
                            .iter()
                            .map(|position| scores.get(position).copied())
                            .collect();
                        let prompt = build_batch_prompt(&refs, &similarity, &relations);
                        let label = format!("记忆 {} 所在批次", members[0].id);
                        call_llm_json::<BatchDecision>(
                            model,
                            CONSOLIDATION_SYSTEM,
                            &prompt,
                            &label,
                            members.len(),
                            options,
                        )
                        .await
                        .map(|(decision, raw)| (decision, Some((label, prompt, raw))))
                    }
                    (None, rule) => Ok((rule.unwrap_or_default(), None)),
                };
                (seed, batch, scores, relations, result)
            });
        }
        // 2) 等待一个在途批次并落库。
        let Some((seed, batch, scores, relations, result)) = inflight.next().await else {
            if deferred.is_empty() || aborted || report.stopped || report.llm_budget_exhausted {
                break;
            }
            // 没有在途批次：被推迟的种子此时一定能组批。
            queue.extend(deferred.drain(..));
            continue;
        };
        for position in &batch {
            busy.remove(position);
        }
        // 有记忆释放：被推迟的种子重新排到队首。
        for position in deferred.drain(..).rev() {
            queue.push_front(position);
        }
        if aborted {
            // 已中断：已发出的请求结果丢弃（未落库，下次重新判定）。
            continue;
        }
        let applied = async {
            let (decision, record) = result?;
            if let Some((label, prompt, raw)) = record {
                record_llm_decision(options, report, &label, &prompt, &raw)?;
            }
            report.reviewed += batch.len();
            let plan = validate_plan(&state, &batch, decision);
            apply_plan(
                provider, &mut state, &batch, &scores, plan, &relations, options, report,
            )
            .await
        }
        .await;
        match applied {
            Ok(requeue) => {
                // 种子还有未判定的候选：排到队首继续；合并后获得新候选的主记忆排到队尾。
                if state.alive[seed] && !state.pending(seed).is_empty() {
                    queue.push_front(seed);
                }
                for position in requeue {
                    if position != seed {
                        queue.push_back(position);
                    }
                }
            }
            Err(err) => {
                let node_id = state.nodes[seed].id.clone();
                fail(report, &node_id, err);
                aborted = true;
            }
        }
        if options.stop_requested() {
            report.stopped = true;
        }
    }
    report.remaining += (0..total)
        .filter(|position| state.alive[*position] && !state.visited[*position])
        .count();
}

/// 致命中断：LLM 连续失败或写重试耗尽。中断本批及后续批次，由调用方
/// （CLI）以非零退出码报错；已完成的批保持一致。
pub(crate) fn fail(report: &mut ConsolidationReport, node_id: &str, err: anyhow::Error) {
    report.fatal_error = Some(format!("{err:#}"));
    report
        .errors
        .push(format!("整理记忆 {node_id} 失败，已中断：{err:#}"));
}

/// 分页拉取活跃记忆（按更新时间从新到旧）；`skip` 跳过最新的若干条。
pub(crate) async fn collect_active_nodes(
    provider: HandleProvider<'_>,
    skip: usize,
    cap: usize,
) -> Vec<MemoryNode> {
    let mut all = Vec::new();
    let mut offset = skip;
    loop {
        // 每页都取当前句柄，Leader 接替后继续分页不丢数据。
        let batch = provider()
            .list_nodes(MemoryListQuery {
                status: Some(MemoryStatus::Active),
                limit: PAGE_LIMIT,
                offset,
                ..Default::default()
            })
            .await;
        if batch.is_empty() {
            break;
        }
        offset += batch.len();
        all.extend(batch);
        if all.len() >= cap {
            tracing::warn!("活跃记忆超过 {cap} 条，本次只整理前 {cap} 条；可用 --offset 分批处理");
            break;
        }
    }
    all
}

/// 为每条记忆找出全部达到阈值的相似记忆（对称邻接表）。
///
/// - 向量库已有向量：全量精确余弦（多线程），不做截断；
/// - 配置了在线 Embedding：先批量向量化，再同样全量精确余弦；
/// - 都没有：逐条经 Memory 召回取候选，再用字符二元组相似度按阈值过滤。
async fn build_neighbors(
    provider: HandleProvider<'_>,
    nodes: &[MemoryNode],
    stored: Option<&StoredVectors>,
    options: &ConsolidationOptions,
    progress: Option<ConsolidationProgress<'_>>,
) -> anyhow::Result<(Vec<Vec<(u32, f32)>>, &'static str)> {
    if let Some(stored) = stored {
        let vectors: Vec<Option<&[f32]>> = nodes
            .iter()
            .map(|node| stored.get(&node.id).map(Vec::as_slice))
            .collect();
        emit(progress, || {
            Progress::new("similar", "计算全量余弦相似度……")
        });
        let started = Instant::now();
        let neighbors = exact_neighbors(&vectors, options.similarity_threshold);
        tracing::info!(
            nodes = nodes.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "全量余弦相似度计算完成"
        );
        return Ok((neighbors, "vector_store"));
    }
    if let Some(embedding) = options.embedding.as_ref() {
        let vectors = embed_nodes(embedding, nodes, progress).await?;
        let refs: Vec<Option<&[f32]>> = vectors.iter().map(|v| Some(v.as_slice())).collect();
        return Ok((
            exact_neighbors(&refs, options.similarity_threshold),
            "vector",
        ));
    }
    let index: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(position, node)| (node.id.as_str(), position))
        .collect();
    let mut neighbors: Vec<Vec<(u32, f32)>> = vec![Vec::new(); nodes.len()];
    let total = nodes.len();
    for (position, node) in nodes.iter().enumerate() {
        emit(progress, || {
            Progress::new(
                "similar",
                format!(
                    "检索中 [{}/{}] {}",
                    position + 1,
                    total,
                    one_line(&node.title, 40)
                ),
            )
            .count(position + 1, total)
        });
        // 每次召回都取当前句柄：Leader 退出后 follower 监控会自动接替。
        let hits = provider()
            .recall(
                RecallAnchors {
                    keywords: node.keywords.clone(),
                    query: node_text(node),
                    strategy: Some(SearchStrategy::Hybrid {
                        semantic_ratio: 0.8,
                    }),
                },
                LEXICAL_RECALL_LIMIT,
            )
            .await;
        let source_text = node_text(node);
        for hit in hits {
            let Some(&other) = index.get(hit.node_id.as_str()) else {
                continue;
            };
            if other == position {
                continue;
            }
            let score = text_similarity(&source_text, &node_text(&nodes[other]));
            if score >= options.similarity_threshold {
                upsert_neighbor(&mut neighbors[position], other, score as f32);
                upsert_neighbor(&mut neighbors[other], position, score as f32);
            }
        }
    }
    Ok((neighbors, "lexical"))
}

/// 全量精确余弦：两两比较全部带向量的记忆，返回达到阈值的对称邻接表。
///
/// 向量先归一化并连续存放，按行跨线程分摊（行 i 只与 j > i 比较）。
fn exact_neighbors(vectors: &[Option<&[f32]>], threshold: f64) -> Vec<Vec<(u32, f32)>> {
    let mut neighbors: Vec<Vec<(u32, f32)>> = vec![Vec::new(); vectors.len()];
    let dimension = vectors
        .iter()
        .flatten()
        .map(|vector| vector.len())
        .find(|len| *len > 0)
        .unwrap_or_default();
    let positions: Vec<usize> = (0..vectors.len())
        .filter(|position| vectors[*position].is_some_and(|v| v.len() == dimension))
        .collect();
    if positions.len() < 2 || dimension == 0 {
        return neighbors;
    }
    let mut matrix: Vec<f32> = Vec::with_capacity(positions.len() * dimension);
    for position in &positions {
        matrix.extend(normalize(vectors[*position].unwrap_or_default()));
    }
    let rows = positions.len();
    let threshold = threshold as f32;
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 32);
    let matrix = &matrix;
    let pairs: Vec<(u32, u32, f32)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|worker| {
                scope.spawn(move || {
                    let mut found = Vec::new();
                    let mut row = worker;
                    while row < rows {
                        let left = &matrix[row * dimension..(row + 1) * dimension];
                        for col in row + 1..rows {
                            let right = &matrix[col * dimension..(col + 1) * dimension];
                            let score = dot(left, right);
                            if score >= threshold {
                                found.push((row as u32, col as u32, score));
                            }
                        }
                        row += threads;
                    }
                    found
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    });
    for (row, col, score) in pairs {
        let left = positions[row as usize];
        let right = positions[col as usize];
        neighbors[left].push((right as u32, score.min(1.0)));
        neighbors[right].push((left as u32, score.min(1.0)));
    }
    neighbors
}

/// 点积（8 路累加，便于编译器向量化）。
fn dot(left: &[f32], right: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    let chunks = left.len() / 8;
    for chunk in 0..chunks {
        let base = chunk * 8;
        for lane in 0..8 {
            acc[lane] += left[base + lane] * right[base + lane];
        }
    }
    let mut sum: f32 = acc.iter().sum();
    for index in chunks * 8..left.len() {
        sum += left[index] * right[index];
    }
    sum
}

/// 批量 embedding（在线端点）。
async fn embed_nodes(
    embedding: &Arc<dyn EmbeddingProvider>,
    nodes: &[MemoryNode],
    progress: Option<ConsolidationProgress<'_>>,
) -> anyhow::Result<Vec<Vec<f32>>> {
    let texts: Vec<String> = nodes.iter().map(node_text).collect();
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for (index, batch) in texts.chunks(EMBEDDING_BATCH).enumerate() {
        emit(progress, || {
            let done = (index * EMBEDDING_BATCH).min(texts.len());
            Progress::new("similar", format!("向量化 [{}/{}]", done, texts.len()))
                .count(done, texts.len())
        });
        let mut embedded = embedding.embed(batch.to_vec()).await?;
        if embedded.len() != batch.len() {
            anyhow::bail!(
                "Embedding 返回数量不一致：expected={} actual={}",
                batch.len(),
                embedded.len()
            );
        }
        vectors.append(&mut embedded);
    }
    Ok(vectors)
}

/// 带连接刷新重试的写操作：Leader 退出后 follower 监控会自动接替并替换
/// 内部句柄，重试时经 [`HandleProvider`] 取最新句柄；句柄未变则直接返回
/// 原错误（说明是操作本身失败，重试无意义）。
pub(crate) async fn retry_write<T, F, Fut>(
    provider: HandleProvider<'_>,
    operation: F,
) -> anyhow::Result<T>
where
    F: Fn(MemoryHandle) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut last_error = None;
    for attempt in 0..WRITE_RETRY_ATTEMPTS {
        let handle = provider();
        match operation(handle.clone()).await {
            Ok(value) => return Ok(value),
            Err(err) => {
                last_error = Some(err);
                if attempt + 1 >= WRITE_RETRY_ATTEMPTS {
                    break;
                }
                tokio::time::sleep(WRITE_RETRY_DELAY).await;
                let refreshed = provider();
                if refreshed.is_same_handle(&handle) {
                    break;
                }
                tracing::info!("Memory 连接已刷新（Leader 接替），重试写操作");
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Memory 写操作失败")))
}

// ---------------------------------------------------------------------------
// 单批判定与落库
// ---------------------------------------------------------------------------

/// 校验后的合并组。
struct ValidGroup {
    primary: usize,
    members: Vec<usize>,
    title: String,
    summary: String,
    keywords: Vec<String>,
    memory_type: MemoryCognitiveType,
    importance: f32,
    reason: String,
}

/// 校验后的单批计划。
#[derive(Default)]
struct ValidPlan {
    groups: Vec<ValidGroup>,
    forget: Vec<(usize, String)>,
    reclassify: Vec<(usize, MemoryCognitiveType, String)>,
    relations: Vec<RelationPlan>,
}

/// 未配置 LLM 时的保守规则：只合并文本完全相同的记忆（主记忆取重要性最高者）。
fn rule_decision(state: &PageState, batch: &[usize]) -> BatchDecision {
    let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
    for position in batch {
        buckets
            .entry(normalized_key(&node_text(&state.nodes[*position])))
            .or_default()
            .push(*position);
    }
    let mut groups = Vec::new();
    for members in buckets.into_values().filter(|members| members.len() >= 2) {
        let primary = members
            .iter()
            .copied()
            .max_by(|left, right| {
                state.nodes[*left]
                    .importance
                    .partial_cmp(&state.nodes[*right].importance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(members[0]);
        let node = &state.nodes[primary];
        groups.push(MergeGroup {
            member_ids: members
                .iter()
                .map(|position| state.nodes[*position].id.clone())
                .collect(),
            primary_id: Some(node.id.clone()),
            title: Some(node.title.clone()),
            summary: Some(node.summary.clone()),
            keywords: node.keywords.clone(),
            memory_type: Some(memory_type_key(&node.memory_type).to_string()),
            importance: Some(f64::from(node.importance)),
            reason: Some("文本完全相同（规则合并）".to_string()),
        });
    }
    BatchDecision {
        groups,
        ..Default::default()
    }
}

/// 校验 LLM 计划：只接受本批内的 ID，一条记忆至多属于一个合并组，合并组至少
/// 两条；遗忘/重分类不得与合并组冲突；未提及的记忆保持原样。
fn validate_plan(state: &PageState, batch: &[usize], decision: BatchDecision) -> ValidPlan {
    let in_batch: HashMap<&str, usize> = batch
        .iter()
        .map(|position| (state.nodes[*position].id.as_str(), *position))
        .collect();
    let mut plan = ValidPlan::default();
    let mut used: HashSet<usize> = HashSet::new();
    for group in decision.groups {
        let mut members: Vec<usize> = Vec::new();
        for id in &group.member_ids {
            if let Some(&position) = in_batch.get(id.trim())
                && !used.contains(&position)
                && !members.contains(&position)
            {
                members.push(position);
            }
        }
        if members.len() < 2 {
            continue;
        }
        let primary = group
            .primary_id
            .as_deref()
            .and_then(|id| in_batch.get(id.trim()).copied())
            .filter(|position| members.contains(position))
            .unwrap_or_else(|| {
                members
                    .iter()
                    .copied()
                    .max_by_key(|position| state.nodes[*position].summary.chars().count())
                    .unwrap_or(members[0])
            });
        let base = &state.nodes[primary];
        let title = group
            .title
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| base.title.clone());
        let summary = group
            .summary
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| base.summary.clone());
        let keywords = if group.keywords.is_empty() {
            base.keywords.clone()
        } else {
            normalize_keywords(&group.keywords)
        };
        let memory_type = group
            .memory_type
            .as_deref()
            .and_then(parse_memory_type)
            .unwrap_or_else(|| base.memory_type.clone());
        let max_importance = members
            .iter()
            .map(|position| state.nodes[*position].importance)
            .fold(0.0f32, f32::max);
        let importance = group
            .importance
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(0.05, 1.0) as f32)
            .unwrap_or(max_importance);
        used.extend(members.iter().copied());
        plan.groups.push(ValidGroup {
            primary,
            members,
            title,
            summary,
            keywords,
            memory_type,
            importance,
            reason: group.reason.unwrap_or_default(),
        });
    }
    let mut forgotten: HashSet<usize> = HashSet::new();
    for item in decision.forget {
        let (id, reason) = item.into_parts();
        if let Some(&position) = in_batch.get(id.trim())
            && !used.contains(&position)
            && forgotten.insert(position)
        {
            plan.forget.push((position, reason));
        }
    }
    for item in decision.reclassify {
        let Some(&position) = in_batch.get(item.id.trim()) else {
            continue;
        };
        let Some(memory_type) = parse_memory_type(&item.memory_type) else {
            continue;
        };
        if used.contains(&position)
            || forgotten.contains(&position)
            || memory_type == state.nodes[position].memory_type
        {
            continue;
        }
        plan.reclassify
            .push((position, memory_type, item.reason.unwrap_or_default()));
    }
    plan.relations = decision.relations;
    plan
}

/// 落库一批计划：合并（重写主记忆 → 重指向关联 → 删除组员）→ 遗忘 → 重分类
/// → 新建关联。写操作逐个带连接刷新重试；试运行只更新内存快照与报告。
#[allow(clippy::too_many_arguments)]
async fn apply_plan(
    provider: HandleProvider<'_>,
    state: &mut PageState,
    batch: &[usize],
    scores: &HashMap<usize, f32>,
    plan: ValidPlan,
    batch_relations: &[MemoryRelation],
    options: &ConsolidationOptions,
    report: &mut ConsolidationReport,
) -> anyhow::Result<Vec<usize>> {
    let dry_run = options.dry_run;
    let mut requeue = Vec::new();
    // 本批内已知的关联权重（from, to, kind）→ weight：写入时取较大值。
    let mut weights = relation_weights(batch_relations);
    // 合并删除的记忆 → 主记忆 ID（用于重映射 LLM 关联的端点）。
    let mut merged_into: HashMap<String, String> = HashMap::new();
    let mut removed: HashSet<String> = HashSet::new();

    for group in &plan.groups {
        let primary_id = state.nodes[group.primary].id.clone();
        // 主记忆已参与本次判定：不再单独作为种子计数；获得新候选时会重新排队。
        state.visited[group.primary] = true;
        let old_type = state.nodes[group.primary].memory_type.clone();
        let before = state.nodes[group.primary].clone();
        let draft = ManualMemoryDraft {
            id: Some(primary_id.clone()),
            memory_type: group.memory_type.clone(),
            title: group.title.clone(),
            summary: group.summary.clone(),
            keywords: group.keywords.clone(),
            importance: group.importance,
            ..Default::default()
        };
        upsert_with_journal(
            provider,
            options,
            "similar",
            Some(&before),
            draft,
            &format!("LLM 合并主记忆：{}", group.reason),
        )
        .await?;
        {
            let node = &mut state.nodes[group.primary];
            node.title = group.title.clone();
            node.summary = group.summary.clone();
            node.keywords = group.keywords.clone();
            node.memory_type = group.memory_type.clone();
            node.importance = group.importance;
        }
        report.updated += 1;
        if group.memory_type != old_type {
            report.reclassified += 1;
        }
        let merged: Vec<usize> = group
            .members
            .iter()
            .copied()
            .filter(|position| *position != group.primary)
            .collect();
        let group_ids: HashSet<String> = group
            .members
            .iter()
            .map(|position| state.nodes[*position].id.clone())
            .collect();
        // 关联重指向：组员指向组外的关系改挂到主记忆，组内互连随合并消失。
        let relations = if dry_run {
            batch_relations.to_vec()
        } else {
            provider()
                .list_relations_batch(group_ids.iter().cloned().collect())
                .await
        };
        report.relations_relinked += relink_relations(
            provider,
            &primary_id,
            &group_ids,
            &relations,
            &mut weights,
            options,
        )
        .await?;
        // 关联重指向完成后再物理删除组员，避免级联删除吞掉待重建的关联信息。
        for position in &merged {
            let node_id = state.nodes[*position].id.clone();
            delete_with_journal(
                provider,
                options,
                &state.nodes[*position],
                "similar",
                &format!("LLM 合并：{}", group.reason),
                Some(&primary_id),
                Some(&relations),
            )
            .await?;
            state.remove(*position);
            merged_into.insert(node_id.clone(), primary_id.clone());
            removed.insert(node_id);
            if state.inherit(*position, group.primary) {
                requeue.push(group.primary);
            }
            report.merged_away += 1;
        }
        report.merged_groups += 1;
        let mut detail = format!(
            "合并 {} 条为「{}」（分类 {:?}）{}",
            group.members.len(),
            one_line(&group.title, 40),
            group.memory_type,
            dry_run_tag(options)
        );
        if !group.reason.trim().is_empty() {
            let _ = write!(detail, "；理由：{}", truncate(&group.reason, 120));
        }
        for position in &group.members {
            let _ = write!(
                detail,
                "\n      {} {} {}「{}」",
                if *position == group.primary {
                    "★"
                } else {
                    "·"
                },
                state.nodes[*position].id,
                similarity_label(batch, *position, scores),
                one_line(&state.nodes[*position].title, 50)
            );
        }
        report.actions.push(detail);
    }

    for (position, reason) in &plan.forget {
        let node_id = state.nodes[*position].id.clone();
        delete_with_journal(
            provider,
            options,
            &state.nodes[*position],
            "similar",
            &format!("LLM 删除：{reason}"),
            None,
            None,
        )
        .await?;
        state.remove(*position);
        removed.insert(node_id.clone());
        report.forgotten += 1;
        report.actions.push(format!(
            "LLM 删除记忆 {} {}「{}」；理由：{}{}",
            node_id,
            similarity_label(batch, *position, scores),
            one_line(&state.nodes[*position].title, 50),
            truncate(reason, 120),
            dry_run_tag(options)
        ));
    }

    for (position, memory_type, reason) in &plan.reclassify {
        let node = state.nodes[*position].clone();
        let draft = ManualMemoryDraft {
            id: Some(node.id.clone()),
            memory_type: memory_type.clone(),
            ..base_draft(&node)
        };
        upsert_with_journal(
            provider,
            options,
            "similar",
            Some(&node),
            draft,
            &format!("LLM 重分类：{reason}"),
        )
        .await?;
        state.nodes[*position].memory_type = memory_type.clone();
        report.updated += 1;
        report.reclassified += 1;
        report.actions.push(format!(
            "重分类「{}」{:?} → {:?}；理由：{}{}",
            one_line(&node.title, 40),
            node.memory_type,
            memory_type,
            truncate(reason, 120),
            dry_run_tag(options)
        ));
    }

    // LLM 新建关联：端点必须是本批记忆或现有关联链上出现过的记忆（防止幻觉
    // ID）；被合并的端点重映射到主记忆，已删除的端点跳过。
    let known: HashSet<&str> =
        batch
            .iter()
            .map(|position| state.nodes[*position].id.as_str())
            .chain(batch_relations.iter().flat_map(|relation| {
                [relation.from_node_id.as_str(), relation.to_node_id.as_str()]
            }))
            .collect();
    for plan_relation in &plan.relations {
        let Some(from) = plan_relation.from_id.as_deref().map(str::trim) else {
            continue;
        };
        let to = plan_relation.to_id.trim();
        if !known.contains(from) || !known.contains(to) {
            tracing::debug!(from, to, "记忆整理忽略未知关联端点");
            continue;
        }
        // 被合并的端点映射到主记忆；被遗忘（删除）的端点跳过。
        let resolve = |id: &str| -> Option<String> {
            let id = merged_into.get(id).map(String::as_str).unwrap_or(id);
            (!removed.contains(id)).then(|| id.to_string())
        };
        let (Some(from), Some(to)) = (resolve(from), resolve(to)) else {
            continue;
        };
        if from == to {
            continue;
        }
        let draft = MemoryRelationDraft {
            from_node_id: from,
            to_node_id: to,
            relation_kind: parse_relation_kind(&plan_relation.kind).unwrap_or_default(),
            weight: plan_relation.weight.unwrap_or(0.6).clamp(0.05, 1.0) as f32,
            note: Some(
                plan_relation
                    .note
                    .clone()
                    .filter(|note| !note.trim().is_empty())
                    .unwrap_or_else(|| "记忆深度整理新建关联".to_string()),
            ),
            ..Default::default()
        };
        upsert_relation_max(provider, draft, &mut weights, options).await?;
        report.relations_created += 1;
    }
    Ok(requeue)
}

/// 关联权重表：(from, to, kind) → weight，写入关联时取较大值。
pub(crate) type RelationWeights = HashMap<(String, String, &'static str), f32>;

/// 由已有关联构建权重表。
pub(crate) fn relation_weights(relations: &[MemoryRelation]) -> RelationWeights {
    relations
        .iter()
        .map(|relation| {
            (
                (
                    relation.from_node_id.clone(),
                    relation.to_node_id.clone(),
                    relation_kind_key(&relation.relation_kind),
                ),
                relation.weight,
            )
        })
        .collect()
}

/// 合并组的关联改挂：组员（非主记忆）与组外记忆之间的关联改挂到主记忆，
/// 组内互连随合并消失；重复关联取较大权重。返回改挂的关联数。
pub(crate) async fn relink_relations(
    provider: HandleProvider<'_>,
    primary_id: &str,
    group_ids: &HashSet<String>,
    relations: &[MemoryRelation],
    weights: &mut RelationWeights,
    options: &ConsolidationOptions,
) -> anyhow::Result<usize> {
    let dry_run = options.dry_run;
    let mut relinked = 0;
    for relation in relations {
        let from_merged =
            group_ids.contains(&relation.from_node_id) && relation.from_node_id != primary_id;
        let to_merged =
            group_ids.contains(&relation.to_node_id) && relation.to_node_id != primary_id;
        if !from_merged && !to_merged {
            continue;
        }
        let other = if from_merged {
            &relation.to_node_id
        } else {
            &relation.from_node_id
        };
        if group_ids.contains(other) {
            continue;
        }
        let (from, to) = if from_merged {
            (primary_id.to_string(), other.clone())
        } else {
            (other.clone(), primary_id.to_string())
        };
        let draft = MemoryRelationDraft {
            from_node_id: from,
            to_node_id: to,
            relation_kind: relation.relation_kind.clone(),
            weight: relation.weight,
            note: relation
                .note
                .clone()
                .or_else(|| Some("记忆深度整理重建关联".to_string())),
            ..Default::default()
        };
        upsert_relation_max(provider, draft, weights, options).await?;
        options.record(
            "delete_relation",
            serde_json::json!({"relation": relation, "relinked_to": primary_id}),
        )?;
        if !dry_run {
            let relation_id = relation.id.clone();
            retry_write(provider, |handle| {
                let relation_id = relation_id.clone();
                async move { handle.delete_relation(relation_id).await }
            })
            .await?;
        }
        relinked += 1;
    }
    Ok(relinked)
}

/// 写入关联：同一 (from, to, kind) 已有关联时取较大权重，避免整理把强关联
/// 覆盖成弱关联。
pub(crate) async fn upsert_relation_max(
    provider: HandleProvider<'_>,
    mut draft: MemoryRelationDraft,
    weights: &mut RelationWeights,
    options: &ConsolidationOptions,
) -> anyhow::Result<()> {
    let dry_run = options.dry_run;
    let key = (
        draft.from_node_id.clone(),
        draft.to_node_id.clone(),
        relation_kind_key(&draft.relation_kind),
    );
    if let Some(existing) = weights.get(&key) {
        draft.weight = draft.weight.max(*existing);
    }
    weights.insert(key, draft.weight);
    options.record("upsert_relation", serde_json::json!({"relation": draft}))?;
    if !dry_run {
        retry_write(provider, |handle| {
            let draft = draft.clone();
            async move { handle.upsert_relation(draft).await }
        })
        .await?;
    }
    Ok(())
}

/// 报告中的相似度标签：种子标「种子」，其余显示与种子的相似度。
fn similarity_label(batch: &[usize], position: usize, scores: &HashMap<usize, f32>) -> String {
    if batch.first() == Some(&position) {
        return "[种子]".to_string();
    }
    match scores.get(&position) {
        Some(score) => format!("[相似 {score:.3}]"),
        None => "[相似 -]".to_string(),
    }
}

// ---------------------------------------------------------------------------
// LLM 判定
// ---------------------------------------------------------------------------

/// 判定系统提示：只输出 JSON；相似只代表话题接近，是否合并由 LLM 判断。
const CONSOLIDATION_SYSTEM: &str = r#"你是长期记忆整理专家。用户给出一批经向量检索判定为「相似」的长期记忆及其现有关联链。注意：相似只说明话题接近，不代表可以合并——请逐条阅读，由你判断哪些记忆真正需要归一。

只输出一个 JSON 对象，不要输出解释、不要使用 Markdown 代码块。字段：
- groups: 合并组数组。每组是「描述同一事实/同一事件/同一知识点、可以无损合并为一条」的记忆：
  {"member_ids":["至少 2 个 ID"],"primary_id":"组内信息最完整的 ID","title":"合并后标题，<=40 字","summary":"合并后摘要，<=300 字，保留全部关键事实、路径、版本号、配置项与结论，去除重复表述，不要编造","keywords":["3~8 个"],"memory_type":"见下方取值","importance":0~1,"reason":"为什么它们是同一件事"}
  一条记忆最多属于一个组；一批中可以有多个组，也可以一个都没有。
  不要合并：只是话题相近但时间/对象不同的独立事件、结论不同或相互矛盾的记忆、互为补充但各自有独立价值的记忆。
  scope（工作区）不同不是拒绝合并的理由：召回不按工作区过滤，同一知识、同一实体、同一条通用经验在多个工作区各存一份只是重复。但若记忆描述的是某个工作区特有的事实（如该项目的目录结构、配置、决策），而另一条描述的是别的项目，则它们是不同的事实，不要合并。合并后主记忆保留 primary_id 所在的工作区。
- forget: 需要删除的低质量记忆 [{"id":"...","reason":"..."}]：内容空泛无信息量，或已被本批其他记忆完全覆盖。不确定时不要删除；已放入 groups 的记忆不要再放这里。
- reclassify: 分类明显错误需要修正的记忆 [{"id":"...","memory_type":"...","reason":"..."}]（合并组的分类在组内给出，不用重复）。
- relations: 需要新建的关联 [{"from_id":"...","to_id":"...","kind":"related_to|depends_on|supports|contradicts|supersedes|caused_by|belongs_to|learned_from|validated_by","weight":0~1,"note":"可选"}]，端点只能是本批记忆或 existing_relations 中出现过的记忆；结论相反用 contradicts，新结论取代旧结论用 supersedes。

memory_type 取值：factual | user_preference | user_habit | skill | project_structure | architecture_decision | problem_incident | domain_knowledge。
未出现在以上任何字段中的记忆保持原样。宁可不合并，也不要丢失有价值的细节。"#;

/// LLM 单批判定结果。
#[derive(Debug, Clone, Default, Deserialize)]
struct BatchDecision {
    #[serde(default)]
    groups: Vec<MergeGroup>,
    #[serde(default)]
    forget: Vec<ForgetItem>,
    #[serde(default)]
    reclassify: Vec<ReclassifyItem>,
    #[serde(default)]
    relations: Vec<RelationPlan>,
}

/// LLM 给出的合并组。
#[derive(Debug, Clone, Deserialize)]
struct MergeGroup {
    #[serde(default)]
    member_ids: Vec<String>,
    #[serde(default)]
    primary_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    memory_type: Option<String>,
    #[serde(default)]
    importance: Option<f64>,
    #[serde(default)]
    reason: Option<String>,
}

/// 遗忘项：兼容 `"id"` 与 `{"id":..,"reason":..}` 两种写法。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ForgetItem {
    Id(String),
    Detail {
        id: String,
        #[serde(default)]
        reason: Option<String>,
    },
}

impl ForgetItem {
    fn into_parts(self) -> (String, String) {
        match self {
            ForgetItem::Id(id) => (id, String::new()),
            ForgetItem::Detail { id, reason } => (id, reason.unwrap_or_default()),
        }
    }
}

/// 重分类项。
#[derive(Debug, Clone, Deserialize)]
struct ReclassifyItem {
    id: String,
    memory_type: String,
    #[serde(default)]
    reason: Option<String>,
}

/// LLM 规划的关联。
#[derive(Debug, Clone, Deserialize)]
struct RelationPlan {
    #[serde(default)]
    from_id: Option<String>,
    to_id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    weight: Option<f64>,
    #[serde(default)]
    note: Option<String>,
}

/// 只发请求（带重试），不触碰报告：可并发执行。结果须由调用方在落库前经
/// [`record_llm_decision`] 记录。
pub(crate) async fn call_llm_json<T: serde::de::DeserializeOwned>(
    model: &LlmEndpointConfig,
    system: &str,
    prompt: &str,
    label: &str,
    items: usize,
    options: &ConsolidationOptions,
) -> anyhow::Result<(T, String)> {
    let attempts = options.llm_retry_attempts.max(1);
    let mut last_err = None;
    for attempt in 1..=attempts {
        match request_json::<T>(model, system, prompt, items).await {
            Ok(result) => return Ok(result),
            Err(err) => {
                tracing::warn!("{label}的 LLM 判定失败（第 {attempt}/{attempts} 次）：{err:#}");
                last_err = Some(err);
                if attempt < attempts {
                    tokio::time::sleep(options.llm_retry_delay).await;
                }
            }
        }
    }
    Err(anyhow::anyhow!(
        "{label}的 LLM 判定连续 {attempts} 次失败，中断整理（本批未写入任何改动）：{:#}",
        last_err.unwrap_or_else(|| anyhow::anyhow!("未知错误"))
    ))
}

/// 记录一次成功的 LLM 判定（计数 + 执行记录），在落库前调用。
pub(crate) fn record_llm_decision(
    options: &ConsolidationOptions,
    report: &mut ConsolidationReport,
    label: &str,
    prompt: &str,
    raw: &str,
) -> anyhow::Result<()> {
    report.llm_calls += 1;
    report.used_llm = true;
    // 完整保留送审内容与 LLM 原始判定，便于追溯每次删除的依据。
    options.record(
        "llm_decision",
        serde_json::json!({
            "label": label,
            "call": report.llm_calls,
            "prompt": serde_json::from_str::<serde_json::Value>(prompt)
                .unwrap_or_else(|_| prompt.into()),
            "response": serde_json::from_str::<serde_json::Value>(raw)
                .unwrap_or_else(|_| raw.to_string().into()),
        }),
    )
}

async fn request_json<T: serde::de::DeserializeOwned>(
    model: &LlmEndpointConfig,
    system: &str,
    prompt: &str,
    items: usize,
) -> anyhow::Result<(T, String)> {
    let started = Instant::now();
    let (response, usage) = complete_text_with_usage(model, system, prompt, LLM_MAX_TOKENS).await?;
    let completion_tokens = usage
        .as_ref()
        .map(|usage| usage.completion_tokens)
        .unwrap_or_default();
    tracing::info!(
        model = %model.model,
        memories = items,
        elapsed_ms = started.elapsed().as_millis(),
        prompt_tokens = usage.as_ref().map(|usage| usage.prompt_tokens).unwrap_or_default(),
        completion_tokens,
        "记忆整理 LLM 判定完成"
    );
    let truncated = completion_tokens >= LLM_MAX_TOKENS as usize;
    let Some(json) = extract_json_object(&response) else {
        if truncated {
            anyhow::bail!(
                "LLM 输出达到 token 上限 {LLM_MAX_TOKENS} 被截断，未返回 JSON 对象（响应开头：{}）",
                response_preview(&response)
            );
        }
        anyhow::bail!(
            "LLM 未返回 JSON 对象（响应开头：{}）",
            response_preview(&response)
        );
    };
    let parsed = serde_json::from_str(json).map_err(|err| {
        let hint = if truncated {
            format!("，输出达到 token 上限 {LLM_MAX_TOKENS} 可能被截断（可调小批大小）")
        } else {
            String::new()
        };
        anyhow::anyhow!(
            "解析整理结果失败：{err}{hint}（响应开头：{}）",
            response_preview(&response)
        )
    })?;
    Ok((parsed, json.to_string()))
}

/// 组装单批判定的用户提示（本批记忆 + 现有关联链）。
fn build_batch_prompt(
    members: &[&MemoryNode],
    similarity: &[Option<f32>],
    relations: &[MemoryRelation],
) -> String {
    let items: Vec<serde_json::Value> = members
        .iter()
        .zip(similarity)
        .enumerate()
        .map(|(index, (node, score))| memory_json(node, index == 0, *score))
        .collect();
    let relations: Vec<serde_json::Value> = relations
        .iter()
        .map(|relation| {
            serde_json::json!({
                "from_id": relation.from_node_id,
                "to_id": relation.to_node_id,
                "kind": relation_kind_key(&relation.relation_kind),
                "weight": relation.weight,
                "note": relation.note,
            })
        })
        .collect();
    serde_json::json!({
        "memories": items,
        "existing_relations": relations,
        "instruction": "判断本批记忆中哪些描述同一件事需要合并（可多组）、哪些需要删除或重新分类、需要新建哪些关联；未提及的记忆保持原样。",
    })
    .to_string()
}

/// 序列化一条记忆供 LLM 阅读。
fn memory_json(node: &MemoryNode, seed: bool, similarity: Option<f32>) -> serde_json::Value {
    serde_json::json!({
        "id": node.id,
        "seed": seed,
        "similarity_to_seed": similarity.map(|score| (f64::from(score) * 1000.0).round() / 1000.0),
        "title": node.title,
        "summary": truncate(&node.summary, PROMPT_SUMMARY_CHARS),
        "keywords": node.keywords,
        "memory_type": memory_type_key(&node.memory_type),
        "importance": node.importance,
        "scope": format!(
            "{:?}:{}",
            node.scope_type,
            node.scope_id.as_deref().unwrap_or("-")
        ),
        "created_at": node.created_at,
        "updated_at": node.updated_at,
    })
}

/// 从 LLM 输出中提取第一个完整 JSON 对象。
fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}

/// LLM 响应开头（单行、限长），用于错误诊断。
fn response_preview(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "<空>".to_string();
    }
    truncate(&flat, 200)
}

// ---------------------------------------------------------------------------
// 规则整理与低质量判定
// ---------------------------------------------------------------------------

/// 由现有记忆生成更新草稿（只改分类时保留其余字段）。
fn base_draft(node: &MemoryNode) -> ManualMemoryDraft {
    ManualMemoryDraft {
        title: node.title.clone(),
        summary: node.summary.clone(),
        keywords: node.keywords.clone(),
        importance: node.importance,
        memory_type: node.memory_type.clone(),
        ..Default::default()
    }
}

/// 规则低质量判定：返回遗忘原因（`None` 表示不遗忘）。
///
/// 保守策略：只有「标题为空」「摘要过短且重要性低」「长期未用且重要性低」
/// 才遗忘，避免误伤短但有用的记忆（如用户偏好）。
fn rule_low_quality(node: &MemoryNode, options: &ConsolidationOptions) -> Option<String> {
    if node.title.trim().is_empty() {
        return Some("标题为空".to_string());
    }
    let summary_chars = node.summary.trim().chars().count();
    if summary_chars < options.min_summary_chars
        && node.importance < options.importance_floor as f32
    {
        return Some(format!(
            "摘要仅 {summary_chars} 字符且重要性 {:.2} 低于 {:.2}",
            node.importance, options.importance_floor
        ));
    }
    let days = days_since(&node.updated_at).or_else(|| days_since(&node.created_at));
    if node.importance < options.importance_floor as f32
        && node.usage_count <= 0
        && days.is_some_and(|days| days > options.stale_days)
    {
        return Some(format!(
            "{} 天未使用且重要性 {:.2} 低于 {:.2}",
            days.unwrap_or_default(),
            node.importance,
            options.importance_floor
        ));
    }
    None
}

/// 解析记忆时间戳（`%Y-%m-%d %H:%M:%S[.f]` 或日期）。
pub(crate) fn parse_timestamp(timestamp: &str) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S%.f"))
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(timestamp, "%Y-%m-%d")
                .map(|date| date.and_hms_opt(0, 0, 0).unwrap_or_default())
        })
        .ok()
}

/// 距今天数（解析失败返回 `None`）。
fn days_since(timestamp: &str) -> Option<i64> {
    let parsed = parse_timestamp(timestamp)?;
    let now = chrono::Local::now().naive_local();
    Some((now - parsed).num_days().max(0))
}

/// 精确去重用的归一化键：去空白、转小写。
pub(crate) fn normalized_key(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// 相似度对比 / 预过滤用的归一化文本。
fn node_text(node: &MemoryNode) -> String {
    format!(
        "{}\n{}\n{}",
        node.title,
        node.summary,
        node.keywords.join(", ")
    )
}

/// 字符二元组集合（过滤空白与 ASCII 标点）。
fn bigrams(text: &str) -> HashSet<String> {
    let chars: Vec<char> = text
        .chars()
        .filter(|ch| !ch.is_whitespace() && !ch.is_ascii_punctuation())
        .collect();
    let mut set = HashSet::new();
    if chars.len() < 2 {
        if let Some(ch) = chars.first() {
            set.insert(ch.to_string());
        }
        return set;
    }
    for pair in chars.windows(2) {
        set.insert(pair.iter().collect::<String>());
    }
    set
}

/// 字符二元组 Jaccard 相似度（0~1）。
///
/// 向量检索不可用时用它判定「高度一致」，也用作向量路径的候选预过滤。
pub fn text_similarity(left: &str, right: &str) -> f64 {
    let left = bigrams(left);
    let right = bigrams(right);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(&right).count() as f64;
    let union = (left.len() + right.len()) as f64 - intersection;
    if union <= 0.0 {
        return 0.0;
    }
    intersection / union
}

/// 余弦相似度（假设两侧已预归一化时等价于点积）。
#[cfg(test)]
fn cosine(left: &[f32], right: &[f32]) -> f64 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    left.iter()
        .zip(right)
        .map(|(a, b)| f64::from(*a) * f64::from(*b))
        .sum()
}

/// 归一化向量（零向量原样返回）。
fn normalize(vector: &[f32]) -> Vec<f32> {
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if norm <= 0.0 {
        return vector.to_vec();
    }
    vector
        .iter()
        .map(|value| (*value as f64 / norm) as f32)
        .collect()
}

/// 解析认知分类（与记忆提取保持同一套别名）。
pub(crate) fn parse_memory_type(raw: &str) -> Option<MemoryCognitiveType> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "factual" | "fact" => Some(MemoryCognitiveType::Factual),
        "user_preference" | "preference" => Some(MemoryCognitiveType::UserPreference),
        "user_habit" | "habit" => Some(MemoryCognitiveType::UserHabit),
        "skill" | "skill_memory" => Some(MemoryCognitiveType::Skill),
        "project_structure" | "project" | "structure" => {
            Some(MemoryCognitiveType::ProjectStructure)
        }
        "architecture_decision" | "decision" | "architecture" => {
            Some(MemoryCognitiveType::ArchitectureDecision)
        }
        "problem_incident" | "incident" | "problem" | "bug" | "failure" => {
            Some(MemoryCognitiveType::ProblemIncident)
        }
        "domain_knowledge" | "knowledge" | "domain" => Some(MemoryCognitiveType::DomainKnowledge),
        _ => None,
    }
}

/// 认知分类的稳定键名（供 LLM 提示使用）。
pub(crate) fn memory_type_key(memory_type: &MemoryCognitiveType) -> &'static str {
    match memory_type {
        MemoryCognitiveType::Factual => "factual",
        MemoryCognitiveType::UserPreference => "user_preference",
        MemoryCognitiveType::UserHabit => "user_habit",
        MemoryCognitiveType::Skill => "skill",
        MemoryCognitiveType::ProjectStructure => "project_structure",
        MemoryCognitiveType::ArchitectureDecision => "architecture_decision",
        MemoryCognitiveType::ProblemIncident => "problem_incident",
        MemoryCognitiveType::DomainKnowledge => "domain_knowledge",
    }
}

/// 关系类型的稳定键名。
fn relation_kind_key(kind: &MemoryRelationKind) -> &'static str {
    match kind {
        MemoryRelationKind::RelatedTo => "related_to",
        MemoryRelationKind::DependsOn => "depends_on",
        MemoryRelationKind::Supports => "supports",
        MemoryRelationKind::Contradicts => "contradicts",
        MemoryRelationKind::Supersedes => "supersedes",
        MemoryRelationKind::CausedBy => "caused_by",
        MemoryRelationKind::BelongsTo => "belongs_to",
        MemoryRelationKind::LearnedFrom => "learned_from",
        MemoryRelationKind::ValidatedBy => "validated_by",
    }
}

/// 解析关系类型。
fn parse_relation_kind(raw: &str) -> Option<MemoryRelationKind> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "related_to" | "related" => Some(MemoryRelationKind::RelatedTo),
        "depends_on" | "depends" => Some(MemoryRelationKind::DependsOn),
        "supports" | "support" => Some(MemoryRelationKind::Supports),
        "contradicts" | "contradict" => Some(MemoryRelationKind::Contradicts),
        "supersedes" | "supersede" => Some(MemoryRelationKind::Supersedes),
        "caused_by" | "caused" => Some(MemoryRelationKind::CausedBy),
        "belongs_to" | "belongs" => Some(MemoryRelationKind::BelongsTo),
        "learned_from" | "learned" => Some(MemoryRelationKind::LearnedFrom),
        "validated_by" | "validated" => Some(MemoryRelationKind::ValidatedBy),
        _ => None,
    }
}

/// 关键词归一：去空白、去重、限量。
pub(crate) fn normalize_keywords(keywords: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for keyword in keywords {
        let keyword = keyword.trim();
        if keyword.is_empty() || normalized.iter().any(|item| item == keyword) {
            continue;
        }
        normalized.push(keyword.to_string());
        if normalized.len() >= 8 {
            break;
        }
    }
    normalized
}

/// 试运行标记。
pub(crate) fn dry_run_tag(options: &ConsolidationOptions) -> &'static str {
    if options.dry_run {
        "（试运行）"
    } else {
        ""
    }
}

/// 报告展示用：折叠换行与连续空白为单个空格后截断。
pub(crate) fn one_line(text: &str, max_chars: usize) -> String {
    truncate(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        max_chars,
    )
}

/// 截断文本用于报告展示。
pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut clipped: String = trimmed.chars().take(max_chars).collect();
    clipped.push('…');
    clipped
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_memory::{MemoryKind, MemoryScopeType};

    fn node(id: &str, title: &str, summary: &str, importance: f32) -> MemoryNode {
        MemoryNode {
            id: id.to_string(),
            kind: MemoryKind::Episode,
            memory_type: MemoryCognitiveType::Factual,
            scope_type: MemoryScopeType::Global,
            scope_id: None,
            title: title.to_string(),
            summary: summary.to_string(),
            keywords: vec!["redis".to_string()],
            importance,
            confidence: 0.8,
            status: MemoryStatus::Active,
            source: None,
            usage_count: 0,
            last_used_at: None,
            created_at: "2024-01-01 00:00:00".to_string(),
            updated_at: "2024-01-01 00:00:00".to_string(),
        }
    }

    #[test]
    fn similarity_detects_identical_and_disjoint_text() {
        assert!((text_similarity("Redis 哨兵部署", "Redis 哨兵部署") - 1.0).abs() < 1e-9);
        assert_eq!(text_similarity("Redis 哨兵", "PostgreSQL 主从"), 0.0);
        assert_eq!(text_similarity("", "anything"), 0.0);
    }

    #[test]
    fn cosine_and_normalize_behave() {
        let a = normalize(&[3.0, 4.0]);
        let b = normalize(&[3.0, 4.0]);
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-6, "同向向量余弦为 1");
        let c = normalize(&[-3.0, -4.0]);
        assert!((cosine(&a, &c) + 1.0).abs() < 1e-6, "反向向量余弦为 -1");
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0, "维度不一致返回 0");
        assert_eq!(cosine(&normalize(&[0.0, 0.0]), &a), 0.0, "零向量返回 0");
    }

    #[test]
    fn low_quality_rules_are_conservative() {
        let options = ConsolidationOptions::default();
        let empty_title = node("a", "  ", "正常长度的摘要内容", 0.9);
        assert!(
            rule_low_quality(&empty_title, &options).is_some(),
            "空标题应遗忘"
        );
        let short = node("b", "偏好", "深色", 0.2);
        assert!(
            rule_low_quality(&short, &options).is_some(),
            "短摘要且低重要性应遗忘"
        );
        let short_but_important = node("c", "偏好", "深色", 0.9);
        assert!(
            rule_low_quality(&short_but_important, &options).is_none(),
            "短但重要的记忆不应遗忘"
        );
        let stale = node("d", "旧记忆", "这是一条足够长的摘要内容", 0.2);
        assert!(
            rule_low_quality(&stale, &options).is_some(),
            "长期未用且低重要性应遗忘"
        );
        let mut fresh = node("e", "新记忆", "这是一条足够长的摘要内容", 0.2);
        fresh.created_at = chrono::Local::now().naive_local().to_string();
        fresh.updated_at = fresh.created_at.clone();
        assert!(
            rule_low_quality(&fresh, &options).is_none(),
            "新记忆不应遗忘"
        );
    }

    #[test]
    fn parses_batch_decision_with_markdown_fence() {
        let raw = "```json\n{\"groups\":[{\"member_ids\":[\"a\",\"b\"],\"primary_id\":\"a\",\"title\":\"合并\",\"summary\":\"摘要\",\"keywords\":[\"redis\"],\"memory_type\":\"skill\",\"importance\":0.9,\"reason\":\"同一事实\"}],\"forget\":[\"c\",{\"id\":\"d\",\"reason\":\"空泛\"}],\"reclassify\":[{\"id\":\"e\",\"memory_type\":\"user_preference\"}],\"relations\":[{\"from_id\":\"a\",\"to_id\":\"x\",\"kind\":\"supersedes\",\"weight\":0.8}]}\n```";
        let json = extract_json_object(raw).expect("应提取 JSON");
        let decision: BatchDecision = serde_json::from_str(json).expect("应可解析");
        assert_eq!(decision.groups.len(), 1);
        assert_eq!(decision.groups[0].member_ids, vec!["a", "b"]);
        let forget: Vec<(String, String)> = decision
            .forget
            .into_iter()
            .map(ForgetItem::into_parts)
            .collect();
        assert_eq!(forget[0], ("c".to_string(), String::new()));
        assert_eq!(forget[1], ("d".to_string(), "空泛".to_string()));
        assert_eq!(decision.reclassify[0].memory_type, "user_preference");
        assert_eq!(
            parse_relation_kind(&decision.relations[0].kind),
            Some(MemoryRelationKind::Supersedes)
        );
        let empty: BatchDecision = serde_json::from_str("{}").expect("空对象表示不做任何改动");
        assert!(empty.groups.is_empty() && empty.forget.is_empty());
    }

    #[test]
    fn response_preview_flattens_and_truncates() {
        assert_eq!(response_preview("  \n "), "<空>");
        assert_eq!(response_preview("思考\n  中\t…"), "思考 中 …");
        let long = "字".repeat(500);
        assert!(response_preview(&long).chars().count() <= 201);
    }

    fn state_of(nodes: Vec<MemoryNode>) -> PageState {
        let len = nodes.len();
        PageState::new(nodes, vec![Vec::new(); len])
    }

    #[test]
    fn validate_plan_rejects_unknown_overlapping_and_singleton_groups() {
        let state = state_of(vec![
            node("a", "甲", "摘要甲甲甲", 0.5),
            node("b", "乙", "摘要乙乙乙乙乙", 0.9),
            node("c", "丙", "摘要丙", 0.4),
            node("d", "丁", "摘要丁", 0.4),
        ]);
        let batch = [0, 1, 2, 3];
        let group = |ids: &[&str], primary: Option<&str>| MergeGroup {
            member_ids: ids.iter().map(|id| id.to_string()).collect(),
            primary_id: primary.map(str::to_string),
            title: None,
            summary: None,
            keywords: Vec::new(),
            memory_type: None,
            importance: None,
            reason: None,
        };
        let decision = BatchDecision {
            groups: vec![
                // 幻觉 ID 被剔除，主记忆缺省取摘要最长者，重要性取组内最高。
                group(&["a", "b", "ghost"], Some("ghost")),
                // 与上一组重叠（b）且剩余不足两条：整组丢弃。
                group(&["b", "c"], None),
                // 单条组丢弃。
                group(&["d"], None),
            ],
            forget: vec![
                ForgetItem::Id("a".to_string()),
                ForgetItem::Id("c".to_string()),
            ],
            reclassify: vec![
                ReclassifyItem {
                    id: "b".to_string(),
                    memory_type: "skill".to_string(),
                    reason: None,
                },
                ReclassifyItem {
                    id: "d".to_string(),
                    memory_type: "skill".to_string(),
                    reason: None,
                },
                ReclassifyItem {
                    id: "c".to_string(),
                    memory_type: "skill".to_string(),
                    reason: None,
                },
            ],
            relations: Vec::new(),
        };
        let plan = validate_plan(&state, &batch, decision);
        assert_eq!(plan.groups.len(), 1, "只保留一个有效合并组");
        assert_eq!(plan.groups[0].members, vec![0, 1]);
        assert_eq!(plan.groups[0].primary, 1, "缺省主记忆取摘要最长者");
        assert!((plan.groups[0].importance - 0.9).abs() < 1e-6);
        assert_eq!(plan.groups[0].title, "乙", "缺省标题取主记忆");
        // a 已在合并组中，不能再被删除；c 正常删除。
        assert_eq!(
            plan.forget.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
            vec![2]
        );
        // b 已合并、c 已删除：只有 d 被重分类。
        assert_eq!(
            plan.reclassify
                .iter()
                .map(|(p, _, _)| *p)
                .collect::<Vec<_>>(),
            vec![3]
        );
    }

    #[test]
    fn page_state_tracks_judged_pairs_and_inherits_neighbors() {
        let nodes = vec![
            node("a", "甲", "摘要", 0.5),
            node("b", "乙", "摘要", 0.5),
            node("c", "丙", "摘要", 0.5),
            node("d", "丁", "摘要", 0.5),
        ];
        // a-b、b-c、b-d 相似。
        let neighbors = vec![
            vec![(1, 0.95)],
            vec![(0, 0.95), (2, 0.92), (3, 0.91)],
            vec![(1, 0.92)],
            vec![(1, 0.91)],
        ];
        let mut state = PageState::new(nodes, neighbors);
        assert_eq!(state.pending(1).len(), 3, "全部达到阈值的相似记忆都待判定");
        state.mark_judged(&[0, 1]);
        assert_eq!(state.pending(0).len(), 0, "同一对只判定一次");
        assert_eq!(
            state.pending(1).iter().map(|(p, _)| *p).collect::<Vec<_>>(),
            vec![2, 3],
            "按相似度降序"
        );
        // b 合并进 a：a 继承 c、d 两个未判定的候选。
        assert_eq!(state.alive_count(), 4);
        state.remove(1);
        state.remove(1);
        assert_eq!(state.alive_count(), 3, "重复删除只计一次");
        assert_eq!(state.removed, 1);
        assert!(state.inherit(1, 0));
        let pending: Vec<usize> = state.pending(0).iter().map(|(p, _)| *p).collect();
        assert_eq!(pending, vec![2, 3]);
        assert!(state.neighbors[2].iter().any(|(p, _)| *p == 0));
    }

    #[test]
    fn cluster_stats_summarize_similarity_graph() {
        let nodes: Vec<MemoryNode> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .map(|id| node(id, id, "摘要", 0.5))
            .collect();
        // 簇 {a,b,c}（b 为中心）与簇 {d,e}；f 无相似。
        let neighbors = vec![
            vec![(1, 0.95)],
            vec![(0, 0.95), (2, 0.92)],
            vec![(1, 0.92)],
            vec![(4, 0.93)],
            vec![(3, 0.93)],
            vec![],
        ];
        let stats = ClusterStats::compute(&nodes, &neighbors, 20);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.largest, 3);
        assert_eq!(stats.degree_buckets[0], 4, "a、c、d、e 各 1 条相似");
        assert_eq!(stats.degree_buckets[1], 1, "b 有 2 条相似");
        assert_eq!(stats.min_calls, 2, "每簇至少一次判定");
        assert_eq!(stats.max_calls, 5, "5 条有相似的记忆各至多一批");
        assert!(stats.min_calls <= stats.max_calls);
        assert_eq!(stats.densest[0].2, "b", "相似最多的排第一");
        assert!(stats.degree_histogram().contains("1 条:4"));
    }

    #[test]
    fn exact_neighbors_keeps_all_pairs_above_threshold() {
        let a = [1.0f32, 0.0, 0.0];
        let b = [0.99f32, 0.1, 0.0];
        let c = [0.98f32, 0.0, 0.2];
        let d = [0.0f32, 1.0, 0.0];
        let vectors: Vec<Option<&[f32]>> = vec![Some(&a), Some(&b), None, Some(&c), Some(&d)];
        let neighbors = exact_neighbors(&vectors, 0.9);
        let ids = |position: usize| -> Vec<u32> {
            let mut ids: Vec<u32> = neighbors[position].iter().map(|(p, _)| *p).collect();
            ids.sort();
            ids
        };
        assert_eq!(ids(0), vec![1, 3], "不截断：全部达到阈值的都保留");
        assert_eq!(ids(1), vec![0, 3]);
        assert!(neighbors[2].is_empty(), "无向量不参与");
        assert!(neighbors[4].is_empty(), "低于阈值不保留");
        let score = neighbors[0].iter().find(|(p, _)| *p == 1).unwrap().1;
        assert!((f64::from(score) - cosine(&normalize(&a), &normalize(&b))).abs() < 1e-5);
    }

    #[test]
    fn render_reports_counts() {
        let report = ConsolidationReport {
            scanned: 10,
            pages: 1,
            processed: 4,
            remaining: 6,
            candidate_pairs: 5,
            nodes_with_candidates: 4,
            max_candidates: 3,
            clusters: ClusterStats::default(),
            reviewed: 7,
            merged_groups: 2,
            merged_away: 3,
            updated: 2,
            reclassified: 1,
            forgotten: 1,
            relations_relinked: 2,
            relations_created: 1,
            llm_calls: 2,
            used_llm: true,
            retrieval_backend: "vector_store",
            without_vectors: 0,
            dry_run: false,
            llm_budget_exhausted: false,
            fatal_error: None,
            actions: vec!["合并 2 条".to_string()],
            errors: Vec::new(),
            stages: ConsolidationStage::ALL.to_vec(),
            dedup_groups: 3,
            dedup_removed: 4,
            distill_logs: 9,
            distill_batches_total: 2,
            distill_batches: 1,
            distill_deleted: 5,
            ..Default::default()
        };
        let text = report.render();
        assert!(text.contains("执行阶段：dedup → entity → distill → similar"));
        assert!(text.contains("归一 3 组，删除重复 4 条"));
        assert!(text.contains("工具日志 9 条，完成 1/2 批"));
        assert!(text.contains("检索后端：vector_store"));
        assert!(text.contains("扫描活跃记忆：10"));
        assert!(text.contains("相似候选：5 对"));
        assert!(text.contains("合并组数：2"));
        assert!(text.contains("删除低质量记忆：1"));
        assert!(text.contains("剩余未处理：6"));
        let fatal = ConsolidationReport {
            fatal_error: Some("LLM 判定连续 3 次失败".to_string()),
            ..report
        };
        let text = fatal.render();
        assert!(text.contains("记忆深度整理中断"));
        assert!(text.contains("致命中断：LLM 判定连续 3 次失败"));
    }

    #[test]
    fn normalized_key_collapses_whitespace_and_case() {
        assert_eq!(
            normalized_key("  Redis  哨兵\n部署 "),
            normalized_key("redis 哨兵 部署")
        );
        assert_ne!(normalized_key("redis 哨兵"), normalized_key("redis 主从"));
    }
}
