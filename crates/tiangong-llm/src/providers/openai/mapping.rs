//! Responses API 请求/响应映射。
//!
//! Responses API 与 Chat Completions 在结构上差异较大：
//! - 顶层用 `input`（items 列表）替代 `messages`
//! - 系统提示放入 `instructions`
//! - 工具定义为 `{type:"function", name, description, parameters}`
//! - 响应输出为 `output` items 列表（message/function_call/reasoning）
//!
//! 为避免 SDK 强类型泄漏并保持与 Chat Completions 一致的实现风格，
//! 这里直接在 `serde_json::Value` 层操作。

use anyhow::Result;
use serde_json::{Value, json};

use crate::message::{ChatMessage, MessageContent, MessageRole, ThinkingContent};
use crate::request::{ProviderRequest, ReasoningEffort};
use crate::response::{ProviderResponse, StopReason};
use crate::tool::{ToolChoice, ToolSpec};
use crate::usage::TokenUsageData;

/// 复用 Chat Completions 的 base_url 规范化逻辑。
pub fn normalize_api_base(base_url: &str) -> Result<String> {
    crate::providers::openai_chatcompletions::mapping::normalize_api_base(base_url)
}

/// 构建 Responses API 请求 JSON。
pub fn build_request_json(req: &ProviderRequest, stream: bool) -> Result<Value> {
    let mut payload = serde_json::Map::new();
    payload.insert("model".to_string(), json!(req.model));
    if let Some(key) = req
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        payload.insert("prompt_cache_key".to_string(), json!(key));
    }

    // Responses API 的 instructions 只能承载单个系统提示，而消息流中可能
    // 插入额外 System 消息（记忆检索结果、错误提示等）。将主 system 与流中
    // System 消息按出现顺序拼接到 instructions，避免静默丢弃。
    let instructions = collect_instructions(req);
    if !instructions.trim().is_empty() {
        payload.insert("instructions".to_string(), json!(instructions));
    }

    payload.insert("input".to_string(), build_input_items(req)?);

    if stream {
        payload.insert("stream".to_string(), json!(true));
    }

    if req.max_tokens > 0 {
        payload.insert("max_output_tokens".to_string(), json!(req.max_tokens));
    }
    if let Some(temperature) = req.temperature {
        payload.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = req.top_p {
        payload.insert("top_p".to_string(), json!(top_p));
    }

    if !req.tools.is_empty() {
        payload.insert("tools".to_string(), build_tools(&req.tools));
        if let Some(choice) = req.tool_choice.as_ref()
            && let Some(tool_choice) = build_tool_choice(choice)
        {
            payload.insert("tool_choice".to_string(), tool_choice);
        }
    }

    // 思考/推理配置：优先 reasoning_effort，其次 thinking。
    // 注意：Responses API 的 reasoning summary 必须显式请求（summary: "auto"），
    // 否则服务端不会返回可展示的思考摘要，前端 thinking 链路将无内容。
    if req.reasoning_effort.is_thinking_enabled() {
        payload.insert(
            "reasoning".to_string(),
            build_reasoning(effort_to_str(req.reasoning_effort)),
        );
    }
    // ReasoningEffort::None 关闭思考：Responses API 没有 disabled 语义，
    // 省略 reasoning 字段即不强制思考。

    Ok(Value::Object(payload))
}

/// 构建 reasoning 请求字段。
///
/// `summary: "auto"` 让模型返回 reasoning summary（思考摘要），是 Responses API
/// 暴露 reasoning 内容的必要条件；缺省时模型不会输出可展示的思考链。
fn build_reasoning(effort: &str) -> Value {
    json!({ "effort": effort, "summary": "auto" })
}

fn effort_to_str(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None => "medium",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        // Responses API 的 reasoning.effort 目前仅支持 low/medium/high，
        // Max 暂降级为 high；若 OpenAI 后续开放 "max" 级别需同步更新此处。
        ReasoningEffort::Max => "high",
    }
}

