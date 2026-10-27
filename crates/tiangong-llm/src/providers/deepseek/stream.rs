use super::error::map_deepseek_error;
use super::mapping::parse_stream_usage;
use crate::response::StopReason;
use crate::stream::ProviderStreamEvent;
use crate::tool::ToolCall;
use futures_util::{StreamExt, stream};
use std::collections::{HashMap, HashSet};
use tiangong_deepseek::types::{ResponseObject, ResponseOutputItem, ResponsesStreamEvent};

/// 把 SDK 的 ResponsesEventStream 映射为统一的 ProviderStreamEvent 流。
///
/// Responses SSE 事件以 `type` 区分、以 `response.completed` /
/// `response.incomplete` / `response.failed` 终止（无 `[DONE]` 标记）。
/// 文本工具调用协议的兜底解析已下沉到 SDK（`tiangong_deepseek::responses`
/// 内置缓冲并合成结构化 function_call 事件组），本函数只做事件类型转换。
pub fn map_deepseek_stream(
    event_stream: tiangong_deepseek::types::ResponsesEventStream,
) -> super::client::DeepSeekStream {
    let state = StreamState::default();
    let mapped = event_stream.scan(state, |state, result| {
        std::future::ready(Some(match result {
            Ok(event) => map_event(state, event)
                .into_iter()
                .map(Ok)
                .collect::<Vec<_>>(),
            Err(err) => vec![Err(map_deepseek_error(err))],
        }))
    });
    Box::pin(mapped.flat_map(stream::iter))
}

