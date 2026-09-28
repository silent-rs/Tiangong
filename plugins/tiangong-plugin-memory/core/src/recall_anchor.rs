//! RecallAnchors 提取器。
//!
//! 统一 Tool 化回忆、粗召回和后续主动回忆的检索锚点生成逻辑。

use std::time::Instant;

use tiangong_llm::{LlmEndpointConfig, TokenUsageData, complete_text_with_usage};

use crate::llm_metrics::{log_memory_llm_call, log_memory_llm_failure};
use crate::types::{MemoryRecallRequest, RecallAnchors, SearchStrategy};

const RECALL_ANCHOR_SYSTEM: &str = "\
你是独立记忆系统的检索锚点规划器。根据当前请求和语境，生成适合长期记忆检索的锚点。

要求：
- 只输出 JSON 对象，不要 Markdown。
- query 应改写为面向历史记忆检索的短查询，不要照抄整段提示词。
- keywords 只保留能区分历史记录的实体、文件名、工具名、媒体/产物类型。
- strategy 可取 skip、keyword、semantic、hybrid。
- 如果当前请求是普通闲聊、无需历史上下文，strategy 使用 skip。
- 用户提到刚刚、刚才、之前、上次、继续、那个、这张图、生成的图片等历史指代时，优先 semantic 或 hybrid。

JSON 格式：
{
  \"query\": \"...\",
  \"keywords\": [\"...\"],
  \"strategy\": \"hybrid\",
  \"semantic_ratio\": 0.7,
  \"limit\": 5
}";

#[derive(Debug, Clone)]
pub(crate) struct PlannedRecall {
    pub anchors: RecallAnchors,
    pub limit: usize,
    pub used_llm: bool,
    /// anchor 规划阶段产生的 LLM token 消耗。
    pub usage: TokenUsageData,
}

#[derive(Debug, Default, serde::Deserialize)]
struct RecallAnchorPlan {
    query: Option<String>,
    keywords: Option<Vec<String>>,
    strategy: Option<String>,
    semantic_ratio: Option<f64>,
    limit: Option<usize>,
}

pub(crate) async fn extract_recall_anchors(
    model: Option<&LlmEndpointConfig>,
    request: &MemoryRecallRequest,
) -> PlannedRecall {
    if let Some(config) = model {
        match plan_with_model(config, request).await {
            Ok(plan) => return plan,
            Err(err) => log_memory_llm_failure(
                "recall_anchor",
                config,
                &err,
                "Memory recall anchor 规划失败，使用规则 fallback",
            ),
        }
    }

    fallback_plan(request)
}

async fn plan_with_model(
    config: &LlmEndpointConfig,
    request: &MemoryRecallRequest,
) -> anyhow::Result<PlannedRecall> {
    let prompt = format!(
        "当前请求:\n{}\n\n调用原因:\n{}\n\n期望内容:\n{}\n\n最近语境:\n{}",
        request.query,
        request.reason.as_deref().unwrap_or(""),
        request.expected.join(", "),
        request.context.join("\n---\n")
    );
    let started = Instant::now();
    let (response, llm_usage) =
        complete_text_with_usage(config, RECALL_ANCHOR_SYSTEM, &prompt, 512).await?;
    log_memory_llm_call(
        "recall_anchor",
        config,
        started.elapsed(),
        llm_usage.as_ref(),
    );
    if response.trim().is_empty() {
        anyhow::bail!("Memory LLM 返回空响应");
    }
    let json = extract_json_object(&response).unwrap_or(response.as_str());
    let parsed: RecallAnchorPlan = serde_json::from_str(json)?;
    let query = parsed
        .query
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .unwrap_or_else(|| request.query.clone());
    let keywords = parsed
        .keywords
        .map(dedupe_strings)
        .filter(|items| !items.is_empty())
        .unwrap_or_else(|| fallback_keywords(request));
    Ok(PlannedRecall {
        anchors: RecallAnchors {
            query,
            keywords,
            strategy: parse_strategy(parsed.strategy.as_deref(), parsed.semantic_ratio),
        },
        limit: parsed.limit.unwrap_or(request.limit).clamp(1, 10),
        used_llm: true,
        usage: llm_usage.unwrap_or_default(),
    })
}

