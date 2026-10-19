use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::message::{ChatMessage, MessageContent, MessageRole};
use crate::request::{ProviderRequest, ReasoningEffort};
use crate::response::{ProviderResponse, StopReason};
use crate::tool::{ToolChoice, ToolSpec, parse_tool_arguments_or_error};
use crate::usage::TokenUsageData;

const INTERNAL_INJECTION_TOOL_NAME: &str = "plugin_injection";
const INTERNAL_INJECTION_REASONING_CONTENT: &str =
    "内部上下文注入：将外部运行时反馈作为工具结果传回，以便继续处理当前任务。";

pub fn to_deepseek_request(
    req: &ProviderRequest,
) -> Result<tiangong_deepseek::types::ChatCompletionRequest> {
    let messages = build_messages(req)?;
    let thinking = build_thinking(req);
    let reasoning_effort = build_reasoning_effort(req);

    // 官方文档：思考模式不支持 temperature/top_p（设了也不生效）。
    // 思考模式开启（thinking 配置存在且未禁用）时省略这两个参数，保持请求干净。
    let thinking_enabled = thinking
        .as_ref()
        .is_some_and(|t| t.thinking_type == "enabled");

    let mut tool_choice_value = None;
    let mut tools_value = None;
    if !req.tools.is_empty() {
        tools_value = Some(build_tools(&req.tools));
        tool_choice_value = build_tool_choice(req.tool_choice.as_ref());
    }

    Ok(tiangong_deepseek::types::ChatCompletionRequest {
        model: req.model.clone(),
        messages,
        max_tokens: Some(req.max_tokens),
        temperature: if thinking_enabled {
            None
        } else {
            req.temperature
        },
        top_p: if thinking_enabled { None } else { req.top_p },
        stream: None,
        stream_options: None,
        stop: if req.stop_sequences.is_empty() {
            None
        } else {
            Some(json!(req.stop_sequences))
        },
        tools: tools_value,
        tool_choice: tool_choice_value,
        thinking,
        reasoning_effort,
        response_format: build_response_format(req),
        user_id: req.session_id.clone(),
    })
}

/// 从 metadata 中提取 response_format（支持 text / json_object）。
///
/// 上层暂无专用字段，DeepSeek 需要强制 JSON 输出时由调用方在 metadata 写入：
/// `{"response_format": "json_object"}` 或 `{"response_format": {"type": "json_object"}}`。
fn build_response_format(req: &ProviderRequest) -> Option<Value> {
    let fmt = req.metadata.as_ref()?.get("response_format")?;
    match fmt {
        Value::String(s) => Some(json!({ "type": s })),
        Value::Object(_) => Some(fmt.clone()),
        _ => None,
    }
}

