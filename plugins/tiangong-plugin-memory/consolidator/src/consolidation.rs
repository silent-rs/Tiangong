//! 记忆深度整理引擎。
//!
//! 对长期积累的记忆库做一次全量「深刻整理」：
//!
//! 1. 获取全部活跃记忆；
//! 2. 逐条做向量检索（配置了 Embedding 端点时本地计算余弦相似度；
//!    否则经 Memory 召回取候选 + 字符二元组相似度），把高度一致的记忆
//!    交给 LLM 归一、合并并重新分类；
//! 3. 每处理完一条就把整组标记为已处理，后续节点跳过已处理的记忆，
//!    直到全部整理完成；
//! 4. 整理过程中识别低质量记忆并遗忘（归档，不再参与召回）。
//!
//! 本 crate 只通过 `tiangong-memory` 的公开 API（[`MemoryHandle`]）读写记忆，
//! 不修改 Memory 插件本身；接入方式与 sidecar 通用模式一致
//! （竞争 Leader，天工运行时以 Follower 经 IPC 转发）。

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
use tiangong_llm::{EmbeddingProvider, LlmEndpointConfig, complete_text_with_usage};
use tiangong_memory::{
    ManualMemoryDraft, MemoryCognitiveType, MemoryHandle, MemoryListQuery, MemoryNode,
    MemoryRelation, MemoryRelationDraft, MemoryRelationKind, MemoryStatus, RecallAnchors,
    SearchStrategy,
};

/// 默认相似度阈值：余弦相似度 / 字符二元组 Jaccard 达到该值视为「高度一致」。
pub const DEFAULT_SIMILARITY_THRESHOLD: f64 = 0.90;
/// 每条记忆最多带出的相似记忆数（组大小上限）。
pub const DEFAULT_GROUP_LIMIT: usize = 6;
/// 每条记忆的相似候选检索上限。
pub const DEFAULT_CANDIDATE_LIMIT: usize = 12;
/// 单次分页拉取的记忆数。
const PAGE_LIMIT: usize = 500;
/// 单次整理扫描的记忆上限（全量两两比较的安全阀）。
const MAX_SCAN_NODES: usize = 5_000;
/// 单次 embedding 请求的文本数。
const EMBEDDING_BATCH: usize = 32;
/// 摘要短于该字符数（且重要性也低）视为低质量。
pub const DEFAULT_MIN_SUMMARY_CHARS: usize = 12;
/// 重要性与使用率双低、超过该天数未更新，视为可遗忘的低质量记忆。
pub const DEFAULT_STALE_DAYS: i64 = 60;
/// 低质量判定的重要性下限。
pub const DEFAULT_IMPORTANCE_FLOOR: f64 = 0.35;
/// LLM 单次整理输出的 token 上限。
const LLM_MAX_TOKENS: u32 = 1600;
/// 报告里保留的处理明细条数上限。
const MAX_REPORT_ACTIONS: usize = 50;
/// 候选预过滤：至少共享的字符二元组数量。
const MIN_SHARED_BIGRAMS: usize = 3;

/// 整理进度回调：每处理一条记忆触发一次。
pub type ConsolidationProgress<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// 相似记忆候选项（含来源记忆的完整元数据）。
#[derive(Debug, Clone)]
pub struct SimilarHit {
    pub node: MemoryNode,
    /// 相似度：向量路径为余弦相似度，词法路径为字符二元组 Jaccard。
    pub score: f64,
    /// 检索后端标识（`vector` / `lexical`）。
    pub backend: &'static str,
}

/// 记忆深度整理参数。
#[derive(Clone)]
pub struct ConsolidationOptions {
    /// 相似度阈值（0~1），越高越保守。
    pub similarity_threshold: f64,
    /// 每组最多参与的相似记忆数。
    pub group_limit: usize,
    /// 每条记忆的相似候选检索上限。
    pub candidate_limit: usize,
    /// Memory LLM；为 `None` 时退化为规则整理（仍做合并与关联重建）。
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
    /// 最多处理多少条记忆（0 表示全部）。
    pub max_nodes: usize,
    /// 跳过最新的 N 条记忆再开始扫描（配合扫描上限分批处理全库）。
    pub offset: usize,
}

impl Default for ConsolidationOptions {
    fn default() -> Self {
        Self {
            similarity_threshold: DEFAULT_SIMILARITY_THRESHOLD,
            group_limit: DEFAULT_GROUP_LIMIT,
            candidate_limit: DEFAULT_CANDIDATE_LIMIT,
            llm: None,
            embedding: None,
            dry_run: false,
            forget_low_quality: true,
            min_summary_chars: DEFAULT_MIN_SUMMARY_CHARS,
            stale_days: DEFAULT_STALE_DAYS,
            importance_floor: DEFAULT_IMPORTANCE_FLOOR,
            max_nodes: 0,
            offset: 0,
        }
    }
}

