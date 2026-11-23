//! Responses API 流式事件映射。
//!
//! Responses SSE 事件以 `type` 字段区分，解析为统一 `ProviderStreamEvent`。

use crate::call_state::CallRegistry;

use serde_json::Value;

use crate::stream::ProviderStreamEvent;

use super::mapping::{extract_reasoning_text, parse_usage};

#[derive(Default)]
pub struct ResponsesStreamParser {
    calls: CallRegistry,
    has_text: bool,
}
impl ResponsesStreamParser {
    pub fn parse_event(
        &mut self,
        payload: &Value,
    ) -> Vec<Result<ProviderStreamEvent, crate::error::LlmError>> {
        parse_stream_event_with_state(payload, &mut self.calls, &mut self.has_text)
    }
}
#[cfg(test)]
fn parse_stream_event(payload: &Value) -> Vec<Result<ProviderStreamEvent, crate::error::LlmError>> {
    ResponsesStreamParser::default().parse_event(payload)
}
fn parse_stream_event_with_state(
    payload: &Value,
    calls: &mut CallRegistry,
    has_text: &mut bool,
) -> Vec<Result<ProviderStreamEvent, crate::error::LlmError>> {
    let mut events = Vec::new();
    let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    match event_type {
        "response.created" | "response.in_progress" => {
            events.push(Ok(ProviderStreamEvent::MessageStart));
        }
        "response.output_text.delta" => {
            if let Some(delta) = payload.get("delta").and_then(Value::as_str)
                && !delta.is_empty()
            {
                *has_text = true;
                events.push(Ok(ProviderStreamEvent::TextDelta(delta.to_string())));
            }
        }
        // 思考摘要增量（summary_text.delta 是主要事件；reasoning_text.delta 为
        // 部分模型的完整思考文本增量）。
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(delta) = payload.get("delta").and_then(Value::as_str)
                && !delta.is_empty()
            {
                events.push(Ok(ProviderStreamEvent::ReasoningDelta(delta.to_string())));
            }
        }
        // 部分模型通过 reasoning_summary_part 携带思考文本，part.text 为完整段落。
        "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => {
            if let Some(text) = payload
                .get("part")
                .and_then(|p| p.get("text"))
                .and_then(Value::as_str)
                && !text.is_empty()
            {
                events.push(Ok(ProviderStreamEvent::ReasoningDelta(text.to_string())));
            }
        }
        // reasoning_text.done / reasoning_summary_text.done 携带累积文本，仅在为
        // 非空且尚未通过 delta 流出时作为兜底（多数情况下 delta 已覆盖，此处跳过）。
        "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
            if let Some(text) = payload.get("text").and_then(Value::as_str)
                && !text.trim().is_empty()
            {
                events.push(Ok(ProviderStreamEvent::ReasoningDelta(text.to_string())));
            }
        }
        "response.function_call_arguments.delta" => {
            match calls.resolve(
                payload.get("item_id").and_then(Value::as_str),
                payload.get("output_index").and_then(Value::as_u64),
            ) {
                Ok(key) => events.push(Ok(ProviderStreamEvent::ToolArgumentsDelta {
                    key,
                    fragment: payload
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                })),
                Err(error) => events.push(Err(error)),
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            if let Some(item) = payload.get("item")
                && item.get("type").and_then(Value::as_str) == Some("function_call")
            {
                match calls.item_events(
                    item.get("id").and_then(Value::as_str),
                    payload.get("output_index").and_then(Value::as_u64),
                    item.get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    item.get("name").and_then(Value::as_str).unwrap_or_default(),
                    if event_type.ends_with(".done") {
                        item.get("arguments").and_then(Value::as_str)
                    } else {
                        None
                    },
                    event_type.ends_with(".done"),
                ) {
                    Ok(mapped) => events.extend(mapped.into_iter().map(Ok)),
                    Err(error) => events.push(Err(error)),
                }
            }
        }
        "response.function_call_arguments.done" => {
            match calls.resolve(
                payload.get("item_id").and_then(Value::as_str),
                payload.get("output_index").and_then(Value::as_u64),
            ) {
                Ok(key) => {
                    if let Some(arguments) = payload.get("arguments").and_then(Value::as_str) {
                        events.push(Ok(ProviderStreamEvent::ToolArgumentsSnapshot {
                            key,
                            arguments: arguments.to_owned(),
                        }));
                    }
                    events.push(Ok(ProviderStreamEvent::ToolCallFinished { key }));
                }
                Err(error) => events.push(Err(error)),
            }
        }
        "response.completed" => {
            if let Some(response) = payload.get("response")
                && let Some(usage) = response.get("usage")
            {
                events.push(Ok(ProviderStreamEvent::Usage(parse_usage(usage))));
            }
            if let Some(output) = payload
                .get("response")
                .and_then(|r| r.get("output"))
                .and_then(Value::as_array)
            {
                for (index, item) in output.iter().enumerate() {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        match calls.item_events(
                            item.get("id").and_then(Value::as_str),
                            Some(index as u64),
                            item.get("call_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            item.get("name").and_then(Value::as_str).unwrap_or_default(),
                            item.get("arguments").and_then(Value::as_str),
                            true,
                        ) {
                            Ok(mapped) => events.extend(mapped.into_iter().map(Ok)),
                            Err(error) => events.push(Err(error)),
                        }
                    } else if !*has_text
                        && item.get("type").and_then(Value::as_str) == Some("message")
                        && let Some(content) = item.get("content").and_then(Value::as_array)
                    {
                        for block in content {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                events.push(Ok(ProviderStreamEvent::TextDelta(text.to_owned())));
                            }
                        }
                    }
                }
            }
            if events
                .iter()
                .any(|event| matches!(event, Ok(ProviderStreamEvent::TextDelta(_))))
            {
                *has_text = true;
            }
            // 兜底解析 reasoning 由 provider 层根据"是否收到过 delta"决定，
            // 避免此处无条件补发导致与流式增量重复。
            events.push(Ok(ProviderStreamEvent::MessageEnd {
                stop_reason: completed_stop_reason(payload),
            }));
        }
        // 裸 error 事件：ChatGPT Codex 后端会以该形态推送 server_is_overloaded
        // 等流内错误（HTTP 200 + `event: error`，error.message 携带原因）。
        // 此前落入 `_ => {}` 被静默丢弃，流「正常」结束但零产出，表现为
        // 莫名的空回复；必须转成 Error 事件走统一的中断/重试路径。
        "error" => {
            let (code, message) = error_event_fields(payload);
            let display = if code.is_empty() {
                message
            } else {
                format!("[{code}] {message}")
            };
            events.push(Ok(ProviderStreamEvent::Error(display)));
            events.push(Ok(ProviderStreamEvent::MessageEnd {
                stop_reason: Some(crate::response::StopReason::Other(event_type.to_string())),
            }));
        }
        "response.incomplete" => {
            if let Some(usage) = payload.get("response").and_then(|r| r.get("usage")) {
                events.push(Ok(ProviderStreamEvent::Usage(parse_usage(usage))));
            }
            let reason = payload
                .get("response")
                .and_then(|r| r.get("incomplete_details"))
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("incomplete");
            events.push(Ok(ProviderStreamEvent::MessageEnd {
                stop_reason: Some(if reason == "max_output_tokens" {
                    crate::response::StopReason::MaxTokens
                } else {
                    crate::response::StopReason::Other(reason.to_owned())
                }),
            }));
        }
        "response.failed" => {
            if let Some(usage) = payload
                .get("response")
                .and_then(|response| response.get("usage"))
            {
                events.push(Ok(ProviderStreamEvent::Usage(parse_usage(usage))));
            }
            let message = payload
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Responses 请求失败")
                .to_string();
            events.push(Ok(ProviderStreamEvent::Error(message)));
            events.push(Ok(ProviderStreamEvent::MessageEnd {
                stop_reason: Some(crate::response::StopReason::Other(event_type.to_string())),
            }));
        }
        // P2：未处理的 reasoning 事件打 debug 日志，便于确认服务端实际返回结构。
        _ if event_type.contains("reasoning") => {
            tracing::debug!(event_type, "未处理的 Responses reasoning 事件");
        }
        _ => {}
    }
    events
}

