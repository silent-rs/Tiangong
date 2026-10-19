use anyhow::{Context, Result, anyhow};
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessage,
    ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequestArgs, FunctionCall,
};
use serde_json::{Value, json};

use crate::message::{ChatMessage, MessageContent, MessageRole};
use crate::request::{ProviderRequest, ReasoningEffort};
use crate::response::{ProviderResponse, StopReason};
use crate::tool::{ToolChoice, ToolSpec, parse_tool_arguments_or_error};
use crate::usage::TokenUsageData;

pub fn normalize_api_base(base_url: &str) -> Result<String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("API_BASE_URL 不能为空"));
    }
    let cleaned = trimmed.trim_end_matches('/');
    let cleaned = cleaned.strip_suffix("/chat/completions").unwrap_or(cleaned);
    Ok(cleaned.to_string())
}

pub fn build_request_json(req: &ProviderRequest, stream: bool) -> Result<Value> {
    let messages = build_openai_messages(req)?;
    let mut request_args_binding = CreateChatCompletionRequestArgs::default();
    let mut request_args = request_args_binding
        .model(req.model.clone())
        .messages(Vec::<ChatCompletionRequestMessage>::new())
        .stream(stream);
    if let Some(temperature) = req.temperature {
        request_args = request_args.temperature(temperature);
    }
    let request = request_args.build().context("构建 OpenAI 请求失败")?;

    let mut payload = serde_json::to_value(request).context("序列化 OpenAI 请求失败")?;
    payload["messages"] = Value::Array(messages);
    inject_max_tokens_config(&mut payload, req.max_tokens);
    if stream {
        inject_stream_usage_option(&mut payload);
    }
    if let Some(temperature) = req.temperature {
        inject_temperature_config(&mut payload, temperature)?;
    }
    if !req.tools.is_empty() {
        inject_function_tools(&mut payload, &req.tools, req.tool_choice.as_ref());
    }
    match req.reasoning_effort.is_thinking_enabled() {
        true => {
            inject_reasoning_effort(&mut payload, req.reasoning_effort);
            inject_thinking_enabled(&mut payload);
        }
        false => inject_thinking_disabled(&mut payload),
    }
    Ok(payload)
}

