use futures_util::{StreamExt, stream};

use crate::client::DeepSeekClient;
use crate::error::DeepSeekError;
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, EventStream, StreamChunk, StreamEvent,
};

/// 把一次性完整响应合成为等价的流事件序列。
///
/// 网关忽略 stream 参数返回 application/json 时，SDK 在内部按完整响应
/// 接住并合成事件流，调用方无需感知差异。
fn complete_response_events(response: ChatCompletionResponse) -> Vec<StreamEvent> {
    let Some(choice) = response.choices.into_iter().next() else {
        return vec![StreamEvent::Done];
    };
    let mut events = Vec::new();
    if let Some(reasoning) = choice
        .message
        .reasoning_content
        .filter(|value| !value.is_empty())
    {
        events.push(StreamEvent::ReasoningDelta(reasoning));
    }
    if let Some(text) = choice.message.content.filter(|value| !value.is_empty()) {
        events.push(StreamEvent::TextDelta(text));
    }
    for (index, tool_call) in choice
        .message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        events.push(StreamEvent::ToolCallStart {
            id: tool_call.id,
            name: tool_call.function.name,
        });
        if !tool_call.function.arguments.is_empty() {
            events.push(StreamEvent::ToolCallDelta {
                index: index as u32,
                arguments: tool_call.function.arguments,
            });
        }
    }
    events.push(StreamEvent::Usage(response.usage));
    events.push(StreamEvent::FinishReason(choice.finish_reason));
    events.push(StreamEvent::Done);
    events
}

pub struct Chat<'c> {
    client: &'c DeepSeekClient,
}

impl<'c> Chat<'c> {
    pub fn new(client: &'c DeepSeekClient) -> Self {
        Self { client }
    }

    pub async fn create(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, DeepSeekError> {
        self.client.post("/chat/completions", &request).await
    }

    /// 创建流式对话。
    ///
    /// SDK 内置文本工具调用协议兜底：当模型把工具调用写进 `content` 文本（原生协议
    /// 或 DSML 协议）而非结构化 `tool_calls` 字段时，SDK 会在正常接收流式响应的同时
    /// 收集协议文本，并在完整响应结束后统一解析为 `StreamEvent::TextProtocolToolCall`。
    pub async fn create_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<EventStream, DeepSeekError> {
        // 仅当请求携带 tools 时才启用文本协议兜底缓冲。即使 tool_choice=none，调用方
        // 也可能保留 tools schema；若模型仍返回文本工具调用，流末会完整识别并交由上层续作。
        let enable_buffer = request.tools.is_some();
        // 建连与等待响应头受用户配置的请求超时约束，避免网关建连后
        // 不返回响应头导致永久等待；SSE 建流成功后不受总时限限制。
        let timeout = self.client.stream_timeout();
        let mut response = tokio::time::timeout(
            timeout,
            self.client.post_stream_raw("/chat/completions", &request),
        )
        .await
        .map_err(|_| DeepSeekError::Timeout(format!("{} ms", timeout.as_millis())))??;

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();

        let flat: EventStream = if content_type.contains("text/event-stream") {
            Box::pin(chat_sse_chunk_stream(response.bytes_stream()))
        } else {
            // 类型不明确（部分网关漏标或错标）时按首块内容探测：
            // JSON 以 `{`/`[` 开头，SSE 事件以 `data:` 等开头。
            let first = response
                .chunk()
                .await
                .map_err(|err| DeepSeekError::Transport(err.to_string()))?
                .unwrap_or_default();
            if content_type.contains("json") || looks_like_json(&first) {
                // 网关忽略 stream 参数返回一次性 JSON：SSE 解析器会把这些行全部当
                // 未知字段丢弃且不报错，在 SDK 内按完整响应接住并合成事件流。
                // 完整读取受超时约束，避免迟迟不结束的响应永久等待。
                let complete: ChatCompletionResponse = crate::client::read_complete_response(
                    response,
                    first,
                    self.client.stream_timeout(),
                )
                .await?;
                Box::pin(stream::iter(
                    complete_response_events(complete).into_iter().map(Ok),
                ))
            } else {
                // 首块不是 JSON：按 SSE 流解析，把首块拼回流头。
                let byte_stream = stream::once(async move { Ok::<_, reqwest::Error>(first) })
                    .chain(response.bytes_stream());
                Box::pin(chat_sse_chunk_stream(byte_stream))
            }
        };

        // 第二层：文本协议兜底缓冲。普通文本正常透传；确认命中文本工具协议后，
        // 等完整响应结束再统一解析，避免按单个 invoke 尾缀提前拆散一组工具调用。
        // 一次性 JSON 分支同样过缓冲，保持文本协议工具调用识别一致。
        if !enable_buffer {
            return Ok(flat);
        }

        let state = BufferState::default();
        let buffered = flat
            .scan(state, |state, result| {
                std::future::ready(Some(apply_buffer(state, result)))
            })
            .flat_map(stream::iter);

        Ok(Box::pin(buffered))
    }
}

/// SSE 字节流 → Vec<StreamEvent> 流（单个 chunk 可能产出多个事件）。
fn chat_sse_chunk_stream<S>(
    byte_stream: S,
) -> impl futures_util::Stream<Item = Result<StreamEvent, DeepSeekError>>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    use eventsource_stream::Eventsource;
    use futures_util::StreamExt;
    let sse_stream = Box::pin(byte_stream.eventsource());
    let chunk_stream = stream::unfold(sse_stream, |mut sse_stream| async move {
        match sse_stream.next().await {
            Some(Ok(message)) => {
                if message.data == "[DONE]" {
                    Some((vec![Ok(StreamEvent::Done)], sse_stream))
                } else {
                    Some((parse_stream_chunk(&message.data), sse_stream))
                }
            }
            Some(Err(err)) => Some((
                vec![Err(DeepSeekError::Stream(err.to_string()))],
                sse_stream,
            )),
            None => None,
        }
    });
    chunk_stream.flat_map(stream::iter)
}

