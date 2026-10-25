//! 相似整理之前的两个前置阶段。
//!
//! - **dedup**：同一范围（工作区）内摘要完全一致的记忆直接归一，不调用 LLM。
//!   主记忆优先取「决策」类，其次重要性、使用次数、更新时间；标题取组内出现
//!   次数最多的，重要性取组内最高；组员的关联全部改挂到主记忆（重复关联取较大
//!   权重）后物理删除组员。摘要过短的不处理；证据类记忆还要求证据路径一致；
//!   工具日志交给 distill 阶段（保留各自的文件路径供 LLM 参考）。
//! - **distill**：工具日志（标题形如 `fs__file_write 产物` 的证据类记忆）本身
//!   信息量很低。按工作区、按时间段（相邻日志间隔不超过窗口）分组，每批连同
//!   证据路径与同一时间段内的经历/决策记忆交给 LLM：提炼出新的经历记忆（关联到
//!   依据的日志与相关上下文），并判定哪些日志可以删除。先写新记忆再删日志；
//!   LLM 没有点名删除的日志一律保留。

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use serde::Deserialize;
use tiangong_memory::{
    ManualMemoryDraft, MemoryCognitiveType, MemoryKind, MemoryNode, MemoryRelationDraft,
    MemoryRelationKind, MemoryScopeType,
};

use crate::consolidation::{
    ConsolidationOptions, ConsolidationProgress, ConsolidationReport, HandleProvider,
    delete_with_journal, dry_run_tag, memory_type_key, normalize_keywords, normalized_key,
    one_line, parse_memory_type, parse_timestamp, relation_weights, relink_relations,
    request_json_with_retry, truncate, upsert_relation_max, upsert_with_journal,
};

/// dedup：摘要短于该字符数的记忆不参与归一（短摘要撞车概率高，信息不足以判断）。
pub const DEFAULT_DEDUP_MIN_SUMMARY_CHARS: usize = 20;
/// distill：相邻工具日志的最大间隔（分钟），超过即切分为新的时间段。
pub const DEFAULT_DISTILL_WINDOW_MINUTES: i64 = 30;
/// distill：单次 LLM 调用最多包含的工具日志数。
pub const DEFAULT_DISTILL_BATCH_SIZE: usize = 40;
/// distill：每批最多附带的上下文记忆数。
const DISTILL_CONTEXT_LIMIT: usize = 20;
/// distill：送给 LLM 的单条摘要最大字符数。
const DISTILL_SUMMARY_CHARS: usize = 300;
/// 单次读取证据路径的节点数。
const EVIDENCE_PATH_CHUNK: usize = 200;

/// 整理阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsolidationStage {
    /// 摘要完全一致的归一（不调用 LLM）。
    Dedup,
    /// 工具日志由 LLM 按上下文提炼。
    Distill,
    /// 相似召回 + LLM 判定（排除工具日志）。
    Similar,
}

impl ConsolidationStage {
    /// 全部阶段（按执行顺序）。
    pub const ALL: [ConsolidationStage; 3] = [
        ConsolidationStage::Dedup,
        ConsolidationStage::Distill,
        ConsolidationStage::Similar,
    ];

    pub fn key(self) -> &'static str {
        match self {
            ConsolidationStage::Dedup => "dedup",
            ConsolidationStage::Distill => "distill",
            ConsolidationStage::Similar => "similar",
        }
    }
}