fn build_messages(req: &ProviderRequest) -> Result<Vec<tiangong_deepseek::types::ChatMessage>> {
    let mut messages = Vec::new();

    if let Some(system) = req.system.as_ref().filter(|s| !s.trim().is_empty()) {
        messages.push(tiangong_deepseek::types::ChatMessage {
            role: tiangong_deepseek::types::MessageRole::System,
            content: Some(Value::String(system.clone())),
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: None,
            prefix: false,
        });
    }

    for message in &req.messages {
        match message.role {
            MessageRole::System => {}
            MessageRole::User => {
                if let Some(msg) = build_user_message(message) {
                    messages.push(msg);
                }
            }
            MessageRole::Assistant => {
                let text = extract_text(message);
                let tool_calls = build_assistant_tool_calls(message);
                let reasoning_content = extract_thinking(message).or_else(|| {
                    fallback_reasoning_content_for_internal_tool_calls(
                        &tool_calls,
                        !req.reasoning_effort.is_thinking_enabled(),
                    )
                });
                if !text.is_empty() || reasoning_content.is_some() || !tool_calls.is_empty() {
                    // 新版 API 要求 assistant 消息的 content 或 tool_calls 至少有一个非空。
                    // 当仅有 reasoning_content（纯思考轮次）时，content 填空字符串占位。
                    let content_value = if text.is_empty() && tool_calls.is_empty() {
                        Some(Value::String(String::new()))
                    } else if text.is_empty() {
                        None
                    } else {
                        Some(Value::String(text))
                    };
                    messages.push(tiangong_deepseek::types::ChatMessage {
                        role: tiangong_deepseek::types::MessageRole::Assistant,
                        content: content_value,
                        reasoning_content,
                        name: None,
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls)
                        },
                        tool_call_id: None,
                        prefix: false,
                    });
                }
            }
            MessageRole::Tool => {
                for result in message.content.iter().filter_map(|c| match c {
                    MessageContent::ToolResult(r) => Some(r),
                    _ => None,
                }) {
                    let content = match &result.content {
                        crate::tool::ToolResultContent::Text(text) => text.clone(),
                        crate::tool::ToolResultContent::Json(value) => value.to_string(),
                    };
                    messages.push(tiangong_deepseek::types::ChatMessage {
                        role: tiangong_deepseek::types::MessageRole::Tool,
                        content: Some(Value::String(content)),
                        reasoning_content: None,
                        name: None,
                        tool_calls: None,
                        tool_call_id: Some(result.tool_call_id.clone()),
                        prefix: false,
                    });
                }
            }
        }
    }

    Ok(messages)
}

fn build_user_message(message: &ChatMessage) -> Option<tiangong_deepseek::types::ChatMessage> {
    let text = extract_text(message);
    let has_images = message
        .content
        .iter()
        .any(|content| matches!(content, MessageContent::Image(_)));

    if !has_images {
        if text.trim().is_empty() {
            return None;
        }
        return Some(tiangong_deepseek::types::ChatMessage {
            role: tiangong_deepseek::types::MessageRole::User,
            content: Some(Value::String(text)),
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: None,
            prefix: false,
        });
    }

    let mut content = Vec::new();
    for block in &message.content {
        match block {
            MessageContent::Image(image) => content.push(json!({
                "type": "image_url",
                "image_url": { "url": image.data }
            })),
            MessageContent::Text(text) if !text.trim().is_empty() => {
                content.push(json!({"type": "text", "text": text}));
            }
            _ => {}
        }
    }
    Some(tiangong_deepseek::types::ChatMessage {
        role: tiangong_deepseek::types::MessageRole::User,
        content: Some(Value::Array(content)),
        reasoning_content: None,
        name: None,
        tool_calls: None,
        tool_call_id: None,
        prefix: false,
    })
}

fn build_assistant_tool_calls(message: &ChatMessage) -> Vec<tiangong_deepseek::types::ToolCall> {
    message
        .content
        .iter()
        .filter_map(|c| match c {
            MessageContent::ToolCall(tc) => Some(tiangong_deepseek::types::ToolCall {
                id: tc.id.clone(),
                kind: "function".to_string(),
                function: tiangong_deepseek::types::FunctionCall {
                    name: tc.name.clone(),
                    arguments: tc.arguments.to_string(),
                },
            }),
            _ => None,
        })
        .collect()
}

