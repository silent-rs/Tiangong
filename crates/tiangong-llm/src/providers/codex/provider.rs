use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use serde_json::Value;

use crate::error::LlmError;
use crate::message::{ChatMessage, MessageContent, MessageRole, ThinkingContent};
use crate::model::ProviderModelInfo;
use crate::provider::{LlmProvider, ProviderCapabilities};
use crate::providers::openai::mapping::{build_request_json, parse_complete_response};
use crate::providers::openai::{ResponsesStreamResponse, map_responses_stream};
use crate::request::ProviderRequest;
use crate::response::ProviderResponse;
use crate::stream::{ProviderStream, ProviderStreamEvent};

use super::client::CodexClient;
use super::config::CodexConfig;

/// Codex（ChatGPT 账号）provider：请求结构同 Responses API，鉴权来自 OAuth 登录态。
#[derive(Clone)]
pub struct CodexProvider {
    client: CodexClient,
}

impl CodexProvider {
    pub fn new(config: CodexConfig) -> Self {
        Self {
            client: CodexClient::new(config),
        }
    }
}

/// 按 Codex 后端约束改写 Responses 请求体（对齐 Codex CLI / opencode）：
/// - 必须 `stream=true`、`store=false`；
/// - 不支持 `max_output_tokens` / `temperature` / `top_p`；
/// - 函数工具统一 `strict=false`，兼容不满足结构化输出约束的动态 schema。
fn adapt_payload(payload: &mut Value) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    for key in ["max_output_tokens", "temperature", "top_p"] {
        object.remove(key);
    }
    object.insert("store".to_string(), Value::Bool(false));
    object.insert("stream".to_string(), Value::Bool(true));
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) == Some("function")
                && let Some(tool) = tool.as_object_mut()
            {
                tool.insert("strict".to_string(), Value::Bool(false));
            }
        }
    }
}

/// 把统一流事件聚合为完整响应（Codex 后端只接受流式请求，`complete` 经此实现）。
async fn collect_stream_response(mut stream: ProviderStream) -> Result<ProviderResponse, LlmError> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: Vec<(String, String, String)> = Vec::new();
    let mut usage = None;
    let mut stop_reason = None;
    while let Some(event) = stream.next().await {
        match event? {
            ProviderStreamEvent::TextDelta(delta) => text.push_str(&delta),
            ProviderStreamEvent::ReasoningDelta(delta) => reasoning.push_str(&delta),
            ProviderStreamEvent::ToolCallStart(call) => {
                let args = if call.arguments.is_null() || call.arguments == serde_json::json!({}) {
                    String::new()
                } else {
                    call.arguments.to_string()
                };
                calls.push((call.id, call.name, args));
            }
            ProviderStreamEvent::ToolCallDelta {
                call_id,
                partial_json,
            } => {
                if let Some(entry) = calls.iter_mut().find(|(id, _, _)| *id == call_id) {
                    entry.2.push_str(&partial_json);
                }
            }
            ProviderStreamEvent::Usage(value) => usage = Some(value),
            ProviderStreamEvent::MessageEnd {
                stop_reason: reason,
            } => {
                if reason.is_some() {
                    stop_reason = reason;
                }
            }
            ProviderStreamEvent::Error(message) => {
                return Err(LlmError::Provider {
                    provider: "codex",
                    message,
                });
            }
            _ => {}
        }
    }
    let mut content = Vec::new();
    if !reasoning.trim().is_empty() {
        content.push(MessageContent::Thinking(ThinkingContent {
            thinking: reasoning.clone(),
            signature: None,
        }));
    }
    if !text.trim().is_empty() {
        content.push(MessageContent::Text(text.trim().to_string()));
    }
    for (id, name, args) in calls {
        let arguments = if args.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&args).unwrap_or_else(|_| serde_json::json!({}))
        };
        content.push(MessageContent::ToolCall(crate::tool::ToolCall {
            id,
            name,
            arguments,
        }));
    }
    Ok(ProviderResponse {
        id: None,
        model: None,
        assistant_message: ChatMessage {
            role: MessageRole::Assistant,
            content,
        },
        reasoning_content: (!reasoning.trim().is_empty()).then_some(reasoning),
        stop_reason,
        usage,
        raw: None,
    })
}

#[async_trait]
impl LlmProvider for CodexProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            system_prompt: true,
            list_models: true,
        }
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, LlmError> {
        collect_stream_response(self.stream(req).await?).await
    }

    async fn stream(&self, req: ProviderRequest) -> Result<ProviderStream, LlmError> {
        let model = req.model.clone();
        let mut payload = build_request_json(&req, true)
            .map_err(|err| LlmError::InvalidRequest(err.to_string()))?;
        adapt_payload(&mut payload);
        match self.client.stream(&model, payload).await? {
            // 服务端返回一次性 JSON：复用非流式解析，合成等价的流事件序列。
            ResponsesStreamResponse::Complete(value) => {
                let response =
                    parse_complete_response(&value).map_err(|err| LlmError::Provider {
                        provider: "codex",
                        message: err.to_string(),
                    })?;
                Ok(Box::pin(stream::iter(
                    crate::stream::complete_response_events(response),
                )))
            }
            ResponsesStreamResponse::Sse(stream) => Ok(map_responses_stream(stream)),
        }
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        self.client.list_models().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapt_payload_strips_unsupported_fields_and_relaxes_tools() {
        let mut payload = serde_json::json!({
            "model": "gpt-5.5",
            "max_output_tokens": 100,
            "temperature": 0.2,
            "top_p": 0.9,
            "prompt_cache_key": "s1",
            "tools": [{ "type": "function", "name": "t", "parameters": {} }]
        });
        adapt_payload(&mut payload);
        assert_eq!(
            payload,
            serde_json::json!({
                "model": "gpt-5.5",
                "prompt_cache_key": "s1",
                "store": false,
                "stream": true,
                "tools": [{ "type": "function", "name": "t", "parameters": {}, "strict": false }]
            })
        );
    }

    #[tokio::test]
    async fn collect_stream_response_aggregates_text_reasoning_and_tool_calls() {
        let events = vec![
            Ok(ProviderStreamEvent::ReasoningDelta("想".to_string())),
            Ok(ProviderStreamEvent::TextDelta("你".to_string())),
            Ok(ProviderStreamEvent::TextDelta("好".to_string())),
            Ok(ProviderStreamEvent::ToolCallStart(crate::tool::ToolCall {
                id: "c1".to_string(),
                name: "get_time".to_string(),
                arguments: serde_json::json!({}),
            })),
            Ok(ProviderStreamEvent::ToolCallDelta {
                call_id: "c1".to_string(),
                partial_json: r#"{"tz":"#.to_string(),
            }),
            Ok(ProviderStreamEvent::ToolCallDelta {
                call_id: "c1".to_string(),
                partial_json: r#""UTC"}"#.to_string(),
            }),
        ];
        let response = collect_stream_response(Box::pin(stream::iter(events)))
            .await
            .unwrap();
        assert_eq!(response.reasoning_content.as_deref(), Some("想"));
        let content = &response.assistant_message.content;
        assert!(matches!(&content[1], MessageContent::Text(t) if t == "你好"));
        assert!(matches!(
            &content[2],
            MessageContent::ToolCall(call) if call.arguments == serde_json::json!({"tz": "UTC"})
        ));
    }

    #[tokio::test]
    async fn collect_stream_response_surfaces_stream_error() {
        let events = vec![Ok(ProviderStreamEvent::Error("boom".to_string()))];
        let err = collect_stream_response(Box::pin(stream::iter(events)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
    }
}
