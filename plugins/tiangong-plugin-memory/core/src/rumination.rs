//! 反刍层（Rumination）
//!
//! Phase B：MicroRumination — 每个 turn 结束后写入 Episode + 更新 Session Injection。
//! Phase C：MesoRumination — 会话结束时提炼 Entity/Decision + 更新 Workspace Injection。
//! Phase D：MetaRumination — 定期过时检测、归档、Profile 更新。

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use crate::command::InjectionLevel;
use crate::llm_metrics::{log_memory_llm_call, log_memory_llm_failure};
use crate::store::MemoryStore;
use crate::types::{
    Decision, EnhancedTurnResult, Entity, EntityType, Episode, MemoryListQuery, MemoryNode,
    MemoryRelationDraft, MemoryRelationKind, MemoryStatus, TurnResult,
};
use crate::writer;
use tiangong_llm::{LlmEndpointConfig, complete_text_with_usage};

const MESO_RUMINATION_SYSTEM: &str = "\
你是独立记忆系统的 MesoRumination 结构化提炼器。根据近期 Episode 提炼可长期复用的工作区 Entity 和 Decision。

要求：
- 只输出 JSON 对象，不要 Markdown，不要解释。
- entities 只保留项目、模块、文档、服务、模型 Provider、Skill 等稳定对象，不要把普通动词或泛词当实体。
- decisions 只保留明确的架构/实现/产品取舍，必须包含 chosen 和至少一个 episode_id。
- 所有 related_episode_ids / episode_ids 必须来自输入 Episode 的 id。
- description/context 只写未来回忆有价值的信息，避免复述输入全文。
- entity_type 可取 project、repository、server、skill、provider、document、module。
- importance 为 0.0 到 1.0。

JSON 格式：
{
  \"entities\": [
    {
      \"name\": \"...\",
      \"entity_type\": \"project\",
      \"description\": \"...\",
      \"file_path\": null,
      \"related_episode_ids\": [\"...\"],
      \"importance\": 0.6
    }
  ],
  \"decisions\": [
    {
      \"title\": \"...\",
      \"context\": \"...\",
      \"alternatives\": [\"...\"],
      \"chosen\": \"...\",
      \"reasons\": [\"...\"],
      \"episode_ids\": [\"...\"]
    }
  ]
}";

pub(crate) struct MesoMemorySet {
    entities: Vec<Entity>,
    decisions: Vec<Decision>,
    used_llm: bool,
}