/// 整理结果报告。
#[derive(Debug, Clone, Default)]
pub struct ConsolidationReport {
    /// 扫描到的活跃记忆数。
    pub scanned: usize,
    /// 分批扫描的批次数（全库超过单批上限时 > 1）。
    pub pages: usize,
    /// 实际处理（作为主记忆）的条数。
    pub processed: usize,
    /// 未处理的剩余条数（受 `max_nodes` 单批上限影响）。
    pub remaining: usize,
    /// 发生合并的组数。
    pub merged_groups: usize,
    /// 被合并归档的记忆条数。
    pub merged_away: usize,
    /// 被重写（归一/重分类）的主记忆条数。
    pub updated: usize,
    /// 其中认知分类发生变化的条数。
    pub reclassified: usize,
    /// 被遗忘（归档）的低质量记忆条数。
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
    /// 是否试运行。
    pub dry_run: bool,
    /// 人类可读的处理明细（限量）。
    pub actions: Vec<String>,
    /// 处理过程中的错误（不中断整理）。
    pub errors: Vec<String>,
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
            } else {
                "完成"
            }
        );
        let _ = writeln!(
            out,
            "- 检索后端：{}；扫描活跃记忆：{}（{} 批）",
            self.retrieval_backend, self.scanned, self.pages
        );
        let _ = writeln!(
            out,
            "- 已处理：{}（剩余未处理：{}）",
            self.processed, self.remaining
        );
        let _ = writeln!(
            out,
            "- 合并组数：{}，合并归档：{}，重写归一：{}（重分类 {}）",
            self.merged_groups, self.merged_away, self.updated, self.reclassified
        );
        let _ = writeln!(out, "- 遗忘低质量记忆：{}", self.forgotten);
        let _ = writeln!(
            out,
            "- 关联链：重指向重建 {}，LLM 新建 {}（LLM 调用 {} 次）",
            self.relations_relinked, self.relations_created, self.llm_calls
        );
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
        out
    }
}

/// 执行一次记忆深度整理。
pub async fn run_consolidation(
    handle: &MemoryHandle,
    options: &ConsolidationOptions,
    progress: Option<ConsolidationProgress<'_>>,
) -> ConsolidationReport {
    let mut report = ConsolidationReport {
        dry_run: options.dry_run,
        ..Default::default()
    };
    // 全库分批：每批扫描上限内的活跃记忆，整理完再取下一批（跳过已整理的
    // 最新若干条），直到某批不足上限即全库整理完成。`offset` 用于从指定位置
    // 开始（断点续跑），`max_nodes` 约束单批处理条数。
    let mut skip = options.offset;
    loop {
        let nodes = collect_active_nodes(handle, skip).await;
        if nodes.is_empty() {
            break;
        }
        let page = nodes.len();
        report.pages += 1;
        report.scanned += page;
        if let Some(callback) = progress {
            callback(&format!(
                "第 {} 批：扫描到 {} 条活跃记忆（累计跳过最新 {} 条）",
                report.pages, page, skip
            ));
        }
        consolidate_page(handle, &nodes, options, progress, &mut report).await;
        skip += page;
        if page < PAGE_LIMIT {
            break;
        }
    }
    report
}

/// 整理一批记忆：相似检索 → 逐条处理（过滤已处理）。
async fn consolidate_page(
    handle: &MemoryHandle,
    nodes: &[MemoryNode],
    options: &ConsolidationOptions,
    progress: Option<ConsolidationProgress<'_>>,
    report: &mut ConsolidationReport,
) {
    let by_id: HashMap<&str, &MemoryNode> =
        nodes.iter().map(|node| (node.id.as_str(), node)).collect();
    // 逐条向量检索：一次性算出每条记忆的相似候选。
    let candidates = match build_candidates(handle, nodes, &by_id, options, progress).await {
        Ok((candidates, backend)) => {
            report.retrieval_backend = backend;
            candidates
        }
        Err(err) => {
            report
                .errors
                .push(format!("相似检索失败，本批仅执行低质量遗忘：{err:#}"));
            report.retrieval_backend = "unavailable";
            HashMap::new()
        }
    };
    let mut processed: HashSet<String> = HashSet::new();
    let mut handled = 0;
    for node in nodes {
        if processed.contains(&node.id) {
            continue;
        }
        if options.max_nodes > 0 && handled >= options.max_nodes {
            break;
        }
        processed.insert(node.id.clone());
        handled += 1;
        report.processed += 1;
        if let Some(callback) = progress {
            callback(&format!(
                "整理中 [{}/{}] {}",
                report.processed,
                nodes.len(),
                truncate(&node.title, 40)
            ));
        }
        if let Err(err) = consolidate_node(
            handle,
            node,
            &by_id,
            &candidates,
            options,
            &mut processed,
            report,
        )
        .await
        {
            report
                .errors
                .push(format!("整理记忆 {} 失败：{err:#}", node.id));
        }
    }
    report.remaining += nodes
        .iter()
        .filter(|node| !processed.contains(&node.id))
        .count();
}
/// 分页拉取活跃记忆（按更新时间从新到旧）；`skip` 跳过最新的若干条。
async fn collect_active_nodes(handle: &MemoryHandle, skip: usize) -> Vec<MemoryNode> {
    let mut all = Vec::new();
    let mut offset = skip;
    loop {
        let batch = handle
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
        if all.len() >= MAX_SCAN_NODES {
            tracing::warn!(
                "活跃记忆超过 {MAX_SCAN_NODES} 条，本次只整理前 {MAX_SCAN_NODES} 条；\
                 可用 --max-nodes 分批处理"
            );
            break;
        }
    }
    all
}