fn extract_thinking(message: &ChatMessage) -> Option<String> {
    let thinking = message
        .content
        .iter()
        .filter_map(|content| match content {
            MessageContent::Thinking(thinking) => Some(thinking.thinking.trim()),
            _ => None,
        })
        .filter(|thinking| !thinking.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    (!thinking.is_empty()).then_some(thinking)
}

fn fallback_reasoning_content_for_internal_tool_calls(
    tool_calls: &[tiangong_deepseek::types::ToolCall],
    thinking_disabled: bool,
) -> Option<String> {
    if thinking_disabled {
        return None;
    }
    let has_tool_calls = !tool_calls.is_empty();
    let only_internal_injections = tool_calls
        .iter()
        .all(|tool_call| tool_call.function.name == INTERNAL_INJECTION_TOOL_NAME);
    (has_tool_calls && only_internal_injections)
        .then(|| INTERNAL_INJECTION_REASONING_CONTENT.to_string())
}

fn build_tools(specs: &[ToolSpec]) -> Vec<tiangong_deepseek::types::ToolSpec> {
    specs
        .iter()
        .map(|spec| tiangong_deepseek::types::ToolSpec {
            kind: "function".to_string(),
            function: tiangong_deepseek::types::FunctionSpec {
                name: spec.name.clone(),
                description: Some(spec.description.clone()),
                parameters: Some(spec.input_schema.clone()),
                strict: false,
            },
        })
        .collect()
}

fn build_tool_choice(choice: Option<&ToolChoice>) -> Option<Value> {
    match choice {
        Some(ToolChoice::Any) => Some(json!("required")),
        Some(ToolChoice::Auto) => Some(json!("auto")),
        Some(ToolChoice::Tool(name)) => Some(json!({
            "type": "function",
            "function": { "name": name }
        })),
        // 明确禁止工具调用（DeepSeek 兼容 OpenAI 的 tool_choice: "none"）。
        Some(ToolChoice::None) => Some(json!("none")),
        None => None,
    }
}

fn build_thinking(req: &ProviderRequest) -> Option<tiangong_deepseek::types::ThinkingConfig> {
    // 官方文档：思考模式由 thinking.type（enabled/disabled）+ reasoning_effort 控制，
    // 不使用 budget_tokens 字段。reasoning_effort 有值即开启思考，无值即关闭。
    Some(tiangong_deepseek::types::ThinkingConfig {
        thinking_type: if req.reasoning_effort.is_thinking_enabled() {
            "enabled"
        } else {
            "disabled"
        }
        .to_string(),
    })
}

fn build_reasoning_effort(
    req: &ProviderRequest,
) -> Option<tiangong_deepseek::types::ReasoningEffort> {
    // 官方 reasoning_effort 取值：low / high / max。
    // 内部 Medium 无对应档位，统一映射到 high（官方默认档）。
    req.reasoning_effort
        .is_thinking_enabled()
        .then(|| match req.reasoning_effort {
            ReasoningEffort::Low => tiangong_deepseek::types::ReasoningEffort::Low,
            ReasoningEffort::Medium | ReasoningEffort::High => {
                tiangong_deepseek::types::ReasoningEffort::High
            }
            ReasoningEffort::Max => tiangong_deepseek::types::ReasoningEffort::Max,
            ReasoningEffort::None => unreachable!("None 已被 is_thinking_enabled 过滤"),
        })
}

fn extract_text(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|c| match c {
            MessageContent::Text(text) => Some(text.clone()),
            MessageContent::ToolResult(result) => Some(match &result.content {
                crate::tool::ToolResultContent::Text(text) => text.clone(),
                crate::tool::ToolResultContent::Json(value) => value.to_string(),
            }),
            MessageContent::File(file) => Some(format!(
                "<attachment title=\"{}\" mime_type=\"{}\">\n{}\n</attachment>",
                file.title.as_deref().unwrap_or("attachment"),
                file.mime_type,
                file.data
            )),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn from_deepseek_response(
    response: tiangong_deepseek::types::ChatCompletionResponse,
    tools_enabled: bool,
) -> Result<ProviderResponse> {
    let choice = response
        .choices
        .first()
        .ok_or_else(|| anyhow!("DeepSeek 响应缺少 choices"))?;

    let text = choice.message.content.as_deref().unwrap_or("").to_string();
    let reasoning_content = choice.message.reasoning_content.clone();

    let mut content = Vec::new();
    if let Some(tool_calls) = &choice.message.tool_calls {
        // 优先使用结构化 tool_calls 字段。
        if !text.trim().is_empty() {
            content.push(MessageContent::Text(text.trim().to_string()));
        }
        for tc in tool_calls {
            let arguments =
                parse_tool_arguments_or_error(&tc.function.name, &tc.id, &tc.function.arguments);
            content.push(MessageContent::ToolCall(crate::tool::ToolCall {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                arguments,
            }));
        }
    } else if tools_enabled {
        // 仅当请求携带 tools 时才做文本协议兜底——无 tools 时（如总结阶段
        // ToolChoice::None）模型不可能走工具调用，兜底只会误伤讨论协议的正常文本。
        match tiangong_deepseek::dsml::parse_dsml_tool_calls(text.trim()) {
            Some(text_calls) if !text_calls.is_empty() => {
                tracing::warn!(
                    count = text_calls.len(),
                    "DeepSeek 返回了文本形式的工具调用，已兜底解析"
                );
                let leftover = tiangong_deepseek::dsml::strip_tool_call_block(text.trim());
                if !leftover.trim().is_empty() {
                    content.push(MessageContent::Text(leftover.trim().to_string()));
                }
                for (idx, call) in text_calls.into_iter().enumerate() {
                    let call_id = format!("textcall_{idx}");
                    let arguments =
                        parse_tool_arguments_or_error(&call.name, &call_id, &call.arguments);
                    content.push(MessageContent::ToolCall(crate::tool::ToolCall {
                        id: call_id,
                        name: call.name,
                        arguments,
                    }));
                }
            }
            _ => {
                if !text.trim().is_empty() {
                    content.push(MessageContent::Text(text.trim().to_string()));
                }
            }
        }
    } else if !text.trim().is_empty() {
        // 无 tools 时文本原样保留，不做协议兜底。
        content.push(MessageContent::Text(text.trim().to_string()));
    }

    let usage = parse_usage(&response.usage);
    let raw = serde_json::to_value(&response).unwrap_or_default();
    let id = response.id;
    let model = response.model;

    Ok(ProviderResponse {
        id: Some(id),
        model: Some(model),
        assistant_message: ChatMessage::new(MessageRole::Assistant, content),
        reasoning_content,
        stop_reason: Some(map_stop_reason(&choice.finish_reason)),
        usage: Some(usage),
        raw: Some(raw),
    })
}

fn parse_usage(usage: &tiangong_deepseek::types::Usage) -> TokenUsageData {
    TokenUsageData {
        prompt_tokens: usage.prompt_tokens as usize,
        completion_tokens: usage.completion_tokens as usize,
        total_tokens: usage.total_tokens as usize,
        prompt_cache_hit_tokens: usage.prompt_cache_hit_tokens.map(|v| v as usize),
        prompt_cache_miss_tokens: usage.prompt_cache_miss_tokens.map(|v| v as usize),
    }
}

pub fn parse_stream_usage(usage: &tiangong_deepseek::types::Usage) -> TokenUsageData {
    parse_usage(usage)
}

pub(super) fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" | "insufficient_system_resource" => StopReason::Other(reason.to_string()),
        _ => StopReason::Other(reason.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ThinkingContent;
    use crate::message::{ImageContent, MessageContent};
    use crate::tool::ToolCall;

    #[test]
    fn multimodal_user_content_preserves_interleaved_order() {
        let message = ChatMessage::new(
            MessageRole::User,
            vec![
                MessageContent::Text("图一".to_string()),
                MessageContent::Image(ImageContent {
                    mime_type: "image/png".to_string(),
                    data: "data:image/png;base64,A".to_string(),
                }),
                MessageContent::Text("图二".to_string()),
                MessageContent::Image(ImageContent {
                    mime_type: "image/png".to_string(),
                    data: "data:image/png;base64,B".to_string(),
                }),
            ],
        );

        let mapped = build_user_message(&message).expect("user message");
        let content = mapped.content.unwrap();
        let content = content.as_array().unwrap();
        assert_eq!(content[0]["text"], json!("图一"));
        assert_eq!(
            content[1]["image_url"]["url"],
            json!("data:image/png;base64,A")
        );
        assert_eq!(content[2]["text"], json!("图二"));
        assert_eq!(
            content[3]["image_url"]["url"],
            json!("data:image/png;base64,B")
        );
    }

    fn response_with_arguments(
        arguments: &str,
    ) -> tiangong_deepseek::types::ChatCompletionResponse {
        tiangong_deepseek::types::ChatCompletionResponse {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "test-model".to_string(),
            choices: vec![tiangong_deepseek::types::Choice {
                index: 0,
                message: tiangong_deepseek::types::ChoiceMessage {
                    role: tiangong_deepseek::types::MessageRole::Assistant,
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![tiangong_deepseek::types::ToolCall {
                        id: "call_bad".to_string(),
                        kind: "function".to_string(),
                        function: tiangong_deepseek::types::FunctionCall {
                            name: "read_file".to_string(),
                            arguments: arguments.to_string(),
                        },
                    }]),
                },
                finish_reason: "tool_calls".to_string(),
                logprobs: None,
            }],
            usage: tiangong_deepseek::types::Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                prompt_cache_hit_tokens: None,
                prompt_cache_miss_tokens: None,
                completion_tokens_details: None,
            },
            system_fingerprint: String::new(),
        }
    }

    #[test]
    fn assistant_thinking_is_passed_back_as_reasoning_content() {
        let req = ProviderRequest {
            session_id: None,
            model: "deepseek-v4-pro".to_string(),
            system: None,
            messages: vec![ChatMessage::new(
                MessageRole::Assistant,
                vec![
                    MessageContent::Thinking(ThinkingContent {
                        thinking: "需要先查询当前数据".to_string(),
                        signature: None,
                    }),
                    MessageContent::ToolCall(ToolCall {
                        id: "call_1".to_string(),
                        name: "current_time".to_string(),
                        arguments: json!({}),
                    }),
                ],
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

        let request = to_deepseek_request(&req).expect("request");
        let assistant = request
            .messages
            .iter()
            .find(|message| message.role == tiangong_deepseek::types::MessageRole::Assistant)
            .expect("assistant message");

        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("需要先查询当前数据")
        );
        assert!(assistant.tool_calls.is_some());
    }

    #[test]
    fn internal_plugin_injection_tool_call_gets_reasoning_fallback() {
        let req = ProviderRequest {
            session_id: None,
            model: "deepseek-v4-pro".to_string(),
            system: None,
            messages: vec![
                ChatMessage::text(MessageRole::User, "继续整理浏览器数据"),
                ChatMessage::new(
                    MessageRole::Assistant,
                    vec![MessageContent::ToolCall(ToolCall {
                        id: "inj_1".to_string(),
                        name: INTERNAL_INJECTION_TOOL_NAME.to_string(),
                        arguments: json!({"source": "browser_data"}),
                    })],
                ),
                ChatMessage::new(
                    MessageRole::Tool,
                    vec![MessageContent::ToolResult(crate::tool::ToolResult {
                        tool_call_id: "inj_1".to_string(),
                        content: crate::tool::ToolResultContent::Text(
                            "数据来源：browser_data\n相关数据：世界杯战况".to_string(),
                        ),
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
            reasoning_effort: ReasoningEffort::Medium,
        };

        let request = to_deepseek_request(&req).expect("request");
        let assistant = request
            .messages
            .iter()
            .find(|message| message.role == tiangong_deepseek::types::MessageRole::Assistant)
            .expect("assistant message");

        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some(INTERNAL_INJECTION_REASONING_CONTENT)
        );
        assert_eq!(
            assistant
                .tool_calls
                .as_ref()
                .and_then(|calls| calls.first())
                .map(|call| call.function.name.as_str()),
            Some(INTERNAL_INJECTION_TOOL_NAME)
        );
    }

    #[test]
    fn internal_plugin_injection_skips_reasoning_fallback_when_thinking_disabled() {
        let req = ProviderRequest {
            session_id: None,
            model: "deepseek-v4-pro".to_string(),
            system: None,
            messages: vec![ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageContent::ToolCall(ToolCall {
                    id: "inj_1".to_string(),
                    name: INTERNAL_INJECTION_TOOL_NAME.to_string(),
                    arguments: json!({"source": "browser_data"}),
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

        let request = to_deepseek_request(&req).expect("request");
        let assistant = request
            .messages
            .iter()
            .find(|message| message.role == tiangong_deepseek::types::MessageRole::Assistant)
            .expect("assistant message");

        assert!(assistant.reasoning_content.is_none());
        assert!(assistant.tool_calls.is_some());
    }

    #[test]
    fn invalid_tool_arguments_become_parse_error() {
        let response =
            from_deepseek_response(response_with_arguments("{\"path\":"), true).expect("response");
        let MessageContent::ToolCall(call) = &response.assistant_message.content[0] else {
            panic!("expected tool call");
        };
        assert!(
            call.arguments
                .get("__parse_error")
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains("工具参数 JSON 无效"))
        );
        assert_eq!(
            call.arguments
                .get("__raw_args_preview")
                .and_then(Value::as_str),
            Some("{\"path\":")
        );
    }

    #[test]
    fn empty_tool_arguments_become_empty_object() {
        let response = from_deepseek_response(response_with_arguments(""), true).expect("response");
        let MessageContent::ToolCall(call) = &response.assistant_message.content[0] else {
            panic!("expected tool call");
        };
        assert_eq!(
            call.arguments,
            serde_json::json!({}),
            "空参数应按无参调用处理（空对象），不再构成解析错误"
        );
    }

    #[test]
    fn tool_choice_none_maps_to_none_string() {
        // ToolChoice::None 必须映射为 "none"，在提供 tools schema 的同时禁止调用工具。
        assert_eq!(
            build_tool_choice(Some(&ToolChoice::None)),
            Some(json!("none"))
        );
    }

    #[test]
    fn reasoning_only_assistant_gets_empty_content_placeholder() {
        // 仅含思考内容（纯思考轮次）、无 text 无 tool_calls 时，
        // content 必须填空字符串占位，否则新版 API 报 "content or tool_calls must be set"。
        let req = ProviderRequest {
            session_id: None,
            model: "deepseek-v4-pro".to_string(),
            system: None,
            messages: vec![ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageContent::Thinking(ThinkingContent {
                    thinking: "纯思考".to_string(),
                    signature: None,
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
        let request = to_deepseek_request(&req).expect("request");
        let assistant = request
            .messages
            .iter()
            .find(|m| m.role == tiangong_deepseek::types::MessageRole::Assistant)
            .expect("assistant message");
        // content 不应为 None（否则 API 报错），应为空字符串占位。
        assert_eq!(
            assistant.content.as_ref().and_then(|v| v.as_str()),
            Some("")
        );
        assert_eq!(assistant.reasoning_content.as_deref(), Some("纯思考"));
    }

    #[test]
    fn non_stream_skips_text_protocol_fallback_without_tools() {
        // 无 tools 时（总结阶段），即使 content 含 DSML 字面量也不应误判为工具调用。
        let response = tiangong_deepseek::types::ChatCompletionResponse {
            id: "test".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "deepseek-v4-pro".to_string(),
            choices: vec![tiangong_deepseek::types::Choice {
                index: 0,
                message: tiangong_deepseek::types::ChoiceMessage {
                    role: tiangong_deepseek::types::MessageRole::Assistant,
                    content: Some("讨论 DSML 协议：\n</｜｜DSML｜｜tool_calls>".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                },
                finish_reason: "stop".to_string(),
                logprobs: None,
            }],
            usage: tiangong_deepseek::types::Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                prompt_cache_hit_tokens: None,
                prompt_cache_miss_tokens: None,
                completion_tokens_details: None,
            },
            system_fingerprint: String::new(),
        };
        // tools_enabled=false → 文本原样保留，不解析为工具调用。
        let result = from_deepseek_response(response, false).expect("response");
        assert_eq!(result.assistant_message.content.len(), 1);
        let MessageContent::Text(text) = &result.assistant_message.content[0] else {
            panic!("应为纯文本");
        };
        assert!(text.contains("讨论 DSML 协议"), "文本应原样保留：{text}");
        assert!(
            text.contains("</｜｜DSML｜｜tool_calls>"),
            "DSML 字面量不应被剥离：{text}"
        );
    }

    #[test]
    fn reasoning_effort_low_maps_to_low() {
        let req = ProviderRequest {
            session_id: None,
            model: "deepseek-v4-flash".to_string(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::Low,
        };
        let request = to_deepseek_request(&req).expect("request");
        assert_eq!(
            request.reasoning_effort,
            Some(tiangong_deepseek::types::ReasoningEffort::Low)
        );
    }

    #[test]
    fn reasoning_effort_medium_high_max_mapping() {
        fn map(effort: ReasoningEffort) -> tiangong_deepseek::types::ReasoningEffort {
            let req = ProviderRequest {
                session_id: None,
                model: "deepseek-v4-pro".to_string(),
                system: None,
                messages: Vec::new(),
                tools: Vec::new(),
                tool_choice: None,
                max_tokens: 1024,
                temperature: None,
                top_p: None,
                stop_sequences: Vec::new(),
                metadata: None,
                reasoning_effort: effort,
            };
            let request = to_deepseek_request(&req).expect("request");
            request.reasoning_effort.expect("应产出 effort")
        }
        use tiangong_deepseek::types::ReasoningEffort as Ds;
        assert_eq!(map(ReasoningEffort::Medium), Ds::High);
        assert_eq!(map(ReasoningEffort::High), Ds::High);
        assert_eq!(map(ReasoningEffort::Max), Ds::Max);
    }

    #[test]
    fn temperature_top_p_omitted_when_thinking_enabled() {
        let req = ProviderRequest {
            session_id: None,
            model: "deepseek-v4-pro".to_string(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 1024,
            temperature: Some(0.7),
            top_p: Some(0.9),
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::High,
        };
        let request = to_deepseek_request(&req).expect("request");
        assert_eq!(request.temperature, None, "思考模式不应发送 temperature");
        assert_eq!(request.top_p, None, "思考模式不应发送 top_p");
    }

    #[test]
    fn temperature_top_p_sent_when_thinking_disabled() {
        let req = ProviderRequest {
            session_id: None,
            model: "deepseek-v4-pro".to_string(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 1024,
            temperature: Some(0.7),
            top_p: Some(0.9),
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::None,
        };
        let request = to_deepseek_request(&req).expect("request");
        assert_eq!(request.temperature, Some(0.7));
        assert_eq!(request.top_p, Some(0.9));
    }

    #[test]
    fn response_format_from_metadata_string() {
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "response_format".to_string(),
            Value::String("json_object".to_string()),
        );
        let req = ProviderRequest {
            session_id: None,
            model: "deepseek-v4-pro".to_string(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: Some(metadata),
            reasoning_effort: ReasoningEffort::None,
        };
        let request = to_deepseek_request(&req).expect("request");
        assert_eq!(
            request.response_format,
            Some(json!({ "type": "json_object" }))
        );
    }

    #[test]
    fn response_format_absent_without_metadata() {
        let req = ProviderRequest {
            session_id: None,
            model: "deepseek-v4-pro".to_string(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: ReasoningEffort::None,
        };
        let request = to_deepseek_request(&req).expect("request");
        assert_eq!(request.response_format, None);
    }
}
