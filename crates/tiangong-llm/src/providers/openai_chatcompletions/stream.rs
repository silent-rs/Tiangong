use std::collections::BTreeMap;

use serde_json::Value;

use crate::stream::CallKey;
use crate::stream::ProviderStreamEvent;

#[derive(Default)]
pub(super) struct OpenAiStreamDecoder {
    tool_calls: BTreeMap<usize, PendingToolCall>,
    next_key: u64,
    ended: bool,
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
        if payload.get("__stream_done").and_then(Value::as_bool) == Some(true) {
            if !self.ended {
                let tools = !self.tool_calls.is_empty();
                events.extend(self.flush_tool_calls());
                events.push(Ok(ProviderStreamEvent::MessageEnd {
                    stop_reason: Some(if tools {
                        crate::response::StopReason::ToolUse
                    } else {
                        crate::response::StopReason::EndTurn
                    }),
                }));
                self.ended = true;
            }
            return events;
        }

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
                if choice.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                    continue;
                }
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
                    self.ended = true;
                    events.extend(self.flush_tool_calls());
                    let reason = match choice
                        .get("finish_reason")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                    {
                        "tool_calls" | "function_call" => crate::response::StopReason::ToolUse,
                        "length" => crate::response::StopReason::MaxTokens,
                        "stop" => crate::response::StopReason::EndTurn,
                        other => crate::response::StopReason::Other(other.to_owned()),
                    };
                    events.push(Ok(ProviderStreamEvent::MessageEnd {
                        stop_reason: Some(reason),
                    }));
                }
            }
        }
        events
    }

    fn flush_tool_calls(&mut self) -> Vec<Result<ProviderStreamEvent, crate::error::LlmError>> {
        let mut events = Vec::new();
        for (index, pending) in std::mem::take(&mut self.tool_calls) {
            let id = if pending.id.trim().is_empty() {
                format!("tool_call_{index}")
            } else {
                pending.id
            };
            let key = CallKey(self.next_key);
            self.next_key += 1;
            events.push(Ok(ProviderStreamEvent::ToolCallMetadata {
                key,
                id,
                name: pending.name,
            }));
            events.push(Ok(ProviderStreamEvent::ToolArgumentsSnapshot {
                key,
                arguments: pending.arguments,
            }));
            events.push(Ok(ProviderStreamEvent::ToolCallFinished { key }));
        }
        events
    }
}

impl PendingToolCall {
    fn push_arguments(&mut self, partial: &str) {
        self.arguments.push_str(partial);
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

        let mut state = crate::collect::ResponseAccumulator::default();
        for event in finished {
            state.push(event);
        }
        let result = state.finish();
        assert!(result.invalid_tool_calls.is_empty());
        let calls: Vec<_> = result
            .response
            .assistant_message
            .content
            .into_iter()
            .filter_map(|block| {
                if let crate::message::MessageContent::ToolCall(call) = block {
                    Some(call)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[1].id, "call_b");
        assert_eq!(calls[0].arguments, serde_json::json!({"path":"a"}));
        assert_eq!(calls[1].arguments, serde_json::json!({"path":"b"}));
    }
}