/// 为每条记忆计算相似候选（逐条向量检索）。
///
/// 配置了 Embedding 端点时本地批量embedding + 余弦相似度（带字符二元组
/// 预过滤，避免全量两两比较）；否则经 Memory 召回取候选，再用字符二元组
/// Jaccard 判定一致性。返回 (候选表, 后端标识)。
async fn build_candidates(
    handle: &MemoryHandle,
    nodes: &[MemoryNode],
    by_id: &HashMap<&str, &MemoryNode>,
    options: &ConsolidationOptions,
    progress: Option<ConsolidationProgress<'_>>,
) -> anyhow::Result<(HashMap<String, Vec<SimilarHit>>, &'static str)> {
    if let Some(embedding) = options.embedding.as_ref() {
        let candidates = embed_candidates(embedding, nodes, options, progress).await?;
        return Ok((candidates, "vector"));
    }
    let mut candidates: HashMap<String, Vec<SimilarHit>> = HashMap::new();
    let total = nodes.len();
    for (index, node) in nodes.iter().enumerate() {
        if let Some(callback) = progress {
            callback(&format!(
                "检索中 [{}/{}] {}",
                index + 1,
                total,
                truncate(&node.title, 40)
            ));
        }
        let hits = handle
            .recall(
                RecallAnchors {
                    keywords: node.keywords.clone(),
                    query: node_text(node),
                    strategy: Some(SearchStrategy::Hybrid {
                        semantic_ratio: 0.8,
                    }),
                },
                options.candidate_limit + 1,
            )
            .await;
        let source_text = node_text(node);
        let mut selected: Vec<SimilarHit> = hits
            .iter()
            .filter(|hit| hit.node_id != node.id)
            .filter_map(|hit| {
                let node = by_id.get(hit.node_id.as_str())?;
                let score = text_similarity(&source_text, &node_text(node));
                (score >= options.similarity_threshold).then(|| SimilarHit {
                    node: (*node).clone(),
                    score,
                    backend: "lexical",
                })
            })
            .collect();
        selected.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        selected.truncate(options.candidate_limit);
        if !selected.is_empty() {
            candidates.insert(node.id.clone(), selected);
        }
    }
    Ok((candidates, "lexical"))
}

/// 批量embedding + 余弦相似度候选（字符二元组预过滤）。
async fn embed_candidates(
    embedding: &Arc<dyn EmbeddingProvider>,
    nodes: &[MemoryNode],
    options: &ConsolidationOptions,
    progress: Option<ConsolidationProgress<'_>>,
) -> anyhow::Result<HashMap<String, Vec<SimilarHit>>> {
    let texts: Vec<String> = nodes.iter().map(node_text).collect();
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for (index, batch) in texts.chunks(EMBEDDING_BATCH).enumerate() {
        if let Some(callback) = progress {
            callback(&format!(
                "向量化 [{}/{}]",
                (index * EMBEDDING_BATCH).min(texts.len()),
                texts.len()
            ));
        }
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
    // 预归一化，比较时只需点积。
    let normalized: Vec<Vec<f32>> = vectors.iter().map(|vector| normalize(vector)).collect();
    // 字符二元组倒排索引：只与共享足够二元组的记忆做余弦比较。
    let mut inverted: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        for bigram in bigrams(&node_text(node)) {
            inverted.entry(bigram).or_default().push(index);
        }
    }
    let mut candidates: HashMap<String, Vec<SimilarHit>> = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        let mut shared: HashMap<usize, usize> = HashMap::new();
        for bigram in bigrams(&node_text(node)) {
            if let Some(postings) = inverted.get(&bigram) {
                for other in postings {
                    if *other != index {
                        *shared.entry(*other).or_insert(0) += 1;
                    }
                }
            }
        }
        let mut scored: Vec<(f64, usize)> = shared
            .into_iter()
            .filter(|(_, count)| *count >= MIN_SHARED_BIGRAMS)
            .map(|(other, _)| (cosine(&normalized[index], &normalized[other]), other))
            .filter(|(score, _)| *score >= options.similarity_threshold)
            .collect();
        scored.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(options.candidate_limit);
        if scored.is_empty() {
            continue;
        }
        let hits = scored
            .into_iter()
            .map(|(score, other)| SimilarHit {
                node: nodes[other].clone(),
                score,
                backend: "vector",
            })
            .collect();
        candidates.insert(node.id.clone(), hits);
    }
    Ok(candidates)
}

