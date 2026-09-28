//! Episode 写入器
//!
//! 负责从 TurnResult 提取 Episode 并写入 Memory 存储。
//! 优先使用 Memory 模型端点抽取结构化 Episode，失败时回退到规则提取。

use std::time::Instant;

use crate::llm_metrics::{log_memory_llm_call, log_memory_llm_failure};
use crate::types::{
    EnhancedTurnResult, Episode, EpisodeOutcome, Evidence, ExtractionOutput, MemoryCognitiveType,
    TurnResult,
};
use tiangong_llm::{LlmEndpointConfig, complete_text_with_usage};

const EPISODE_WRITER_SYSTEM: &str = "\
你是独立记忆系统的 EpisodeWriter。根据一个 turn 的执行结果提取可长期保存的事件记忆。

要求：
- 只输出 JSON 对象，不要 Markdown，不要解释。
- title 用一句话概括事件，避免超过 40 个中文字符。
- summary 只保留未来回忆需要的信息，必须包含关键产物 URL、文件路径、重要工具结果；避免复述无用过程。
- outcome 可取 success、partial_success、failed、abandoned。
- keywords 只保留 3-10 个检索关键词。
- tool_calls 只保留本 turn 中实际有记忆价值的工具名，不要包含 recall_memory。
- memory_type 必须从 factual、user_preference、user_habit、skill、project_structure、architecture_decision、problem_incident、domain_knowledge 中选择最贴切的一类。
- importance 为 0.0 到 1.0；包含媒体 URL、文件产物、代码变更、关键决策时提高到 0.7 以上。

JSON 格式：
{
  \"title\": \"...\",
  \"summary\": \"...\",
  \"memory_type\": \"factual\",
  \"outcome\": \"success\",
  \"keywords\": [\"...\"],
  \"tool_calls\": [\"...\"],
  \"importance\": 0.8
}";

#[derive(Debug, Default, serde::Deserialize)]
struct EpisodeExtraction {
    title: Option<String>,
    summary: Option<String>,
    memory_type: Option<String>,
    outcome: Option<String>,
    keywords: Option<Vec<String>>,
    tool_calls: Option<Vec<String>>,
    importance: Option<f32>,
}

/// 从 TurnResult 提取 Episode
///
/// 仅在 `turn_result.had_tool_calls == true` 时才生成 Episode。
pub(crate) fn extract_episode(turn_result: &TurnResult) -> Option<Episode> {
    if !turn_result.had_tool_calls && turn_result.artifacts.is_empty() {
        return None;
    }
    Some(extract_episode_fallback(turn_result))
}

pub(crate) async fn extract_episode_with_model(
    turn_result: &TurnResult,
    model: Option<&LlmEndpointConfig>,
) -> Option<Episode> {
    if !turn_result.had_tool_calls && turn_result.artifacts.is_empty() {
        return None;
    }
    let Some(model) = model else {
        return extract_episode(turn_result);
    };

    match extract_episode_with_model_inner(turn_result, model).await {
        Ok(episode) => Some(episode),
        Err(err) => {
            log_memory_llm_failure(
                "episode_writer",
                model,
                &err,
                "EpisodeWriter LLM 抽取失败，使用规则 fallback",
            );
            extract_episode(turn_result)
        }
    }
}

async fn extract_episode_with_model_inner(
    turn_result: &TurnResult,
    model: &LlmEndpointConfig,
) -> anyhow::Result<Episode> {
    let prompt = build_writer_prompt(turn_result);
    let started = Instant::now();
    let (response, usage) =
        complete_text_with_usage(model, EPISODE_WRITER_SYSTEM, &prompt, 900).await?;
    log_memory_llm_call("episode_writer", model, started.elapsed(), usage.as_ref());
    let json = extract_json_object(&response).unwrap_or(response.as_str());
    let extracted: EpisodeExtraction = serde_json::from_str(json)?;
    Ok(build_episode_from_extraction(turn_result, extracted))
}

