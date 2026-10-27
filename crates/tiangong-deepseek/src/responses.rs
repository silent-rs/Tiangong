use futures_util::{StreamExt, stream};

use crate::client::DeepSeekClient;
use crate::error::DeepSeekError;
use crate::types::{
    CreateResponseRequest, ResponseObject, ResponseOutputItem, ResponsesEventStream,
    ResponsesStreamEvent,
};

pub struct Responses<'c> {
    client: &'c DeepSeekClient,
}

impl<'c> Responses<'c> {
    pub fn new(client: &'c DeepSeekClient) -> Self {
        Self { client }
    }

    /// 创建非流式响应。
    ///
    /// Responses API 为无状态设计：服务端不存储会话，多轮对话需在
    /// `input` 中回传完整历史。输入超出上下文窗口时服务端直接返回 400。
    ///
    /// 无论调用方传入什么 `stream` 值，此方法始终强制关闭流式，
    /// 保证返回值按普通 JSON 解析。
    pub async fn create(
        &self,
        mut request: CreateResponseRequest,
    ) -> Result<ResponseObject, DeepSeekError> {
        request.stream = Some(false);
        self.client.post("/responses", &request).await
    }

    /// 创建流式响应。
    ///
    /// DeepSeek 不发送 `[DONE]` 标记；终止事件
    /// （`response.completed` / `response.incomplete` / `response.failed`）
    /// 产出后流即结束，其余后续消息被忽略。
    /// 创建流式响应。
    ///
    /// DeepSeek 不发送 `[DONE]` 标记；终止事件
    /// （`response.completed` / `response.incomplete` / `response.failed`）
    /// 产出后流即结束，其余后续消息被忽略。
    ///
    /// 与 chat 一致，SDK 内置文本工具调用协议兜底：当请求携带 tools 且模型把
    /// 工具调用写进 `output_text` 正文（原生协议或 DSML 协议）而非结构化
    /// `function_call` item 时，SDK 在正常接收流式响应的同时收集协议文本，
    /// 并在终止事件前统一解析、合成结构化 function_call 事件组，调用方无需感知。
    pub async fn create_stream(
        &self,
        mut request: CreateResponseRequest,
    ) -> Result<ResponsesEventStream, DeepSeekError> {
        request.stream = Some(true);
        // 仅当请求携带 tools 时才启用文本协议兜底缓冲。即使 tool_choice=none，
        // 调用方也可能保留 tools schema；若模型仍返回文本工具调用，流末会完整
        // 识别并交由上层续作。
        let enable_buffer = request.tools.is_some();
        // 建连与等待响应头受用户配置的请求超时约束，避免网关建连后
        // 不返回响应头导致永久等待；SSE 建流成功后不受总时限限制。
        let timeout = self.client.stream_timeout();
        let mut response =
            tokio::time::timeout(timeout, self.client.post_stream_raw("/responses", &request))
                .await
                .map_err(|_| DeepSeekError::Timeout(format!("{} ms", timeout.as_millis())))??;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let flat: ResponsesEventStream = if !content_type.contains("text/event-stream") {
            // 类型不明确（部分网关漏标或错标）时按首块内容探测。
            let first = response
                .chunk()
                .await
                .map_err(|err| DeepSeekError::Transport(err.to_string()))?
                .unwrap_or_default();
            if content_type.contains("json") || looks_like_json(&first) {
                // 网关忽略 stream 参数返回一次性 JSON：SSE 解析器会把这些行全部当
                // 未知字段丢弃且不报错，在 SDK 内按完整响应接住并合成事件流。
                // 完整读取受超时约束，避免迟迟不结束的响应永久等待。
                let complete: ResponseObject = crate::client::read_complete_response(
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
                Box::pin(responses_sse_event_stream(byte_stream))
            }
        } else {
            Box::pin(responses_sse_event_stream(response.bytes_stream()))
        };
        // 第二层：文本协议兜底缓冲。普通文本正常透传；确认命中文本工具协议后，
        // 等终止事件到达再统一解析，避免按单个 invoke 尾缀提前拆散一组工具调用。
        if !enable_buffer {
            return Ok(flat);
        }
        let state = ResponsesBufferState::default();
        let buffered = flat
            .scan(state, |state, result| {
                std::future::ready(Some(apply_responses_buffer(state, result)))
            })
            .flat_map(stream::iter);
        Ok(Box::pin(buffered))
    }
}

/// 文本协议缓冲状态（三态状态机见 [`crate::protocol_buffer`]）。
#[derive(Default)]
struct ResponsesBufferState {
    buffer: crate::protocol_buffer::TextProtocolBuffer,
}

/// 对单个 Responses 流事件施加文本协议缓冲，可能产出 0~N 个事件。
///
/// 普通文本正常透传；确认命中文本工具协议后，等终止事件到达再统一解析，
/// 并把工具调用合成为结构化 function_call 事件组（added → arguments.delta →
/// done），与真实流式行为保持一致。
fn apply_responses_buffer(
    state: &mut ResponsesBufferState,
    result: Result<ResponsesStreamEvent, DeepSeekError>,
) -> Vec<Result<ResponsesStreamEvent, DeepSeekError>> {
    /// 用透传文本重建 OutputTextDelta 事件（合成分片无真实序号，置 0）。
    fn text_delta_event(delta: String) -> ResponsesStreamEvent {
        ResponsesStreamEvent::OutputTextDelta {
            sequence_number: 0,
            item_id: String::new(),
            output_index: 0,
            content_index: 0,
            delta,
        }
    }
    match result {
        Ok(ResponsesStreamEvent::OutputTextDelta { delta, .. }) => state
            .buffer
            .push_text(&delta)
            .into_iter()
            .map(|text| Ok(text_delta_event(text)))
            .collect(),
        // 结构化工具调用 item 出现：此后不可能再走文本协议，冲刷缓冲并透传。
        Ok(
            event @ ResponsesStreamEvent::OutputItemAdded {
                item: ResponseOutputItem::FunctionCall(_) | ResponseOutputItem::CustomToolCall(_),
                ..
            },
        ) => {
            let mut events: Vec<_> = state
                .buffer
                .note_structured_tool_call()
                .into_iter()
                .map(|text| Ok(text_delta_event(text)))
                .collect();
            events.push(Ok(event));
            events
        }
        // 终止事件：统一解析缓冲文本后再放行，保证合成事件组先于终止事件。
        Ok(
            terminal @ (ResponsesStreamEvent::ResponseCompleted { .. }
            | ResponsesStreamEvent::ResponseIncomplete { .. }
            | ResponsesStreamEvent::ResponseFailed { .. }),
        ) => {
            let outcome = state.buffer.finish();
            let mut events = Vec::new();
            if !outcome.leftover_text.is_empty() {
                events.push(Ok(text_delta_event(outcome.leftover_text)));
            }
            for (index, call) in outcome.tool_calls.into_iter().enumerate() {
                events.extend(text_protocol_call_events(index as u32, call));
            }
            events.push(Ok(terminal));
            events
        }
        other => vec![other],
    }
}

/// 把文本协议解析出的单个工具调用合成为结构化 function_call 事件组。
fn text_protocol_call_events(
    index: u32,
    call: crate::dsml::DsmlToolCall,
) -> Vec<Result<ResponsesStreamEvent, DeepSeekError>> {
    let call_id = format!("textcall_{index}");
    let make_item = |arguments: String| {
        ResponseOutputItem::FunctionCall(crate::types::OutputFunctionCall {
            id: None,
            status: None,
            call_id: call_id.clone(),
            name: call.name.clone(),
            arguments,
        })
    };
    vec![
        Ok(ResponsesStreamEvent::OutputItemAdded {
            sequence_number: 0,
            output_index: index,
            item: make_item(String::new()),
        }),
        Ok(ResponsesStreamEvent::FunctionCallArgumentsDelta {
            sequence_number: 0,
            item_id: call_id.clone(),
            output_index: index,
            delta: call.arguments.clone(),
        }),
        Ok(ResponsesStreamEvent::OutputItemDone {
            sequence_number: 0,
            output_index: index,
            item: make_item(call.arguments),
        }),
    ]
}

/// SSE 字节流转 Responses 流事件；终止事件后流结束。
fn responses_sse_event_stream<S>(
    byte_stream: S,
) -> impl futures_util::Stream<Item = Result<ResponsesStreamEvent, DeepSeekError>>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    use eventsource_stream::Eventsource;
    use futures_util::StreamExt;
    let sse_stream = Box::pin(byte_stream.eventsource());
    stream::unfold(
        (sse_stream, false),
        |(mut sse_stream, terminated)| async move {
            if terminated {
                return None;
            }
            match sse_stream.next().await {
                Some(Ok(message)) => {
                    let event = parse_stream_event(&message.data);
                    let is_terminal = matches!(
                        &event,
                        Ok(ResponsesStreamEvent::ResponseCompleted { .. }
                            | ResponsesStreamEvent::ResponseIncomplete { .. }
                            | ResponsesStreamEvent::ResponseFailed { .. })
                    );
                    Some((event, (sse_stream, is_terminal)))
                }
                Some(Err(err)) => Some((
                    Err(DeepSeekError::Stream(err.to_string())),
                    (sse_stream, true),
                )),
                None => None,
            }
        },
    )
}