/// 流式映射状态。
#[derive(Default)]
struct StreamState {
    /// output item 的服务端 id（如 `fc_1`）→ 回传用 call_id。
    /// 参数增量事件只携带 item_id，需要由此还原 call_id。
    item_call_ids: HashMap<String, String>,
    /// output_index → call_id：部分网关的增量事件不带（或带不同的）
    /// item_id，以输出序号双键关联兜底。
    index_call_ids: HashMap<u32, String>,
    /// 已发过 ToolCallStart 的 call_id（completed 统一转出时去重）。
    started_call_ids: HashSet<String>,
    /// 已收到过参数增量的 call_id（done / completed 兜底补发完整参数时去重）。
    received_argument_deltas: HashSet<String>,
    /// 是否收到过正文 / 思考增量（completed 兜底补发时去重）。
    has_text_delta: bool,
    has_reasoning_delta: bool,
}
fn map_event(state: &mut StreamState, event: ResponsesStreamEvent) -> Vec<ProviderStreamEvent> {
    match event {
        ResponsesStreamEvent::ResponseCreated { .. } => vec![ProviderStreamEvent::MessageStart],
        ResponsesStreamEvent::ResponseInProgress { .. } => Vec::new(),
        ResponsesStreamEvent::ReasoningTextDelta { delta, .. } => {
            state.has_reasoning_delta = true;
            vec![ProviderStreamEvent::ReasoningDelta(delta)]
        }
        ResponsesStreamEvent::OutputTextDelta { delta, .. } => {
            state.has_text_delta = true;
            vec![ProviderStreamEvent::TextDelta(delta)]
        }
        ResponsesStreamEvent::OutputItemAdded {
            output_index, item, ..
        } => match item {
            ResponseOutputItem::FunctionCall(call) => {
                let call_id = if !call.call_id.trim().is_empty() {
                    call.call_id.clone()
                } else {
                    // call_id 缺失时以输出序号生成稳定占位，
                    // 避免多个调用合并到同一空 id。
                    call.id
                        .clone()
                        .unwrap_or_else(|| format!("item_{output_index}"))
                };
                if let Some(item_id) = call.id.as_deref() {
                    state
                        .item_call_ids
                        .insert(item_id.to_string(), call_id.clone());
                }
                state.index_call_ids.insert(output_index, call_id.clone());
                state.started_call_ids.insert(call_id.clone());
                // 不在 added 时下发参数：部分网关在 added item 携带完整参数后
                // 仍会跟增量事件，立即下发会重复拼接；完整参数统一在
                // done / completed 按需补发（增量以先到者为准）。
                vec![ProviderStreamEvent::ToolCallStart(ToolCall {
                    id: call_id,
                    name: call.name.clone(),
                    arguments: serde_json::json!({}),
                })]
            }
            _ => Vec::new(),
        },
        ResponsesStreamEvent::FunctionCallArgumentsDelta {
            item_id,
            output_index,
            delta,
            ..
        } => {
            let call_id = state
                .item_call_ids
                .get(&item_id)
                .or_else(|| state.index_call_ids.get(&output_index))
                .cloned()
                .unwrap_or(item_id);
            state.received_argument_deltas.insert(call_id.clone());
            vec![ProviderStreamEvent::ToolCallDelta {
                call_id,
                partial_json: delta,
            }]
        }
        ResponsesStreamEvent::OutputItemDone {
            output_index, item, ..
        } => match item {
            ResponseOutputItem::FunctionCall(call) => {
                let call_id = if !call.call_id.trim().is_empty() {
                    call.call_id.clone()
                } else {
                    state
                        .index_call_ids
                        .get(&output_index)
                        .cloned()
                        .or_else(|| call.id.clone())
                        .unwrap_or_else(|| format!("item_{output_index}"))
                };
                let mut events = Vec::new();
                // 网关在 added item 携带完整参数、且未发任何增量事件时，
                // 在 done 补发一次完整参数，确保工具参数不丢失。
                if !call.arguments.trim().is_empty()
                    && !state.received_argument_deltas.contains(&call_id)
                {
                    state.received_argument_deltas.insert(call_id.clone());
                    events.push(ProviderStreamEvent::ToolCallDelta {
                        call_id: call_id.clone(),
                        partial_json: call.arguments.clone(),
                    });
                }
                events.push(ProviderStreamEvent::ToolCallEnd { call_id });
                events
            }
            _ => Vec::new(),
        },
        ResponsesStreamEvent::ResponseCompleted { response, .. } => {
            let mut events = Vec::new();
            // 兜底补发正文与思考：部分网关/一次性 JSON 合成流不发增量事件，
            // completed 的 output 里带完整内容，此处补发确保不丢文本。
            if !state.has_reasoning_delta {
                let reasoning = collect_reasoning_text(&response);
                if !reasoning.is_empty() {
                    events.push(ProviderStreamEvent::ReasoningDelta(reasoning));
                }
            }
            if !state.has_text_delta {
                let text = collect_message_text(&response);
                if !text.is_empty() {
                    events.push(ProviderStreamEvent::TextDelta(text));
                }
            }
            // 终止事件统一转出工具调用：一次性 JSON 合成流与不发增量事件的
            // 网关只在此处携带完整 function_call items。未发过 Start 的调用
            // 先补 Start（携带工具名），再按需补发完整参数；上层聚合依赖
            // Start 登记工具名，缺 Start 会导致整个调用被丢弃。
            for item in &response.output {
                if let ResponseOutputItem::FunctionCall(call) = item {
                    let call_id = resolve_call_id(&call.call_id, call.id.as_deref());
                    if !state.started_call_ids.contains(&call_id) {
                        state.started_call_ids.insert(call_id.clone());
                        events.push(ProviderStreamEvent::ToolCallStart(ToolCall {
                            id: call_id.clone(),
                            name: call.name.clone(),
                            arguments: serde_json::json!({}),
                        }));
                    }
                    if !call.arguments.trim().is_empty()
                        && !state.received_argument_deltas.contains(&call_id)
                    {
                        events.push(ProviderStreamEvent::ToolCallDelta {
                            call_id,
                            partial_json: call.arguments.clone(),
                        });
                    }
                }
            }
            if let Some(usage) = &response.usage {
                events.push(ProviderStreamEvent::Usage(parse_stream_usage(usage)));
            }
            let has_tool_call = response
                .output
                .iter()
                .any(|item| matches!(item, ResponseOutputItem::FunctionCall(_)));
            events.push(ProviderStreamEvent::MessageEnd {
                stop_reason: Some(if has_tool_call {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                }),
            });
            events
        }
        ResponsesStreamEvent::ResponseIncomplete { response, .. } => {
            let mut events = Vec::new();
            if let Some(usage) = &response.usage {
                events.push(ProviderStreamEvent::Usage(parse_stream_usage(usage)));
            }
            // incomplete 是截断而非错误（如 max_output_tokens 上限），不得发
            // Error：上层收到 Error 会立即中断收尾，导致截断信息被静默吞掉。
            events.push(ProviderStreamEvent::MessageEnd {
                stop_reason: Some(incomplete_stop_reason(&response)),
            });
            events
        }
        ResponsesStreamEvent::ResponseFailed { response, .. } => {
            let mut events = Vec::new();
            if let Some(usage) = &response.usage {
                events.push(ProviderStreamEvent::Usage(parse_stream_usage(usage)));
            }
            // MessageEnd 先于 Error：上层遇 Error 即中断，先收尾才能保留 stop_reason。
            events.push(ProviderStreamEvent::MessageEnd {
                stop_reason: Some(StopReason::Other("failed".to_string())),
            });
            let message = response
                .error
                .as_ref()
                .and_then(|error| error.message.clone())
                .unwrap_or_else(|| "Responses 请求失败".to_string());
            events.push(ProviderStreamEvent::Error(message));
            events
        }
        // 其余事件（文本/思考完成片、web_search、custom 工具、未知事件）
        // 不产生统一流事件，静默忽略。
        ResponsesStreamEvent::OutputTextDone { .. }
        | ResponsesStreamEvent::ReasoningTextDone { .. }
        | ResponsesStreamEvent::FunctionCallArgumentsDone { .. }
        | ResponsesStreamEvent::CustomToolCallInputDelta { .. }
        | ResponsesStreamEvent::CustomToolCallInputDone { .. }
        | ResponsesStreamEvent::ContentPartAdded { .. }
        | ResponsesStreamEvent::ContentPartDone { .. }
        | ResponsesStreamEvent::WebSearchCallInProgress { .. }
        | ResponsesStreamEvent::WebSearchCallSearching { .. }
        | ResponsesStreamEvent::WebSearchCallCompleted { .. }
        | ResponsesStreamEvent::Unknown { .. } => Vec::new(),
    }
}