fn fallback_plan(request: &MemoryRecallRequest) -> PlannedRecall {
    let keywords = fallback_keywords(request);
    let strategy = fallback_strategy(request, &keywords);
    let query = if strategy == Some(SearchStrategy::Skip) {
        String::new()
    } else {
        build_fallback_query(request, &keywords)
    };

    PlannedRecall {
        anchors: RecallAnchors {
            query,
            keywords,
            strategy,
        },
        limit: request.limit.clamp(1, 10),
        used_llm: false,
        usage: TokenUsageData::default(),
    }
}

fn fallback_keywords(request: &MemoryRecallRequest) -> Vec<String> {
    let mut keywords = request.expected.clone();
    let text = format!(
        "{}\n{}\n{}",
        request.query,
        request.reason.as_deref().unwrap_or(""),
        request.context.join("\n")
    );

    keywords.extend(extract_urls(&text));
    keywords.extend(extract_paths(&text));
    keywords.extend(extract_tool_names(&text));
    keywords.extend(extract_code_symbols(&text));
    keywords.extend(extract_media_markers(&text));
    keywords.extend(extract_text_terms(&request.query, 8));
    dedupe_strings(keywords)
}

fn fallback_strategy(request: &MemoryRecallRequest, keywords: &[String]) -> Option<SearchStrategy> {
    if request.query.trim().is_empty() || is_plain_chitchat(&request.query, request) {
        return Some(SearchStrategy::Skip);
    }
    if contains_history_reference(&request.query) {
        return Some(SearchStrategy::Semantic);
    }
    if keywords.iter().any(|item| is_precise_anchor(item)) {
        return Some(SearchStrategy::Keyword);
    }
    Some(SearchStrategy::Hybrid {
        semantic_ratio: 0.6,
    })
}

fn build_fallback_query(request: &MemoryRecallRequest, keywords: &[String]) -> String {
    if !keywords.is_empty()
        && (contains_history_reference(&request.query) || request.query.chars().count() > 80)
    {
        return keywords
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
    }
    request.query.clone()
}

fn parse_strategy(raw: Option<&str>, semantic_ratio: Option<f64>) -> Option<SearchStrategy> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "skip" | "none" | "off" => Some(SearchStrategy::Skip),
        "keyword" => Some(SearchStrategy::Keyword),
        "semantic" => Some(SearchStrategy::Semantic),
        "hybrid" => Some(SearchStrategy::Hybrid {
            semantic_ratio: semantic_ratio.unwrap_or(0.6).clamp(0.0, 1.0),
        }),
        _ => None,
    }
}

fn contains_history_reference(text: &str) -> bool {
    [
        "刚刚",
        "刚才",
        "之前",
        "上次",
        "继续",
        "那个",
        "这张图",
        "生成的图片",
        "previous",
        "earlier",
        "that one",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn is_plain_chitchat(query: &str, request: &MemoryRecallRequest) -> bool {
    if !request.expected.is_empty() || !request.context.is_empty() {
        return false;
    }
    let normalized = query.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "你好" | "hello" | "hi" | "谢谢" | "thanks" | "早上好" | "晚上好" | "你是谁" | "你能做什么"
    )
}

fn is_precise_anchor(item: &str) -> bool {
    item.contains("://")
        || item.starts_with('/')
        || item.starts_with("./")
        || item.starts_with("../")
        || item.contains("::")
        || item.contains(".rs")
        || item.contains(".md")
        || item.contains(".json")
        || item.contains(".png")
        || item.contains(".jpg")
        || item.contains(".mp4")
}

fn extract_text_terms(text: &str, limit: usize) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || "，。！？；：、,.!?;:'\"()（）".contains(c))
        .map(str::trim)
        .filter(|item| item.chars().count() >= 2)
        .filter(|item| !contains_history_reference(item))
        .take(limit)
        .map(String::from)
        .collect()
}

fn extract_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for prefix in ["https://", "http://", "data:image/"] {
        let mut start = 0;
        while let Some(offset) = text[start..].find(prefix) {
            let url_start = start + offset;
            let rest = &text[url_start..];
            let url_end = rest
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(c, '"' | '\'' | ')' | ']' | '}' | ',' | '，' | '。' | '\\')
                })
                .unwrap_or(rest.len());
            let url = rest[..url_end].trim_matches(['"', '\'', ')', ']', '}', ',', '，', '。']);
            if !url.is_empty() {
                urls.push(url.to_string());
            }
            start = url_start + url_end;
        }
    }
    dedupe_strings(urls)
}