/// 判断响应首块是否是 JSON（跳过空白后以 `{` 或 `[` 开头）。
fn looks_like_json(first: &[u8]) -> bool {
    first
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'{' || *byte == b'[')
}

/// 把一次性完整响应合成为等价的流事件序列。
///
/// 网关忽略 stream 参数返回 application/json 时，SDK 在内部按完整响应
/// 接住并合成事件流，调用方无需感知差异。ResponseCompleted 为终止事件，
/// 流在其后自然结束。
fn complete_response_events(response: ResponseObject) -> Vec<ResponsesStreamEvent> {
    vec![
        ResponsesStreamEvent::ResponseCreated {
            sequence_number: 0,
            response: response.clone(),
        },
        ResponsesStreamEvent::ResponseCompleted {
            sequence_number: 1,
            response,
        },
    ]
}

/// 解析单条 SSE data 为流事件。
///
/// 未知事件类型（服务端新增而 SDK 尚未支持）降级为
/// `ResponsesStreamEvent::Unknown` 透传，不中断流。
pub(crate) fn parse_stream_event(data: &str) -> Result<ResponsesStreamEvent, DeepSeekError> {
    let value: serde_json::Value = serde_json::from_str(data)
        .map_err(|err| DeepSeekError::Serialization(format!("{err}: {data}")))?;
    let event_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    match serde_json::from_value::<ResponsesStreamEvent>(value) {
        Ok(event) => Ok(event),
        Err(err) => {
            tracing::warn!(
                event_type,
                error = %err,
                "无法解析的 Responses 流事件，按未知事件透传"
            );
            Ok(ResponsesStreamEvent::Unknown { event_type })
        }
    }
}