fn extract_episode_fallback(turn_result: &TurnResult) -> Episode {
    let summary = build_episode_summary(turn_result);
    let title_source = if turn_result.user_input.trim().is_empty() {
        summary.as_str()
    } else {
        turn_result.user_input.as_str()
    };
    let title = derive_title(title_source);
    let outcome = EpisodeOutcome::Success; // Phase B 默认成功；Phase C 可由 LLM 判定
    let importance = estimate_importance(turn_result);
    let keywords = extract_keywords(&summary);
    let memory_type = classify_memory_type(turn_result, &title, &summary, &keywords);

    Episode::new(
        turn_result.session_id.clone(),
        title,
        summary,
        outcome,
        keywords,
        turn_result.tool_calls.clone(),
        importance,
    )
    .with_memory_type(memory_type)
}

fn build_episode_from_extraction(
    turn_result: &TurnResult,
    extracted: EpisodeExtraction,
) -> Episode {
    let fallback = extract_episode_fallback(turn_result);
    let title = extracted
        .title
        .map(|item| compact_text(&item, 80))
        .filter(|item| !item.is_empty())
        .unwrap_or(fallback.title);
    let summary = extracted
        .summary
        .map(|item| compact_text(&item, 1600))
        .filter(|item| !item.is_empty())
        .unwrap_or(fallback.summary);
    let keywords = extracted
        .keywords
        .map(dedupe_strings)
        .filter(|items| !items.is_empty())
        .unwrap_or(fallback.keywords);
    let mut tool_calls = turn_result.tool_calls.clone();
    if let Some(extracted_tool_calls) = extracted.tool_calls {
        tool_calls.extend(extracted_tool_calls);
    }
    let tool_calls = dedupe_strings(
        tool_calls
            .into_iter()
            .filter(|name| name != "recall_memory")
            .collect(),
    );
    let importance = extracted
        .importance
        .map(|value| value.clamp(0.0, 1.0))
        .unwrap_or(fallback.importance);
    let memory_type = extracted
        .memory_type
        .as_deref()
        .and_then(parse_memory_type)
        .unwrap_or(fallback.memory_type);
    Episode::new(
        turn_result.session_id.clone(),
        title,
        summary,
        parse_outcome(extracted.outcome.as_deref()).unwrap_or(fallback.outcome),
        keywords,
        tool_calls,
        importance,
    )
    .with_memory_type(memory_type)
}

fn build_writer_prompt(turn_result: &TurnResult) -> String {
    let mut lines = vec![
        format!("session_id: {}", turn_result.session_id),
        format!("turn_id: {}", turn_result.turn_id),
        format!("had_tool_calls: {}", turn_result.had_tool_calls),
    ];
    if !turn_result.user_input.trim().is_empty() {
        lines.push(format!("user_input:\n{}", turn_result.user_input.trim()));
    }
    if !turn_result.summary.trim().is_empty() {
        lines.push(format!(
            "assistant_summary:\n{}",
            turn_result.summary.trim()
        ));
    }
    if !turn_result.tool_calls.is_empty() {
        lines.push(format!("tool_calls: {}", turn_result.tool_calls.join(", ")));
    }
    if !turn_result.artifacts.is_empty() {
        lines.push("artifacts:".to_string());
        for artifact in &turn_result.artifacts {
            lines.push(format!(
                "- kind={:?} tool={} title={} url={} path={} summary={}",
                artifact.kind,
                artifact.tool_name.as_deref().unwrap_or(""),
                artifact.title.as_deref().unwrap_or(""),
                artifact.url.as_deref().unwrap_or(""),
                artifact.path.as_deref().unwrap_or(""),
                artifact.summary.as_deref().unwrap_or("")
            ));
        }
    }
    lines.join("\n\n")
}

fn parse_outcome(raw: Option<&str>) -> Option<EpisodeOutcome> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "success" => Some(EpisodeOutcome::Success),
        "partial_success" | "partialsuccess" | "partial" => Some(EpisodeOutcome::PartialSuccess),
        "failed" | "failure" | "fail" => Some(EpisodeOutcome::Failed),
        "abandoned" | "cancelled" | "canceled" => Some(EpisodeOutcome::Abandoned),
        _ => None,
    }
}

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

