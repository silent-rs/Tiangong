use std::collections::{HashMap, HashSet};

use tiangong_plugin_memory_protocol::rumination::{
    EnhancedTurnResult, MemoryCandidate, TurnArtifact, TurnArtifactKind, TurnMessage, TurnStatus,
};
use tiangong_types::{ContentBlock, Message, MessageRole, PluginSession};

pub(crate) fn build_turn_memory_result(
    session: &PluginSession,
    messages: &[&Message],
    user_input: &str,
    turn_status: TurnStatus,
) -> EnhancedTurnResult {
    let mut candidates = Vec::new();
    let mut step_index = 0usize;
    let mut path_by_call_id = HashMap::new();
    for message in messages {
        if message.role == MessageRole::Assistant {
            for call in message.tool_calls() {
                if let Some(path) = call
                    .arguments
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                {
                    path_by_call_id.insert(call.id.as_str(), path);
                }
            }
        }
    }
    for message in messages {
        if message.role != MessageRole::Tool {
            continue;
        }
        let tool_name = message.tool_name().unwrap_or("");
        let summary = message.text_content();
        let file_path = path_by_call_id
            .get(message.tool_call_id().unwrap_or_default())
            .copied();
        if let Some(candidate) = evaluate_tool_result_for_memory(
            tool_name,
            !message.tool_is_error(),
            &summary,
            file_path,
            step_index,
        ) {
            candidates.push(candidate);
            step_index += 1;
        }
    }

    let tool_calls = extract_turn_tool_calls(messages);
    let artifacts = extract_turn_artifacts(messages);
    let summary = build_turn_memory_summary(messages, &artifacts);
    let turn_messages = messages
        .iter()
        .filter_map(|message| {
            let role = match message.kind() {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => return None,
                MessageRole::Notice => return None,
                MessageRole::Tool => "tool",
            };
            // 非真人输入的用户消息（插件反馈图片 HostInjected、压缩恢复锚点
            // CompressedResume、Agent 协作消息）不是用户意图，不进入反刍轮次消息。
            if message.kind() == MessageRole::User && !message.is_user_input() {
                return None;
            }
            let content = compact_single_memory_text(&message.text_content(), 400);
            (!content.is_empty()).then(|| TurnMessage {
                role: role.to_string(),
                content,
            })
        })
        .collect();

    EnhancedTurnResult {
        session_id: session.id.clone(),
        turn_id: scru128::new().to_string(),
        had_tool_calls: !tool_calls.is_empty(),
        turn_status,
        user_input: user_input.to_string(),
        summary,
        tool_calls,
        artifacts,
        workspace_id: Some(session.workspace_id.clone()),
        memory_candidates: candidates,
        turn_messages,
    }
}

fn evaluate_tool_result_for_memory(
    tool_name: &str,
    success: bool,
    result_summary: &str,
    file_path: Option<&str>,
    step_index: usize,
) -> Option<MemoryCandidate> {
    let summary = result_summary.trim();
    if summary.is_empty() {
        return None;
    }
    Some(MemoryCandidate {
        tool_name: tool_name.to_string(),
        step_index,
        hint: String::new(),
        suggested_kinds: Vec::new(),
        file_path: file_path.map(String::from),
        url: None,
        result_summary: Some(compact_single_memory_text(summary, 240)),
        success,
        tool_source: None,
        is_recalled_context: false,
    })
}

fn extract_turn_tool_calls(messages: &[&Message]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for message in messages {
        if message.role != MessageRole::Tool {
            continue;
        }
        let text = message.text_content();
        for name in parse_tool_calls_line(&text)
            .into_iter()
            .chain(parse_tool_trace_name(&text))
        {
            if name != "recall_memory" && seen.insert(name.clone()) {
                names.push(name);
            }
        }
    }
    names
}