#[derive(Debug, Default, serde::Deserialize)]
struct MesoExtraction {
    #[serde(default)]
    entities: Vec<MesoEntityExtraction>,
    #[serde(default)]
    decisions: Vec<MesoDecisionExtraction>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct MesoEntityExtraction {
    name: Option<String>,
    entity_type: Option<String>,
    description: Option<String>,
    file_path: Option<String>,
    #[serde(default, alias = "episode_ids")]
    related_episode_ids: Vec<String>,
    importance: Option<f32>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct MesoDecisionExtraction {
    title: Option<String>,
    context: Option<String>,
    #[serde(default)]
    alternatives: Vec<String>,
    chosen: Option<String>,
    #[serde(default)]
    reasons: Vec<String>,
    #[serde(default)]
    episode_ids: Vec<String>,
}

/// Micro 反刍：turn 结束后处理
///
/// 1. 若 turn 包含工具调用，提取 Episode 并写入 SQLite + Tantivy
/// 2. 更新 Session Injection（Phase C 实现）
pub(crate) async fn process_micro(
    store: &mut MemoryStore,
    turn_result: &TurnResult,
    workspace_id: Option<&str>,
    model: Option<&LlmEndpointConfig>,
) -> Result<()> {
    // 1. 提取并写入 Episode（嵌套 if let 用 let-chains 合并以满足 clippy）
    if let Some(episode) = writer::extract_episode_with_model(turn_result, model).await {
        let episode_for_links = episode.clone();
        match store.write_episode(episode, workspace_id).await {
            Ok(()) => {
                store
                    .link_episode_to_related_recent(&episode_for_links, workspace_id)
                    .unwrap_or_else(|e| {
                        tracing::warn!("Micro 反刍：自动关联 Episode 失败: {}", e);
                        0
                    });
            }
            Err(e) => {
                tracing::warn!("Micro 反刍：写入 Episode 失败: {}", e);
            }
        }
    }

    // 2. 更新 Session Injection（最近 3 条 Episode 摘要）
    let recent = store.recent_episodes_for_session(workspace_id, &turn_result.session_id, 3);
    if !recent.is_empty() {
        let content = build_session_injection(&recent, &turn_result.session_id);
        store
            .update_injection(InjectionLevel::Session, &turn_result.session_id, &content)
            .unwrap_or_else(|e| {
                tracing::warn!("Micro 反刍：更新 Session Injection 失败: {}", e);
            });
    }

    Ok(())
}

/// 构建 Session 级注入内容（最近几条 Episode 摘要）
fn build_session_injection(episodes: &[Episode], session_id: &str) -> String {
    let now = chrono::Local::now().naive_local();
    let items = episodes
        .iter()
        .map(|ep| format!("- {}: {}", ep.title, ep.summary))
        .collect::<Vec<_>>()
        .join("\n");
    format!("# Session Memory ({session_id})\n更新时间: {now}\n\n## 本会话近期活动\n{items}\n")
}

/// 在独立反刍 worker 中执行增强版 Micro 的耗时模型提取。
pub(crate) async fn extract_enhanced_micro(
    enhanced: &EnhancedTurnResult,
    model: Option<&LlmEndpointConfig>,
) -> crate::types::ExtractionOutput {
    tracing::debug!(
        candidate_count = enhanced.memory_candidates.len(),
        "增强版 Micro 反刍：后台执行多类型提取"
    );
    writer::extract_multi_type_memories_with_model(enhanced, model).await
}

/// 提交已完成模型提取的增强版 Micro 结果。
///
/// Actor 只在这里串行执行去重、写入、关联和 Injection 更新，避免耗时模型调用
/// 阻塞下一轮 LoadInjection / Recall。
pub(crate) async fn apply_enhanced_micro(
    store: &mut MemoryStore,
    enhanced: &EnhancedTurnResult,
    extraction: &crate::types::ExtractionOutput,
    workspace_id: Option<&str>,
) -> Result<()> {
    // 1. 去重写入 Episode
    let mut written_episode_ids = Vec::new();
    for episode in &extraction.episodes {
        if let Some(existing_id) = check_episode_dedup(store, &episode.title, &episode.keywords) {
            tracing::debug!(
                existing_id = %existing_id,
                title = %episode.title,
                "Episode 去重：更新已有记忆"
            );
            if let Err(e) = store
                .update_episode_summary(&existing_id, &episode.summary, &episode.keywords)
                .await
            {
                tracing::warn!("Episode 去重更新失败: {}", e);
            }
            written_episode_ids.push(existing_id);
        } else if let Err(e) = store.write_episode(episode.clone(), workspace_id).await {
            tracing::warn!("Episode 写入失败: {}", e);
        } else {
            written_episode_ids.push(episode.id.clone());
        }
    }

    // 2. 写入 Entity
    let mut written_entity_ids = Vec::new();
    for entity in &extraction.entities {
        match store.upsert_entity(entity.clone(), workspace_id) {
            Ok(()) => written_entity_ids.push(entity.id.clone()),
            Err(e) => tracing::warn!("Entity 写入失败: {}", e),
        }
    }

    // 3. 写入 Decision
    let mut written_decision_ids = Vec::new();
    for decision in &extraction.decisions {
        match store.upsert_decision(decision.clone(), workspace_id) {
            Ok(()) => written_decision_ids.push(decision.id.clone()),
            Err(e) => tracing::warn!("Decision 写入失败: {}", e),
        }
    }

    // 5. 写入 Evidence（补齐之前缺失的持久化）
    let mut written_evidence_ids = Vec::new();
    for evidence in &extraction.evidences {
        match store.write_evidence(evidence.clone(), workspace_id) {
            Ok(id) => written_evidence_ids.push(id),
            Err(e) => tracing::warn!("Evidence 写入失败: {}", e),
        }
    }

    // 6. 跨类型关联
    link_written_memories(
        store,
        &written_episode_ids,
        &written_entity_ids,
        &written_decision_ids,
    );

    // 6. 更新 Session Injection
    let recent = store.recent_episodes_for_session(workspace_id, &enhanced.session_id, 3);
    if !recent.is_empty() {
        let content = build_session_injection(&recent, &enhanced.session_id);
        store
            .update_injection(InjectionLevel::Session, &enhanced.session_id, &content)
            .unwrap_or_else(|e| {
                tracing::warn!("增强版 Micro 反刍：更新 Session Injection 失败: {}", e);
            });
    }

    Ok(())
}

/// Episode 去重检查。
///
/// 关键词重叠 ≥ 0.7 + 标题相似度 > 0.6 → 认为重复，返回已有 ID。
fn check_episode_dedup(store: &MemoryStore, title: &str, keywords: &[String]) -> Option<String> {
    if keywords.is_empty() {
        return None;
    }
    let candidates = store.search(title, 5);
    for hit in candidates {
        let keyword_overlap = compute_keyword_overlap(keywords, &hit.title);
        let title_sim = compute_title_similarity(title, &hit.title);
        if keyword_overlap >= 0.7 && title_sim > 0.6 {
            return Some(hit.node_id);
        }
    }
    None
}

fn compute_keyword_overlap(query_keywords: &[String], target_text: &str) -> f64 {
    if query_keywords.is_empty() {
        return 0.0;
    }
    let lower = target_text.to_ascii_lowercase();
    let matched = query_keywords
        .iter()
        .filter(|kw| lower.contains(&kw.to_ascii_lowercase()))
        .count();
    matched as f64 / query_keywords.len() as f64
}

fn compute_title_similarity(a: &str, b: &str) -> f64 {
    let a_lower = a.to_ascii_lowercase();
    let b_lower = b.to_ascii_lowercase();
    if a_lower == b_lower {
        return 1.0;
    }
    let a_chars: std::collections::HashSet<char> = a_lower.chars().collect();
    let b_chars: std::collections::HashSet<char> = b_lower.chars().collect();
    if a_chars.is_empty() && b_chars.is_empty() {
        return 1.0;
    }
    let intersection = a_chars.intersection(&b_chars).count();
    let union = a_chars.union(&b_chars).count();
    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
}

/// 跨类型关联写入。
///
/// - Episode → Entity: BelongsTo
/// - Decision → Episode: LearnedFrom
fn link_written_memories(
    store: &mut MemoryStore,
    episode_ids: &[String],
    entity_ids: &[String],
    decision_ids: &[String],
) {
    for entity_id in entity_ids {
        for episode_id in episode_ids {
            if let Err(e) = store.upsert_relation(MemoryRelationDraft {
                from_node_id: episode_id.clone(),
                to_node_id: entity_id.clone(),
                relation_kind: MemoryRelationKind::BelongsTo,
                weight: 0.8,
                note: None,
                ..Default::default()
            }) {
                tracing::warn!("Episode→Entity 关联写入失败: {}", e);
            }
        }
    }
    for decision_id in decision_ids {
        for episode_id in episode_ids {
            if let Err(e) = store.upsert_relation(MemoryRelationDraft {
                from_node_id: decision_id.clone(),
                to_node_id: episode_id.clone(),
                relation_kind: MemoryRelationKind::LearnedFrom,
                weight: 0.8,
                note: None,
                ..Default::default()
            }) {
                tracing::warn!("Decision→Episode 关联写入失败: {}", e);
            }
        }
    }
}

/// Meso 反刍（Phase C）：会话结束时调用
///
/// 从最近的 Episodes 中提炼关键词/实体，更新 Workspace Injection 文件。
/// Meso 反刍的后台提取任务：Actor 主循环读取的 store 快照。
///
/// 快照跨线程交给反刍 worker 做 LLM 提炼——Actor 主循环不得内联长任务，
/// 否则会阻塞 micro 反刍的入队确认，进而拉长 WASM 收尾的实例锁持有窗口。
pub(crate) struct MesoJob {
    pub(crate) workspace_id: String,
    pub(crate) model: Option<LlmEndpointConfig>,
    pub(crate) full_episodes: Vec<Episode>,
    pub(crate) top_kws: Vec<String>,
    pub(crate) recent_titles: Vec<String>,
    pub(crate) existing_entities: Vec<Entity>,
    pub(crate) existing_decisions: Vec<Decision>,
}

/// 读取 Meso 提炼所需的 store 快照（纯读操作，可在 Actor 主循环内执行）。
///
/// 无可用 Episode 时返回 None（跳过本次反刍）。
pub(crate) fn prepare_meso_job(
    store: &MemoryStore,
    workspace_id: &str,
    model: Option<LlmEndpointConfig>,
) -> Option<MesoJob> {
    let full_episodes = store.recent_episodes(Some(workspace_id), 30);
    let episodes = full_episodes
        .iter()
        .map(|episode| (episode.title.clone(), episode.keywords.clone()))
        .collect::<Vec<_>>();
    if episodes.is_empty() {
        tracing::debug!("Meso 反刍：无可用 Episode，跳过");
        return None;
    }

    // 提炼高频关键词（不调用 LLM，纯统计）
    let mut kw_count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (_, kws) in &episodes {
        for kw in kws {
            if !kw.is_empty() {
                *kw_count.entry(kw.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut top_keywords: Vec<(&String, &usize)> = kw_count.iter().collect();
    top_keywords.sort_by(|a, b| b.1.cmp(a.1));
    let top_kws: Vec<String> = top_keywords
        .into_iter()
        .take(15)
        .map(|(k, _)| k.clone())
        .collect();

    let recent_titles: Vec<String> = episodes
        .iter()
        .take(5)
        .map(|(title, _)| format!("- {title}"))
        .collect();
    Some(MesoJob {
        workspace_id: workspace_id.to_string(),
        model,
        full_episodes,
        top_kws,
        recent_titles,
        existing_entities: store.list_entities(Some(workspace_id)),
        existing_decisions: store.list_decisions(Some(workspace_id)),
    })
}

/// Meso 反刍的提取结果（worker 产出，回到 Actor 落库）。
pub(crate) struct MesoOutcome {
    pub(crate) workspace_id: String,
    pub(crate) memories: MesoMemorySet,
    pub(crate) top_kws: Vec<String>,
    pub(crate) recent_titles: Vec<String>,
}

/// Meso 提炼（LLM 段）：在反刍 worker 上执行，不持有 store。
pub(crate) async fn extract_meso(job: MesoJob) -> MesoOutcome {
    let MesoJob {
        workspace_id,
        model,
        full_episodes,
        top_kws,
        recent_titles,
        existing_entities,
        existing_decisions,
    } = job;
    let MesoMemorySet {
        ref entities,
        ref decisions,
        used_llm,
    } = extract_meso_memories(
        model.as_ref(),
        &full_episodes,
        &top_kws,
        &existing_entities,
        &existing_decisions,
    )
    .await;
    tracing::debug!(
        workspace_id = %workspace_id,
        used_llm,
        episode_count = full_episodes.len(),
        entity_count = entities.len(),
        decision_count = decisions.len(),
        "Meso 反刍结构化提炼完成"
    );
    MesoOutcome {
        workspace_id,
        memories: MesoMemorySet {
            entities: entities.clone(),
            decisions: decisions.clone(),
            used_llm,
        },
        top_kws,
        recent_titles,
    }
}

/// Meso 落库（Actor 串行执行）：写入 Entity/Decision、关系与 Workspace Injection。
pub(crate) async fn apply_meso(store: &mut MemoryStore, outcome: MesoOutcome) -> Result<()> {
    let MesoOutcome {
        workspace_id,
        memories:
            MesoMemorySet {
                entities,
                decisions,
                used_llm,
            },
        top_kws,
        recent_titles,
    } = outcome;
    for entity in &entities {
        store.upsert_entity(entity.clone(), Some(&workspace_id))?;
        link_entity_to_source_episodes(store, entity);
    }
    for decision in &decisions {
        store.upsert_decision(decision.clone(), Some(&workspace_id))?;
        link_decision_to_source_episodes(store, decision);
    }

    let now = chrono::Local::now().naive_local();
    let content = format!(
        "# Workspace Memory Summary\n\
        更新时间: {now}\n\n\
        ## 近期活动（最新 5 条）\n\
        {}\n\n\
        ## 高频关键词\n\
        {}\n\n\
        ## 实体记忆\n\
        {}\n\n\
        ## 决策记忆\n\
        {}\n",
        recent_titles.join("\n"),
        top_kws.join(", "),
        entities
            .iter()
            .take(8)
            .map(|entity| format!("- {}: {}", entity.name, entity.description))
            .collect::<Vec<_>>()
            .join("\n"),
        decisions
            .iter()
            .take(8)
            .map(|decision| format!("- {} -> {}", decision.title, decision.chosen))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    // 写入 Workspace Injection 文件
    store.update_injection(InjectionLevel::Workspace, &workspace_id, &content)?;
    tracing::info!(
        "Meso 反刍完成，workspace={workspace_id}，used_llm={used_llm}，关键词={:?}，entities={}，decisions={}",
        &top_kws[..top_kws.len().min(5)],
        entities.len(),
        decisions.len()
    );

    Ok(())
}

fn link_entity_to_source_episodes(store: &MemoryStore, entity: &Entity) {
    for episode_id in &entity.related_episodes {
        if let Err(err) = store.upsert_relation(MemoryRelationDraft {
            id: None,
            from_node_id: entity.id.clone(),
            to_node_id: episode_id.clone(),
            relation_kind: MemoryRelationKind::LearnedFrom,
            weight: entity.importance.clamp(0.4, 0.9),
            note: Some("自动关联：实体由该 Episode 提炼".to_string()),
        }) {
            tracing::warn!("Meso 反刍：写入 Entity 关系失败: {err}");
        }
    }
}

fn link_decision_to_source_episodes(store: &MemoryStore, decision: &Decision) {
    for episode_id in &decision.episode_ids {
        if let Err(err) = store.upsert_relation(MemoryRelationDraft {
            id: None,
            from_node_id: decision.id.clone(),
            to_node_id: episode_id.clone(),
            relation_kind: MemoryRelationKind::LearnedFrom,
            weight: 0.9,
            note: Some("自动关联：决策由该 Episode 提炼".to_string()),
        }) {
            tracing::warn!("Meso 反刍：写入 Decision 关系失败: {err}");
        }
    }
}

async fn extract_meso_memories(
    model: Option<&LlmEndpointConfig>,
    episodes: &[Episode],
    top_keywords: &[String],
    existing_entities: &[Entity],
    existing_decisions: &[Decision],
) -> MesoMemorySet {
    if let Some(model) = model {
        match extract_meso_memories_with_model(
            model,
            episodes,
            existing_entities,
            existing_decisions,
        )
        .await
        {
            Ok(memories) => return memories,
            Err(err) => log_memory_llm_failure(
                "meso_rumination",
                model,
                &err,
                "MesoRumination LLM 提炼失败，使用规则 fallback",
            ),
        }
    }
    extract_meso_memories_fallback(
        episodes,
        top_keywords,
        existing_entities,
        existing_decisions,
    )
}

async fn extract_meso_memories_with_model(
    model: &LlmEndpointConfig,
    episodes: &[Episode],
    existing_entities: &[Entity],
    existing_decisions: &[Decision],
) -> anyhow::Result<MesoMemorySet> {
    let prompt = build_meso_prompt(episodes);
    let started = Instant::now();
    let (response, usage) =
        complete_text_with_usage(model, MESO_RUMINATION_SYSTEM, &prompt, 1600).await?;
    log_memory_llm_call("meso_rumination", model, started.elapsed(), usage.as_ref());
    let json = extract_json_object(&response).unwrap_or(response.as_str());
    let extraction: MesoExtraction = serde_json::from_str(json)?;
    build_meso_memories_from_extraction(extraction, episodes, existing_entities, existing_decisions)
}

fn extract_meso_memories_fallback(
    episodes: &[Episode],
    top_keywords: &[String],
    existing_entities: &[Entity],
    existing_decisions: &[Decision],
) -> MesoMemorySet {
    MesoMemorySet {
        entities: extract_entities_from_episodes(episodes, top_keywords, existing_entities),
        decisions: extract_decisions_from_episodes(episodes, existing_decisions),
        used_llm: false,
    }
}

fn build_meso_memories_from_extraction(
    extraction: MesoExtraction,
    episodes: &[Episode],
    existing_entities: &[Entity],
    existing_decisions: &[Decision],
) -> anyhow::Result<MesoMemorySet> {
    let known_episode_ids = episodes
        .iter()
        .map(|episode| episode.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut entities = Vec::new();
    for raw in extraction.entities {
        let name = required_text(raw.name, "entity.name")?;
        let description = required_text(raw.description, "entity.description")?;
        let entity_type =
            parse_entity_type(raw.entity_type.as_deref()).unwrap_or_else(|| classify_entity(&name));
        let related_episode_ids = validate_episode_ids(
            raw.related_episode_ids,
            &known_episode_ids,
            "entity.related_episode_ids",
        )?;
        let existing = find_existing_entity(existing_entities, &name, &entity_type);
        let now = chrono::Local::now().naive_local().to_string();
        let mut related = existing
            .map(|entity| entity.related_episodes.clone())
            .unwrap_or_default();
        related.extend(related_episode_ids);
        let related_episodes = dedupe_strings(related);
        entities.push(Entity {
            id: existing
                .map(|entity| entity.id.clone())
                .unwrap_or_else(|| scru128::new().to_string()),
            name,
            entity_type,
            description: compact_text(&description, 700),
            file_path: raw.file_path.filter(|item| !item.trim().is_empty()),
            related_episodes,
            importance: raw.importance.unwrap_or(0.6).clamp(0.0, 1.0),
            created_at: existing
                .map(|entity| entity.created_at.clone())
                .unwrap_or_else(|| now.clone()),
            updated_at: now,
        });
    }

    let mut decisions = Vec::new();
    for raw in extraction.decisions {
        let title = required_text(raw.title, "decision.title")?;
        let context = required_text(raw.context, "decision.context")?;
        let chosen = required_text(raw.chosen, "decision.chosen")?;
        let episode_ids =
            validate_episode_ids(raw.episode_ids, &known_episode_ids, "decision.episode_ids")?;
        let existing = find_existing_decision_by_parts(existing_decisions, &title, &episode_ids);
        let now = chrono::Local::now().naive_local().to_string();
        decisions.push(Decision {
            id: existing
                .map(|decision| decision.id.clone())
                .unwrap_or_else(|| scru128::new().to_string()),
            title: if title.starts_with("决策：") {
                title
            } else {
                format!("决策：{title}")
            },
            context: compact_text(&context, 1200),
            alternatives: dedupe_strings(raw.alternatives),
            chosen: compact_text(&chosen, 300),
            reasons: dedupe_strings(raw.reasons),
            episode_ids,
            created_at: existing
                .map(|decision| decision.created_at.clone())
                .unwrap_or(now),
        });
    }

    if entities.is_empty() && decisions.is_empty() {
        anyhow::bail!("LLM Meso 输出未包含有效 Entity 或 Decision");
    }

    Ok(MesoMemorySet {
        entities,
        decisions,
        used_llm: true,
    })
}

fn extract_entities_from_episodes(
    episodes: &[Episode],
    top_keywords: &[String],
    existing_entities: &[Entity],
) -> Vec<Entity> {
    let mut entities = Vec::new();
    for keyword in top_keywords
        .iter()
        .map(|keyword| keyword.trim())
        .filter(|keyword| is_entity_keyword(keyword))
        .take(10)
    {
        let entity_type = classify_entity(keyword);
        let related = episodes
            .iter()
            .filter(|episode| {
                episode
                    .keywords
                    .iter()
                    .any(|item| item.eq_ignore_ascii_case(keyword))
                    || episode.title.contains(keyword)
                    || episode.summary.contains(keyword)
            })
            .map(|episode| episode.id.clone())
            .collect::<Vec<_>>();
        if related.is_empty() {
            continue;
        }
        let now = chrono::Local::now().naive_local().to_string();
        let existing = find_existing_entity(existing_entities, keyword, &entity_type);
        let mut related_episodes = existing
            .map(|entity| entity.related_episodes.clone())
            .unwrap_or_default();
        related_episodes.extend(related);
        related_episodes = dedupe_strings(related_episodes);
        entities.push(Entity {
            id: existing
                .map(|entity| entity.id.clone())
                .unwrap_or_else(|| scru128::new().to_string()),
            name: keyword.to_string(),
            entity_type,
            description: format!(
                "从近期 {} 条 Episode 中提炼出的工作区实体：{}",
                related_episodes.len(),
                keyword
            ),
            file_path: extract_entity_path(keyword, episodes),
            related_episodes,
            importance: 0.5,
            created_at: existing
                .map(|entity| entity.created_at.clone())
                .unwrap_or_else(|| now.clone()),
            updated_at: now,
        });
    }
    entities
}

fn extract_decisions_from_episodes(
    episodes: &[Episode],
    existing_decisions: &[Decision],
) -> Vec<Decision> {
    episodes
        .iter()
        .filter(|episode| contains_decision_signal(&episode.title, &episode.summary))
        .take(10)
        .map(|episode| {
            let now = chrono::Local::now().naive_local().to_string();
            let existing = find_existing_decision(existing_decisions, episode);
            Decision {
                id: existing
                    .map(|decision| decision.id.clone())
                    .unwrap_or_else(|| scru128::new().to_string()),
                title: format!("决策：{}", episode.title),
                context: episode.summary.clone(),
                alternatives: extract_alternatives(&episode.summary),
                chosen: infer_chosen(&episode.summary).unwrap_or_else(|| episode.title.clone()),
                reasons: episode.keywords.clone(),
                episode_ids: vec![episode.id.clone()],
                created_at: existing
                    .map(|decision| decision.created_at.clone())
                    .unwrap_or(now),
            }
        })
        .collect()
}

fn find_existing_entity<'a>(
    existing_entities: &'a [Entity],
    name: &str,
    entity_type: &EntityType,
) -> Option<&'a Entity> {
    existing_entities.iter().find(|entity| {
        entity.name.eq_ignore_ascii_case(name)
            && std::mem::discriminant(&entity.entity_type) == std::mem::discriminant(entity_type)
    })
}

fn find_existing_decision<'a>(
    existing_decisions: &'a [Decision],
    episode: &Episode,
) -> Option<&'a Decision> {
    let title = format!("决策：{}", episode.title);
    find_existing_decision_by_parts(
        existing_decisions,
        &title,
        std::slice::from_ref(&episode.id),
    )
}

fn find_existing_decision_by_parts<'a>(
    existing_decisions: &'a [Decision],
    title: &str,
    episode_ids: &[String],
) -> Option<&'a Decision> {
    existing_decisions.iter().find(|decision| {
        decision
            .episode_ids
            .iter()
            .any(|id| episode_ids.iter().any(|episode_id| episode_id == id))
            || decision.title == title
    })
}

fn is_entity_keyword(keyword: &str) -> bool {
    let trimmed = keyword.trim();
    trimmed.chars().count() >= 3
        && !matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "the" | "and" | "for" | "with" | "用户请求" | "结果摘要" | "工具调用" | "结构化产物"
        )
}

fn classify_entity(keyword: &str) -> EntityType {
    let lower = keyword.to_ascii_lowercase();
    if lower.ends_with(".rs") || lower.contains("::") {
        EntityType::Module
    } else if lower.ends_with(".md") || lower.ends_with(".json") || lower.ends_with(".toml") {
        EntityType::Document
    } else if lower.contains("server") || lower.contains("ipc") {
        EntityType::Server
    } else if lower.contains("skill") {
        EntityType::Skill
    } else if lower.contains("model") || lower.contains("provider") || lower.contains("llm") {
        EntityType::Provider
    } else {
        EntityType::Project
    }
}

fn parse_entity_type(raw: Option<&str>) -> Option<EntityType> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "project" => Some(EntityType::Project),
        "repository" | "repo" => Some(EntityType::Repository),
        "server" => Some(EntityType::Server),
        "skill" => Some(EntityType::Skill),
        "provider" => Some(EntityType::Provider),
        "document" | "doc" => Some(EntityType::Document),
        "module" => Some(EntityType::Module),
        _ => None,
    }
}