fn extract_paths(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| {
            let cleaned = token
                .trim_matches(['"', '\'', ')', ']', '}', ',', '，', '。'])
                .strip_prefix("path=")
                .unwrap_or_else(|| token.trim_matches(['"', '\'', ')', ']', '}', ',', '，', '。']));
            let cleaned = cleaned
                .split(['"', '\'', ')', ']', '}', ',', '，', '。'])
                .next()
                .unwrap_or(cleaned);
            (cleaned.starts_with('/')
                || cleaned.starts_with("./")
                || cleaned.starts_with("../")
                || cleaned.contains(".rs")
                || cleaned.contains(".md")
                || cleaned.contains(".json")
                || cleaned.contains(".png")
                || cleaned.contains(".jpg")
                || cleaned.contains(".mp4"))
            .then(|| cleaned.to_string())
        })
        .collect::<Vec<_>>()
        .pipe(dedupe_strings)
}

fn extract_tool_names(text: &str) -> Vec<String> {
    let known = [
        "recall_memory",
        "generate_image",
        "write_file",
        "replace_in_file",
        "read_file",
        "run_command",
        "apply_patch",
        "cargo_test",
    ];
    known
        .iter()
        .filter(|name| text.contains(**name))
        .map(|name| (*name).to_string())
        .collect()
}

fn extract_code_symbols(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|token| token.trim_matches(['`', '"', '\'', ',', '，', '。', '(', ')']))
        .filter(|token| {
            token.contains("::")
                || token.starts_with("fn ")
                || token.starts_with("struct ")
                || token.starts_with("enum ")
                || token.starts_with("trait ")
        })
        .map(String::from)
        .collect::<Vec<_>>()
        .pipe(dedupe_strings)
}

fn extract_media_markers(text: &str) -> Vec<String> {
    let markers = [
        ("图片", "media"),
        ("图像", "media"),
        ("image", "media"),
        ("视频", "video"),
        ("video", "video"),
        ("文件", "file"),
        ("file", "file"),
        ("产物", "artifact"),
        ("artifact", "artifact"),
    ];
    markers
        .iter()
        .filter(|(marker, _)| text.contains(marker))
        .map(|(_, keyword)| (*keyword).to_string())
        .collect::<Vec<_>>()
        .pipe(dedupe_strings)
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end >= start).then_some(&text[start..=end])
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

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}

impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(query: &str) -> MemoryRecallRequest {
        MemoryRecallRequest {
            query: query.to_string(),
            limit: 5,
            ..MemoryRecallRequest::default()
        }
    }

    #[test]
    fn fallback_extracts_history_reference_as_semantic() {
        let mut req = request("继续用刚刚生成的图片");
        req.expected = vec!["media".to_string()];
        let plan = fallback_plan(&req);
        assert_eq!(plan.anchors.strategy, Some(SearchStrategy::Semantic));
        assert!(plan.anchors.keywords.contains(&"media".to_string()));
    }

    #[test]
    fn fallback_extracts_precise_file_path_as_keyword() {
        let req = request("查看 /tmp/tiangong/output.png 的历史记录");
        let plan = fallback_plan(&req);
        assert_eq!(plan.anchors.strategy, Some(SearchStrategy::Keyword));
        assert!(
            plan.anchors
                .keywords
                .iter()
                .any(|item| item == "/tmp/tiangong/output.png")
        );
    }

    #[test]
    fn fallback_extracts_media_url() {
        let req = request("继续使用 https://example.invalid/image.png 这张图");
        let plan = fallback_plan(&req);
        assert!(
            plan.anchors
                .keywords
                .iter()
                .any(|item| item == "https://example.invalid/image.png")
        );
    }

    #[test]
    fn fallback_skips_plain_chitchat() {
        let req = request("你好");
        let plan = fallback_plan(&req);
        assert_eq!(plan.anchors.strategy, Some(SearchStrategy::Skip));
        assert!(plan.anchors.query.is_empty());
    }

    #[test]
    fn fallback_skips_empty_query() {
        let req = request("");
        let plan = fallback_plan(&req);
        assert_eq!(plan.anchors.strategy, Some(SearchStrategy::Skip));
    }
}