/// 判断响应首块是否是 JSON（跳过空白后以 `{` 或 `[` 开头）。
fn looks_like_json(first: &[u8]) -> bool {
    first
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'{' || *byte == b'[')
}

/// 文本协议缓冲状态（三态状态机见 [`crate::protocol_buffer`]）。
#[derive(Default)]
struct BufferState {
    buffer: crate::protocol_buffer::TextProtocolBuffer,
}
/// 对单个事件施加缓冲逻辑，可能产出 0~N 个事件。
fn apply_buffer(
    state: &mut BufferState,
    result: Result<StreamEvent, DeepSeekError>,
) -> Vec<Result<StreamEvent, DeepSeekError>> {
    match result {
        Ok(StreamEvent::TextDelta(delta)) => state
            .buffer
            .push_text(&delta)
            .into_iter()
            .map(|text| Ok(StreamEvent::TextDelta(text)))
            .collect(),
        Ok(StreamEvent::ToolCallStart { .. }) => {
            let mut events: Vec<_> = state
                .buffer
                .note_structured_tool_call()
                .into_iter()
                .map(|text| Ok(StreamEvent::TextDelta(text)))
                .collect();
            events.push(result);
            events
        }
        Ok(StreamEvent::Done) => {
            let outcome = state.buffer.finish();
            let mut events = Vec::new();
            if !outcome.leftover_text.is_empty() {
                events.push(Ok(StreamEvent::TextDelta(outcome.leftover_text)));
            }
            for (idx, call) in outcome.tool_calls.into_iter().enumerate() {
                events.push(Ok(StreamEvent::TextProtocolToolCall {
                    id: format!("textcall_{idx}"),
                    name: call.name,
                    arguments: call.arguments,
                }));
            }
            events.push(Ok(StreamEvent::Done));
            events
        }
        // Usage 等其余事件不是内容结束标记，原样透传；
        // 完整 content 的统一识别留到 Done。
        other => vec![other],
    }
}
/// 解析单个 SSE chunk，可能产出多个事件。
///
/// 同一 delta 理论上可同时携带 reasoning_content / content / tool_calls，
/// 收集全部而非只取首个非空字段。
pub(crate) fn parse_stream_chunk(data: &str) -> Vec<Result<StreamEvent, DeepSeekError>> {
    let chunk: StreamChunk = match serde_json::from_str(data) {
        Ok(chunk) => chunk,
        Err(err) => return vec![Err(DeepSeekError::Serialization(format!("{err}: {data}")))],
    };

    let mut events = Vec::new();

    if let Some(usage) = chunk.usage {
        events.push(Ok(StreamEvent::Usage(usage)));
    }

    for choice in chunk.choices {
        // OpenAI 兼容协议会发送只含 role 的首片（{"role":"assistant"}）和只含
        // finish_reason 的结束片（delta 全空）。这些是正常 chunk，不应报错。
        let delta = choice.delta;
        if let Some(reasoning) = delta.reasoning_content.filter(|s| !s.is_empty()) {
            events.push(Ok(StreamEvent::ReasoningDelta(reasoning)));
        }
        if let Some(content) = delta.content.filter(|s| !s.is_empty()) {
            events.push(Ok(StreamEvent::TextDelta(content)));
        }
        if let Some(tool_calls) = delta.tool_calls {
            for tc in tool_calls {
                if let Some(func) = tc.function.as_ref()
                    && let Some(name) = func.name.as_ref().filter(|n| !n.is_empty())
                {
                    let id = tc.id.clone().unwrap_or_default();
                    events.push(Ok(StreamEvent::ToolCallStart {
                        id,
                        name: name.clone(),
                    }));
                }
                if let Some(func) = tc.function
                    && let Some(args) = func.arguments.filter(|a| !a.is_empty())
                {
                    events.push(Ok(StreamEvent::ToolCallDelta {
                        index: tc.index,
                        arguments: args,
                    }));
                }
            }
        }
        if let Some(reason) = choice.finish_reason {
            events.push(Ok(StreamEvent::FinishReason(reason)));
        }
    }

    events
}