fn classify_memory_type(
    turn_result: &TurnResult,
    title: &str,
    summary: &str,
    keywords: &[String],
) -> MemoryCognitiveType {
    let text = format!(
        "{}\n{}\n{}\n{}",
        title,
        summary,
        turn_result.user_input,
        keywords.join(" ")
    )
    .to_ascii_lowercase();

    if contains_any(
        &text,
        &["偏好", "喜欢", "不喜欢", "倾向", "prefer", "preference"],
    ) {
        return MemoryCognitiveType::UserPreference;
    }
    if contains_any(
        &text,
        &["习惯", "通常", "总是", "每次", "默认", "habit", "usually"],
    ) {
        return MemoryCognitiveType::UserHabit;
    }
    if contains_any(
        &text,
        &["skill", "技能", "工具用法", "命令用法", "操作步骤"],
    ) {
        return MemoryCognitiveType::Skill;
    }
    if contains_any(
        &text,
        &[
            "目录结构",
            "项目结构",
            "模块",
            "crate",
            ".rs",
            ".toml",
            ".md",
            "workspace",
            "frontend",
            "src/",
        ],
    ) {
        return MemoryCognitiveType::ProjectStructure;
    }
    if contains_any(
        &text,
        &[
            "决定",
            "选择",
            "采用",
            "取舍",
            "架构",
            "方案",
            "decision",
            "decided",
            "choose",
            "chosen",
            "adopted",
            "instead of",
        ],
    ) {
        return MemoryCognitiveType::ArchitectureDecision;
    }
    if contains_any(
        &text,
        &[
            "失败", "报错", "错误", "故障", "修复", "bug", "error", "failed", "failure", "panic",
            "timeout",
        ],
    ) {
        return MemoryCognitiveType::ProblemIncident;
    }
    if contains_any(
        &text,
        &[
            "知识",
            "概念",
            "原理",
            "规则",
            "规范",
            "domain",
            "knowledge",
        ],
    ) {
        return MemoryCognitiveType::DomainKnowledge;
    }
    MemoryCognitiveType::Factual
}

fn contains_any(text: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| text.contains(marker))
}

fn build_episode_summary(turn_result: &TurnResult) -> String {
    let mut lines = Vec::new();
    if !turn_result.user_input.trim().is_empty() {
        lines.push(format!("用户请求: {}", turn_result.user_input.trim()));
    }
    if !turn_result.summary.trim().is_empty() {
        lines.push(format!("结果摘要: {}", turn_result.summary.trim()));
    }
    if !turn_result.tool_calls.is_empty() {
        lines.push(format!("工具调用: {}", turn_result.tool_calls.join(", ")));
    }
    if !turn_result.artifacts.is_empty() {
        lines.push("结构化产物:".to_string());
        for artifact in &turn_result.artifacts {
            let mut parts = vec![format!("{:?}", artifact.kind).to_lowercase()];
            if let Some(tool_name) = artifact.tool_name.as_deref() {
                parts.push(format!("tool={tool_name}"));
            }
            if let Some(title) = artifact.title.as_deref() {
                parts.push(format!("title={}", title.trim()));
            }
            if let Some(url) = artifact.url.as_deref() {
                parts.push(format!("url={}", url.trim()));
            }
            if let Some(path) = artifact.path.as_deref() {
                parts.push(format!("path={}", path.trim()));
            }
            if let Some(summary) = artifact.summary.as_deref() {
                parts.push(format!("summary={}", summary.trim()));
            }
            lines.push(format!("- {}", parts.join(" ")));
        }
    }
    lines.join("\n")
}

/// 从 summary 派生标题（取前 50 个字符）
fn derive_title(summary: &str) -> String {
    let trimmed = summary.trim();
    let title: String = trimmed.chars().take(50).collect();
    if title.len() < trimmed.len() {
        format!("{title}…")
    } else {
        title
    }
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end >= start).then_some(&text[start..=end])
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
        .filter(|item| seen.insert(item.to_lowercase()))
        .collect()
}

/// 估算重要度（0.0 ~ 1.0）
fn estimate_importance(turn_result: &TurnResult) -> f32 {
    // Phase B 简单规则：有工具调用的 turn 基础重要度 0.5
    if turn_result
        .artifacts
        .iter()
        .any(|artifact| artifact.url.is_some() || artifact.path.is_some())
    {
        0.8
    } else if turn_result.had_tool_calls {
        0.5
    } else {
        0.1
    }
}