/// 解析回传用 call_id：优先 call_id 字段，缺失时退回 item 的服务端 id。
fn resolve_call_id(call_id: &str, item_id: Option<&str>) -> String {
    if !call_id.trim().is_empty() {
        call_id.to_string()
    } else {
        item_id.unwrap_or_default().to_string()
    }
}

/// 收集全部 message item 的输出文本（等价 SDK 的 `output_text` 便捷访问）。
fn collect_message_text(response: &ResponseObject) -> String {
    response
        .output
        .iter()
        .filter_map(|item| match item {
            ResponseOutputItem::Message(message) => Some(message),
            _ => None,
        })
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            tiangong_deepseek::types::OutputContentBlock::OutputText(text) => {
                Some(text.text.clone())
            }
            _ => None,
        })
        .collect()
}

/// 收集全部 reasoning item 的思考文本。
fn collect_reasoning_text(response: &ResponseObject) -> String {
    response
        .output
        .iter()
        .filter_map(|item| match item {
            ResponseOutputItem::Reasoning(reasoning) => Some(reasoning),
            _ => None,
        })
        .flat_map(|reasoning| reasoning.content.iter())
        .filter_map(|block| match block {
            tiangong_deepseek::types::OutputContentBlock::ReasoningText(text) => {
                Some(text.text.clone())
            }
            _ => None,
        })
        .collect()
}