/// 收集主 system 与消息流中的 System 消息，按顺序拼接为 instructions。
///
/// Responses API 的 instructions 仅支持单个字符串，故将对话中插入的 System
/// 消息（如记忆检索结果、错误提示）追加到主系统提示之后。
fn collect_instructions(req: &ProviderRequest) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(system) = req
        .system
        .as_ref()
        .map(|s| s.trim())
        .filter(|v| !v.is_empty())
    {
        parts.push(system.to_string());
    }
    for message in &req.messages {
        if !matches!(message.role, MessageRole::System) {
            continue;
        }
        let text = collect_text(message);
        if !text.trim().is_empty() {
            parts.push(text.trim().to_string());
        }
    }
    parts.join("\n\n")
}

fn build_tools(specs: &[ToolSpec]) -> Value {
    let tools = specs
        .iter()
        .map(|spec| {
            json!({
                "type": "function",
                "name": spec.name,
                "description": spec.description,
                "parameters": spec.input_schema,
            })
        })
        .collect::<Vec<_>>();
    Value::Array(tools)
}

fn build_tool_choice(choice: &ToolChoice) -> Option<Value> {
    match choice {
        ToolChoice::Auto => Some(json!("auto")),
        ToolChoice::Any => Some(json!("required")),
        ToolChoice::Tool(name) => Some(json!({ "type": "function", "name": name })),
        ToolChoice::None => Some(json!("none")),
    }
}

fn build_input_items(req: &ProviderRequest) -> Result<Value> {
    let mut items = Vec::new();
    for message in &req.messages {
        match message.role {
            MessageRole::System => {
                // System 消息已由 collect_instructions 拼入 instructions，此处不重复入 input。
            }
            MessageRole::User => {
                if let Some(item) = build_user_item(message)? {
                    items.push(item);
                }
            }
            MessageRole::Assistant => {
                // 助手文本与工具调用分别作为独立 input items 推入历史。
                let text = collect_text(message);
                if !text.trim().is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": text,
                    }));
                }
                for tool_call in message.content.iter().filter_map(|content| match content {
                    MessageContent::ToolCall(tool_call) => Some(tool_call),
                    _ => None,
                }) {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": tool_call.id,
                        "name": tool_call.name,
                        "arguments": tool_call.arguments.to_string(),
                    }));
                }
            }
            MessageRole::Tool => {
                for result in message.content.iter().filter_map(|content| match content {
                    MessageContent::ToolResult(result) => Some(result),
                    _ => None,
                }) {
                    let output = match &result.content {
                        crate::tool::ToolResultContent::Text(text) => text.clone(),
                        crate::tool::ToolResultContent::Json(value) => value.to_string(),
                    };
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": result.tool_call_id,
                        "output": output,
                    }));
                }
            }
        }
    }
    Ok(Value::Array(items))
}

fn build_user_item(message: &ChatMessage) -> Result<Option<Value>> {
    let has_media = message
        .content
        .iter()
        .any(|content| matches!(content, MessageContent::Image(_) | MessageContent::File(_)));

    if !has_media {
        let text = collect_text(message);
        if text.trim().is_empty() {
            return Ok(None);
        }
        return Ok(Some(json!({
            "type": "message",
            "role": "user",
            "content": text,
        })));
    }

    // 多模态消息按 ChatMessage 内容块原始顺序逐块映射（与 Anthropic /
    // Chat Completions 一致），适配层不重排内容。
    let mut content = Vec::new();
    for block in &message.content {
        match block {
            MessageContent::Image(image) => content.push(json!({
                "type": "input_image",
                "image_url": image.data,
            })),
            // 文档附件（PDF/Office）的 Responses API 原生 file block 映射。
            // 官方规范：{ "type": "file", "file": { "filename": "...", "file_data": "data:<mime>;base64,..." } }
            // 注：当前上层策略下文件附件统一走本地脚本解析，不会产生 MessageContent::File；
            // 此分支保留以支持未来直传场景，并维持 provider 层映射的完整性。
            MessageContent::File(file) => {
                let filename = file_filename_with_fallback(&file.title, &file.mime_type);
                content.push(json!({
                    "type": "file",
                    "file": {
                        "filename": filename,
                        "file_data": file.data,
                    }
                }));
            }
            other => {
                if let Some(text) = text_part(other)
                    && !text.trim().is_empty()
                {
                    content.push(json!({ "type": "input_text", "text": text }));
                }
            }
        }
    }
    Ok(Some(json!({
        "type": "message",
        "role": "user",
        "content": content,
    })))
}