/// 提取流内 `error` 事件的错误码与消息（两个字段可在顶层或 `error` 对象内）。
///
/// Codex 后端的降载错误形如 `{"type":"error","code":"server_is_overloaded",
/// "message":"…"}`，也存在仅顶层字段或仅 `error.message` 的变体。错误码进入
/// 展示文案（`[code] message`）便于日志定位，也是上层判断可否重试的依据。
pub(crate) fn error_event_fields(payload: &Value) -> (String, String) {
    let error = payload.get("error");
    let code = error
        .and_then(|e| e.get("code"))
        .or_else(|| payload.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let message = error
        .and_then(|e| e.get("message"))
        .or_else(|| payload.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Responses 流式请求失败")
        .to_string();
    (code, message)
}

fn completed_stop_reason(payload: &Value) -> Option<crate::response::StopReason> {
    let response = payload.get("response")?;
    let has_tool_call = response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        });
    Some(if has_tool_call {
        crate::response::StopReason::ToolUse
    } else {
        crate::response::StopReason::EndTurn
    })
}

/// 从 `response.completed` 事件的 payload 中提取最终 reasoning 文本。
///
/// 由 provider 层在有状态 stream mapper 中调用：仅当本次流式过程中
/// **未收到任何 reasoning delta** 时，才用此兜底补发思考内容，避免与
/// 流式增量重复。
pub(super) fn extract_completed_reasoning(payload: &Value) -> Option<String> {
    let response = payload.get("response")?;
    let output = response.get("output").and_then(Value::as_array)?;
    let mut parts = Vec::new();
    for item in output {
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            let text = extract_reasoning_text(item);
            if !text.trim().is_empty() {
                parts.push(text);
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::ProviderStreamEvent;
    use serde_json::json;

    fn reasoning_delta(text: &str) -> bool {
        matches!(text, "思考中")
    }

    #[test]
    fn parses_reasoning_summary_delta() {
        let payload = json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "思考中"
        });
        let events = parse_stream_event(&payload);
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(ProviderStreamEvent::ReasoningDelta(text)) => assert!(reasoning_delta(text)),
            other => panic!("expected reasoning delta, got {other:?}"),
        }
    }

    #[test]
    fn parses_reasoning_summary_part() {
        // reasoning_summary_part.added 通过 part.text 携带思考段落。
        let payload = json!({
            "type": "response.reasoning_summary_part.added",
            "part": { "type": "summary_text", "text": "思考中" }
        });
        let events = parse_stream_event(&payload);
        match &events[0] {
            Ok(ProviderStreamEvent::ReasoningDelta(text)) => assert!(reasoning_delta(text)),
            other => panic!("expected reasoning delta, got {other:?}"),
        }
    }

    #[test]
    fn completed_event_reports_tool_use_stop_reason() {
        let payload = json!({
            "type": "response.completed",
            "response": {
                "output": [{
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{}"
                }]
            }
        });
        let events = parse_stream_event(&payload);
        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ProviderStreamEvent::MessageEnd {
                stop_reason: Some(crate::response::StopReason::ToolUse)
            })
        )));
    }

    #[test]
    fn completed_event_reports_end_turn_stop_reason() {
        let payload = json!({
            "type": "response.completed",
            "response": {
                "output": [{
                    "type": "message",
                    "content": [{"type": "output_text", "text": "答案"}]
                }]
            }
        });
        let events = parse_stream_event(&payload);
        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ProviderStreamEvent::MessageEnd {
                stop_reason: Some(crate::response::StopReason::EndTurn)
            })
        )));
    }

    #[test]
    fn completed_event_emits_usage_and_end_only() {
        // completed 事件本身只发 usage + end，reasoning 兜底由 provider 层根据
        // 是否收到过 delta 决定（调用 extract_completed_reasoning）。
        let payload = json!({
            "type": "response.completed",
            "response": {
                "output": [
                    {
                        "type": "reasoning",
                        "summary": [{ "type": "summary_text", "text": "最终思考" }]
                    },
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "答案" }]
                    }
                ],
                "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
            }
        });
        let events = parse_stream_event(&payload);
        // completed 事件不应自动输出 reasoning（避免与流式增量重复）。
        let no_reasoning = !events
            .iter()
            .any(|e| matches!(e, Ok(ProviderStreamEvent::ReasoningDelta(_))));
        assert!(
            no_reasoning,
            "completed 事件不应自动输出 reasoning: {events:?}"
        );
        let has_usage = events
            .iter()
            .any(|e| matches!(e, Ok(ProviderStreamEvent::Usage(_))));
        assert!(has_usage);
        let has_end = events
            .iter()
            .any(|e| matches!(e, Ok(ProviderStreamEvent::MessageEnd { .. })));
        assert!(has_end);
    }

    #[test]
    fn extract_completed_reasoning_collects_summary() {
        let payload = json!({
            "type": "response.completed",
            "response": {
                "output": [
                    {
                        "type": "reasoning",
                        "summary": [{ "type": "summary_text", "text": "最终思考" }]
                    }
                ]
            }
        });
        let reasoning = extract_completed_reasoning(&payload).unwrap();
        assert_eq!(reasoning, "最终思考");
    }

    #[test]
    fn extract_completed_reasoning_returns_none_when_no_reasoning() {
        let payload = json!({
            "type": "response.completed",
            "response": {
                "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "答案" }] }]
            }
        });
        assert!(extract_completed_reasoning(&payload).is_none());
    }

    #[test]
    fn completed_without_reasoning_emits_only_usage_and_end() {
        let payload = json!({
            "type": "response.completed",
            "response": {
                "output": [
                    {
                        "type": "message",
                        "content": [{ "type": "output_text", "text": "答案" }]
                    }
                ],
                "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
            }
        });
        let events = parse_stream_event(&payload);
        let no_reasoning = !events
            .iter()
            .any(|e| matches!(e, Ok(ProviderStreamEvent::ReasoningDelta(_))));
        assert!(no_reasoning, "无 reasoning item 时不应输出 reasoning");
    }

    #[test]
    fn completed_event_backfills_function_call_arguments() {
        // 某些中转不发 function_call_arguments.delta，导致流式拼接的 arguments 为空。
        // response.completed 的 output 含完整 function_call items（带 arguments），
        // 应兜底提取并通过 ToolCallDelta 补发。
        let payload = json!({
            "type": "response.completed",
            "response": {
                "output": [
                    {
                        "type": "function_call",
                        "call_id": "call_abc",
                        "name": "run_command",
                        "arguments": "{\"command\": \"python3 -c 'print(1)'\"}"
                    }
                ],
                "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
            }
        });
        let events = parse_stream_event(&payload);
        let has_tool_delta = events.iter().any(|e| {
            matches!(
                e,
                Ok(ProviderStreamEvent::ToolArgumentsSnapshot { arguments, .. })
                    if arguments.contains("python3")
            )
        });
        assert!(
            has_tool_delta,
            "completed 应兜底补发 function_call arguments，实际 events: {events:?}"
        );
    }

    #[test]
    fn completed_event_skips_empty_function_call_arguments() {
        // arguments 为空字符串时不应补发（无意义）
        let payload = json!({
            "type": "response.completed",
            "response": {
                "output": [
                    {
                        "type": "function_call",
                        "call_id": "call_xyz",
                        "name": "run_command",
                        "arguments": ""
                    }
                ],
                "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
            }
        });
        let events = parse_stream_event(&payload);
        let has_tool_delta = events
            .iter()
            .any(|e| matches!(e, Ok(ProviderStreamEvent::ToolArgumentsDelta { .. })));
        assert!(!has_tool_delta, "空 arguments 不应补发 ToolCallDelta");
    }

    #[test]
    fn stateful_parser_maps_item_id_to_call_id_for_argument_deltas() {
        let mut parser = ResponsesStreamParser::default();
        let added = json!({
            "type": "response.output_item.added",
            "item": {
                "id": "fc_item",
                "type": "function_call",
                "call_id": "call_real",
                "name": "run_command",
                "arguments": ""
            }
        });
        let delta = json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_item",
            "delta": "{\"command\":\"pwd\"}"
        });
        let done = json!({
            "type": "response.function_call_arguments.done",
            "item_id": "fc_item",
            "arguments": "{\"command\":\"pwd\"}"
        });

        let added_events = parser.parse_event(&added);
        assert!(added_events.iter().any(
            |e| matches!(e, Ok(ProviderStreamEvent::ToolCallMetadata { id, .. }) if id == "call_real")
        ));

        let delta_events = parser.parse_event(&delta);
        assert!(
            delta_events.iter().any(|e| {
                matches!(
                    e,
                    Ok(ProviderStreamEvent::ToolArgumentsDelta { fragment, .. })
                        if fragment.contains("pwd")
                )
            }),
            "delta 应使用真实 call_id: {delta_events:?}"
        );

        let done_events = parser.parse_event(&done);
        assert!(
            done_events
                .iter()
                .any(|e| { matches!(e, Ok(ProviderStreamEvent::ToolCallFinished { .. })) }),
            "done 应使用真实 call_id: {done_events:?}"
        );
    }

    #[test]
    fn stateful_parser_does_not_backfill_arguments_after_delta() {
        let mut parser = ResponsesStreamParser::default();
        let added = json!({
            "type": "response.output_item.added",
            "item": {
                "id": "fc_item",
                "type": "function_call",
                "call_id": "call_real",
                "name": "run_command",
                "arguments": ""
            }
        });
        let delta = json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_item",
            "delta": "{\"command\":\"pwd\"}"
        });
        let completed = json!({
            "type": "response.completed",
            "response": {
                "output": [
                    {
                        "id": "fc_item",
                        "type": "function_call",
                        "call_id": "call_real",
                        "name": "run_command",
                        "arguments": "{\"command\":\"pwd\"}"
                    }
                ],
                "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
            }
        });

        let _ = parser.parse_event(&added);
        let _ = parser.parse_event(&delta);
        let completed_events = parser.parse_event(&completed);
        let tool_delta_count = completed_events
            .iter()
            .filter(|e| matches!(e, Ok(ProviderStreamEvent::ToolArgumentsDelta { .. })))
            .count();
        assert_eq!(
            tool_delta_count, 0,
            "已收到 delta 后 completed 不应再补发完整 arguments: {completed_events:?}"
        );
    }

    #[test]
    fn bare_error_event_carries_code_in_message() {
        let payload = json!({
            "type": "error",
            "error": { "code": "server_is_overloaded", "message": "The server is overloaded" }
        });
        let events = parse_stream_event(&payload);
        match &events[0] {
            Ok(ProviderStreamEvent::Error(message)) => {
                assert!(message.contains("[server_is_overloaded]"), "{message}");
                assert!(message.contains("The server is overloaded"));
            }
            other => panic!("expected error event, got {other:?}"),
        }
    }

    #[test]
    fn bare_error_event_without_code_keeps_plain_message() {
        let payload = json!({ "type": "error", "message": "boom" });
        let events = parse_stream_event(&payload);
        match &events[0] {
            Ok(ProviderStreamEvent::Error(message)) => assert_eq!(message, "boom"),
            other => panic!("expected error event, got {other:?}"),
        }
    }

    #[test]
    fn error_event_fields_reads_top_level_and_nested_variants() {
        let nested = json!({
            "type": "error",
            "error": { "code": "server_is_overloaded", "message": "busy" }
        });
        assert_eq!(
            super::error_event_fields(&nested),
            ("server_is_overloaded".to_string(), "busy".to_string())
        );
        let top_level = json!({ "type": "error", "code": "c1", "message": "m1" });
        assert_eq!(
            super::error_event_fields(&top_level),
            ("c1".to_string(), "m1".to_string())
        );
        let bare = json!({ "type": "error" });
        assert_eq!(
            super::error_event_fields(&bare),
            (String::new(), "Responses 流式请求失败".to_string())
        );
    }
}