fn extract_turn_artifacts(messages: &[&Message]) -> Vec<TurnArtifact> {
    let mut artifacts = Vec::new();
    let mut seen = HashSet::new();
    for message in messages {
        if message.role == MessageRole::Assistant {
            for block in &message.content {
                let media = match block {
                    ContentBlock::Media { url, title, .. } => Some((url.clone(), title.clone())),
                    ContentBlock::Image { asset, .. } | ContentBlock::AssetReference { asset } => {
                        Some((asset.local_path.clone(), Some(asset.original_name.clone())))
                    }
                    ContentBlock::Text { .. } | ContentBlock::ModelInstruction { .. } => None,
                };
                let Some((url, title)) = media else {
                    continue;
                };
                if seen.insert(format!("media:{url}")) {
                    artifacts.push(TurnArtifact {
                        kind: TurnArtifactKind::Media,
                        tool_name: None,
                        title,
                        url: Some(url),
                        path: None,
                        summary: None,
                        is_recalled_context: false,
                    });
                }
            }
            continue;
        }
        if message.role != MessageRole::Tool {
            continue;
        }
        let text = message.text_content();
        let tool_name = parse_tool_trace_name(&text);
        for artifact in parse_media_artifacts_from_tool_trace(&text, tool_name.as_deref()) {
            let key = artifact
                .url
                .as_deref()
                .or(artifact.path.as_deref())
                .unwrap_or_default()
                .to_string();
            if !key.is_empty() && seen.insert(key) {
                artifacts.push(artifact);
            }
        }
        if let Some(path) = tool_name
            .as_deref()
            .filter(|name| *name == "write_file" || *name == "replace_in_file")
            .and_then(|_| parse_written_path(&text))
            && seen.insert(format!("file:{path}"))
        {
            artifacts.push(TurnArtifact {
                kind: TurnArtifactKind::File,
                tool_name: tool_name.clone(),
                title: Some("文件产物".to_string()),
                url: None,
                path: Some(path),
                summary: parse_summary_line(&text),
                is_recalled_context: false,
            });
        }
        if let Some(tool_name) = tool_name
            && should_record_tool_result(&tool_name)
        {
            let summary =
                parse_summary_line(&text).unwrap_or_else(|| compact_single_memory_text(&text, 240));
            if !summary.is_empty() && seen.insert(format!("tool:{tool_name}:{summary}")) {
                artifacts.push(TurnArtifact {
                    kind: TurnArtifactKind::ToolResult,
                    tool_name: Some(tool_name),
                    title: Some("工具结果".to_string()),
                    url: None,
                    path: None,
                    summary: Some(summary),
                    is_recalled_context: false,
                });
            }
        }
    }
    artifacts.into_iter().take(12).collect()
}

