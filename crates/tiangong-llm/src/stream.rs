use std::pin::Pin;

use futures_util::Stream;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::tool::ToolCall;
use crate::usage::TokenUsageData;

/// 统一流式事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderStreamEvent {
    MessageStart,
    ReasoningDelta(String),
    ReasoningSignatureDelta(String),
    TextDelta(String),
    ToolCallStart(ToolCall),
    ToolCallDelta {
        call_id: String,
        partial_json: String,
    },
    ToolCallEnd {
        call_id: String,
    },
    MessageEnd {
        stop_reason: Option<crate::response::StopReason>,
    },
    Usage(TokenUsageData),
    Error(String),
}

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderStreamEvent, LlmError>> + Send>>;

/// 把一次性完整响应合成为流事件序列。
///
/// 服务端忽略 stream 参数返回普通 JSON 时，复用非流式解析结果，
/// 让流式消费方无差别处理。
pub(crate) fn complete_response_events(
    response: crate::response::ProviderResponse,
) -> Vec<Result<ProviderStreamEvent, LlmError>> {
    use crate::message::MessageContent;

    let mut events = vec![Ok(ProviderStreamEvent::MessageStart)];
    if let Some(reasoning) = response
        .reasoning_content
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        events.push(Ok(ProviderStreamEvent::ReasoningDelta(
            reasoning.to_string(),
        )));
    }
    for block in &response.assistant_message.content {
        match block {
            MessageContent::Text(text) if !text.trim().is_empty() => {
                events.push(Ok(ProviderStreamEvent::TextDelta(text.clone())));
            }
            // 完整参数直接放在 ToolCallStart，无需再跟 Delta 分片。
            MessageContent::ToolCall(call) => {
                events.push(Ok(ProviderStreamEvent::ToolCallStart(call.clone())));
            }
            _ => {}
        }
    }
    if let Some(usage) = response.usage {
        events.push(Ok(ProviderStreamEvent::Usage(usage)));
    }
    events.push(Ok(ProviderStreamEvent::MessageEnd {
        stop_reason: response.stop_reason,
    }));
    events
}

/// 错误码 + 消息是否为上游瞬时错误信号（流间统一重试判定用）。
///
/// 三类信号：
/// - `server_is_overloaded`：降载（节点/桶级状态，重发可能落到其他节点）；
/// - `server_error`：上游内部错误，实测消息自带 "You can retry your request"；
/// - 消息含 "retry your request" / "overload" 的变体（无错误码的裸 error 事件）。
pub(crate) fn retryable_stream_error_signature(code: &str, message: &str) -> bool {
    let lower_code = code.to_ascii_lowercase();
    let lower = message.to_ascii_lowercase();
    // 错误码按子串匹配，覆盖各家族命名：server_is_overloaded（OpenAI/Codex）、
    // overloaded_error（Anthropic）、server_error 等。
    lower_code.contains("overload")
        || lower_code.contains("server_error")
        || lower.contains("overload")
        || lower.contains("retry your request")
}
/// 已组装为 `[code] message` 的错误文案是否为瞬时错误（流中段统一重试判定用）。
pub(crate) fn retryable_stream_error_message(message: &str) -> bool {
    match message
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
    {
        Some((code, rest)) => retryable_stream_error_signature(code, rest.trim_start()),
        None => retryable_stream_error_signature("", message),
    }
}

#[cfg(test)]
mod tests {
    use super::retryable_stream_error_signature;

    #[test]
    fn retryable_signature_covers_provider_error_code_families() {
        // OpenAI / Codex
        assert!(retryable_stream_error_signature(
            "server_is_overloaded",
            "busy"
        ));
        assert!(retryable_stream_error_signature("server_error", "internal"));
        // Anthropic
        assert!(retryable_stream_error_signature(
            "overloaded_error",
            "Overloaded"
        ));
        // 变体大小写
        assert!(retryable_stream_error_signature("ServerOverloaded", "x"));
        // 不可重试
        assert!(!retryable_stream_error_signature(
            "invalid_request_error",
            "bad"
        ));
        assert!(!retryable_stream_error_signature(
            "authentication_error",
            "no key"
        ));
        // 仅文案命中
        assert!(retryable_stream_error_signature(
            "",
            "You can retry your request"
        ));
        assert!(!retryable_stream_error_signature("", "boom"));
    }
}