/// 处理单条记忆：低质量遗忘 → 相似组 → LLM/规则整理。
async fn consolidate_node(
    handle: &MemoryHandle,
    node: &MemoryNode,
    by_id: &HashMap<&str, &MemoryNode>,
    candidates: &HashMap<String, Vec<SimilarHit>>,
    options: &ConsolidationOptions,
    processed: &mut HashSet<String>,
    report: &mut ConsolidationReport,
) -> anyhow::Result<()> {
    // 1) 规则低质量遗忘。
    if options.forget_low_quality
        && let Some(reason) = rule_low_quality(node, options)
    {
        if !options.dry_run {
            handle
                .set_node_status(node.id.clone(), MemoryStatus::Archived)
                .await?;
        }
        report.forgotten += 1;
        report.actions.push(format!(
            "遗忘低质量记忆「{}」（{}）{}",
            truncate(&node.title, 40),
            reason,
            dry_run_tag(options)
        ));
        return Ok(());
    }
    // 2) 取出该记忆的相似组（过滤已处理）。
    let group: Vec<SimilarHit> = candidates
        .get(&node.id)
        .map(|hits| {
            hits.iter()
                .filter(|hit| !processed.contains(&hit.node.id))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    // 组大小受 group_limit 约束（候选已按相似度降序）。
    let mut group = group;
    group.truncate(options.group_limit);
    if group.is_empty() {
        return Ok(());
    }
    // 3) LLM 整理；未配置或调用失败时退化为规则整理。
    let mut group_ids = vec![node.id.clone()];
    group_ids.extend(group.iter().map(|hit| hit.node.id.clone()));
    let relations = handle.list_relations_batch(group_ids).await;
    let decision = match options.llm.as_ref() {
        Some(model) => match request_consolidation(model, node, &group, &relations).await {
            Ok(decision) => {
                report.llm_calls += 1;
                report.used_llm = true;
                Some(decision)
            }
            Err(err) => {
                report.errors.push(format!(
                    "记忆 {} 的 LLM 整理失败，已按规则整理：{err:#}",
                    node.id
                ));
                None
            }
        },
        None => None,
    };
    apply_decision(
        handle,
        node,
        &group,
        by_id,
        decision.as_ref(),
        &relations,
        options,
        processed,
        report,
    )
    .await
}

/// 应用整理决策：合并归一 / 保留重分类 / 整组遗忘，并重建关联链。
#[allow(clippy::too_many_arguments)]
async fn apply_decision(
    handle: &MemoryHandle,
    node: &MemoryNode,
    group: &[SimilarHit],
    by_id: &HashMap<&str, &MemoryNode>,
    decision: Option<&ConsolidationDecision>,
    relations: &[MemoryRelation],
    options: &ConsolidationOptions,
    processed: &mut HashSet<String>,
    report: &mut ConsolidationReport,
) -> anyhow::Result<()> {
    let action = decision
        .map(ConsolidationDecision::action)
        .unwrap_or(ConsolidationAction::Merge);
    let primary_id = decision
        .and_then(|decision| decision.primary_id.clone())
        .filter(|id| *id == node.id || group.iter().any(|hit| hit.node.id == *id))
        .unwrap_or_else(|| node.id.clone());
    // 整组（含主记忆）标记为已处理，后续节点不再处理它们。
    processed.insert(primary_id.clone());
    for hit in group {
        processed.insert(hit.node.id.clone());
    }
    let group_ids: HashSet<String> = group
        .iter()
        .map(|hit| hit.node.id.clone())
        .chain(std::iter::once(primary_id.clone()))
        .collect();

    match action {
        ConsolidationAction::Forget => {
            let mut count = 0;
            for id in &group_ids {
                if !options.dry_run {
                    handle
                        .set_node_status(id.clone(), MemoryStatus::Archived)
                        .await?;
                }
                count += 1;
            }
            report.forgotten += count;
            report.actions.push(format!(
                "遗忘整组低质量/重复记忆 {} 条{}",
                count,
                dry_run_tag(options)
            ));
            return Ok(());
        }
        ConsolidationAction::Keep => {
            // 相似但各有价值：仅按需重分类主记忆、应用 LLM 关联与遗忘建议。
            if let Some(decision) = decision {
                if let Some(memory_type) =
                    decision.memory_type.as_deref().and_then(parse_memory_type)
                    && memory_type != node.memory_type
                    && !options.dry_run
                {
                    handle
                        .upsert_manual_memory(ManualMemoryDraft {
                            id: Some(node.id.clone()),
                            memory_type: memory_type.clone(),
                            ..base_draft(node)
                        })
                        .await?;
                    report.updated += 1;
                    report.reclassified += 1;
                    report.actions.push(format!(
                        "重分类记忆「{}」为 {:?}{}",
                        truncate(&node.title, 40),
                        memory_type,
                        dry_run_tag(options)
                    ));
                }
                let (relinked, created) = relink_relations(
                    handle,
                    &node.id,
                    &[],
                    &decision.relations,
                    relations,
                    &group_ids,
                    options,
                )
                .await?;
                report.relations_relinked += relinked;
                report.relations_created += created;
                forget_members(handle, decision, &group_ids, node, options, report).await?;
            }
            return Ok(());
        }
        ConsolidationAction::Merge => {}
    }

    // ── 合并归一 ──
    let base = group_base(node, group, by_id, &primary_id);
    let (title, summary, keywords, memory_type, importance) = match decision {
        Some(decision) => (
            decision
                .title
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| base.title.clone()),
            decision
                .summary
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| base.summary.clone()),
            if decision.keywords.is_empty() {
                base.keywords.clone()
            } else {
                normalize_keywords(&decision.keywords)
            },
            decision
                .memory_type
                .as_deref()
                .and_then(parse_memory_type)
                .unwrap_or_else(|| base.memory_type.clone()),
            decision
                .importance
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(0.05, 1.0) as f32)
                .unwrap_or(base.importance),
        ),
        None => (
            base.title.clone(),
            base.summary.clone(),
            base.keywords.clone(),
            base.memory_type.clone(),
            base.importance,
        ),
    };
    if !options.dry_run {
        handle
            .upsert_manual_memory(ManualMemoryDraft {
                id: Some(primary_id.clone()),
                memory_type: memory_type.clone(),
                title: title.clone(),
                summary: summary.clone(),
                keywords: keywords.clone(),
                importance,
                ..Default::default()
            })
            .await?;
    }
    report.updated += 1;
    if memory_type != base.memory_type {
        report.reclassified += 1;
    }
    // 其余组员归档（内容已并入主记忆）。
    let archived_ids: Vec<String> = group_ids
        .iter()
        .filter(|id| **id != primary_id)
        .cloned()
        .collect();
    for id in &archived_ids {
        if !options.dry_run {
            handle
                .set_node_status(id.clone(), MemoryStatus::Archived)
                .await?;
        }
        report.merged_away += 1;
    }
    let planned = decision
        .map(|decision| decision.relations.as_slice())
        .unwrap_or(&[]);
    let (relinked, created) = relink_relations(
        handle,
        &primary_id,
        &archived_ids,
        planned,
        relations,
        &group_ids,
        options,
    )
    .await?;
    report.relations_relinked += relinked;
    report.relations_created += created;
    report.merged_groups += 1;
    let source = if decision.is_some() { "LLM" } else { "规则" };
    report.actions.push(format!(
        "{}合并 {} 条一致记忆为「{}」（分类 {:?}）{}",
        source,
        archived_ids.len() + 1,
        truncate(&title, 40),
        memory_type,
        dry_run_tag(options)
    ));
    Ok(())
}