fn collect_text(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(text_part)
        .collect::<Vec<_>>()
        .join("\n")
}

fn text_part(content: &MessageContent) -> Option<String> {
    match content {
        MessageContent::Text(text) => Some(text.clone()),
        MessageContent::ToolResult(result) => Some(match &result.content {
            crate::tool::ToolResultContent::Text(text) => text.clone(),
            crate::tool::ToolResultContent::Json(value) => value.to_string(),
        }),
        // File 由 build_user_item 走原生 file block，此处不再降级为文本，
        // 避免 base64 data URL 塞进文本导致 token 膨胀且模型无法读取。
        _ => None,
    }
}

/// 推断文档附件的文件名，供 Responses API 的 file block 使用。
///
/// 优先用 title（前端传入的用户可见文件名）；否则按 MIME 兜底一个通用名。
fn file_filename_with_fallback(title: &Option<String>, mime_type: &str) -> String {
    if let Some(title) = title {
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let ext = match mime_type {
        "application/pdf" => "pdf",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        _ => "bin",
    };
    format!("attachment.{ext}")
}

/// 解析 Responses API 完整响应。
pub fn parse_complete_response(payload: &Value) -> Result<ProviderResponse> {
    let mut content = Vec::new();
    let mut reasoning_content: Option<String> = None;

    if let Some(output) = payload.get("output").and_then(Value::as_array) {
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    let text = item
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|part| {
                                    let part_type = part.get("type").and_then(Value::as_str);
                                    if part_type == Some("output_text")
                                        || part_type == Some("reasoning_text")
                                        || part_type == Some("text")
                                    {
                                        part.get("text").and_then(Value::as_str).map(String::from)
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default();
                    if !text.trim().is_empty() {
                        content.push(MessageContent::Text(strip_think(text.trim())));
                    }
                }
                Some("function_call") => {
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|raw| serde_json::from_str(raw).ok())
                        .unwrap_or_else(|| json!({}));
                    if !name.is_empty() {
                        content.push(MessageContent::ToolCall(crate::tool::ToolCall {
                            id: call_id,
                            name,
                            arguments,
                        }));
                    }
                }
                Some("reasoning") if reasoning_content.is_none() => {
                    let thinking = extract_reasoning_text(item);
                    reasoning_content = Some(thinking.clone());
                    content.push(MessageContent::Thinking(ThinkingContent {
                        thinking,
                        signature: None,
                    }));
                }
                Some("reasoning") => content.push(MessageContent::Thinking(ThinkingContent {
                    thinking: extract_reasoning_text(item),
                    signature: None,
                })),
                _ => {}
            }
        }
    }

    let usage = payload.get("usage").map(parse_usage);
    let status = payload.get("status").and_then(Value::as_str).unwrap_or("");
    let stop_reason = map_status_to_stop_reason(status, &content);

    Ok(ProviderResponse {
        id: payload.get("id").and_then(Value::as_str).map(String::from),
        model: payload
            .get("model")
            .and_then(Value::as_str)
            .map(String::from),
        assistant_message: ChatMessage {
            role: MessageRole::Assistant,
            content,
        },
        reasoning_content,
        stop_reason,
        usage,
        raw: Some(payload.clone()),
    })
}

