use std::time::Duration;

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::error::LlmError;
use crate::message::{ChatMessage, MessageContent, MessageRole, ThinkingContent};
use crate::model::ProviderModelInfo;
use crate::provider::{LlmProvider, ProviderCapabilities};
use crate::providers::openai::error_event_fields;
use crate::providers::openai::map_responses_stream;
use crate::providers::openai::mapping::build_request_json;
use crate::request::ProviderRequest;
use crate::response::ProviderResponse;
use crate::stream::{ProviderStream, ProviderStreamEvent};

use super::client::CodexClient;
use super::config::CodexConfig;
/// 把 HTTP Responses 请求体改写为 WS `response.create` 消息：
/// 去掉 `stream` 字段（WS 上由消息类型表达），其余字段原样包裹。
fn ws_request_payload(mut payload: Value) -> Value {
    let mut message = serde_json::Map::new();
    message.insert(
        "type".to_string(),
        Value::String("response.create".to_string()),
    );
    if let Some(object) = payload.as_object_mut() {
        object.remove("stream");
        message.extend(object.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    Value::Object(message)
}

/// 流内 `error` 事件是否为上游瞬时错误（产出前可安全重试），是则返回用于
/// 日志与上抛的错误文案。瞬时错误签名与消费层统一重试共用同一口径
/// （`crate::stream::retryable_stream_error_signature`）。
/// - `server_is_overloaded`：降载是节点/桶级状态，重建连接可能路由到未降载节点；
/// - `server_error`：上游内部错误，实测消息自带 "You can retry your request"；
/// - 消息含 "retry your request" / "overload" 的变体（无错误码的裸 error 事件）。
///
/// 其余错误（鉴权、请求非法等）重试无意义，原样交给上层。
fn retryable_stream_error(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let (code, message) = error_event_fields(value);
    crate::stream::retryable_stream_error_signature(&code, &message).then(|| {
        if code.is_empty() {
            message
        } else {
            format!("[{code}] {message}")
        }
    })
}
/// 探测阶段判定「内容已开始」的事件：任意增量（思考/正文/工具调用）或输出项
/// 新增。出现后停止缓冲、把流交出（此后由自愈层接管错误处理）。
fn is_content_event(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("response.output_text.delta")
            | Some("response.reasoning_summary_text.delta")
            | Some("response.reasoning_text.delta")
            | Some("response.reasoning_summary_part.added")
            | Some("response.reasoning_summary_part.done")
            | Some("response.function_call_arguments.delta")
            | Some("response.output_item.added")
    )
}

/// WS 打开阶段（连接/发送/探测）最多尝试次数：与消费层统一流间重试额度一致。
const WS_MAX_ATTEMPTS: usize = 10;
/// 重试退避基数：第 n 次失败后等待 min(基数 × n, 上限)。
const WS_RETRY_BACKOFF: Duration = Duration::from_millis(600);
/// 重试退避上限。
const WS_RETRY_BACKOFF_CAP: Duration = Duration::from_secs(10);
/// 探测阶段最多缓冲的事件数：超出仍无内容/终态事件时按现状交出，防异常流无限缓冲。
const WS_PROBE_MAX_EVENTS: usize = 32;
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

    /// 组装 WS 请求上下文：`(url, 头列表)`。会话亲和头以 `session-id` /
    /// `session_id` 双形态注入，与后续 SSE 形态的请求保持一致。
    async fn ws_context(
        &self,
        session_id: Option<&str>,
    ) -> Result<(String, Vec<(String, String)>), LlmError> {
        let session_headers = super::client::with_session_headers(HeaderMap::new(), session_id);
        self.client.ws_request_context(session_headers).await
    }

    /// 经 WS 打开事件流（连接/发送/探测，含打开阶段重试），全程不降级 HTTP。
    ///
    /// 传输形态参与上游分桶（HTTP 与 WS 指纹不同），一旦回落 HTTP 就会丢失会话
    /// 亲和与前缀缓存路由，因此重试与重建都留在同一条 WS 通道上；降载是节点/
    /// 桶级状态，重建连接可能路由到未降载的节点。
    ///
    /// 流交出后的中段瞬时错误（无可见产出）统一由 `provider_client` 消费层
    /// 重发请求处理（全 provider 一致，见其 `MAX_STREAM_RETRY_ATTEMPTS`），
    /// 本层不再叠加自愈，避免双层重试相乘。
    async fn stream_via_ws(
        &self,
        session_id: Option<&str>,
        payload: &Value,
    ) -> Result<ProviderStream, LlmError> {
        let (stream, _) = self
            .open_events_stream(session_id, payload, WS_MAX_ATTEMPTS)
            .await?;
        Ok(stream)
    }
    /// 打开一次事件流（探测 + 重试），返回事件流与消耗的尝试次数。
    ///
    /// 探测阶段缓冲 `response.created` / `response.in_progress` 等前置事件，
    /// 直到首个内容事件（思考/正文/工具增量）或终态事件才把流交出：上游会
    /// 先受理请求、数秒后才推 `server_is_overloaded` 等流内错误（实测 4~21s），
    /// 只看首帧会漏掉这类错误。探测期内的连接级失败与可重试流内错误就地
    /// 重建连接重试（零产出，重试不会重复内容）。
    async fn open_events_stream(
        &self,
        session_id: Option<&str>,
        payload: &Value,
        max_attempts: usize,
    ) -> Result<(ProviderStream, usize), LlmError> {
        let ws_payload = ws_request_payload(payload.clone());
        let mut last_error: Option<LlmError> = None;
        let mut attempt = 0;
        while attempt < max_attempts {
            attempt += 1;
            if attempt > 1 {
                tokio::time::sleep(std::cmp::min(
                    WS_RETRY_BACKOFF * (attempt - 1) as u32,
                    WS_RETRY_BACKOFF_CAP,
                ))
                .await;
            }
            let (url, headers) = match self.ws_context(session_id).await {
                Ok(context) => context,
                // 鉴权/配置错误重试无意义，直接上抛给用户处理。
                Err(err @ (LlmError::Configuration(_) | LlmError::Authentication(_))) => {
                    return Err(err);
                }
                Err(err) => {
                    tracing::warn!(error = %err, attempt, ?session_id, "Codex WS 请求上下文获取失败，重试");
                    last_error = Some(err);
                    continue;
                }
            };
            let mut events = match super::ws::request(
                session_id,
                &url,
                &headers,
                ws_payload.clone(),
            )
            .await
            {
                Ok(response) => response.events,
                Err(err) => {
                    tracing::warn!(error = %err, attempt, ?session_id, "Codex WS 发送失败，重建连接重试");
                    last_error = Some(err);
                    continue;
                }
            };
            let mut head: Vec<Result<Value, async_openai::error::OpenAIError>> = Vec::new();
            let mut failure: Option<LlmError> = None;
            while head.len() < WS_PROBE_MAX_EVENTS {
                match events.next().await {
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, attempt, ?session_id, "Codex WS 探测期连接断开，重建连接重试");
                        failure = Some(LlmError::Transport(format!("Codex WS 事件流中断：{err}")));
                        break;
                    }
                    Some(Ok(value)) => {
                        if let Some(detail) = retryable_stream_error(&value) {
                            tracing::warn!(error = %detail, attempt, ?session_id, "Codex WS 流内瞬时错误（产出前），丢弃连接重建重试");
                            failure = Some(LlmError::Provider {
                                provider: "codex",
                                message: detail,
                            });
                            break;
                        }
                        let settled =
                            is_content_event(&value) || super::ws::is_terminal_event(&value);
                        head.push(Ok(value));
                        if settled {
                            tracing::info!(
                                ?session_id,
                                attempt,
                                probed = head.len(),
                                "Codex 请求经 WS 传输"
                            );
                            return Ok((
                                map_responses_stream(Box::pin(stream::iter(head).chain(events))),
                                attempt,
                            ));
                        }
                    }
                    None => {
                        tracing::warn!(
                            attempt,
                            ?session_id,
                            "Codex WS 探测期流即结束，重建连接重试"
                        );
                        failure = Some(LlmError::Transport("Codex WS 探测期流结束".to_string()));
                        break;
                    }
                }
            }
            match failure {
                None => {
                    // 探测事件数达上限仍无内容/终态事件：按现状交出，不无限缓冲。
                    tracing::info!(
                        ?session_id,
                        attempt,
                        probed = head.len(),
                        "Codex 请求经 WS 传输（探测上限）"
                    );
                    return Ok((
                        map_responses_stream(Box::pin(stream::iter(head).chain(events))),
                        attempt,
                    ));
                }
                Some(err) => {
                    // 非 clean 放弃：Drop 策略丢弃连接不入池。
                    drop(events);
                    last_error = Some(err);
                }
            }
        }
        tracing::warn!(
            ?session_id,
            attempts = max_attempts,
            error = ?last_error.as_ref().map(ToString::to_string),
            "Codex WS 重试耗尽"
        );
        Err(last_error.unwrap_or_else(|| LlmError::Transport("Codex WS 请求失败".to_string())))
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
        assistant_message: ChatMessage::new(MessageRole::Assistant, content),
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
        let mut payload = build_request_json(&req, true)
            .map_err(|err| LlmError::InvalidRequest(err.to_string()))?;
        adapt_payload(&mut payload);
        // 前缀稳定度观测：块级缓存只在「上一轮已发送内容原样保留」时复用，
        // 这里记录本会话的追加/改写情况，便于对照日志判断命中前提是否成立。
        if let Some(session_id) = req
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let report = super::prefix::observe(session_id, &payload);
            tracing::info!(
                session_id,
                comparable = report.comparable,
                instructions_same = report.instructions_same,
                append_only = report.append_only,
                prev_items = report.prev_items,
                cur_items = report.cur_items,
                first_diff_item = ?report.first_diff_item,
                "Codex 请求前缀稳定度"
            );
        }
        // 只走 WS：HTTP 与 WS 是两种指纹形态，上游按形态分桶，混用会打断会话
        // 亲和与前缀缓存路由，因此连接失败只在 WS 上重建重试，不降级 HTTP。
        let session_id = req
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        self.stream_via_ws(session_id, &payload).await
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

    #[test]
    fn retryable_error_event_detected_by_code() {
        let value = serde_json::json!({
            "type": "error",
            "code": "server_is_overloaded",
            "message": "The server is currently overloaded"
        });
        let detail = retryable_stream_error(&value).unwrap();
        assert!(detail.contains("server_is_overloaded"), "{detail}");
        assert!(detail.contains("The server is currently overloaded"));
    }
    #[test]
    fn retryable_error_event_covers_server_error() {
        // 冒烟实测命中的形态：code=server_error，消息自带重试提示
        let value = serde_json::json!({
            "type": "error",
            "code": "server_error",
            "message": "An error occurred while processing your request. You can retry your request, or contact us through our help center at help.openai.com if the error persists."
        });
        assert!(retryable_stream_error(&value).is_some());
    }

    #[test]
    fn retryable_error_event_detected_by_message_only() {
        let value = serde_json::json!({
            "type": "error",
            "error": { "message": "Server is OVERLOADED, retry later" }
        });
        assert!(retryable_stream_error(&value).is_some());
    }

    #[test]
    fn non_retryable_error_event_is_passed_through() {
        let value = serde_json::json!({
            "type": "error",
            "code": "invalid_request_error",
            "message": "unsupported field"
        });
        assert!(retryable_stream_error(&value).is_none());
    }

    #[test]
    fn non_error_events_are_not_retryable() {
        for value in [
            serde_json::json!({ "type": "response.completed" }),
            serde_json::json!({ "type": "response.failed" }),
            serde_json::json!({ "type": "response.created" }),
        ] {
            assert!(retryable_stream_error(&value).is_none(), "{value}");
        }
    }

    #[test]
    fn content_event_classification_for_probe() {
        for event_type in [
            "response.output_text.delta",
            "response.reasoning_summary_text.delta",
            "response.reasoning_text.delta",
            "response.function_call_arguments.delta",
            "response.output_item.added",
        ] {
            let value = serde_json::json!({ "type": event_type });
            assert!(is_content_event(&value), "{event_type}");
        }
        for event_type in [
            "response.created",
            "response.in_progress",
            "response.completed",
            "response.failed",
            "error",
        ] {
            let value = serde_json::json!({ "type": event_type });
            assert!(!is_content_event(&value), "{event_type}");
        }
    }
    #[test]
    fn retryable_error_message_parses_code_prefix() {
        assert!(crate::stream::retryable_stream_error_message(
            "[server_is_overloaded] Our servers are currently overloaded. Please try again later."
        ));
        assert!(crate::stream::retryable_stream_error_message(
            "[server_error] An error occurred. You can retry your request."
        ));
        assert!(crate::stream::retryable_stream_error_message(
            "Our servers are OVERLOADED"
        ));
        assert!(!crate::stream::retryable_stream_error_message(
            "[invalid_request_error] unsupported field"
        ));
        assert!(!crate::stream::retryable_stream_error_message("boom"));
    }
}