fn build_turn_memory_summary(messages: &[&Message], artifacts: &[TurnArtifact]) -> String {
    let assistant_summary = messages
        .iter()
        .rev()
        .find(|message| {
            message.role == MessageRole::Assistant && !message.text_content().trim().is_empty()
        })
        .map(|message| compact_single_memory_text(&message.text_content(), 600))
        .unwrap_or_default();
    if !assistant_summary.is_empty() {
        return assistant_summary;
    }
    artifacts
        .iter()
        .filter_map(|artifact| {
            artifact
                .url
                .as_deref()
                .or(artifact.path.as_deref())
                .or(artifact.summary.as_deref())
                .map(str::to_string)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_tool_calls_line(content: &str) -> Vec<String> {
    content
        .lines()
        .find_map(|line| line.trim().strip_prefix("tool_calls:"))
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_tool_trace_name(content: &str) -> Option<String> {
    let first_line = content.lines().next()?.trim();
    let rest = first_line.strip_prefix("工具执行 [")?;
    let end = rest.find(']')?;
    Some(rest[..end].to_string()).filter(|name| !name.is_empty())
}

fn parse_summary_line(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        line.trim()
            .strip_prefix("summary:")
            .map(str::trim)
            .map(String::from)
            .filter(|item| !item.is_empty())
    })
}

fn compact_single_memory_text(text: &str, max_chars: usize) -> String {
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

fn parse_media_artifacts_from_tool_trace(
    content: &str,
    tool_name: Option<&str>,
) -> Vec<TurnArtifact> {
    let mut artifacts = Vec::new();
    let summary = parse_summary_line(content);
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("![") && trimmed.ends_with(')') {
            let Some(close_alt) = trimmed.find("](") else {
                continue;
            };
            let title = trimmed[2..close_alt].trim();
            let url = trimmed[close_alt + 2..trimmed.len() - 1].trim();
            if !url.is_empty() {
                artifacts.push(TurnArtifact {
                    kind: TurnArtifactKind::Media,
                    tool_name: tool_name.map(String::from),
                    title: (!title.is_empty()).then(|| title.to_string()),
                    url: Some(url.to_string()),
                    path: None,
                    summary: summary.clone(),
                    is_recalled_context: false,
                });
            }
            continue;
        }
        if let Some(url) = parse_video_url_line(trimmed) {
            artifacts.push(TurnArtifact {
                kind: TurnArtifactKind::Media,
                tool_name: tool_name.map(String::from),
                title: Some("生成的视频".to_string()),
                url: Some(url),
                path: None,
                summary: summary.clone(),
                is_recalled_context: false,
            });
        }
    }
    artifacts
}

fn parse_video_url_line(line: &str) -> Option<String> {
    let raw = line
        .strip_prefix("Video URL:")
        .or_else(|| line.strip_prefix("video_url:"))
        .map(str::trim)?;
    let url = raw.split_whitespace().next().unwrap_or(raw);
    (url.starts_with("http://") || url.starts_with("https://")).then(|| url.to_string())
}

fn parse_written_path(content: &str) -> Option<String> {
    let command = content.lines().find_map(|line| {
        line.trim()
            .strip_prefix("命令:")
            .map(str::trim)
            .filter(|line| !line.is_empty())
    })?;
    command
        .split_whitespace()
        .find_map(|part| part.strip_prefix("path=").map(str::to_string))
        .filter(|path| !path.is_empty())
}

fn should_record_tool_result(tool_name: &str) -> bool {
    !matches!(
        tool_name,
        "list_dir"
            | "tree_dir"
            | "current_time"
            | "index_search"
            | "search_code"
            | "recall_memory"
            | "get_skill_detail"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_types::UserSource;

    fn test_session(messages: Vec<Message>) -> PluginSession {
        PluginSession {
            id: "s-1".to_string(),
            title: String::new(),
            cwd: "/tmp/ws".to_string(),
            workspace_id: "ws".to_string(),
            reasoning_effort: None,
            messages,
            context_summary: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// 构造 RFC 0017 的插件反馈图片注入消息（role=User、phase=HostInjected，
    /// ModelInstruction + Image 块，text_content 为空）。
    fn host_injected_image_message() -> Message {
        let mut message = Message::new(MessageRole::User, String::new());
        message.content = vec![
            ContentBlock::model_instruction("[injected-images provenance]"),
            ContentBlock::image(
                tiangong_types::StoredAsset {
                    asset_id: "a-1".to_string(),
                    local_path: "/media/shot.png".to_string(),
                    original_name: "shot.png".to_string(),
                    mime_type: "image/png".to_string(),
                    size: 1024,
                    kind: tiangong_types::MediaKind::Image,
                },
                None,
            ),
        ];
        message.with_source(UserSource::HostInjected)
    }

    #[test]
    fn turn_messages_exclude_host_injected_image_message() {
        let user = Message::new(MessageRole::User, "帮我看下这张截图");
        let session = test_session(vec![user, host_injected_image_message()]);
        let messages: Vec<&Message> = session.messages.iter().collect();
        let result = build_turn_memory_result(
            &session,
            &messages,
            "帮我看下这张截图",
            TurnStatus::Completed,
        );
        // 注入图片消息不进入反刍轮次消息，也不产生空 user 轮次。
        assert_eq!(result.turn_messages.len(), 1);
        assert_eq!(result.turn_messages[0].role, "user");
        assert_eq!(result.turn_messages[0].content, "帮我看下这张截图");
        assert_eq!(result.user_input, "帮我看下这张截图");
    }

    #[test]
    fn compact_text_removes_empty_lines_and_uses_ascii_ellipsis() {
        let text = compact_single_memory_text(" one \n\n two ", 4);
        assert_eq!(text, "one\n...");
    }

    #[test]
    fn tool_names_are_unique_and_exclude_recall() {
        let message = Message::tool_result(
            "",
            "read_file",
            "tool_calls: read_file, recall_memory, read_file",
            false,
        );
        let names = extract_turn_tool_calls(&[&message]);
        assert_eq!(names, vec!["read_file"]);
    }
}