fn extract_entity_path(keyword: &str, episodes: &[Episode]) -> Option<String> {
    let lower = keyword.to_ascii_lowercase();
    if !(lower.contains('/') || lower.contains(".rs") || lower.contains(".md")) {
        return None;
    }
    episodes
        .iter()
        .flat_map(|episode| episode.summary.split_whitespace())
        .find(|token| token.contains(keyword))
        .map(|token| {
            token
                .trim_matches(|c: char| matches!(c, '"' | '\'' | ')' | ']' | '}' | ',' | '，'))
                .to_string()
        })
}

fn contains_decision_signal(title: &str, summary: &str) -> bool {
    let text = format!("{title}\n{summary}").to_ascii_lowercase();
    [
        "decide",
        "decided",
        "choose",
        "chose",
        "selected",
        "adopted",
        "instead of",
        "rather than",
        "方案",
        "选择",
        "采用",
        "决定",
        "取舍",
        "替代",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn extract_alternatives(summary: &str) -> Vec<String> {
    let markers = [
        " vs ",
        " versus ",
        " or ",
        " instead of ",
        "而不是",
        "还是",
        "对比",
    ];
    if !markers.iter().any(|marker| summary.contains(marker)) {
        return Vec::new();
    }
    summary
        .split([',', '，', ';', '；', '\n'])
        .map(str::trim)
        .filter(|item| item.chars().count() >= 3)
        .take(4)
        .map(String::from)
        .collect()
}

fn infer_chosen(summary: &str) -> Option<String> {
    for marker in [
        "choose ",
        "chose ",
        "selected ",
        "adopted ",
        "采用",
        "选择",
        "决定",
    ] {
        if let Some((_, rest)) = summary.split_once(marker) {
            let chosen = rest
                .split([',', '，', ';', '；', '.', '。', '\n'])
                .next()
                .unwrap_or(rest)
                .trim();
            if !chosen.is_empty() {
                return Some(chosen.chars().take(80).collect());
            }
        }
    }
    None
}

fn build_meso_prompt(episodes: &[Episode]) -> String {
    let mut lines = Vec::new();
    lines.push("近期 Episodes:".to_string());
    for episode in episodes.iter().take(30) {
        lines.push(format!(
            "- id={}\n  title={}\n  summary={}\n  outcome={:?}\n  keywords={}\n  tools={}",
            episode.id,
            compact_text(&episode.title, 160),
            compact_text(&episode.summary, 900),
            episode.outcome,
            episode.keywords.join(", "),
            episode.tool_calls.join(", ")
        ));
    }
    lines.join("\n")
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end >= start).then_some(&text[start..=end])
}

fn required_text(raw: Option<String>, field: &str) -> anyhow::Result<String> {
    let value = raw.unwrap_or_default();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        anyhow::bail!("LLM Meso 输出缺少必填字段: {field}");
    }
    Ok(trimmed.to_string())
}