fn build_openai_messages(req: &ProviderRequest) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    if let Some(system) = req.system.as_ref().filter(|value| !value.trim().is_empty()) {
        messages.push(serde_json::to_value(ChatCompletionRequestMessage::System(
            ChatCompletionRequestSystemMessageArgs::default()
                .content(system.clone())
                .build()
                .context("构建 system 消息失败")?,
        ))?);
    }

    for message in &req.messages {
        match message.role {
            MessageRole::System => {}
            MessageRole::User => {
                if let Some(message) = build_openai_user_message(message)? {
                    messages.push(serde_json::to_value(message)?);
                }
            }
            MessageRole::Tool => {
                for result in message.content.iter().filter_map(|content| match content {
                    MessageContent::ToolResult(result) => Some(result),
                    _ => None,
                }) {
                    let content = match &result.content {
                        crate::tool::ToolResultContent::Text(text) => text.clone(),
                        crate::tool::ToolResultContent::Json(value) => value.to_string(),
                    };
                    messages.push(serde_json::to_value(ChatCompletionRequestMessage::Tool(
                        ChatCompletionRequestToolMessage {
                            content: content.into(),
                            tool_call_id: result.tool_call_id.clone(),
                        },
                    ))?);
                }
            }
            MessageRole::Assistant => {
                let text = extract_message_text(message);
                let mut reasoning = message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        MessageContent::Thinking(thinking) => Some(thinking.thinking.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .concat();
                let calls = message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        MessageContent::ToolCall(call) => Some(call),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                // 内部注入并非模型输出，DeepSeek 思考模式仍要求其携带思考字段。
                if reasoning.is_empty()
                    && req.model.to_ascii_lowercase().contains("deepseek")
                    && req.reasoning_effort.is_thinking_enabled()
                    && !calls.is_empty()
                    && calls.iter().all(|call| call.name == "plugin_injection")
                {
                    reasoning =
                        "内部上下文注入：将外部运行时反馈作为工具结果传回，以便继续处理当前任务。"
                            .into();
                }
                let tool_calls = message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        MessageContent::ToolCall(tool_call) => {
                            Some(ChatCompletionMessageToolCalls::Function(
                                ChatCompletionMessageToolCall {
                                    id: tool_call.id.clone(),
                                    function: FunctionCall {
                                        name: tool_call.name.clone(),
                                        arguments: tool_call.arguments.to_string(),
                                    },
                                },
                            ))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !text.is_empty() || !tool_calls.is_empty() || !reasoning.is_empty() {
                    let mut args = ChatCompletionRequestAssistantMessageArgs::default();
                    if !text.is_empty() || tool_calls.is_empty() {
                        args.content(text);
                    }
                    if !tool_calls.is_empty() {
                        args.tool_calls(tool_calls);
                    }
                    // SDK 消息类型不包含兼容服务的 reasoning_content 扩展字段。
                    let mut value = serde_json::to_value(ChatCompletionRequestMessage::Assistant(
                        args.build().context("构建 assistant 消息失败")?,
                    ))?;
                    if !reasoning.is_empty() {
                        value["reasoning_content"] = Value::String(reasoning);
                    }
                    messages.push(value);
                }
            }
        }
    }

    Ok(messages)
}

fn build_openai_user_message(
    message: &ChatMessage,
) -> Result<Option<ChatCompletionRequestMessage>> {
    let has_images = message
        .content
        .iter()
        .any(|content| matches!(content, MessageContent::Image(_)));
    let text = extract_message_text(message);
    if !has_images {
        if text.is_empty() {
            return Ok(None);
        }
        return Ok(Some(
            ChatCompletionRequestUserMessageArgs::default()
                .content(text)
                .build()
                .context("构建 user 消息失败")?
                .into(),
        ));
    }

    let mut content = Vec::new();
    for block in &message.content {
        match block {
            MessageContent::Image(image) => content.push(json!({
                "type": "image_url",
                "image_url": { "url": image.data }
            })),
            _ => {
                if let Some(text) = user_text_part(block)
                    && !text.trim().is_empty()
                {
                    content.push(json!({ "type": "text", "text": text }));
                }
            }
        }
    }
    Ok(Some(
        serde_json::from_value(json!({
            "role": "user",
            "content": content
        }))
        .context("构建 OpenAI 多模态 user 消息失败")?,
    ))
}

fn user_text_part(content: &MessageContent) -> Option<String> {
    match content {
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
        MessageContent::Thinking(_)
        | MessageContent::RedactedThinking(_)
        | MessageContent::ToolCall(_)
        | MessageContent::Image(_) => None,
    }
}

fn extract_message_text(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(user_text_part)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn parse_complete_response(payload: &Value) -> Result<ProviderResponse> {
    let choice = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| anyhow!("OpenAI 响应缺少 choices"))?;

    let text = choice
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let reasoning_content = choice
        .get("message")
        .and_then(|message| message.get("reasoning_content"))
        .and_then(Value::as_str)
        .map(|value| value.to_string());

    let tool_calls = choice
        .get("message")
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut content = Vec::new();
    if !text.trim().is_empty() {
        content.push(MessageContent::Text(strip_think_tags(text.trim())));
    }
    for tool_call in tool_calls {
        let id = tool_call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = tool_call
            .get("function")
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let raw_args = tool_call
            .get("function")
            .and_then(|v| v.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let arguments = parse_tool_arguments_or_error(name, id, raw_args);
        if !name.is_empty() {
            content.push(MessageContent::ToolCall(crate::tool::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments,
            }));
        }
    }

    let usage = payload.get("usage").map(parse_usage);

    Ok(ProviderResponse {
        id: payload
            .get("id")
            .and_then(Value::as_str)
            .map(|v| v.to_string()),
        model: payload
            .get("model")
            .and_then(Value::as_str)
            .map(|v| v.to_string()),
        assistant_message: ChatMessage::new(MessageRole::Assistant, content),
        reasoning_content,
        stop_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(map_stop_reason),
        usage,
        raw: Some(payload.clone()),
    })
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        other => StopReason::Other(other.to_string()),
    }
}

pub fn parse_usage(usage: &Value) -> TokenUsageData {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let completion = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or((prompt + completion) as u64) as usize;
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .map(|value| value as usize);
    TokenUsageData {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        prompt_cache_hit_tokens: cached,
        prompt_cache_miss_tokens: usage
            .get("prompt_cache_miss_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .or_else(|| cached.map(|hit| prompt.saturating_sub(hit))),
    }
}

fn inject_stream_usage_option(payload: &mut Value) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    obj.insert("stream_options".to_string(), json!({"include_usage": true}));
}