/// 归档决策中标记为低质量的组内记忆。
async fn forget_members(
    handle: &MemoryHandle,
    decision: &ConsolidationDecision,
    group_ids: &HashSet<String>,
    node: &MemoryNode,
    options: &ConsolidationOptions,
    report: &mut ConsolidationReport,
) -> anyhow::Result<()> {
    for id in &decision.forget_ids {
        // 只允许遗忘组内记忆，避免 LLM 幻觉 ID 波及无关记忆。
        if !group_ids.contains(id) || id == &node.id {
            continue;
        }
        if !options.dry_run {
            handle
                .set_node_status(id.clone(), MemoryStatus::Archived)
                .await?;
        }
        report.forgotten += 1;
        report
            .actions
            .push(format!("遗忘组内低质量记忆 {id}{}", dry_run_tag(options)));
    }
    Ok(())
}

/// 重建关联链：把已归档记忆的关系重指向主记忆，并落库 LLM 规划的新关系。
///
/// 返回 (机械重指向数, LLM 新建数)。
#[allow(clippy::too_many_arguments)]
async fn relink_relations(
    handle: &MemoryHandle,
    primary_id: &str,
    archived_ids: &[String],
    planned: &[RelationPlan],
    relations: &[MemoryRelation],
    group_ids: &HashSet<String>,
    options: &ConsolidationOptions,
) -> anyhow::Result<(usize, usize)> {
    let archived: HashSet<&str> = archived_ids.iter().map(String::as_str).collect();
    let mut relinked = 0;
    for relation in relations {
        let from_archived = archived.contains(relation.from_node_id.as_str());
        let to_archived = archived.contains(relation.to_node_id.as_str());
        if !from_archived && !to_archived {
            continue;
        }
        // 组内互连的关系随合并自然消失，无需重指向。
        if from_archived && to_archived {
            continue;
        }
        let other = if from_archived {
            relation.to_node_id.as_str()
        } else {
            relation.from_node_id.as_str()
        };
        if other == primary_id || group_ids.contains(other) {
            continue;
        }
        let note = relation
            .note
            .clone()
            .or_else(|| Some("记忆深度整理重建关联".to_string()));
        let draft = if from_archived {
            MemoryRelationDraft {
                from_node_id: primary_id.to_string(),
                to_node_id: other.to_string(),
                relation_kind: relation.relation_kind.clone(),
                weight: relation.weight,
                note,
                ..Default::default()
            }
        } else {
            MemoryRelationDraft {
                from_node_id: other.to_string(),
                to_node_id: primary_id.to_string(),
                relation_kind: relation.relation_kind.clone(),
                weight: relation.weight,
                note,
                ..Default::default()
            }
        };
        if !options.dry_run {
            handle.upsert_relation(draft).await?;
            handle.delete_relation(relation.id.clone()).await?;
        }
        relinked += 1;
    }
    // LLM 规划的新关系：目标必须在组内或原关联链上出现过，防止幻觉 ID。
    let known_external: HashSet<&str> = relations
        .iter()
        .flat_map(|relation| [relation.from_node_id.as_str(), relation.to_node_id.as_str()])
        .filter(|id| !group_ids.contains(*id) && *id != primary_id)
        .collect();
    let mut created = 0;
    for plan in planned {
        let target = plan.to_id.trim();
        if target.is_empty() || target == primary_id {
            continue;
        }
        if !group_ids.contains(target) && !known_external.contains(target) {
            tracing::debug!(target, "记忆整理忽略未知关联目标");
            continue;
        }
        let draft = MemoryRelationDraft {
            from_node_id: primary_id.to_string(),
            to_node_id: target.to_string(),
            relation_kind: parse_relation_kind(&plan.kind).unwrap_or_default(),
            weight: plan.weight.unwrap_or(0.6).clamp(0.0, 1.0) as f32,
            note: Some(
                plan.note
                    .clone()
                    .unwrap_or_else(|| "记忆深度整理新建关联".to_string()),
            ),
            ..Default::default()
        };
        if !options.dry_run {
            handle.upsert_relation(draft).await?;
        }
        created += 1;
    }
    Ok((relinked, created))
}

