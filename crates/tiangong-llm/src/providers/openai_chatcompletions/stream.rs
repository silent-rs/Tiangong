use std::collections::BTreeMap;

use serde_json::Value;

use crate::stream::ProviderStreamEvent;
use crate::tool::ToolCall;

#[derive(Default)]
pub(super) struct OpenAiStreamDecoder {
    tool_calls: BTreeMap<usize, PendingToolCall>,
}

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl OpenAiStreamDecoder {
    pub(super) fn parse_payload(
        &mut self,
        payload: &Value,
    ) -> Vec<Result<ProviderStreamEvent, crate::error::LlmError>> {
        let mut events = Vec::new();
        // 流内错误载荷：OpenAI 兼容网关会在流中以 data:{"error":{...}} 推送
        // 错误；此前 payload 无 choices/usage 被静默丢弃，空回复表现为
        // 「正常」结束。统一转 Error 事件（错误码进文案），交给消费层的
        // 中断/统一重试链路处理。
        if payload.get("error").is_some_and(|error| !error.is_null()) {
            let (code, message) = crate::providers::openai::error_event_fields(payload);
            let display = if code.is_empty() {
                message
            } else {
                format!("[{code}] {message}")
            };
            return vec![
                Ok(ProviderStreamEvent::Error(display)),
                Ok(ProviderStreamEvent::MessageEnd {
                    stop_reason: Some(crate::response::StopReason::Other("error".to_string())),
                }),
            ];
        }
        if let Some(usage) = payload.get("usage") {
            events.push(Ok(ProviderStreamEvent::Usage(super::mapping::parse_usage(
                usage,
            ))));
        }
        if let Some(choices) = payload.get("choices").and_then(Value::as_array) {
            for choice in choices {
                let delta = choice.get("delta").unwrap_or(&Value::Null);
                if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
                    && !reasoning.is_empty()
                {
                    events.push(Ok(ProviderStreamEvent::ReasoningDelta(
                        reasoning.to_string(),
                    )));
                }
                if let Some(content) = delta.get("content").and_then(Value::as_str)
                    && !content.is_empty()
                {
                    events.push(Ok(ProviderStreamEvent::TextDelta(content.to_string())));
                }
                if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for (position, tool_call) in tool_calls.iter().enumerate() {
                        let index = tool_call
                            .get("index")
                            .and_then(Value::as_u64)
                            .and_then(|value| usize::try_from(value).ok())
                            .unwrap_or(position);
                        let pending = self.tool_calls.entry(index).or_default();
                        if let Some(id) = tool_call
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            && pending.id.is_empty()
                        {
                            pending.id = id.to_string();
                        }
                        if let Some(function) = tool_call.get("function") {
                            if let Some(name) = function
                                .get("name")
                                .and_then(Value::as_str)
                                .filter(|value| !value.is_empty())
                                && pending.name.is_empty()
                            {
                                pending.name = name.to_string();
                            }
                            if let Some(arguments) =
                                function.get("arguments").and_then(Value::as_str)
                            {
                                pending.push_arguments(arguments);
                            }
                        }
                    }
                }
                if choice
                    .get("finish_reason")
                    .is_some_and(|reason| !reason.is_null())
                {
                    events.extend(self.flush_tool_calls());
                }
            }
        }
        events
    }

    fn flush_tool_calls(&mut self) -> Vec<Result<ProviderStreamEvent, crate::error::LlmError>> {
        let mut events = Vec::new();
        for (index, pending) in std::mem::take(&mut self.tool_calls) {
            if pending.name.trim().is_empty() {
                tracing::warn!(index, tool_call_id = %pending.id, "忽略缺少名称的 OpenAI 流式工具调用");
                continue;
            }
            let id = if pending.id.trim().is_empty() {
                format!("tool_call_{index}")
            } else {
                pending.id
            };
            events.push(Ok(ProviderStreamEvent::ToolCallStart(ToolCall {
                id: id.clone(),
                name: pending.name,
                arguments: serde_json::json!({}),
            })));
            if !pending.arguments.is_empty() {
                events.push(Ok(ProviderStreamEvent::ToolCallDelta {
                    call_id: id,
                    partial_json: pending.arguments,
                }));
            }
        }
        events
    }
}

impl PendingToolCall {
    fn push_arguments(&mut self, partial: &str) {
        if partial.is_empty() || self.arguments == partial {
            return;
        }
        if self.arguments.is_empty() {
            self.arguments.push_str(partial);
            return;
        }

        let combined = format!("{}{}", self.arguments, partial);
        if serde_json::from_str::<Value>(&combined).is_ok()
            || serde_json::from_str::<Value>(&self.arguments).is_err()
        {
            self.arguments = combined;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unwrap_events(
        decoder: &mut OpenAiStreamDecoder,
        payload: Value,
    ) -> Vec<ProviderStreamEvent> {
        decoder
            .parse_payload(&payload)
            .into_iter()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn parse_payload_surfaces_in_stream_error() {
        // OpenAI 兼容网关的流内错误载荷：转 Error 事件且错误码进文案，
        // 不再被静默丢弃。
        let mut decoder = OpenAiStreamDecoder::default();
        let events = unwrap_events(
            &mut decoder,
            serde_json::json!({
                "error": {
                    "code": "server_is_overloaded",
                    "message": "Our servers are currently overloaded"
                }
            }),
        );
        match &events[0] {
            ProviderStreamEvent::Error(message) => {
                assert!(message.contains("server_is_overloaded"), "{message}");
                assert!(message.contains("currently overloaded"), "{message}");
            }
            other => panic!("expected error event, got {other:?}"),
        }
    }
    #[test]
    fn accumulates_parallel_calls_and_flushes_in_provider_order() {
        let mut decoder = OpenAiStreamDecoder::default();
        let first = unwrap_events(
            &mut decoder,
            serde_json::json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [
                            {
                                "index": 1,
                                "id": "call_b",
                                "function": { "name": "read_file", "arguments": "{\"path\":\"b" }
                            },
                            {
                                "index": 0,
                                "id": "call_a",
                                "function": { "name": "read_file", "arguments": "{\"path\":\"a" }
                            }
                        ]
                    },
                    "finish_reason": null
                }]
            }),
        );
        assert!(first.is_empty());

        let finished = unwrap_events(
            &mut decoder,
            serde_json::json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [
                            { "index": 1, "function": { "arguments": "\"}" } },
                            { "index": 0, "function": { "arguments": "\"}" } }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }]
            }),
        );

        let calls = finished
            .as_chunks::<2>()
            .0
            .iter()
            .map(|events| match (&events[0], &events[1]) {
                (
                    ProviderStreamEvent::ToolCallStart(call),
                    ProviderStreamEvent::ToolCallDelta {
                        call_id,
                        partial_json,
                    },
                ) => (call.id.as_str(), call_id.as_str(), partial_json.as_str()),
                _ => panic!("应按 Start + Delta 输出完整工具调用"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            calls,
            vec![
                ("call_a", "call_a", r#"{"path":"a"}"#),
                ("call_b", "call_b", r#"{"path":"b"}"#),
            ]
        );
    }
}