fn validate_episode_ids(
    ids: Vec<String>,
    known_episode_ids: &std::collections::HashSet<&str>,
    field: &str,
) -> anyhow::Result<Vec<String>> {
    let ids = dedupe_strings(ids);
    if ids.is_empty() {
        anyhow::bail!("LLM Meso 输出缺少必填字段: {field}");
    }
    if let Some(unknown) = ids
        .iter()
        .find(|id| !known_episode_ids.contains(id.as_str()))
    {
        anyhow::bail!("LLM Meso 输出包含未知 Episode id: {unknown}");
    }
    Ok(ids)
}

fn compact_text(text: &str, max_chars: usize) -> String {
    let normalized = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    let mut clipped = normalized.chars().take(max_chars).collect::<String>();
    clipped.push_str("...");
    clipped
}

fn dedupe_strings(items: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .filter(|item| seen.insert(item.to_ascii_lowercase()))
        .collect()
}

#[derive(Debug, Default)]
pub(crate) struct MetaRuminationReport {
    pub(crate) checked_nodes: usize,
    pub(crate) checked_paths: usize,
    pub(crate) checked_urls: usize,
    pub(crate) low_activity_archived: usize,
    pub(crate) invalid_reference_archived: usize,
    pub(crate) missing_paths: usize,
    pub(crate) expired_urls: usize,
    pub(crate) project_archived: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetaArchiveReason {
    MissingPath,
    ExpiredUrl,
    ProjectArchived,
}

/// Meta 反刍的后台评估任务：Actor 主循环读取的节点快照。
///
/// 评估内容（在 worker 上执行）：检测低活跃（30 天未使用且 importance < 0.3）
/// 节点、检查文件路径/产物 URL/项目归档标记，归档过时引用节点。
pub(crate) struct MetaJob {
    pub(crate) stale_ids: Vec<String>,
    pub(crate) active_nodes: Vec<MemoryNode>,
}

/// 读取 Meta 评估所需的节点快照（纯读操作，可在 Actor 主循环内执行）。
pub(crate) fn prepare_meta_job(store: &MemoryStore) -> MetaJob {
    let stale_ids = store
        .list_stale_nodes(30, 0.3)
        .into_iter()
        .map(|(node_id, _importance)| node_id)
        .collect();
    let active_nodes = store.list_nodes(&MemoryListQuery {
        status: Some(MemoryStatus::Active),
        limit: 500,
        ..Default::default()
    });
    MetaJob {
        stale_ids,
        active_nodes,
    }
}

/// Meta 评估结果：待归档节点与统计（worker 产出，回到 Actor 归档）。
pub(crate) struct MetaOutcome {
    pub(crate) archive_ids: Vec<String>,
    pub(crate) report: MetaRuminationReport,
}

/// Meta 评估（本地文件/URL 检查，无 LLM）：在反刍 worker 上执行，不持有 store。
pub(crate) fn evaluate_meta(job: MetaJob) -> MetaOutcome {
    tracing::debug!("Meta 反刍开始");
    let MetaJob {
        stale_ids,
        active_nodes,
    } = job;
    let mut report = MetaRuminationReport {
        checked_nodes: active_nodes.len(),
        ..MetaRuminationReport::default()
    };
    report.low_activity_archived = stale_ids.len();
    let mut archived_node_ids = stale_ids.into_iter().collect::<HashSet<_>>();

    for node in active_nodes {
        if archived_node_ids.contains(&node.id) {
            continue;
        }
        let evaluation = evaluate_node_references(&node);
        report.checked_paths += evaluation.checked_paths;
        report.checked_urls += evaluation.checked_urls;

        if let Some(reason) = evaluation.archive_reason {
            match reason {
                MetaArchiveReason::MissingPath => report.missing_paths += 1,
                MetaArchiveReason::ExpiredUrl => report.expired_urls += 1,
                MetaArchiveReason::ProjectArchived => report.project_archived += 1,
            }
            report.invalid_reference_archived += 1;
            archived_node_ids.insert(node.id.clone());
            tracing::info!(
                node_id = %node.id,
                title = %node.title,
                reason = ?reason,
                "Meta 反刍归档过时引用节点"
            );
        }
    }

    MetaOutcome {
        archive_ids: archived_node_ids.into_iter().collect(),
        report,
    }
}

/// Meta 归档（Actor 串行执行）。
pub(crate) async fn apply_meta(store: &mut MemoryStore, outcome: MetaOutcome) {
    let MetaOutcome {
        archive_ids,
        report,
    } = outcome;
    for node_id in &archive_ids {
        store.archive_node(node_id).await;
    }
    tracing::info!(
        checked_nodes = report.checked_nodes,
        checked_paths = report.checked_paths,
        checked_urls = report.checked_urls,
        low_activity_archived = report.low_activity_archived,
        invalid_reference_archived = report.invalid_reference_archived,
        missing_paths = report.missing_paths,
        expired_urls = report.expired_urls,
        project_archived = report.project_archived,
        "Meta 反刍完成"
    );
}

#[derive(Debug, Default)]
struct MetaReferenceEvaluation {
    checked_paths: usize,
    checked_urls: usize,
    archive_reason: Option<MetaArchiveReason>,
}

fn evaluate_node_references(node: &MemoryNode) -> MetaReferenceEvaluation {
    let text = node_reference_text(node);
    let mut evaluation = MetaReferenceEvaluation::default();

    if contains_project_archived_marker(&text) {
        evaluation.archive_reason = Some(MetaArchiveReason::ProjectArchived);
        return evaluation;
    }

    let paths = extract_reference_paths(&text);
    evaluation.checked_paths = paths.len();
    if paths.iter().any(|path| !Path::new(path).exists()) {
        evaluation.archive_reason = Some(MetaArchiveReason::MissingPath);
        return evaluation;
    }

    let urls = extract_reference_urls(&text);
    evaluation.checked_urls = urls.len();
    if urls.iter().any(|url| is_expired_reference_url(url)) {
        evaluation.archive_reason = Some(MetaArchiveReason::ExpiredUrl);
    }

    evaluation
}

fn node_reference_text(node: &MemoryNode) -> String {
    let mut parts = vec![node.title.as_str(), node.summary.as_str()];
    if let Some(source) = node.source.as_deref() {
        parts.push(source);
    }
    let mut text = parts.join("\n");
    if !node.keywords.is_empty() {
        text.push('\n');
        text.push_str(&node.keywords.join(" "));
    }
    text
}

fn contains_project_archived_marker(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("project archived")
        || lower.contains("workspace archived")
        || text.contains("项目归档")
        || text.contains("项目已归档")
        || text.contains("工作区归档")
        || text.contains("工作区已归档")
}

fn extract_reference_paths(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.split_whitespace()
        .filter_map(clean_reference_token)
        .filter(|token| is_probable_path(token))
        .filter(|token| seen.insert(token.to_ascii_lowercase()))
        .map(str::to_string)
        .collect()
}

fn extract_reference_urls(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.split_whitespace()
        .filter_map(clean_reference_token)
        .filter(|token| {
            token.starts_with("http://")
                || token.starts_with("https://")
                || token.starts_with("data:image/")
        })
        .filter(|token| seen.insert(token.to_ascii_lowercase()))
        .map(str::to_string)
        .collect()
}

fn clean_reference_token(token: &str) -> Option<&str> {
    let cleaned = token.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\''
                | '`'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | ','
                | '.'
                | ';'
                | ':'
                | '，'
                | '。'
                | '；'
                | '、'
        )
    });
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn is_probable_path(token: &str) -> bool {
    if token.starts_with("http://")
        || token.starts_with("https://")
        || token.starts_with("data:image/")
    {
        return false;
    }
    if token.starts_with('/') || token.starts_with("./") || token.starts_with("../") {
        return true;
    }
    let lower = token.to_ascii_lowercase();
    (token.contains('/') || token.contains('\\'))
        && [
            ".rs", ".ts", ".tsx", ".js", ".jsx", ".vue", ".md", ".json", ".toml", ".yaml", ".yml",
            ".sql", ".png", ".jpg", ".jpeg", ".webp", ".svg", ".gif", ".mp4", ".mov", ".pdf",
        ]
        .iter()
        .any(|ext| lower.contains(ext))
}