// ---------------------------------------------------------------------------
// LLM 整理
// ---------------------------------------------------------------------------

/// 整理系统提示：只输出 JSON，保守合并。
const CONSOLIDATION_SYSTEM: &str = r#"你是记忆整理专家。用户会给你一组「高度一致」的长期记忆（含主记忆）及其现有关联链，请把它们归一整理。

只输出 JSON 对象，不要输出解释、不要使用 Markdown 代码块。字段：
- decision: "merge" | "keep" | "forget"
  merge = 组内记忆应合并成一条（写入主记忆，其余归档）；
  keep = 虽然相似但各有独立价值，保留全部（可仅修正主记忆分类）；
  forget = 整组都是低质量/无价值记忆，全部遗忘。
- primary_id: 主记忆 ID（merge/keep 必填，必须是给定 ID 之一；通常选信息最完整的一条）。
- title: 合并后的标题（merge 必填，<=40 字，保留关键主体）。
- summary: 合并后的摘要（merge 必填，<=300 字；保留关键事实、路径、版本号、配置项等细节，去除重复表述，不要编造未提供的信息）。
- keywords: 3~8 个关键词（数组）。
- memory_type: factual | user_preference | user_habit | skill | project_structure | architecture_decision | problem_incident | domain_knowledge。
- importance: 0~1 的重要性（取组内最高并可按价值微调）。
- forget_ids: 组内需要遗忘（归档）的低质量记忆 ID 数组（内容空泛、已被其它记忆完全覆盖、无独立价值）。
- relations: 重建的关联链数组，元素 {"to_id":"<记忆ID>","kind":"related_to|depends_on|supports|contradicts|supersedes|caused_by|belongs_to|learned_from|validated_by","weight":0~1,"note":"可选"}，表示从主记忆指向目标记忆；不要指向组内其它记忆。

原则：信息不冲突才合并；结论相反的记忆不要合并，用 contradicts 关系保留；宁可 keep 也不要丢失有价值细节。"#;