fn incomplete_stop_reason(response: &ResponseObject) -> StopReason {
    match response
        .incomplete_details
        .as_ref()
        .and_then(|details| details.reason.as_deref())
    {
        Some("max_output_tokens") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Other("content_filter".to_string()),
        _ => StopReason::Other("incomplete".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::LlmError;
    use futures_util::StreamExt;
    use tiangong_deepseek::types::{
        OutputContentBlock, OutputFunctionCall, OutputMessage, ResponseStatus, TextBlock,
    };

    async fn collect(
        events: Vec<Result<ResponsesStreamEvent, tiangong_deepseek::DeepSeekError>>,
    ) -> Vec<Result<ProviderStreamEvent, LlmError>> {
        let stream: tiangong_deepseek::types::ResponsesEventStream =
            Box::pin(futures_util::stream::iter(events));
        map_deepseek_stream(stream)
            .map(|result| result)
            .collect()
            .await
    }

    fn text_event(delta: &str) -> ResponsesStreamEvent {
        ResponsesStreamEvent::OutputTextDelta {
            sequence_number: 1,
            item_id: "msg_1".to_string(),
            output_index: 1,
            content_index: 0,
            delta: delta.to_string(),
        }
    }

    fn function_call_item(arguments: &str) -> ResponseOutputItem {
        ResponseOutputItem::FunctionCall(OutputFunctionCall {
            id: Some("fc_1".to_string()),
            status: None,
            call_id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: arguments.to_string(),
        })
    }

    fn completed_response(output: Vec<ResponseOutputItem>) -> ResponseObject {
        ResponseObject {
            id: "resp_1".to_string(),
            object: "response".to_string(),
            created_at: 0,
            status: ResponseStatus::Completed,
            error: None,
            incomplete_details: None,
            model: "deepseek-v4-pro".to_string(),
            output,
            usage: Some(tiangong_deepseek::types::ResponseUsage {
                input_tokens: 10,
                input_tokens_details: None,
                output_tokens: 5,
                output_tokens_details: None,
                total_tokens: 15,
            }),
        }
    }

    #[tokio::test]
    async fn text_stream_maps_to_start_delta_end() {
        let events = vec![
            Ok(ResponsesStreamEvent::ResponseCreated {
                sequence_number: 0,
                response: completed_response(Vec::new()),
            }),
            Ok(text_event("你")),
            Ok(text_event("好")),
            Ok(ResponsesStreamEvent::ResponseCompleted {
                sequence_number: 9,
                response: completed_response(vec![ResponseOutputItem::Message(OutputMessage {
                    id: Some("msg_1".to_string()),
                    status: None,
                    role: "assistant".to_string(),
                    content: vec![OutputContentBlock::OutputText(TextBlock {
                        text: "你好".to_string(),
                    })],
                })]),
            }),
        ];
        let out = collect(events).await;
        let flattened: Vec<_> = out.into_iter().map(|e| e.expect("ok")).collect();
        assert!(matches!(flattened[0], ProviderStreamEvent::MessageStart));
        assert!(matches!(
            &flattened[1],
            ProviderStreamEvent::TextDelta(text) if text == "你"
        ));
        // usage 在 completed 时发出。
        assert!(
            flattened
                .iter()
                .any(|event| matches!(event, ProviderStreamEvent::Usage(_)))
        );
        assert!(matches!(
            flattened.last(),
            Some(ProviderStreamEvent::MessageEnd {
                stop_reason: Some(StopReason::EndTurn)
            })
        ));
    }

    #[tokio::test]
    async fn function_call_delta_events_assemble_to_tool_call() {
        let events = vec![
            Ok(ResponsesStreamEvent::ResponseCreated {
                sequence_number: 0,
                response: completed_response(Vec::new()),
            }),
            Ok(ResponsesStreamEvent::OutputItemAdded {
                sequence_number: 2,
                output_index: 0,
                item: function_call_item(""),
            }),
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDelta {
                sequence_number: 3,
                item_id: "fc_1".to_string(),
                output_index: 0,
                delta: "{\"path\":".to_string(),
            }),
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDelta {
                sequence_number: 4,
                item_id: "fc_1".to_string(),
                output_index: 0,
                delta: "\"/tmp\"}".to_string(),
            }),
            Ok(ResponsesStreamEvent::OutputItemDone {
                sequence_number: 5,
                output_index: 0,
                item: function_call_item("{\"path\":\"/tmp\"}"),
            }),
            Ok(ResponsesStreamEvent::ResponseCompleted {
                sequence_number: 6,
                response: completed_response(vec![function_call_item("{\"path\":\"/tmp\"}")]),
            }),
        ];
        let out = collect(events).await;
        let flattened: Vec<_> = out.into_iter().map(|e| e.expect("ok")).collect();
        // 增量已收到，completed 不再重复补发参数。
        let argument_deltas: Vec<_> = flattened
            .iter()
            .filter_map(|event| match event {
                ProviderStreamEvent::ToolCallDelta {
                    call_id,
                    partial_json,
                } => Some((call_id.clone(), partial_json.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            argument_deltas,
            vec![
                ("call_1".to_string(), "{\"path\":".to_string()),
                ("call_1".to_string(), "\"/tmp\"}".to_string()),
            ],
            "completed 兜底不应重复补发：{argument_deltas:?}"
        );
        assert!(matches!(
            &flattened[1],
            ProviderStreamEvent::ToolCallStart(call) if call.id == "call_1"
        ));
        assert!(flattened.iter().any(|event| matches!(
            event,
            ProviderStreamEvent::ToolCallEnd { call_id } if call_id == "call_1"
        )));
        assert!(matches!(
            flattened.last(),
            Some(ProviderStreamEvent::MessageEnd {
                stop_reason: Some(StopReason::ToolUse)
            })
        ));
    }

    #[tokio::test]
    async fn completed_backfills_full_arguments_without_deltas() {
        // 网关不发参数增量：completed 的 output 里补发完整参数。
        let events = vec![
            Ok(ResponsesStreamEvent::ResponseCreated {
                sequence_number: 0,
                response: completed_response(Vec::new()),
            }),
            Ok(ResponsesStreamEvent::OutputItemAdded {
                sequence_number: 2,
                output_index: 0,
                item: function_call_item("{\"path\":\"/tmp\"}"),
            }),
            Ok(ResponsesStreamEvent::ResponseCompleted {
                sequence_number: 6,
                response: completed_response(vec![function_call_item("{\"path\":\"/tmp\"}")]),
            }),
        ];
        let out = collect(events).await;
        let flattened: Vec<_> = out.into_iter().map(|e| e.expect("ok")).collect();
        // added 已携带完整参数并即时下发，completed 不重复。
        let argument_deltas: Vec<_> = flattened
            .iter()
            .filter_map(|event| match event {
                ProviderStreamEvent::ToolCallDelta {
                    call_id,
                    partial_json,
                } => Some((call_id.clone(), partial_json.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            argument_deltas,
            vec![("call_1".to_string(), "{\"path\":\"/tmp\"}".to_string())]
        );
    }

    #[tokio::test]
    async fn failed_terminal_event_maps_to_error_and_end() {
        let mut response = completed_response(Vec::new());
        response.status = ResponseStatus::Failed;
        response.error = Some(tiangong_deepseek::types::ResponseError {
            code: Some("server_error".to_string()),
            message: Some("内部错误".to_string()),
        });
        let events = vec![Ok(ResponsesStreamEvent::ResponseFailed {
            sequence_number: 3,
            response,
        })];
        let out = collect(events).await;
        let flattened: Vec<_> = out.into_iter().map(|e| e.expect("ok")).collect();
        assert!(flattened.iter().any(|event| matches!(
            event,
            ProviderStreamEvent::Error(message) if message == "内部错误"
        )));
        // MessageEnd 先于 Error：上层遇 Error 即中断，先收尾才能保留 stop_reason。
        assert!(flattened.iter().any(|event| matches!(
            event,
            ProviderStreamEvent::MessageEnd {
                stop_reason: Some(StopReason::Other(reason))
            } if reason == "failed"
        )));
        assert!(matches!(
            flattened.last(),
            Some(ProviderStreamEvent::Error(_))
        ));
    }

    #[tokio::test]
    async fn incomplete_terminal_event_does_not_emit_error() {
        // incomplete 是截断而非错误：只发 Usage + MessageEnd，不发 Error，
        // 避免上层收到 Error 中断收尾导致截断信息被吞。
        let mut response = completed_response(Vec::new());
        response.status = ResponseStatus::Incomplete;
        response.incomplete_details = Some(tiangong_deepseek::types::IncompleteDetails {
            reason: Some("max_output_tokens".to_string()),
        });
        let events = vec![Ok(ResponsesStreamEvent::ResponseIncomplete {
            sequence_number: 3,
            response,
        })];
        let out = collect(events).await;
        let flattened: Vec<_> = out.into_iter().map(|e| e.expect("ok")).collect();
        assert!(
            !flattened
                .iter()
                .any(|event| matches!(event, ProviderStreamEvent::Error(_))),
            "incomplete 不应发 Error：{flattened:?}"
        );
        assert!(matches!(
            flattened.last(),
            Some(ProviderStreamEvent::MessageEnd {
                stop_reason: Some(StopReason::MaxTokens)
            })
        ));
        assert!(
            flattened
                .iter()
                .any(|event| matches!(event, ProviderStreamEvent::Usage(_)))
        );
    }

    #[tokio::test]
    async fn full_arguments_in_added_item_do_not_duplicate_with_deltas() {
        // 网关在 added item 携带完整参数后仍发增量：增量是权威，
        // added 的快照不得再次下发（否则参数重复拼接）。
        let events = vec![
            Ok(ResponsesStreamEvent::OutputItemAdded {
                sequence_number: 1,
                output_index: 0,
                item: function_call_item("{\"path\":\"/tmp\"}"),
            }),
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDelta {
                sequence_number: 2,
                item_id: "fc_1".to_string(),
                output_index: 0,
                delta: "{\"path\":".to_string(),
            }),
            Ok(ResponsesStreamEvent::FunctionCallArgumentsDelta {
                sequence_number: 3,
                item_id: "fc_1".to_string(),
                output_index: 0,
                delta: "\"/tmp\"}".to_string(),
            }),
            Ok(ResponsesStreamEvent::OutputItemDone {
                sequence_number: 4,
                output_index: 0,
                item: function_call_item("{\"path\":\"/tmp\"}"),
            }),
            Ok(ResponsesStreamEvent::ResponseCompleted {
                sequence_number: 5,
                response: completed_response(vec![function_call_item("{\"path\":\"/tmp\"}")]),
            }),
        ];
        let out = collect(events).await;
        let flattened: Vec<_> = out.into_iter().map(|e| e.expect("ok")).collect();
        let joined: String = flattened
            .iter()
            .filter_map(|event| match event {
                ProviderStreamEvent::ToolCallDelta { partial_json, .. } => {
                    Some(partial_json.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            joined, "{\"path\":\"/tmp\"}",
            "参数只应来自增量拼接，added 快照不得重复下发：{flattened:?}"
        );
    }
}