fn is_expired_reference_url(url: &str) -> bool {
    if url.starts_with("data:image/") {
        return false;
    }
    let lower = url.to_ascii_lowercase();
    lower.contains(".invalid")
        || lower.contains("/expired")
        || lower.contains("/404")
        || lower.contains("/410")
        || lower.contains("not-found")
        || lower.contains("missing")
        || lower.contains("gone")
}

#[cfg(test)]
mod tests {
    use super::{
        MesoDecisionExtraction, MesoEntityExtraction, MesoExtraction,
        build_meso_memories_from_extraction, extract_meso_memories_fallback,
    };
    use crate::db::sqlite::test_helpers::open_in_memory;
    use crate::types::{EntityType, Episode, EpisodeOutcome};

    fn make_episode(session_id: &str) -> Episode {
        Episode::new(
            session_id.to_string(),
            "功能开发标题".to_string(),
            "完成了一个功能的开发".to_string(),
            EpisodeOutcome::Success,
            vec!["Rust".to_string(), "异步".to_string(), "测试".to_string()],
            vec!["write_file".to_string()],
            0.7,
        )
    }

    #[test]
    fn process_meso_generates_injection_content_with_keywords() {
        let db = open_in_memory().unwrap();
        // 写入 5 个 Episode，关键词包含 "Rust"
        for i in 0..5 {
            let ep = make_episode(&format!("sess-{i}"));
            db.insert_episode(&ep, Some("ws-test")).unwrap();
        }

        let summaries = db.recent_episode_summaries(30).unwrap();
        assert_eq!(summaries.len(), 5);

        // 统计关键词出现频次
        let mut kw_count = std::collections::HashMap::new();
        for (_, kws) in &summaries {
            for kw in kws {
                *kw_count.entry(kw.clone()).or_insert(0usize) += 1;
            }
        }
        // "Rust" 应出现 5 次（高频词）
        assert_eq!(kw_count.get("Rust").copied().unwrap_or(0), 5);
    }