#[cfg(test)]
mod buffer_tests {
    use super::*;
    use crate::types::Usage;

    fn text_delta(s: &str) -> Result<StreamEvent, DeepSeekError> {
        Ok(StreamEvent::TextDelta(s.into()))
    }

    fn collect(events: Vec<Result<StreamEvent, DeepSeekError>>) -> Vec<String> {
        let mut state = BufferState::default();
        let mut out = Vec::new();
        for event in events {
            for mapped in apply_buffer(&mut state, event) {
                out.push(format!("{:?}", mapped.expect("应为 Ok")));
            }
        }
        out
    }

    #[test]
    fn plain_text_without_angle_bracket_passes_through() {
        // 不含 < 的纯文本逐字透传，不触发缓冲。
        let out = collect(vec![text_delta("普通回复"), Ok(StreamEvent::Done)]);
        assert!(out.iter().any(|e| e.contains("TextDelta(\"普通回复")));
        assert!(!out.iter().any(|e| e.contains("TextProtocolToolCall")));
    }

    #[test]
    fn angle_bracket_in_normal_text_flushes_after_window() {
        // 正常文本里的 < 触发缓冲，窗口满（10 字符）仍无 tool/dsml → 整块吐出。
        let out = collect(vec![
            text_delta("比较 a < b 这是正常文本无关键词"),
            Ok(StreamEvent::Done),
        ]);
        let combined: String = out
            .iter()
            .filter(|e| e.contains("TextDelta"))
            .cloned()
            .collect();
        assert!(
            combined.contains("比较 a < b 这是正常文本无关键词"),
            "正常尖括号文本应完整吐出：{out:?}"
        );
        assert!(!out.iter().any(|e| e.contains("TextProtocolToolCall")));
    }

    #[test]
    fn dsml_text_buffered_and_resolved_as_tool_call() {
        let dsml = "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"read_file\">\n<｜｜DSML｜｜parameter name=\"path\" string=\"true\">/tmp/x</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>";
        let out = collect(vec![text_delta(dsml), Ok(StreamEvent::Done)]);
        assert!(
            !out.iter().any(|e| e.contains("DSML") || e.contains("｜｜")),
            "DSML 标记原文不应透传：{out:?}"
        );
        assert!(
            out.iter()
                .any(|e| e.contains("TextProtocolToolCall") && e.contains("read_file")),
            "应产出工具调用：{out:?}"
        );
    }