fn inject_max_tokens_config(payload: &mut Value, max_tokens: u32) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    obj.insert("max_tokens".to_string(), json!(max_tokens));
}

fn inject_temperature_config(payload: &mut Value, temperature: f32) -> Result<()> {
    let Some(obj) = payload.as_object_mut() else {
        return Ok(());
    };
    let number = serde_json::Number::from_f64(temperature as f64)
        .ok_or_else(|| anyhow!("temperature 无效"))?;
    obj.insert("temperature".to_string(), Value::Number(number));
    Ok(())
}

fn inject_function_tools(
    payload: &mut Value,
    functions: &[ToolSpec],
    tool_choice: Option<&ToolChoice>,
) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    let tools = functions
        .iter()
        .map(|spec| {
            json!({
                "type": "function",
                "function": {
                    "name": spec.name,
                    "description": spec.description,
                    "parameters": spec.input_schema,
                }
            })
        })
        .collect::<Vec<_>>();
    obj.insert("tools".to_string(), Value::Array(tools));
    match tool_choice {
        Some(ToolChoice::Any) => {
            obj.insert(
                "tool_choice".to_string(),
                Value::String("required".to_string()),
            );
        }
        Some(ToolChoice::Tool(name)) => {
            obj.insert(
                "tool_choice".to_string(),
                json!({
                    "type": "function",
                    "function": { "name": name },
                }),
            );
        }
        // 明确禁止工具调用：显式 tool_choice: "none"。
        // OpenAI Chat Completions 原生支持；提供 tools schema 但不允许调用，
        // 用于保持 KV cache 前缀一致同时杜绝误调用（如总结阶段）。
        Some(ToolChoice::None) => {
            obj.insert("tool_choice".to_string(), Value::String("none".to_string()));
        }
        // 兼容 vLLM / 部分 OpenAI-compatible 后端：
        // 显式 `tool_choice: "auto"` 可能要求服务端开启
        // --enable-auto-tool-choice 和 --tool-call-parser。
        // 省略该字段时，OpenAI Chat Completions 语义仍会在提供 tools 后
        // 使用默认自动选择策略，且不会触发这些后端的 400。
        Some(ToolChoice::Auto) => {}
        None => {}
    }
}

fn inject_reasoning_effort(payload: &mut Value, effort: ReasoningEffort) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    let effort_str = match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Max => "max",
        ReasoningEffort::None => return,
    };
    obj.insert("reasoning_effort".to_string(), json!(effort_str));
}

fn inject_thinking_disabled(payload: &mut Value) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    obj.insert("thinking".to_string(), json!({"type": "disabled"}));
}

fn inject_thinking_enabled(payload: &mut Value) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    // 只表达"开启思考"，不带预算——思考强度由上游模型自行决定。
    obj.insert("thinking".to_string(), json!({"type": "enabled"}));
}