    #[test]
    fn llm_meso_extraction_builds_valid_structured_memories() {
        let episode = make_episode("sess-llm-meso");
        let extraction = MesoExtraction {
            entities: vec![MesoEntityExtraction {
                name: Some("tiangong-memory".to_string()),
                entity_type: Some("server".to_string()),
                description: Some("Memory 独立服务与检索系统".to_string()),
                file_path: None,
                related_episode_ids: vec![episode.id.clone()],
                importance: Some(0.8),
            }],
            decisions: vec![MesoDecisionExtraction {
                title: Some("使用内置向量索引".to_string()),
                context: Some("为了降低用户启动成本，优先采用内置向量索引".to_string()),
                alternatives: vec!["外部向量服务".to_string(), "内置 flat index".to_string()],
                chosen: Some("内置 flat index".to_string()),
                reasons: vec!["降低启动复杂度".to_string()],
                episode_ids: vec![episode.id.clone()],
            }],
        };

        let memories =
            build_meso_memories_from_extraction(extraction, &[episode], &[], &[]).unwrap();

        assert!(memories.used_llm);
        assert_eq!(memories.entities.len(), 1);
        assert_eq!(memories.decisions.len(), 1);
        assert!(matches!(
            memories.entities[0].entity_type,
            EntityType::Server
        ));
        assert!(memories.decisions[0].title.starts_with("决策："));
    }

    #[test]
    fn invalid_llm_meso_output_is_rejected_so_rule_fallback_can_continue() {
        let episode = make_episode("sess-llm-fallback");
        let extraction = MesoExtraction {
            entities: vec![MesoEntityExtraction {
                name: Some("Rust".to_string()),
                entity_type: Some("project".to_string()),
                description: Some("Rust 相关工作区实体".to_string()),
                file_path: None,
                related_episode_ids: vec!["unknown-episode".to_string()],
                importance: Some(0.5),
            }],
            decisions: Vec::new(),
        };

        let rejected = build_meso_memories_from_extraction(
            extraction,
            std::slice::from_ref(&episode),
            &[],
            &[],
        );
        assert!(rejected.is_err(), "未知 Episode id 应触发严格校验失败");

        let fallback = extract_meso_memories_fallback(
            std::slice::from_ref(&episode),
            &episode.keywords,
            &[],
            &[],
        );
        assert!(!fallback.used_llm);
        assert!(
            fallback
                .entities
                .iter()
                .any(|entity| entity.name.eq_ignore_ascii_case("Rust")),
            "LLM 输出不可用时规则版 Meso 仍应能产生实体记忆"
        );
    }
}