pub(super) fn extract_reasoning_text(item: &Value) -> String {
    // 优先取 content[].text（reasoning_text），其次取 summary[].text。
    if let Some(content) = item.get("content").and_then(Value::as_array)
        && !content.is_empty()
    {
        let text = content
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
        if !text.trim().is_empty() {
            return text;
        }
    }
    item.get("summary")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn map_status_to_stop_reason(status: &str, content: &[MessageContent]) -> Option<StopReason> {
    match status {
        "completed" => {
            if content
                .iter()
                .any(|c| matches!(c, MessageContent::ToolCall(_)))
            {
                Some(StopReason::ToolUse)
            } else {
                Some(StopReason::EndTurn)
            }
        }
        "incomplete" => Some(StopReason::MaxTokens),
        "failed" | "cancelled" => Some(StopReason::Other(status.to_string())),
        _ => None,
    }
}

/// 解析 Responses 用量。
pub fn parse_usage(usage: &Value) -> TokenUsageData {
    let prompt = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let completion = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or((prompt + completion) as u64) as usize;
    let cached = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .map(|v| v as usize);
    TokenUsageData {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        prompt_cache_hit_tokens: cached,
        prompt_cache_miss_tokens: cached.map(|c| prompt.saturating_sub(c)),
    }
}

/// 去除 `<think>...</think>` 标签，复用 Chat Completions 的实现。
fn strip_think(text: &str) -> String {
    crate::providers::openai_chatcompletions::mapping::strip_think_tags(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn streaming_request_uses_regular_mode() {
        let req = ProviderRequest {
            session_id: None,
            model: "gpt-5.6-sol".to_string(),
            system: None,
            messages: vec![ChatMessage::text(MessageRole::User, "你好")],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::None,
        };
        let payload = build_request_json(&req, true).unwrap();
        assert_eq!(payload["stream"], true);
        assert!(!payload.as_object().unwrap().contains_key("background"));
    }

    #[test]
    fn parses_text_response() {
        let payload = json!({
            "id": "resp_1",
            "model": "gpt-4o",
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        { "type": "output_text", "text": "你好" }
                    ]
                }
            ],
            "usage": {
                "input_tokens": 5,
                "output_tokens": 2,
                "total_tokens": 7
            }
        });
        let resp = parse_complete_response(&payload).unwrap();
        assert_eq!(resp.id.as_deref(), Some("resp_1"));
        assert_eq!(resp.assistant_message.content.len(), 1);
        match &resp.assistant_message.content[0] {
            MessageContent::Text(text) => assert_eq!(text, "你好"),
            other => panic!("expected text, got {other:?}"),
        }
        let usage = resp.usage.unwrap();
        assert_eq!(usage.total_tokens, 7);
        assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn parses_function_call_response() {
        let payload = json!({
            "id": "resp_2",
            "model": "gpt-4o",
            "status": "completed",
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"北京\"}"
                }
            ],
            "usage": null
        });
        let resp = parse_complete_response(&payload).unwrap();
        match &resp.assistant_message.content[0] {
            MessageContent::ToolCall(call) => {
                assert_eq!(call.id, "call_1");
                assert_eq!(call.name, "get_weather");
                assert_eq!(call.arguments["city"], "北京");
            }
            other => panic!("expected tool call, got {other:?}"),
        }
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn parses_reasoning_text() {
        let payload = json!({
            "id": "resp_3",
            "model": "o3",
            "status": "completed",
            "output": [
                {
                    "type": "reasoning",
                    "summary": [{ "type": "summary_text", "text": "思考中" }]
                },
                {
                    "type": "message",
                    "content": [{ "type": "output_text", "text": "答案" }]
                }
            ],
            "usage": {}
        });
        let resp = parse_complete_response(&payload).unwrap();
        assert_eq!(resp.reasoning_content.as_deref(), Some("思考中"));
    }

    #[test]
    fn request_rebuilds_function_call_and_output_items() {
        let req = ProviderRequest {
            session_id: None,
            model: "o3".to_string(),
            system: None,
            messages: vec![
                ChatMessage::new(
                    MessageRole::Assistant,
                    vec![
                        MessageContent::Thinking(ThinkingContent {
                            thinking: String::new(),
                            signature: None,
                        }),
                        MessageContent::ToolCall(crate::tool::ToolCall {
                            id: "call_1".to_string(),
                            name: "get_weather".to_string(),
                            arguments: json!({"city": "北京"}),
                        }),
                    ],
                ),
                ChatMessage::new(
                    MessageRole::Tool,
                    vec![MessageContent::ToolResult(crate::tool::ToolResult {
                        tool_call_id: "call_1".to_string(),
                        content: crate::tool::ToolResultContent::Text("晴".to_string()),
                        is_error: false,
                    })],
                ),
            ],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::None,
        };
        let payload = build_request_json(&req, false).unwrap();
        let input = payload["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[0]["name"], "get_weather");
        assert_eq!(input[0]["arguments"], "{\"city\":\"北京\"}");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_1");
    }

    #[test]
    fn request_rebuilds_latest_function_call_item() {
        let req = ProviderRequest {
            session_id: None,
            model: "o3".to_string(),
            system: None,
            messages: vec![ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageContent::ToolCall(crate::tool::ToolCall {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: json!({"city": "北京"}),
                })],
            )],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::None,
        };
        let payload = build_request_json(&req, false).unwrap();
        let input = payload["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[0]["name"], "get_weather");
        assert_eq!(input[0]["arguments"], "{\"city\":\"北京\"}");
    }

    #[test]
    fn builds_request_payload() {
        let req = ProviderRequest {
            session_id: None,
            model: "gpt-4o".to_string(),
            system: Some("你是助手".to_string()),
            messages: vec![crate::message::ChatMessage::text(MessageRole::User, "你好")],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 1024,
            temperature: Some(0.5),
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::None,
        };
        let payload = build_request_json(&req, false).unwrap();
        assert_eq!(payload["model"], "gpt-4o");
        assert_eq!(payload["instructions"], "你是助手");
        assert_eq!(payload["max_output_tokens"], 1024);
        assert_eq!(payload["temperature"], 0.5);
        assert_eq!(payload["input"][0]["type"], "message");
        assert_eq!(payload["input"][0]["role"], "user");
        assert_eq!(payload["input"][0]["content"], "你好");
    }

    #[test]
    fn collects_stream_system_messages_into_instructions() {
        // 消息流中的 System 消息（如记忆检索结果）应拼入 instructions，
        // 而非静默丢弃。
        let req = ProviderRequest {
            session_id: None,
            model: "gpt-4o".to_string(),
            system: Some("主系统提示".to_string()),
            messages: vec![
                crate::message::ChatMessage::text(MessageRole::System, "[记忆] 相关上下文"),
                crate::message::ChatMessage::text(MessageRole::User, "你好"),
            ],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 100,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::None,
        };
        let payload = build_request_json(&req, false).unwrap();
        let instructions = payload["instructions"].as_str().unwrap();
        assert!(instructions.contains("主系统提示"));
        assert!(instructions.contains("[记忆] 相关上下文"));
        // System 消息不应出现在 input 中。
        let input = payload["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn builds_request_with_tools() {
        let req = ProviderRequest {
            session_id: None,
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![crate::message::ChatMessage::text(MessageRole::User, "天气")],
            tools: vec![ToolSpec {
                name: "get_weather".to_string(),
                description: "获取天气".to_string(),
                input_schema: json!({"type": "object"}),
            }],
            tool_choice: Some(ToolChoice::Any),
            max_tokens: 512,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::None,
        };
        let payload = build_request_json(&req, true).unwrap();
        assert_eq!(payload["tools"][0]["type"], "function");
        assert_eq!(payload["tools"][0]["name"], "get_weather");
        assert_eq!(payload["tool_choice"], "required");
        assert_eq!(payload["stream"], true);
    }

    #[test]
    fn reasoning_effort_requests_summary_auto() {
        // reasoning_effort 分支必须带上 summary: "auto"，否则服务端不返回思考摘要。
        let req = ProviderRequest {
            session_id: None,
            model: "o3".to_string(),
            system: None,
            messages: vec![crate::message::ChatMessage::text(MessageRole::User, "hi")],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 0,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::High,
        };
        let payload = build_request_json(&req, false).unwrap();
        assert_eq!(payload["reasoning"]["effort"], "high");
        assert_eq!(payload["reasoning"]["summary"], "auto");
    }

    #[test]
    fn thinking_budget_requests_summary_auto() {
        // thinking 分支（默认 medium effort）同样必须带上 summary。
        let req = ProviderRequest {
            session_id: None,
            model: "o3".to_string(),
            system: None,
            messages: vec![crate::message::ChatMessage::text(MessageRole::User, "hi")],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 0,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::Medium,
        };
        let payload = build_request_json(&req, false).unwrap();
        assert_eq!(payload["reasoning"]["effort"], "medium");
        assert_eq!(payload["reasoning"]["summary"], "auto");
    }

    #[test]
    fn thinking_disabled_omits_reasoning() {
        let req = ProviderRequest {
            session_id: None,
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![crate::message::ChatMessage::text(MessageRole::User, "hi")],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 0,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::None,
        };
        let payload = build_request_json(&req, false).unwrap();
        assert!(payload.get("reasoning").is_none());
    }

    #[test]
    fn normalizes_base_url_strips_chat_completions() {
        // 复用 openai mapping 的 normalize_api_base。
        let base = normalize_api_base("https://api.openai.com/v1/chat/completions").unwrap();
        assert_eq!(base, "https://api.openai.com/v1");
        let err = normalize_api_base("  ");
        assert!(err.is_err(), "空 base_url 应报错：{err:?}");
    }

    #[test]
    fn user_item_preserves_content_block_order() {
        // 多模态消息逐块映射：输出顺序与输入内容块顺序一致，不重排。
        let image = |data: &str| {
            MessageContent::Image(crate::message::ImageContent {
                mime_type: "image/png".to_string(),
                data: data.to_string(),
            })
        };
        let message = ChatMessage::new(
            MessageRole::User,
            vec![
                MessageContent::Text("t0".to_string()),
                image("data:image/png;base64,AAA"),
                MessageContent::Text("t1".to_string()),
                image("data:image/png;base64,BBB"),
                MessageContent::Text("t2".to_string()),
            ],
        );
        let item = build_user_item(&message)
            .unwrap()
            .expect("应生成 user item");
        let content = item["content"].as_array().expect("应有 content 数组");
        let shape = content
            .iter()
            .map(|block| match block["type"].as_str() {
                Some("input_text") => format!("text:{}", block["text"].as_str().unwrap()),
                Some("input_image") => format!("image:{}", block["image_url"].as_str().unwrap()),
                other => panic!("意外的内容块类型：{other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shape,
            vec![
                "text:t0",
                "image:data:image/png;base64,AAA",
                "text:t1",
                "image:data:image/png;base64,BBB",
                "text:t2",
            ]
        );
    }

    #[test]
    fn user_item_with_pdf_uses_native_file_block() {
        // Responses API 文件输入必须用 {"type":"file","file":{...}} 结构，
        // file_data 为 data URI。错误结构（如 input_file/file_url）会被静默丢弃，
        // 导致模型无法感知文件（issue #149）。
        let message = ChatMessage::new(
            MessageRole::User,
            vec![
                MessageContent::Text("总结这份文档".to_string()),
                MessageContent::File(crate::message::FileContent {
                    mime_type: "application/pdf".to_string(),
                    data: "data:application/pdf;base64,JVBERi0xLjQ=".to_string(),
                    title: Some("report.pdf".to_string()),
                }),
            ],
        );
        let item = build_user_item(&message)
            .unwrap()
            .expect("应生成 user item");
        let content = item
            .get("content")
            .and_then(|v| v.as_array())
            .expect("应有 content 数组");

        let file_block = content
            .iter()
            .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("file"))
            .expect("应包含 type=file 的内容块");

        assert_eq!(
            file_block
                .get("file")
                .and_then(|f| f.get("filename"))
                .and_then(|v| v.as_str()),
            Some("report.pdf")
        );
        assert_eq!(
            file_block
                .get("file")
                .and_then(|f| f.get("file_data"))
                .and_then(|v| v.as_str()),
            Some("data:application/pdf;base64,JVBERi0xLjQ=")
        );
        // 不应再出现已废弃的 input_file/file_url 结构
        assert!(
            content
                .iter()
                .all(|v| v.get("type").and_then(|t| t.as_str()) != Some("input_file")),
            "不应使用 input_file 类型"
        );
    }

    #[test]
    fn file_block_filename_falls_back_by_mime() {
        assert_eq!(
            file_filename_with_fallback(&None, "application/pdf"),
            "attachment.pdf"
        );
        assert_eq!(
            file_filename_with_fallback(
                &None,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            ),
            "attachment.xlsx"
        );
        assert_eq!(
            file_filename_with_fallback(&Some("  ".to_string()), "application/pdf"),
            "attachment.pdf"
        );
        assert_eq!(
            file_filename_with_fallback(&Some("用户文档.docx".to_string()), "application/pdf"),
            "用户文档.docx"
        );
    }
}