pub fn strip_think_tags(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find("<think>") {
        result.push_str(&remaining[..start]);
        if let Some(end) = remaining[start..].find("</think>") {
            remaining = &remaining[start + end + 8..];
        } else {
            return result;
        }
    }
    result.push_str(remaining);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ImageContent, MessageContent};

    #[test]
    fn reasoning_history_survives_rounds_reload_and_internal_injection() {
        let mut req: ProviderRequest = serde_json::from_value(json!({
            "model":"deepseek-v4-flash", "system":"固定提示", "messages":[],
            "max_tokens":1024, "temperature":null, "top_p":null,
            "reasoning_effort":"high", "tools":[{
                "name":"read_file", "description":"读取", "input_schema":{"type":"object"}
            }]
        }))
        .unwrap();
        let mut prefix = Vec::new();
        for round in 0..4 {
            req.messages.push(ChatMessage::text(
                MessageRole::User,
                format!("轮次 {round}"),
            ));
            for (index, tool_name) in [Some("read_file"), None, Some("plugin_injection")]
                .into_iter()
                .enumerate()
            {
                let reasoning = format!("  思考 {round}/{index}\n保持空白\n");
                let mut content = Vec::new();
                if tool_name != Some("plugin_injection") {
                    content.push(MessageContent::Thinking(crate::message::ThinkingContent {
                        thinking: reasoning.clone(),
                        signature: None,
                    }));
                }
                let id = format!("call_{round}_{index}");
                if let Some(name) = tool_name {
                    content.push(MessageContent::ToolCall(crate::tool::ToolCall {
                        id: id.clone(),
                        name: name.into(),
                        arguments: json!({"path":"a.rs"}),
                    }));
                }
                req.messages
                    .push(ChatMessage::new(MessageRole::Assistant, content));
                for stream in [false, true] {
                    let payload = build_request_json(&req, stream).unwrap();
                    let messages = payload["messages"].as_array().unwrap();
                    assert_eq!(&messages[..prefix.len()], prefix.as_slice());
                    let assistant = messages.last().unwrap();
                    assert_eq!(assistant["role"], "assistant");
                    if tool_name != Some("plugin_injection") {
                        assert_eq!(assistant["reasoning_content"], reasoning);
                    } else {
                        assert!(
                            assistant["reasoning_content"]
                                .as_str()
                                .unwrap()
                                .contains("内部上下文注入")
                        );
                    }
                    prefix = messages.clone();
                }
                if tool_name.is_some() {
                    req.messages.push(ChatMessage::new(
                        MessageRole::Tool,
                        vec![MessageContent::ToolResult(crate::tool::ToolResult {
                            tool_call_id: id,
                            content: crate::tool::ToolResultContent::Text("结果".into()),
                            is_error: round == 2,
                        })],
                    ));
                }
                req = serde_json::from_slice(&serde_json::to_vec(&req).unwrap()).unwrap();
            }
        }
        req.messages.push(ChatMessage::text(
            MessageRole::Assistant,
            "没有思考的旧消息",
        ));
        let payload = build_request_json(&req, false).unwrap();
        assert!(
            payload["messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .get("reasoning_content")
                .is_none()
        );
        for (model, effort) in [
            ("other-model", ReasoningEffort::High),
            ("deepseek-v4-flash", ReasoningEffort::None),
        ] {
            req.model = model.into();
            req.reasoning_effort = effort;
            let payload = build_request_json(&req, false).unwrap();
            for message in payload["messages"].as_array().unwrap() {
                if message["tool_calls"][0]["function"]["name"] == "plugin_injection" {
                    assert!(message.get("reasoning_content").is_none());
                }
            }
        }
    }

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

        let mapped = build_openai_user_message(&message)
            .expect("mapping")
            .expect("user message");
        let value = serde_json::to_value(mapped).unwrap();
        let content = value["content"].as_array().unwrap();
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

    #[test]
    fn invalid_tool_arguments_become_parse_error() {
        let payload = json!({
            "id": "chatcmpl-test",
            "model": "test-model",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_bad",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "total_tokens": 2
            }
        });

        let response = parse_complete_response(&payload).expect("response should parse");
        let MessageContent::ToolCall(call) = &response.assistant_message.content[0] else {
            panic!("expected tool call");
        };
        assert_eq!(call.name, "read_file");
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
    fn empty_tool_arguments_become_parse_error() {
        let payload = json!({
            "id": "chatcmpl-test",
            "model": "test-model",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_empty",
                        "type": "function",
                        "function": {
                            "name": "run_shell",
                            "arguments": ""
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });

        let response = parse_complete_response(&payload).expect("response should parse");
        let MessageContent::ToolCall(call) = &response.assistant_message.content[0] else {
            panic!("expected tool call");
        };
        assert_eq!(call.name, "run_shell");
        assert_eq!(
            call.arguments,
            serde_json::json!({}),
            "空参数应按无参调用处理（空对象），不再构成解析错误"
        );
    }

    #[test]
    fn tool_choice_none_injects_explicit_none() {
        // ToolChoice::None 必须显式注入 tool_choice: "none"，
        // 在提供 tools schema 的同时禁止模型调用工具（如总结阶段）。
        let mut payload = json!({});
        let tools = vec![ToolSpec {
            name: "read_file".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
        }];
        inject_function_tools(&mut payload, &tools, Some(&ToolChoice::None));
        assert_eq!(payload["tool_choice"], json!("none"));
        // tools schema 仍然注入，保持 KV cache 前缀一致
        assert!(payload["tools"].is_array());
    }
}