    #[test]
    fn dsml_preserves_explanatory_text() {
        let text = format!(
            "我来读取文件。\n{dsml}",
            dsml = "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"read_file\">\n<｜｜DSML｜｜parameter name=\"path\" string=\"true\">/tmp/x</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>"
        );
        let out = collect(vec![text_delta(&text), Ok(StreamEvent::Done)]);
        assert!(
            out.iter().any(|e| e.contains("我来读取文件")),
            "说明文字应补发：{out:?}"
        );
    }

    #[test]
    fn dsml_split_across_chunks_is_detected() {
        // 分片到达：每个碎片单独不含完整关键词，但累积后窗口内出现 dsml → 确认。
        let out = collect(vec![
            text_delta("<"),
            text_delta("｜"),
            text_delta("｜DS"),
            text_delta("ML｜｜invoke name=\"fn\">"),
            text_delta(
                "<｜｜DSML｜｜parameter name=\"x\" string=\"true\">1</｜｜DSML｜｜parameter>",
            ),
            text_delta("</｜｜DSML｜｜invoke>"),
            Ok(StreamEvent::Done),
        ]);
        let leaked: Vec<_> = out
            .iter()
            .filter(|e| e.contains("DSML") || e.contains("｜｜"))
            .collect();
        assert!(leaked.is_empty(), "分片到达不应透传 DSML 碎片：{leaked:?}");
        assert!(out.iter().any(|e| e.contains("TextProtocolToolCall")));
    }

    #[test]
    fn structured_tool_call_flushes_pending_and_disables_buffering() {
        // Probing 期间出现结构化 ToolCallStart：先吐出 pending，后续不再缓冲。
        let out = collect(vec![
            text_delta("读取"),  // 无 <，正常透传
            text_delta("<未完"), // < 触发 Probing
            Ok(StreamEvent::ToolCallStart {
                id: "call_0".into(),
                name: "read_file".into(),
            }),
            text_delta("正在执行"), // 结构化已出现，不再缓冲
            Ok(StreamEvent::Done),
        ]);
        assert!(
            out.iter().any(|e| e.contains("TextDelta(\"读取")),
            "缓冲前文本应透传：{out:?}"
        );
        assert!(
            out.iter().any(|e| e.contains("TextDelta(\"<未完")),
            "pending 应在 ToolCallStart 前吐出：{out:?}"
        );
        assert!(out.iter().any(|e| e.contains("ToolCallStart")));
        assert!(
            out.iter().any(|e| e.contains("TextDelta(\"正在执行")),
            "结构化工具调用后文本应正常透传：{out:?}"
        );
    }

    #[test]
    fn probing_exits_when_window_exhausted() {
        // < 后 10 字符内无 tool/dsml → 窗口耗尽整块吐出，恢复正常透传。
        let out = collect(vec![
            text_delta("前文"),
            text_delta("<abcdefghij"), // 10 字符无关键词 → 窗口耗尽吐出
            text_delta("后续正常"),    // 已回 Idle，正常透传
            Ok(StreamEvent::Done),
        ]);
        let combined: String = out
            .iter()
            .filter(|e| e.contains("TextDelta"))
            .cloned()
            .collect();
        assert!(
            combined.contains("<abcdefghij"),
            "窗口耗尽应整块吐出：{out:?}"
        );
        assert!(
            out.iter().any(|e| e.contains("TextDelta(\"后续正常")),
            "退出后应正常透传：{out:?}"
        );
        assert!(!out.iter().any(|e| e.contains("TextProtocolToolCall")));
    }

    #[test]
    fn code_block_not_infinitely_buffered() {
        // 代码块含多个 <，每个 < 后窗口内均无关键词 → 各自整块吐出。
        let out = collect(vec![
            text_delta("```html\n<div><span>hello</span></div>\n```"),
            Ok(StreamEvent::Done),
        ]);
        let combined: String = out
            .iter()
            .filter(|e| e.contains("TextDelta"))
            .cloned()
            .collect();
        assert!(
            combined.contains("<div>") && combined.contains("<span>hello</span>"),
            "代码块应完整吐出不被吞：{out:?}"
        );
        assert!(!out.iter().any(|e| e.contains("TextProtocolToolCall")));
    }