/// LLM 整理决策。
#[derive(Debug, Clone, Deserialize)]
struct ConsolidationDecision {
    #[serde(default)]
    decision: String,
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
    forget_ids: Vec<String>,
    #[serde(default)]
    relations: Vec<RelationPlan>,
}

/// LLM 规划的关联。
#[derive(Debug, Clone, Deserialize)]
struct RelationPlan {
    to_id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    weight: Option<f64>,
    #[serde(default)]
    note: Option<String>,
}

/// 整理动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsolidationAction {
    Merge,
    Keep,
    Forget,
}

impl ConsolidationDecision {
    fn action(&self) -> ConsolidationAction {
        match self.decision.trim().to_ascii_lowercase().as_str() {
            "forget" | "archive" => ConsolidationAction::Forget,
            "keep" | "keep_all" | "separate" => ConsolidationAction::Keep,
            _ => ConsolidationAction::Merge,
        }
    }
}

/// 调用 LLM 完成一组记忆的归一整理。
async fn request_consolidation(
    model: &LlmEndpointConfig,
    node: &MemoryNode,
    group: &[SimilarHit],
    relations: &[MemoryRelation],
) -> anyhow::Result<ConsolidationDecision> {
    let prompt = build_consolidation_prompt(node, group, relations);
    let started = Instant::now();
    let (response, usage) =
        complete_text_with_usage(model, CONSOLIDATION_SYSTEM, &prompt, LLM_MAX_TOKENS).await?;
    tracing::info!(
        model = %model.model,
        elapsed_ms = started.elapsed().as_millis(),
        prompt_tokens = usage.as_ref().map(|usage| usage.prompt_tokens).unwrap_or_default(),
        completion_tokens = usage.as_ref().map(|usage| usage.completion_tokens).unwrap_or_default(),
        "记忆整理 LLM 调用完成"
    );
    let json =
        extract_json_object(&response).ok_or_else(|| anyhow::anyhow!("LLM 未返回 JSON 对象"))?;
    let decision: ConsolidationDecision =
        serde_json::from_str(json).map_err(|err| anyhow::anyhow!("解析整理结果失败：{err}"))?;
    if decision.action() == ConsolidationAction::Merge
        && decision
            .summary
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
    {
        anyhow::bail!("LLM 合并结果缺少 summary");
    }
    Ok(decision)
}

/// 组装整理用的用户提示（组内记忆 + 主记忆 + 现有关联链）。
fn build_consolidation_prompt(
    node: &MemoryNode,
    group: &[SimilarHit],
    relations: &[MemoryRelation],
) -> String {
    let mut items = vec![memory_json(node, None)];
    items.extend(group.iter().map(|hit| {
        let similarity = if hit.backend == "vector" {
            Some(hit.score)
        } else {
            None
        };
        memory_json(&hit.node, similarity)
    }));
    let relations: Vec<serde_json::Value> = relations
        .iter()
        .map(|relation| {
            serde_json::json!({
                "from_id": relation.from_node_id,
                "to_id": relation.to_node_id,
                "kind": relation_kind_key(&relation.relation_kind),
                "note": relation.note,
            })
        })
        .collect();
    serde_json::json!({
        "primary_id": node.id,
        "memories": items,
        "existing_relations": relations,
        "instruction": "请把高度一致的记忆归一合并；低质量记忆放入 forget_ids；按需重建关联链。",
    })
    .to_string()
}

/// 序列化一条记忆供 LLM 阅读。
fn memory_json(node: &MemoryNode, similarity: Option<f64>) -> serde_json::Value {
    serde_json::json!({
        "id": node.id,
        "title": node.title,
        "summary": node.summary,
        "keywords": node.keywords,
        "memory_type": memory_type_key(&node.memory_type),
        "importance": node.importance,
        "similarity": similarity,
    })
}

/// 从 LLM 输出中提取第一个完整 JSON 对象。
fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}

// ---------------------------------------------------------------------------
// 规则整理与低质量判定
// ---------------------------------------------------------------------------

/// 组内基础信息（主记忆或其替身），用于在 LLM 缺字段时回退。
struct GroupBase {
    title: String,
    summary: String,
    keywords: Vec<String>,
    memory_type: MemoryCognitiveType,
    importance: f32,
}

/// 取主记忆对应的基础信息；主记忆不是当前节点时从组内命中取。
fn group_base(
    node: &MemoryNode,
    group: &[SimilarHit],
    by_id: &HashMap<&str, &MemoryNode>,
    primary_id: &str,
) -> GroupBase {
    let source = if primary_id == node.id {
        Some(node)
    } else {
        group
            .iter()
            .find(|hit| hit.node.id == primary_id)
            .map(|hit| &hit.node)
            .or_else(|| by_id.get(primary_id).copied())
    };
    match source {
        Some(node) => GroupBase {
            title: node.title.clone(),
            summary: node.summary.clone(),
            keywords: node.keywords.clone(),
            memory_type: node.memory_type.clone(),
            importance: node.importance,
        },
        None => GroupBase {
            title: node.title.clone(),
            summary: node.summary.clone(),
            keywords: node.keywords.clone(),
            memory_type: node.memory_type.clone(),
            importance: node.importance,
        },
    }
}