/// 工具日志：标题形如 `<工具名> 产物`（工具名只含字母、数字、下划线）的证据类记忆。
///
/// 这类记忆由反刍的规则兜底路径为每次带文件路径的工具调用自动生成，摘要通常
/// 只是「写入成功」之类的工具返回值；有具体标题的证据（如「发布说明 产物」）
/// 不算工具日志。
pub fn is_tool_log(node: &MemoryNode) -> bool {
    node.kind == MemoryKind::Evidence
        && node.title.strip_suffix(" 产物").is_some_and(|tool| {
            !tool.is_empty()
                && tool
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
}

/// 范围键：只在同一范围内归一 / 提炼。
fn scope_key(node: &MemoryNode) -> String {
    let scope = match node.scope_type {
        MemoryScopeType::Global => "global",
        MemoryScopeType::Workspace => "workspace",
        MemoryScopeType::Session => "session",
    };
    format!("{scope}:{}", node.scope_id.as_deref().unwrap_or("-"))
}

/// 读取证据路径（只读，不计使用次数）。
async fn load_evidence_paths(
    provider: HandleProvider<'_>,
    ids: Vec<String>,
) -> anyhow::Result<HashMap<String, String>> {
    let mut paths = HashMap::new();
    for chunk in ids.chunks(EVIDENCE_PATH_CHUNK) {
        let items = provider()
            .evidence_paths(chunk.to_vec())
            .await
            .map_err(|err| {
                err.context(
                    "读取证据路径失败；若天工仍在运行升级前启动的 Memory，请完全退出并重启天工后重试",
                )
            })?;
        paths.extend(items.into_iter().map(|item| (item.node_id, item.path)));
    }
    Ok(paths)
}

// ---------------------------------------------------------------------------
// dedup
// ---------------------------------------------------------------------------

/// 主记忆优先级：决策 > 其他；同类按重要性、使用次数、更新时间从高到低。
fn primary_rank(node: &MemoryNode) -> (bool, f32, i64, &str) {
    (
        node.kind == MemoryKind::Decision,
        node.importance,
        node.usage_count,
        node.updated_at.as_str(),
    )
}

fn pick_primary(nodes: &[MemoryNode], members: &[usize]) -> usize {
    members
        .iter()
        .copied()
        .max_by(|left, right| {
            primary_rank(&nodes[*left])
                .partial_cmp(&primary_rank(&nodes[*right]))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(members[0])
}

/// 出现次数最多的标题；并列时取主记忆的标题，其次按组内顺序。
fn most_common_title(nodes: &[MemoryNode], members: &[usize], primary: usize) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for position in members {
        *counts.entry(nodes[*position].title.trim()).or_default() += 1;
    }
    let best = counts.values().copied().max().unwrap_or_default();
    let primary_title = nodes[primary].title.trim();
    if counts.get(primary_title) == Some(&best) {
        return primary_title.to_string();
    }
    members
        .iter()
        .map(|position| nodes[*position].title.trim())
        .find(|title| counts.get(title) == Some(&best))
        .unwrap_or(primary_title)
        .to_string()
}

/// dedup 分桶键：（范围，归一化摘要）。
///
/// 实体的摘要常是通用描述（如「开源社区」「推理框架」），不同实体可能摘要
/// 相同：实体还要求标题一致（忽略大小写与空白）。
fn dedup_key(node: &MemoryNode) -> (String, String) {
    let entity_title = if node.kind == MemoryKind::Entity {
        normalized_key(&node.title)
    } else {
        String::new()
    };
    (
        format!("{}\u{1f}{entity_title}", scope_key(node)),
        normalized_key(&node.summary),
    )
}

/// dedup 阶段：摘要完全一致的记忆归一。返回 `Err` 表示写入失败，应中断整理。
pub(crate) async fn run_dedup(
    provider: HandleProvider<'_>,
    nodes: &[MemoryNode],
    options: &ConsolidationOptions,
    removed: &mut HashSet<String>,
    progress: Option<ConsolidationProgress<'_>>,
    report: &mut ConsolidationReport,
) -> anyhow::Result<()> {
    // 1) 按（范围，归一化摘要）分桶。
    let mut buckets: HashMap<(String, String), Vec<usize>> = HashMap::new();
    let mut order: Vec<(String, String)> = Vec::new();
    for (position, node) in nodes.iter().enumerate() {
        if removed.contains(&node.id) || is_tool_log(node) {
            continue;
        }
        if node.summary.trim().chars().count() < options.dedup_min_summary_chars {
            continue;
        }
        let key = dedup_key(node);
        let entry = buckets.entry(key.clone()).or_default();
        if entry.is_empty() {
            order.push(key);
        }
        entry.push(position);
    }
    let candidates: Vec<Vec<usize>> = order
        .into_iter()
        .filter_map(|key| buckets.remove(&key))
        .filter(|members| members.len() >= 2)
        .collect();
    // 2) 证据类记忆还要求证据路径一致（路径不同说明是不同产物）。
    let evidence_ids: Vec<String> = candidates
        .iter()
        .flatten()
        .filter(|position| nodes[**position].kind == MemoryKind::Evidence)
        .map(|position| nodes[*position].id.clone())
        .collect();
    let paths = if evidence_ids.is_empty() {
        HashMap::new()
    } else {
        load_evidence_paths(provider, evidence_ids).await?
    };
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for members in candidates {
        let mut by_path: Vec<(String, Vec<usize>)> = Vec::new();
        for position in members {
            let path = paths.get(&nodes[position].id).cloned().unwrap_or_default();
            match by_path.iter_mut().find(|(key, _)| *key == path) {
                Some((_, list)) => list.push(position),
                None => by_path.push((path, vec![position])),
            }
        }
        groups.extend(
            by_path
                .into_iter()
                .map(|(_, list)| list)
                .filter(|list| list.len() >= 2),
        );
    }
    if let Some(callback) = progress {
        callback(&format!(
            "dedup：{} 组摘要完全一致的记忆，涉及 {} 条",
            groups.len(),
            groups.iter().map(Vec::len).sum::<usize>()
        ));
    }
    // 3) 逐组归一。
    for (index, members) in groups.iter().enumerate() {
        if options.stop_requested() {
            report.stopped = true;
            break;
        }
        let primary = pick_primary(nodes, members);
        let base = &nodes[primary];
        let title = most_common_title(nodes, members, primary);
        let importance = members
            .iter()
            .map(|position| nodes[*position].importance)
            .fold(base.importance, f32::max);
        let changed = title != base.title.trim() || importance > base.importance;
        if changed {
            let draft = ManualMemoryDraft {
                id: Some(base.id.clone()),
                memory_type: base.memory_type.clone(),
                title: title.clone(),
                summary: base.summary.clone(),
                keywords: base.keywords.clone(),
                importance,
                ..Default::default()
            };
            upsert_with_journal(
                provider,
                options,
                "dedup",
                Some(base),
                draft,
                "摘要完全一致归一：标题取多数、重要性取最高",
            )
            .await?;
        }
        if changed {
            report.dedup_updated += 1;
        }
        let group_ids: HashSet<String> = members
            .iter()
            .map(|position| nodes[*position].id.clone())
            .collect();
        // 关联读取是只读操作：试运行也读取，统计才准确。
        let relations = provider()
            .list_relations_batch(group_ids.iter().cloned().collect())
            .await;
        let mut weights = relation_weights(&relations);
        report.dedup_relinked += relink_relations(
            provider,
            &base.id,
            &group_ids,
            &relations,
            &mut weights,
            options,
        )
        .await?;
        for position in members.iter().filter(|position| **position != primary) {
            let node_id = nodes[*position].id.clone();
            delete_with_journal(
                provider,
                options,
                &nodes[*position],
                "dedup",
                "摘要完全一致",
                Some(&base.id),
                Some(&relations),
            )
            .await?;
            removed.insert(node_id);
            report.dedup_removed += 1;
        }
        report.dedup_groups += 1;
        let mut detail = format!(
            "dedup 归一 {} 条为「{}」（重要性 {:.2}）{}",
            members.len(),
            one_line(&title, 40),
            importance,
            dry_run_tag(options)
        );
        for position in members {
            let node = &nodes[*position];
            let _ = write!(
                detail,
                "\n      {} {} [{:?}]「{}」",
                if *position == primary { "★" } else { "·" },
                node.id,
                node.kind,
                one_line(&node.title, 50)
            );
        }
        report.actions.push(detail);
        if let Some(callback) = progress
            && (index + 1) % 50 == 0
        {
            callback(&format!("dedup 进度 [{}/{}]", index + 1, groups.len()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// distill
// ---------------------------------------------------------------------------

const DISTILL_SYSTEM: &str = r#"你是长期记忆整理专家。用户给出同一工作区、同一时间段内由工具调用自动记录的「工具日志」（logs：工具名、工具返回的结果摘要、涉及的文件路径），以及同一时间段内的经历/决策记忆作参考（context）。单条工具日志信息量很低（如「写入文件成功」），请结合文件路径与上下文判断这段时间实际做了什么，再整理这批日志。

只输出一个 JSON 对象，不要输出解释、不要使用 Markdown 代码块。字段：
- experiences: 从日志中提炼出、值得长期记住的经历 [{"title":"<=40 字","summary":"<=300 字：做了什么、涉及哪些关键文件/产物路径、得出什么结论","keywords":["3~8 个"],"memory_type":"见下方取值","importance":0~1,"source_ids":["依据的日志 ID，至少 1 个"],"related_ids":["相关的 context 记忆 ID，可为空"]}]。
  context 已经完整记录的内容不要重复提炼；同一件事只提炼一条；没有值得提炼的内容时给空数组。不要编造日志和上下文中没有的信息。
- delete: 可以删除的日志 [{"id":"...","reason":"..."}]：内容已提炼进 experiences、已被 context 覆盖，或本身没有长期价值（常规读写成功、临时文件、重复操作）。
- keep: 需要保留的日志 [{"id":"...","reason":"..."}]：含有独有且重要的信息（如最终产物、发布物、关键配置文件路径），且未被提炼。

未出现在 delete 中的日志一律保留；同时出现在 keep 与 delete 中按保留处理。
memory_type 取值：factual | user_preference | user_habit | skill | project_structure | architecture_decision | problem_incident | domain_knowledge。"#;

#[derive(Debug, Clone, Default, Deserialize)]
struct DistillDecision {
    #[serde(default)]
    experiences: Vec<DistilledExperience>,
    #[serde(default)]
    keep: Vec<IdReason>,
    #[serde(default)]
    delete: Vec<IdReason>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct DistilledExperience {
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    memory_type: Option<String>,
    #[serde(default)]
    importance: Option<f64>,
    #[serde(default)]
    source_ids: Vec<String>,
    #[serde(default)]
    related_ids: Vec<String>,
}

/// 兼容 `"id"` 与 `{"id":..,"reason":..}` 两种写法。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum IdReason {
    Id(String),
    Detail {
        id: String,
        #[serde(default)]
        reason: Option<String>,
    },
}

impl IdReason {
    fn into_parts(self) -> (String, String) {
        match self {
            IdReason::Id(id) => (id.trim().to_string(), String::new()),
            IdReason::Detail { id, reason } => (id.trim().to_string(), reason.unwrap_or_default()),
        }
    }
}

/// 一批待提炼的工具日志。
struct DistillBatch {
    scope: String,
    logs: Vec<usize>,
}

/// 按范围、时间段切分工具日志：同一范围内按创建时间排序，相邻间隔超过窗口
/// 即切段；每段再按批大小切批。
fn plan_distill_batches(
    nodes: &[MemoryNode],
    logs: &[usize],
    window_minutes: i64,
    batch_size: usize,
) -> Vec<DistillBatch> {
    let mut by_scope: HashMap<String, Vec<usize>> = HashMap::new();
    let mut scopes: Vec<String> = Vec::new();
    for position in logs {
        let scope = scope_key(&nodes[*position]);
        let entry = by_scope.entry(scope.clone()).or_default();
        if entry.is_empty() {
            scopes.push(scope);
        }
        entry.push(*position);
    }
    scopes.sort();
    let batch_size = batch_size.max(1);
    let mut batches = Vec::new();
    for scope in scopes {
        let mut list = by_scope.remove(&scope).unwrap_or_default();
        list.sort_by(|left, right| nodes[*left].created_at.cmp(&nodes[*right].created_at));
        let mut segment: Vec<usize> = Vec::new();
        let mut last: Option<chrono::NaiveDateTime> = None;
        let flush = |segment: &mut Vec<usize>, batches: &mut Vec<DistillBatch>| {
            for chunk in segment.chunks(batch_size) {
                batches.push(DistillBatch {
                    scope: scope.clone(),
                    logs: chunk.to_vec(),
                });
            }
            segment.clear();
        };
        for position in list {
            let at = parse_timestamp(&nodes[position].created_at);
            if let (Some(previous), Some(current)) = (last, at)
                && (current - previous).num_minutes() > window_minutes
            {
                flush(&mut segment, &mut batches);
            }
            if at.is_some() {
                last = at;
            }
            segment.push(position);
        }
        flush(&mut segment, &mut batches);
    }
    batches
}

/// 选取同一范围、批次时间段前后窗口内的经历/决策记忆作为上下文（重要性高的优先）。
fn pick_context(
    nodes: &[MemoryNode],
    pool: &[usize],
    batch: &[usize],
    window_minutes: i64,
) -> Vec<usize> {
    let times: Vec<chrono::NaiveDateTime> = batch
        .iter()
        .filter_map(|position| parse_timestamp(&nodes[*position].created_at))
        .collect();
    let (Some(start), Some(end)) = (times.iter().min(), times.iter().max()) else {
        return Vec::new();
    };
    let window = chrono::Duration::minutes(window_minutes);
    let (from, to) = (*start - window, *end + window);
    let mut picked: Vec<usize> = pool
        .iter()
        .copied()
        .filter(|position| {
            parse_timestamp(&nodes[*position].created_at).is_some_and(|at| at >= from && at <= to)
        })
        .collect();
    picked.sort_by(|left, right| {
        nodes[*right]
            .importance
            .partial_cmp(&nodes[*left].importance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    picked.truncate(DISTILL_CONTEXT_LIMIT);
    picked.sort_by(|left, right| nodes[*left].created_at.cmp(&nodes[*right].created_at));
    picked
}

fn build_distill_prompt(
    nodes: &[MemoryNode],
    batch: &DistillBatch,
    context: &[usize],
    paths: &HashMap<String, String>,
) -> String {
    let logs: Vec<serde_json::Value> = batch
        .logs
        .iter()
        .map(|position| {
            let node = &nodes[*position];
            serde_json::json!({
                "id": node.id,
                "tool": node.title.strip_suffix(" 产物").unwrap_or(&node.title),
                "summary": truncate(&node.summary, DISTILL_SUMMARY_CHARS),
                "path": paths.get(&node.id),
                "created_at": node.created_at,
            })
        })
        .collect();
    let context: Vec<serde_json::Value> = context
        .iter()
        .map(|position| {
            let node = &nodes[*position];
            serde_json::json!({
                "id": node.id,
                "kind": match node.kind {
                    MemoryKind::Decision => "decision",
                    _ => "episode",
                },
                "title": node.title,
                "summary": truncate(&node.summary, DISTILL_SUMMARY_CHARS),
                "memory_type": memory_type_key(&node.memory_type),
                "created_at": node.created_at,
            })
        })
        .collect();
    serde_json::json!({
        "scope": batch.scope,
        "logs": logs,
        "context": context,
        "instruction": "结合文件路径与上下文提炼这段时间的经历，并判定哪些工具日志可以删除；未点名删除的日志保留。",
    })
    .to_string()
}

/// distill 阶段：工具日志由 LLM 按上下文提炼。返回 `Err` 表示 LLM 连续失败或
/// 写入失败，应中断整理。
pub(crate) async fn run_distill(
    provider: HandleProvider<'_>,
    nodes: &[MemoryNode],
    options: &ConsolidationOptions,
    removed: &mut HashSet<String>,
    progress: Option<ConsolidationProgress<'_>>,
    report: &mut ConsolidationReport,
) -> anyhow::Result<()> {
    let logs: Vec<usize> = (0..nodes.len())
        .filter(|position| {
            !removed.contains(&nodes[*position].id) && is_tool_log(&nodes[*position])
        })
        .collect();
    report.distill_logs = logs.len();
    let Some(model) = options.llm.as_ref() else {
        report
            .actions
            .push("distill：未配置 LLM，跳过工具日志提炼".to_string());
        return Ok(());
    };
    let batches = plan_distill_batches(
        nodes,
        &logs,
        options.distill_window_minutes,
        options.distill_batch_size,
    );
    report.distill_batches_total = batches.len();
    // 上下文候选：各范围内的经历 / 决策记忆。
    let mut pools: HashMap<String, Vec<usize>> = HashMap::new();
    for (position, node) in nodes.iter().enumerate() {
        if matches!(node.kind, MemoryKind::Episode | MemoryKind::Decision)
            && !removed.contains(&node.id)
        {
            pools.entry(scope_key(node)).or_default().push(position);
        }
    }
    if let Some(callback) = progress {
        callback(&format!(
            "distill：{} 条工具日志，按 {} 分钟时间段分为 {} 批（每批最多 {} 条）",
            logs.len(),
            options.distill_window_minutes,
            batches.len(),
            options.distill_batch_size
        ));
    }
    let base_calls = report.llm_calls;
    for (index, batch) in batches.iter().enumerate() {
        if options.stop_requested() {
            report.stopped = true;
            break;
        }
        if options.max_llm_calls > 0 && report.llm_calls - base_calls >= options.max_llm_calls {
            report.distill_budget_exhausted = true;
            break;
        }
        if let Some(callback) = progress {
            callback(&format!(
                "distill [{}/{}] {}：{} 条日志（已删除日志 {} 条，新经历 {} 条）",
                index + 1,
                batches.len(),
                batch.scope,
                batch.logs.len(),
                report.distill_deleted,
                report.distill_created
            ));
        }
        let ids: Vec<String> = batch
            .logs
            .iter()
            .map(|position| nodes[*position].id.clone())
            .collect();
        let paths = load_evidence_paths(provider, ids.clone()).await?;
        let pool = pools.get(&batch.scope).map(Vec::as_slice).unwrap_or(&[]);
        let context = pick_context(nodes, pool, &batch.logs, options.distill_window_minutes);
        let prompt = build_distill_prompt(nodes, batch, &context, &paths);
        let decision: DistillDecision = request_json_with_retry(
            model,
            DISTILL_SYSTEM,
            &prompt,
            &format!("distill 第 {} 批（{}）", index + 1, batch.scope),
            batch.logs.len() + context.len(),
            options,
            report,
        )
        .await?;
        report.distill_batches += 1;
        apply_distill(
            provider, nodes, batch, &context, decision, options, removed, report,
        )
        .await?;
    }
    Ok(())
}

/// 落库一批提炼结果：写新经历 → 建关联 → 删除日志（被提炼的日志先把关联改挂
/// 到新经历）。试运行只更新报告。
#[allow(clippy::too_many_arguments)]
async fn apply_distill(
    provider: HandleProvider<'_>,
    nodes: &[MemoryNode],
    batch: &DistillBatch,
    context: &[usize],
    decision: DistillDecision,
    options: &ConsolidationOptions,
    removed: &mut HashSet<String>,
    report: &mut ConsolidationReport,
) -> anyhow::Result<()> {
    let dry_run = options.dry_run;
    let log_ids: HashSet<&str> = batch
        .logs
        .iter()
        .map(|position| nodes[*position].id.as_str())
        .collect();
    let context_ids: HashSet<&str> = context
        .iter()
        .map(|position| nodes[*position].id.as_str())
        .collect();
    let keep: HashSet<String> = decision
        .keep
        .into_iter()
        .map(|item| item.into_parts().0)
        .collect();
    let mut delete: Vec<(String, String)> = Vec::new();
    for item in decision.delete {
        let (id, reason) = item.into_parts();
        if log_ids.contains(id.as_str())
            && !keep.contains(&id)
            && !delete.iter().any(|(known, _)| *known == id)
        {
            delete.push((id, reason));
        }
    }
    let deleting: HashSet<&str> = delete.iter().map(|(id, _)| id.as_str()).collect();
    let first = &nodes[batch.logs[0]];
    let workspace_id = match first.scope_type {
        MemoryScopeType::Workspace => first.scope_id.clone(),
        _ => None,
    };
    let mut weights = HashMap::new();
    // 被提炼的日志 → 新经历 ID（删除前把日志的关联改挂过去）。
    let mut distilled_into: HashMap<String, String> = HashMap::new();
    let mut lines: Vec<String> = Vec::new();
    for (index, experience) in decision.experiences.into_iter().enumerate() {
        let title = experience.title.trim().to_string();
        let summary = experience.summary.trim().to_string();
        let mut sources: Vec<String> = Vec::new();
        for id in &experience.source_ids {
            let id = id.trim();
            if log_ids.contains(id) && !sources.iter().any(|known| known == id) {
                sources.push(id.to_string());
            }
        }
        if title.is_empty() || summary.is_empty() || sources.is_empty() {
            continue;
        }
        let memory_type = experience
            .memory_type
            .as_deref()
            .and_then(parse_memory_type)
            .unwrap_or(MemoryCognitiveType::Factual);
        let importance = experience
            .importance
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(0.05, 1.0) as f32)
            .unwrap_or(0.6);
        let draft = ManualMemoryDraft {
            memory_type: memory_type.clone(),
            title: title.clone(),
            summary: summary.clone(),
            keywords: normalize_keywords(&experience.keywords),
            importance,
            workspace_id: workspace_id.clone(),
            ..Default::default()
        };
        let written = upsert_with_journal(
            provider,
            options,
            "distill",
            None,
            draft,
            &format!("提炼自工具日志：{}", sources.join(",")),
        )
        .await?;
        let new_id = match written {
            Some(node) => node.id,
            None => format!("dry-run-distill-{}-{index}", report.distill_batches),
        };
        report.distill_created += 1;
        // 关联：新经历 → 保留下来的依据日志（learned_from）、→ 相关上下文（related_to）。
        let mut targets: Vec<(String, MemoryRelationKind, &str)> = Vec::new();
        for source in &sources {
            if deleting.contains(source.as_str()) {
                distilled_into
                    .entry(source.clone())
                    .or_insert_with(|| new_id.clone());
            } else {
                targets.push((
                    source.clone(),
                    MemoryRelationKind::LearnedFrom,
                    "提炼自工具日志",
                ));
            }
        }
        for related in &experience.related_ids {
            let related = related.trim();
            if context_ids.contains(related) {
                targets.push((
                    related.to_string(),
                    MemoryRelationKind::RelatedTo,
                    "同一时间段的经历",
                ));
            }
        }
        for (to, kind, note) in targets {
            let draft = MemoryRelationDraft {
                from_node_id: new_id.clone(),
                to_node_id: to,
                relation_kind: kind,
                weight: 0.6,
                note: Some(note.to_string()),
                ..Default::default()
            };
            upsert_relation_max(provider, draft, &mut weights, options).await?;
            report.distill_relations += 1;
        }
        lines.push(format!(
            "\n      + {}「{}」← {} 条日志：{}",
            new_id,
            one_line(&title, 40),
            sources.len(),
            one_line(&summary, 80)
        ));
    }
    // 删除日志：被提炼的日志先把已有关联改挂到新经历。
    let by_id: HashMap<&str, &MemoryNode> = batch
        .logs
        .iter()
        .map(|position| (nodes[*position].id.as_str(), &nodes[*position]))
        .collect();
    for (id, reason) in &delete {
        // 先读出删除前的关联（快照与改挂都用这一份；只读，试运行也读取）。
        let relations = provider().list_relations(id.clone()).await;
        if let Some(new_id) = distilled_into.get(id)
            && !dry_run
            && !relations.is_empty()
        {
            let group: HashSet<String> = [new_id.clone(), id.clone()].into_iter().collect();
            report.distill_relations +=
                relink_relations(provider, new_id, &group, &relations, &mut weights, options)
                    .await?;
        }
        if let Some(node) = by_id.get(id.as_str()) {
            delete_with_journal(
                provider,
                options,
                node,
                "distill",
                &format!("LLM 判定删除工具日志：{reason}"),
                distilled_into.get(id).map(String::as_str),
                Some(&relations),
            )
            .await?;
        }
        removed.insert(id.clone());
        report.distill_deleted += 1;
    }
    let kept = batch.logs.len() - delete.len();
    report.distill_kept += kept;
    let mut detail = format!(
        "distill {}（{} ~ {}）：{} 条日志 → 新经历 {}，删除 {}，保留 {}{}",
        batch.scope,
        first.created_at.get(..16).unwrap_or(&first.created_at),
        nodes[*batch.logs.last().unwrap_or(&batch.logs[0])]
            .created_at
            .get(11..16)
            .unwrap_or(""),
        batch.logs.len(),
        lines.len(),
        delete.len(),
        kept,
        dry_run_tag(options)
    );
    for line in lines {
        detail.push_str(&line);
    }
    if let Some((_, reason)) = delete.iter().find(|(_, reason)| !reason.trim().is_empty()) {
        let _ = write!(detail, "\n      删除理由示例：{}", one_line(reason, 100));
    }
    report.actions.push(detail);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_memory::MemoryStatus;

    fn node(id: &str, kind: MemoryKind, title: &str, created_at: &str) -> MemoryNode {
        MemoryNode {
            id: id.to_string(),
            kind,
            memory_type: MemoryCognitiveType::Factual,
            scope_type: MemoryScopeType::Workspace,
            scope_id: Some("ws".to_string()),
            title: title.to_string(),
            summary: "调用插件 fs 的 file_write：成功".to_string(),
            keywords: Vec::new(),
            importance: 0.5,
            confidence: 0.0,
            status: MemoryStatus::Active,
            source: None,
            usage_count: 0,
            last_used_at: None,
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
        }
    }

    #[test]
    fn tool_log_detection_requires_evidence_and_plain_tool_name() {
        let at = "2025-01-01 10:00:00";
        assert!(is_tool_log(&node(
            "a",
            MemoryKind::Evidence,
            "fs__file_write 产物",
            at
        )));
        assert!(is_tool_log(&node(
            "b",
            MemoryKind::Evidence,
            "Bash2 产物",
            at
        )));
        assert!(!is_tool_log(&node(
            "c",
            MemoryKind::Episode,
            "fs__file_write 产物",
            at
        )));
        assert!(!is_tool_log(&node(
            "d",
            MemoryKind::Evidence,
            "发布说明 产物",
            at
        )));
        assert!(!is_tool_log(&node("e", MemoryKind::Evidence, " 产物", at)));
        assert!(!is_tool_log(&node(
            "f",
            MemoryKind::Evidence,
            "fs-write 产物",
            at
        )));
    }

    #[test]
    fn distill_batches_split_by_gap_and_size() {
        let nodes = vec![
            node("a", MemoryKind::Evidence, "t 产物", "2025-01-01 10:00:00"),
            node("b", MemoryKind::Evidence, "t 产物", "2025-01-01 10:20:00"),
            node("c", MemoryKind::Evidence, "t 产物", "2025-01-01 10:45:00"),
            // 间隔 31 分钟：新时间段。
            node("d", MemoryKind::Evidence, "t 产物", "2025-01-01 11:16:00.5"),
            node("e", MemoryKind::Evidence, "t 产物", "2025-01-01 11:17:00"),
        ];
        let logs: Vec<usize> = (0..nodes.len()).rev().collect();
        let batches = plan_distill_batches(&nodes, &logs, 30, 2);
        let ids: Vec<Vec<&str>> = batches
            .iter()
            .map(|batch| batch.logs.iter().map(|p| nodes[*p].id.as_str()).collect())
            .collect();
        assert_eq!(ids, vec![vec!["a", "b"], vec!["c"], vec!["d", "e"]]);
    }

    #[test]
    fn dedup_primary_prefers_decision_and_common_title() {
        let mut nodes = vec![
            node("a", MemoryKind::Episode, "标题甲", "2025-01-01 10:00:00"),
            node("b", MemoryKind::Decision, "标题乙", "2025-01-01 09:00:00"),
            node("c", MemoryKind::Episode, "标题甲", "2025-01-01 11:00:00"),
        ];
        nodes[0].importance = 0.9;
        let members = vec![0, 1, 2];
        let primary = pick_primary(&nodes, &members);
        assert_eq!(nodes[primary].id, "b", "kind 冲突时优先决策");
        assert_eq!(most_common_title(&nodes, &members, primary), "标题甲");
    }

    #[test]
    fn dedup_key_requires_same_title_for_entities() {
        let at = "2025-01-01 10:00:00";
        let vllm = node("a", MemoryKind::Entity, "vLLM", at);
        let sglang = node("b", MemoryKind::Entity, "SGLang", at);
        let upper = node("c", MemoryKind::Entity, "Tiangong ", at);
        let lower = node("d", MemoryKind::Entity, "tiangong", at);
        assert_ne!(dedup_key(&vllm), dedup_key(&sglang), "不同实体不应归一");
        assert_eq!(dedup_key(&upper), dedup_key(&lower), "同名实体忽略大小写");
        let episode = node("e", MemoryKind::Episode, "修复问题", at);
        let decision = node("f", MemoryKind::Decision, "决策：修复问题", at);
        assert_eq!(
            dedup_key(&episode),
            dedup_key(&decision),
            "经历与决策只看摘要"
        );
        let mut other_scope = episode.clone();
        other_scope.scope_id = Some("ws-2".to_string());
        assert_ne!(dedup_key(&episode), dedup_key(&other_scope));
    }

    #[test]
    fn distill_context_window_and_limit() {
        let mut nodes = vec![node(
            "log",
            MemoryKind::Evidence,
            "t 产物",
            "2025-01-01 10:00:00",
        )];
        nodes.push(node(
            "near",
            MemoryKind::Episode,
            "附近",
            "2025-01-01 10:25:00",
        ));
        nodes.push(node(
            "far",
            MemoryKind::Episode,
            "很远",
            "2025-01-01 11:00:00",
        ));
        let context = pick_context(&nodes, &[1, 2], &[0], 30);
        assert_eq!(context, vec![1]);
    }
}