    #[test]
    fn usage_waits_for_done_before_resolution() {
        // Usage 可能先于 Done 到达，不能用它提前解析尚未结束的 content。
        let dsml = "<｜｜DSML｜｜invoke name=\"fn\">\n<｜｜DSML｜｜parameter name=\"x\" string=\"true\">1</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>";
        let usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            prompt_cache_hit_tokens: None,
            prompt_cache_miss_tokens: None,
            completion_tokens_details: None,
        };
        let mut state = BufferState::default();
        assert!(apply_buffer(&mut state, text_delta(dsml)).is_empty());
        let usage_events = apply_buffer(&mut state, Ok(StreamEvent::Usage(usage)));
        assert!(
            usage_events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::Usage(_))))
        );
        assert!(
            usage_events
                .iter()
                .all(|event| !matches!(event, Ok(StreamEvent::TextProtocolToolCall { .. })))
        );

        let done_events = apply_buffer(&mut state, Ok(StreamEvent::Done));
        assert!(
            done_events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::TextProtocolToolCall { .. })))
        );
    }

    #[test]
    fn complete_response_resolves_multiple_invokes_as_one_group() {
        let dsml = "<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name=\"fn_a\">
<｜｜DSML｜｜parameter name=\"x\" string=\"true\">1</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
<｜｜DSML｜｜invoke name=\"fn_b\">
<｜｜DSML｜｜parameter name=\"y\" string=\"true\">2</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
</｜｜DSML｜｜tool_calls>";
        let mut state = BufferState::default();
        let first_invoke_end =
            dsml.find("</｜｜DSML｜｜invoke>").unwrap() + "</｜｜DSML｜｜invoke>".len();
        assert!(
            apply_buffer(&mut state, text_delta(&dsml[..first_invoke_end])).is_empty(),
            "第一个 invoke 结束时不能提前解析"
        );
        assert!(
            apply_buffer(&mut state, text_delta(&dsml[first_invoke_end..])).is_empty(),
            "完整 content 结束前不能提前解析"
        );

        let out = apply_buffer(&mut state, Ok(StreamEvent::Done));
        let calls = out
            .iter()
            .filter_map(|event| match event {
                Ok(StreamEvent::TextProtocolToolCall { id, name, .. }) => {
                    Some((id.as_str(), name.as_str()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls, vec![("textcall_0", "fn_a"), ("textcall_1", "fn_b")]);
        assert!(
            out.iter().all(
                |event| !matches!(event, Ok(StreamEvent::TextDelta(text)) if text.contains("DSML"))
            ),
            "完整工具调用组不应泄漏协议标记：{out:?}"
        );
    }

    #[test]
    fn long_tool_arguments_are_not_downgraded_to_plain_text() {
        let value = "a".repeat(5_000);
        let dsml = format!(
            "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"write_file\">\n<｜｜DSML｜｜parameter name=\"content\" string=\"true\">{value}</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>"
        );
        let out = collect(vec![text_delta(&dsml), Ok(StreamEvent::Done)]);
        assert!(
            out.iter()
                .any(|event| event.contains("TextProtocolToolCall") && event.contains("write_file")),
            "长参数仍应识别为工具调用"
        );
        assert!(
            !out.iter()
                .any(|event| event.contains("TextDelta") && event.contains("DSML")),
            "长参数工具调用不应作为普通文本透传"
        );
    }

    #[test]
    fn toolbar_text_does_not_enter_protocol_buffering() {
        let mut state = BufferState::default();
        let out = apply_buffer(
            &mut state,
            text_delta("<toolbar>普通内容继续输出，不是工具协议"),
        );
        assert!(out.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::TextDelta(text)) if text.contains("<toolbar>")
        )));
        assert!(
            matches!(
                out.last(),
                Some(Ok(StreamEvent::TextDelta(_))) | Some(Ok(StreamEvent::Done))
            ),
            "非协议文本应即时吐出而非滞留缓冲：{out:?}"
        );
    }
}