/// 从文本中粗略提取关键词（去停用词后取前 10 个词）
fn extract_keywords(text: &str) -> Vec<String> {
    // 简单分词：按空白和常见标点拆分，过滤短词
    let stop_words = [
        "的", "了", "在", "是", "和", "有", "为", "与", "a", "the", "is", "in", "to", "of",
    ];
    let words: Vec<String> = text
        .split(|c: char| c.is_whitespace() || "，。！？；：、,.!?;:'\"()（）".contains(c))
        .filter(|w| w.len() >= 2)
        .filter(|w| !stop_words.contains(w))
        .take(10)
        .map(String::from)
        .collect();
    words
}

// ── 多类型记忆提取 ──

const MULTI_TYPE_EXTRACTION_SYSTEM: &str = "\
你是独立记忆系统的多类型记忆提取器。根据一个对话轮次的执行结果，判断哪些内容值得记忆并提取为结构化记忆。

要求：
- 只输出 JSON 对象，不要 Markdown，不要解释。
- 仔细判断每条工具结果是否值得记忆：纯信息查询（天气、翻译、简单问答）和日常只读操作（ls、cat、pwd、read_file）不值得记忆。
- 值得记忆的信号包括：文件修改、架构/实现决策、构建测试结果、关键发现、用户偏好表达。
- 工具使用经验值得记忆：包括工具调用失败后的修正过程、成功发现的有效调用方式、skill 的正确使用方法。这些应提取为 memory_type=\"skill\" 的 Episode。
- 用户透露的个人信息（所在城市、常用语言、偏好设置等）值得记忆为 user_preference 类型。
- episodes: 每个值得记忆的事件提取一个 Episode。没有值得记忆的事件时返回空数组。
- entities: 发现的稳定实体（项目、模块、文档、服务）才提取，不要把临时搜索结果当实体。
- decisions: 有明确的架构/实现/产品取舍时才提取 Decision，必须包含 chosen 和 reasons。
- evidences: 有文件产物或工具结果摘要时提取 Evidence。

JSON 格式：
{
  \"episodes\": [
    {
      \"title\": \"...\",
      \"summary\": \"...\",
      \"memory_type\": \"factual\",
      \"outcome\": \"success\",
      \"keywords\": [\"...\"],
      \"tool_calls\": [\"...\"],
      \"importance\": 0.8
    }
  ],
  \"entities\": [
    {
      \"name\": \"...\",
      \"entity_type\": \"module\",
      \"description\": \"...\",
      \"file_path\": \"...\"
    }
  ],
  \"decisions\": [
    {
      \"title\": \"...\",
      \"context\": \"...\",
      \"alternatives\": [\"...\"],
      \"chosen\": \"...\",
      \"reasons\": [\"...\"]
    }
  ],
  \"evidences\": [
    {
      \"title\": \"...\",
      \"summary\": \"...\",
      \"file_path\": \"...\",
      \"source_tool\": \"...\"
    }
  ]
}";