/// 未提供 LLM 决策时的主记忆草稿（保留现有分类）。
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

/// 距今天数（解析失败返回 `None`）。
fn days_since(timestamp: &str) -> Option<i64> {
    let parsed = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S%.f"))
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(timestamp, "%Y-%m-%d")
                .map(|date| date.and_hms_opt(0, 0, 0).unwrap_or_default())
        })
        .ok()?;
    let now = chrono::Local::now().naive_local();
    Some((now - parsed).num_days().max(0))
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
fn parse_memory_type(raw: &str) -> Option<MemoryCognitiveType> {
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
fn memory_type_key(memory_type: &MemoryCognitiveType) -> &'static str {
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
fn normalize_keywords(keywords: &[String]) -> Vec<String> {
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
fn dry_run_tag(options: &ConsolidationOptions) -> &'static str {
    if options.dry_run {
        "（试运行）"
    } else {
        ""
    }
}

/// 截断文本用于报告展示。
fn truncate(text: &str, max_chars: usize) -> String {
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
    fn parses_llm_decision_with_markdown_fence() {
        let raw = "```json\n{\"decision\":\"merge\",\"primary_id\":\"a\",\"title\":\"合并\",\"summary\":\"摘要\",\"keywords\":[\"redis\"],\"memory_type\":\"skill\",\"importance\":0.9,\"forget_ids\":[\"b\"],\"relations\":[{\"to_id\":\"x\",\"kind\":\"supersedes\",\"weight\":0.8,\"note\":\"n\"}]}\n```";
        let json = extract_json_object(raw).expect("应提取 JSON");
        let decision: ConsolidationDecision = serde_json::from_str(json).expect("应可解析");
        assert_eq!(decision.action(), ConsolidationAction::Merge);
        assert_eq!(decision.primary_id.as_deref(), Some("a"));
        assert_eq!(decision.memory_type.as_deref(), Some("skill"));
        assert_eq!(decision.forget_ids, vec!["b".to_string()]);
        assert_eq!(decision.relations.len(), 1);
        assert_eq!(
            parse_relation_kind(&decision.relations[0].kind),
            Some(MemoryRelationKind::Supersedes)
        );
    }

    #[test]
    fn decision_action_defaults_to_merge() {
        let decision: ConsolidationDecision =
            serde_json::from_str(r#"{"decision":"unknown"}"#).expect("应可解析");
        assert_eq!(decision.action(), ConsolidationAction::Merge);
        let keep: ConsolidationDecision =
            serde_json::from_str(r#"{"decision":"keep"}"#).expect("应可解析");
        assert_eq!(keep.action(), ConsolidationAction::Keep);
        let forget: ConsolidationDecision =
            serde_json::from_str(r#"{"decision":"forget"}"#).expect("应可解析");
        assert_eq!(forget.action(), ConsolidationAction::Forget);
    }

    #[test]
    fn render_reports_counts() {
        let report = ConsolidationReport {
            scanned: 10,
            pages: 1,
            processed: 4,
            remaining: 6,
            merged_groups: 2,
            merged_away: 3,
            updated: 2,
            reclassified: 1,
            forgotten: 1,
            relations_relinked: 2,
            relations_created: 1,
            llm_calls: 2,
            used_llm: true,
            retrieval_backend: "vector",
            dry_run: false,
            actions: vec!["合并 2 条一致记忆".to_string()],
            errors: Vec::new(),
        };
        let text = report.render();
        assert!(text.contains("检索后端：vector"));
        assert!(text.contains("扫描活跃记忆：10"));
        assert!(text.contains("合并组数：2"));
        assert!(text.contains("遗忘低质量记忆：1"));
        assert!(text.contains("剩余未处理：6"));
    }

    #[test]
    fn group_base_falls_back_to_group_member() {
        let source = node("a", "主记忆", "摘要甲", 0.8);
        let member = node("b", "组员", "摘要乙", 0.7);
        let group = vec![SimilarHit {
            node: member.clone(),
            score: 0.95,
            backend: "vector",
        }];
        let by_id: HashMap<&str, &MemoryNode> =
            [("a", &source), ("b", &member)].into_iter().collect();
        let base = group_base(&source, &group, &by_id, "b");
        assert_eq!(base.title, "组员", "主记忆是组员时应取组员信息");
        let base = group_base(&source, &group, &by_id, "a");
        assert_eq!(base.title, "主记忆");
        let base = group_base(&source, &group, &by_id, "missing");
        assert_eq!(base.title, "主记忆", "未知主记忆回退到当前节点");
    }
}