#[derive(Debug, Default, serde::Deserialize)]
struct MultiTypeExtraction {
    #[serde(default)]
    episodes: Vec<ExtractedEpisode>,
    #[serde(default)]
    entities: Vec<ExtractedEntity>,
    #[serde(default)]
    decisions: Vec<ExtractedDecision>,
    #[serde(default)]
    evidences: Vec<ExtractedEvidence>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ExtractedEpisode {
    title: Option<String>,
    summary: Option<String>,
    memory_type: Option<String>,
    outcome: Option<String>,
    keywords: Option<Vec<String>>,
    tool_calls: Option<Vec<String>>,
    importance: Option<f32>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ExtractedEntity {
    name: Option<String>,
    entity_type: Option<String>,
    description: Option<String>,
    file_path: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ExtractedDecision {
    title: Option<String>,
    context: Option<String>,
    alternatives: Option<Vec<String>>,
    chosen: Option<String>,
    reasons: Option<Vec<String>>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ExtractedEvidence {
    title: Option<String>,
    summary: Option<String>,
    file_path: Option<String>,
    source_tool: Option<String>,
}

/// 从增强版轮次结果中提取多种类型的记忆。
///
/// 优先使用 Memory LLM 判断哪些工具结果值得记忆并提取为结构化记忆；
/// 未配置 Memory LLM 时走规则 fallback（只提取 Episode）。
pub(crate) async fn extract_multi_type_memories_with_model(
    enhanced: &EnhancedTurnResult,
    model: Option<&LlmEndpointConfig>,
) -> ExtractionOutput {
    let turn_result = TurnResult {
        session_id: enhanced.session_id.clone(),
        turn_id: enhanced.turn_id.clone(),
        had_tool_calls: enhanced.had_tool_calls,
        user_input: enhanced.user_input.clone(),
        summary: enhanced.summary.clone(),
        tool_calls: enhanced.tool_calls.clone(),
        artifacts: enhanced.artifacts.clone(),
        workspace_id: enhanced.workspace_id.clone(),
    };

    let Some(model) = model else {
        return extract_multi_type_fallback(&turn_result, enhanced);
    };

    match extract_multi_type_with_llm(enhanced, model).await {
        Ok(output) => output,
        Err(err) => {
            log_memory_llm_failure(
                "multi_type_extraction",
                model,
                &err,
                "多类型 LLM 提取失败，使用规则 fallback",
            );
            extract_multi_type_fallback(&turn_result, enhanced)
        }
    }
}

async fn extract_multi_type_with_llm(
    enhanced: &EnhancedTurnResult,
    model: &LlmEndpointConfig,
) -> anyhow::Result<ExtractionOutput> {
    let prompt = build_multi_type_prompt(enhanced);
    let started = Instant::now();
    let (response, usage) =
        complete_text_with_usage(model, MULTI_TYPE_EXTRACTION_SYSTEM, &prompt, 1500).await?;
    log_memory_llm_call(
        "multi_type_extraction",
        model,
        started.elapsed(),
        usage.as_ref(),
    );

    let json = extract_json_object(&response).unwrap_or(response.as_str());
    let extracted: MultiTypeExtraction = serde_json::from_str(json)?;

    let session_id = &enhanced.session_id;
    let now = chrono::Local::now().naive_local().to_string();

    let episodes = extracted
        .episodes
        .into_iter()
        .filter_map(|e| {
            let title = e.title?.trim().to_string();
            let summary = e.summary?.trim().to_string();
            if title.is_empty() || summary.is_empty() {
                return None;
            }
            Some(Episode {
                id: scru128::new().to_string(),
                session_id: session_id.clone(),
                memory_type: e
                    .memory_type
                    .as_deref()
                    .and_then(parse_memory_type)
                    .unwrap_or(MemoryCognitiveType::Factual),
                title,
                summary,
                outcome: parse_outcome(e.outcome.as_deref()).unwrap_or(EpisodeOutcome::Success),
                keywords: e.keywords.unwrap_or_default(),
                tool_calls: e.tool_calls.unwrap_or_default(),
                importance: e.importance.unwrap_or(0.5).clamp(0.0, 1.0),
                created_at: now.clone(),
            })
        })
        .collect();

    let entities = extracted
        .entities
        .into_iter()
        .filter_map(|e| {
            let name = e.name?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some(crate::types::Entity {
                id: scru128::new().to_string(),
                name,
                entity_type: parse_entity_type(e.entity_type.as_deref()),
                description: e.description.unwrap_or_default(),
                file_path: e.file_path,
                related_episodes: Vec::new(),
                importance: 0.5,
                created_at: now.clone(),
                updated_at: now.clone(),
            })
        })
        .collect();

    let decisions = extracted
        .decisions
        .into_iter()
        .filter_map(|e| {
            let title = e.title?.trim().to_string();
            let chosen = e.chosen?.trim().to_string();
            if title.is_empty() || chosen.is_empty() {
                return None;
            }
            Some(crate::types::Decision {
                id: scru128::new().to_string(),
                title,
                context: e.context.unwrap_or_default(),
                alternatives: e.alternatives.unwrap_or_default(),
                chosen,
                reasons: e.reasons.unwrap_or_default(),
                episode_ids: Vec::new(),
                created_at: now.clone(),
            })
        })
        .collect();

    let evidences = extracted
        .evidences
        .into_iter()
        .filter_map(|e| {
            let summary = e.summary.as_deref().unwrap_or_default().trim();
            if summary.is_empty() {
                return None;
            }
            Some(Evidence {
                title: e.title.unwrap_or_default(),
                summary: summary.to_string(),
                file_path: e.file_path,
                url: None,
                source_tool: e.source_tool,
            })
        })
        .collect();

    Ok(ExtractionOutput {
        episodes,
        entities,
        decisions,
        evidences,
    })
}

fn build_multi_type_prompt(enhanced: &EnhancedTurnResult) -> String {
    let mut lines = vec![
        format!("session_id: {}", enhanced.session_id),
        format!("turn_id: {}", enhanced.turn_id),
    ];
    if !enhanced.user_input.trim().is_empty() {
        lines.push(format!("user_input:\n{}", enhanced.user_input.trim()));
    }
    if !enhanced.summary.trim().is_empty() {
        lines.push(format!("assistant_summary:\n{}", enhanced.summary.trim()));
    }
    if !enhanced.tool_calls.is_empty() {
        lines.push(format!("tool_calls: {}", enhanced.tool_calls.join(", ")));
    }
    if !enhanced.artifacts.is_empty() {
        lines.push("artifacts:".to_string());
        for artifact in &enhanced.artifacts {
            lines.push(format!(
                "- kind={:?} tool={} url={} path={} summary={}",
                artifact.kind,
                artifact.tool_name.as_deref().unwrap_or(""),
                artifact.url.as_deref().unwrap_or(""),
                artifact.path.as_deref().unwrap_or(""),
                artifact.summary.as_deref().unwrap_or("")
            ));
        }
    }
    if !enhanced.memory_candidates.is_empty() {
        lines.push("tool_results:".to_string());
        for candidate in &enhanced.memory_candidates {
            lines.push(format!(
                "- tool={} success={} path={} summary={}",
                candidate.tool_name,
                candidate.success,
                candidate.file_path.as_deref().unwrap_or(""),
                candidate.result_summary.as_deref().unwrap_or("")
            ));
        }
    }
    if !enhanced.turn_messages.is_empty() {
        lines.push("messages:".to_string());
        for msg in &enhanced.turn_messages {
            lines.push(format!("{}: {}", msg.role, msg.content));
        }
    }
    lines.join("\n\n")
}

fn parse_entity_type(raw: Option<&str>) -> crate::types::EntityType {
    match raw.unwrap_or_default().trim().to_ascii_lowercase().as_str() {
        "project" => crate::types::EntityType::Project,
        "repository" | "repo" => crate::types::EntityType::Repository,
        "server" => crate::types::EntityType::Server,
        "skill" => crate::types::EntityType::Skill,
        "provider" => crate::types::EntityType::Provider,
        "document" | "doc" => crate::types::EntityType::Document,
        _ => crate::types::EntityType::Module,
    }
}

/// 规则 fallback：未配置 Memory LLM 时只提取 Episode（恢复原生行为）。
fn extract_multi_type_fallback(
    turn_result: &TurnResult,
    enhanced: &EnhancedTurnResult,
) -> ExtractionOutput {
    let episodes = if !enhanced.memory_candidates.is_empty() {
        extract_episode(turn_result).into_iter().collect()
    } else {
        Vec::new()
    };

    let evidences: Vec<_> = enhanced
        .memory_candidates
        .iter()
        .filter(|c| c.file_path.is_some() || c.url.is_some())
        .map(|c| Evidence {
            title: format!("{} 产物", c.tool_name),
            summary: c.result_summary.clone().unwrap_or_default(),
            file_path: c.file_path.clone(),
            url: c.url.clone(),
            source_tool: Some(c.tool_name.clone()),
        })
        .collect();

    ExtractionOutput {
        episodes,
        entities: Vec::new(),
        decisions: Vec::new(),
        evidences,
    }
}
