use futures_util::StreamExt;
use futures_util::stream;
use serde_json::Value;

use crate::config::AnthropicConfig;
use crate::error::AnthropicError;
use crate::types::{
    ContentBlock, ContentBlockDeltaData, ContentBlockStartData, EventStream, MessageDeltaData,
    MessageStartData, MessagesCreateRequest, MessagesCreateResponse, ModelsListResponse,
    StreamEvent, ThinkingConfig,
};

/// 把一次性完整响应合成为等价的流事件序列。
///
/// 网关忽略 stream 参数返回 application/json 时，SDK 在内部按完整响应
/// 接住并合成事件流，调用方无需感知差异。
fn complete_response_events(response: MessagesCreateResponse) -> Vec<StreamEvent> {
    let mut events = vec![StreamEvent::MessageStart {
        message: MessageStartData {
            id: response.id,
            model: response.model,
            role: response.role,
            content: Vec::new(),
            stop_reason: None,
            stop_sequence: None,
            usage: None,
        },
    }];
    for (index, block) in response.content.into_iter().enumerate() {
        let content_block = match block {
            ContentBlock::Text { text } => ContentBlockStartData::Text { text },
            ContentBlock::ToolUse { id, name, input } => {
                ContentBlockStartData::ToolUse { id, name, input }
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } => ContentBlockStartData::Thinking {
                thinking,
                signature: signature.unwrap_or_default(),
            },
            ContentBlock::RedactedThinking { data } => {
                ContentBlockStartData::RedactedThinking { data }
            }
            ContentBlock::ToolResult { .. } | ContentBlock::Unknown => continue,
        };
        events.push(StreamEvent::ContentBlockStart {
            index,
            content_block,
        });
        events.push(StreamEvent::ContentBlockStop { index });
    }
    events.push(StreamEvent::MessageDelta {
        delta: MessageDeltaData {
            stop_reason: response.stop_reason,
            stop_sequence: response.stop_sequence,
        },
        usage: response.usage,
    });
    events.push(StreamEvent::MessageStop);
    events
}

/// SSE 字节流转 Anthropic 事件流。
fn anthropic_sse_stream<S>(byte_stream: S) -> EventStream
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    use eventsource_stream::Eventsource;
    use futures_util::StreamExt;
    let sse_stream = Box::pin(byte_stream.eventsource());
    Box::pin(stream::unfold(sse_stream, |mut sse_stream| async move {
        match sse_stream.next().await {
            Some(Ok(message)) => {
                let item = parse_sse_event(&message.event, &message.data);
                Some((item, sse_stream))
            }
            Some(Err(err)) => Some((Err(AnthropicError::Stream(err.to_string())), sse_stream)),
            None => None,
        }
    }))
}

/// 判断响应首块是否是 JSON（跳过空白后以 `{` 或 `[` 开头）。
fn looks_like_json(first: &[u8]) -> bool {
    first
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'{' || *byte == b'[')
}

/// 聚合首块与剩余响应体并解析为完整响应，整体受 timeout 约束。
async fn read_complete_response(
    mut response: reqwest::Response,
    first: bytes::Bytes,
    timeout: std::time::Duration,
) -> Result<MessagesCreateResponse, AnthropicError> {
    let read = async {
        let mut buf = first.to_vec();
        while let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? {
            buf.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<MessagesCreateResponse>(&buf).map_err(|err| {
            AnthropicError::Serialization(format!(
                "{err}: {}",
                String::from_utf8_lossy(&buf[..buf.len().min(256)])
            ))
        })
    };
    tokio::time::timeout(timeout, read)
        .await
        .map_err(|_| AnthropicError::Timeout(format!("{} ms", timeout.as_millis())))?
}

#[derive(Clone)]
pub struct AnthropicClient {
    http_client: reqwest::Client,
    stream_http_client: reqwest::Client,
    config: AnthropicConfig,
}

impl AnthropicClient {
    pub fn from_config(config: AnthropicConfig) -> Result<Self, AnthropicError> {
        let http_client = reqwest::Client::builder()
            .default_headers(config.headers.clone())
            .timeout(config.timeout)
            .build()
            .map_err(|err| AnthropicError::Transport(err.to_string()))?;
        let stream_http_client = reqwest::Client::builder()
            .default_headers(config.headers.clone())
            .build()
            .map_err(|err| AnthropicError::Transport(err.to_string()))?;
        Ok(Self {
            http_client,
            stream_http_client,
            config,
        })
    }

    pub async fn create(
        &self,
        mut request: MessagesCreateRequest,
    ) -> Result<MessagesCreateResponse, AnthropicError> {
        Self::normalize_thinking_for_model(&mut request);
        let response = self
            .request_builder("/v1/messages")
            .json(&request)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        parse_json_response(response).await
    }

    pub async fn create_stream(
        &self,
        mut request: MessagesCreateRequest,
    ) -> Result<EventStream, AnthropicError> {
        Self::normalize_thinking_for_model(&mut request);
        request.stream = Some(true);
        // 建连与等待响应头受用户配置的请求超时约束，避免网关建连后
        // 不返回响应头导致永久等待；SSE 建流成功后不受总时限限制。
        let response = tokio::time::timeout(
            self.config.timeout,
            self.stream_request_builder("/v1/messages")
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .json(&request)
                .send(),
        )
        .await
        .map_err(|_| AnthropicError::Timeout(format!("{} ms", self.config.timeout.as_millis())))?
        .map_err(map_reqwest_error)?;

        if !response.status().is_success() {
            return Err(parse_error_response(response).await);
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if content_type.contains("text/event-stream") {
            return Ok(anthropic_sse_stream(response.bytes_stream()));
        }

        // 类型不明确（部分网关漏标或错标）时按首块内容探测：JSON 以 `{`/`[`
        // 开头，SSE 事件以 `data:` 等开头。
        let mut response = response;
        let first = response
            .chunk()
            .await
            .map_err(map_reqwest_error)?
            .unwrap_or_default();
        if content_type.contains("json") || looks_like_json(&first) {
            // 网关忽略 stream 参数返回一次性 JSON：SSE 解析器会把这些行
            // 全部当未知字段丢弃且不报错，在 SDK 内按完整响应接住并合成
            // 事件流。完整读取受超时约束，避免迟迟不结束的响应永久等待。
            let complete = read_complete_response(response, first, self.config.timeout).await?;
            return Ok(Box::pin(stream::iter(
                complete_response_events(complete).into_iter().map(Ok),
            )));
        }
        // 首块不是 JSON：按 SSE 流解析，把首块拼回流头。
        let byte_stream = stream::once(async move { Ok::<_, reqwest::Error>(first) })
            .chain(response.bytes_stream());
        Ok(anthropic_sse_stream(byte_stream))
    }

    pub async fn list_models(&self) -> Result<ModelsListResponse, AnthropicError> {
        let response = self
            .request_builder("/v1/models")
            .send()
            .await
            .map_err(map_reqwest_error)?;
        parse_json_response(response).await
    }

    fn request_builder(&self, path: &str) -> reqwest::RequestBuilder {
        self.request_builder_with_client(&self.http_client, path)
    }

    /// thinking 形态统一规范化：
    ///
    /// 实测智谱 GLM 的 Anthropic 兼容端点已完整支持 adaptive +
    /// output_config.effort（low 档不思考、high 档思考），Claude 官方
    /// 与中转端点新模型仅接受该形态，因此开启思考时对所有模型统一下发：
    ///
    /// - `Adaptive` / `None`：保持不变；
    /// - `Disabled` + Claude 系模型：置为不发 thinking。自适应思考无法关闭，
    ///   显式 disabled 会被新模型 400 拒收，省略字段即回落到模型默认行为；
    /// - `Disabled` + 其他模型（智谱 GLM 等兼容端点）：保留显式 disabled，
    ///   这些端点省略 thinking 字段时默认开启思考，必须显式关闭。
    fn normalize_thinking_for_model(request: &mut MessagesCreateRequest) {
        match request.thinking.take() {
            Some(ThinkingConfig::Disabled) if is_claude_model(&request.model) => {
                tracing::debug!(
                    model = %request.model,
                    "thinking disabled dropped: adaptive thinking cannot be turned off"
                );
                request.thinking = None;
            }
            other => request.thinking = other,
        }
    }

    fn stream_request_builder(&self, path: &str) -> reqwest::RequestBuilder {
        self.request_builder_with_client(&self.stream_http_client, path)
    }

    fn request_builder_with_client(
        &self,
        client: &reqwest::Client,
        path: &str,
    ) -> reqwest::RequestBuilder {
        let url = format!(
            "{}/{}",
            self.config.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        let mut builder = client
            .post(url.clone())
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", &self.config.api_version)
            .header(reqwest::header::ACCEPT, "application/json");
        if path == "/v1/models" {
            builder = client
                .get(url)
                .header("x-api-key", &self.config.api_key)
                .header("anthropic-version", &self.config.api_version)
                .header(reqwest::header::ACCEPT, "application/json");
        }
        if let Some(beta) = &self.config.beta {
            builder = builder.header("anthropic-beta", beta);
        }
        builder
    }
}

fn map_reqwest_error(err: reqwest::Error) -> AnthropicError {
    if err.is_timeout() {
        AnthropicError::Transport(format!("timeout: {err}"))
    } else {
        AnthropicError::Transport(err.to_string())
    }
}

async fn parse_error_response(response: reqwest::Response) -> AnthropicError {
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    let body_text = String::from_utf8_lossy(&body).to_string();
    classify_http_error(status, body_text)
}

async fn parse_json_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, AnthropicError> {
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|err| AnthropicError::Transport(err.to_string()))?;

    if !status.is_success() {
        let body_text = String::from_utf8_lossy(&body).to_string();
        return Err(classify_http_error(status, body_text));
    }

    serde_json::from_slice(&body).map_err(|err| {
        AnthropicError::Serialization(format!(
            "{err}: {}",
            String::from_utf8_lossy(&body[..body.len().min(512)])
        ))
    })
}

fn classify_http_error(status: reqwest::StatusCode, body_text: String) -> AnthropicError {
    if is_rate_limited_status_or_body(status, &body_text) {
        return AnthropicError::RateLimited(body_text);
    }

    match status.as_u16() {
        400 => AnthropicError::InvalidRequest(body_text),
        401 | 403 => AnthropicError::Authentication(body_text),
        _ if status.is_server_error() => AnthropicError::Transport(body_text),
        _ => AnthropicError::Api(body_text),
    }
}

fn is_rate_limited_status_or_body(status: reqwest::StatusCode, body_text: &str) -> bool {
    let lower = body_text.to_ascii_lowercase();
    matches!(status.as_u16(), 429 | 529)
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("overloaded_error")
        || lower.contains("\"type\":\"overloaded_error\"")
}

fn parse_sse_event(event_type: &str, data: &str) -> Result<StreamEvent, AnthropicError> {
    if event_type == "ping" {
        return Ok(StreamEvent::Ping);
    }
    if event_type == "error" {
        let payload: Value = serde_json::from_str(data)
            .map_err(|err| AnthropicError::Serialization(err.to_string()))?;
        return Ok(StreamEvent::Error {
            message: payload.to_string(),
        });
    }

    let value: Value =
        serde_json::from_str(data).map_err(|err| AnthropicError::Serialization(err.to_string()))?;
    match event_type {
        "message_start" => Ok(StreamEvent::MessageStart {
            message: serde_json::from_value::<MessageStartData>(
                value
                    .get("message")
                    .cloned()
                    .ok_or_else(|| AnthropicError::Serialization(data.to_string()))?,
            )
            .map_err(|err| AnthropicError::Serialization(err.to_string()))?,
        }),
        "content_block_start" => Ok(StreamEvent::ContentBlockStart {
            index: value
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| AnthropicError::Serialization(data.to_string()))?
                as usize,
            content_block: serde_json::from_value::<ContentBlockStartData>(
                value
                    .get("content_block")
                    .cloned()
                    .ok_or_else(|| AnthropicError::Serialization(data.to_string()))?,
            )
            .map_err(|err| AnthropicError::Serialization(err.to_string()))?,
        }),
        "content_block_delta" => Ok(StreamEvent::ContentBlockDelta {
            index: value
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| AnthropicError::Serialization(data.to_string()))?
                as usize,
            delta: serde_json::from_value::<ContentBlockDeltaData>(
                value
                    .get("delta")
                    .cloned()
                    .ok_or_else(|| AnthropicError::Serialization(data.to_string()))?,
            )
            .map_err(|err| AnthropicError::Serialization(err.to_string()))?,
        }),
        "content_block_stop" => Ok(StreamEvent::ContentBlockStop {
            index: value
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| AnthropicError::Serialization(data.to_string()))?
                as usize,
        }),
        "message_delta" => Ok(StreamEvent::MessageDelta {
            delta: serde_json::from_value::<MessageDeltaData>(
                value
                    .get("delta")
                    .cloned()
                    .ok_or_else(|| AnthropicError::Serialization(data.to_string()))?,
            )
            .map_err(|err| AnthropicError::Serialization(err.to_string()))?,
            usage: value
                .get("usage")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|err| AnthropicError::Serialization(err.to_string()))?,
        }),
        "message_stop" => Ok(StreamEvent::MessageStop),
        _other => Ok(StreamEvent::Unknown),
    }
}

/// 是否 Anthropic 官方 Claude 系模型（含中转端点上带前缀的命名，如
/// `anthropic/claude-opus-5-5`）。
fn is_claude_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("claude")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with(max_tokens: u32, thinking: Option<ThinkingConfig>) -> MessagesCreateRequest {
        MessagesCreateRequest {
            model: "claude-sonnet-4-5".to_string(),
            max_tokens,
            system: None,
            messages: Vec::new(),
            temperature: None,
            stop_sequences: None,
            top_p: None,
            metadata: None,
            tools: None,
            tool_choice: None,
            stream: None,
            thinking,
            output_config: None,
        }
    }

    #[test]
    fn keeps_adaptive_thinking_across_models() {
        // 全模型统一 adaptive：官方新旧家族与第三方兼容端点（智谱 GLM
        // 实测已支持 adaptive + effort）形态保持不变，序列化无预算字段。
        for model in [
            "claude-opus-5-5",
            "claude-sonnet-4-5",
            "claude-3-7-sonnet",
            "glm-5.3",
            "glm-5.3-flash",
            "step-5-preview",
        ] {
            let mut request = request_with(32_768, Some(ThinkingConfig::Adaptive));
            request.model = model.to_string();
            AnthropicClient::normalize_thinking_for_model(&mut request);
            assert_eq!(
                request.thinking,
                Some(ThinkingConfig::Adaptive),
                "{model} should stay adaptive"
            );
            let payload = serde_json::to_value(&request).expect("serialize");
            assert_eq!(
                payload["thinking"],
                serde_json::json!({"type": "adaptive"}),
                "{model} payload should carry adaptive shape"
            );
        }
    }

    #[test]
    fn disabled_thinking_is_dropped_for_claude() {
        // 自适应思考无法关闭：disabled 会被 Claude 新模型 400 拒收，省略字段走默认。
        for model in ["claude-opus-5-5", "anthropic/Claude-Sonnet-4-5"] {
            let mut request = request_with(32_768, Some(ThinkingConfig::Disabled));
            request.model = model.to_string();
            AnthropicClient::normalize_thinking_for_model(&mut request);
            assert_eq!(request.thinking, None, "{model}");
        }
    }

    #[test]
    fn disabled_thinking_is_kept_for_compatible_endpoints() {
        // 智谱 GLM 等兼容端点省略 thinking 即默认思考，关闭时必须显式下发 disabled。
        for model in ["glm-5.3", "glm-5.3-flash", "step-5-preview"] {
            let mut request = request_with(32_768, Some(ThinkingConfig::Disabled));
            request.model = model.to_string();
            AnthropicClient::normalize_thinking_for_model(&mut request);
            assert_eq!(request.thinking, Some(ThinkingConfig::Disabled), "{model}");
            let payload = serde_json::to_value(&request).expect("serialize");
            assert_eq!(payload["thinking"], serde_json::json!({"type": "disabled"}));
        }
    }

    #[test]
    fn effort_serializes_into_output_config() {
        // adaptive 深度档位序列化为 output_config.effort，档位值 snake_case。
        let mut request = request_with(32_768, Some(ThinkingConfig::Adaptive));
        request.model = "claude-opus-5-5".to_string();
        request.output_config = Some(crate::types::OutputConfig {
            effort: Some(crate::types::EffortLevel::Xhigh),
        });
        let payload = serde_json::to_value(&request).expect("serialize");
        assert_eq!(
            payload["output_config"],
            serde_json::json!({"effort": "xhigh"})
        );
        // 无 output_config 时字段整体省略。
        request.output_config = None;
        let payload = serde_json::to_value(&request).expect("serialize");
        assert!(payload.get("output_config").is_none());
    }

    #[test]
    fn complete_response_events_synthesizes_stream_sequence() {
        // 网关忽略 stream 参数返回一次性 JSON 时，完整响应应合成为等价事件流：
        // MessageStart → 每个内容块 Start/Stop（完整内容在 Start）→ MessageDelta → MessageStop。
        let response: MessagesCreateResponse = serde_json::from_value(serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [
                { "type": "text", "text": "一次性完整回复" },
                { "type": "tool_use", "id": "call_tool", "name": "read_file", "input": {"path": "TODO.md"} }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 3, "output_tokens": 5 }
        }))
        .unwrap();

        let events = complete_response_events(response);

        assert!(
            matches!(&events[0], StreamEvent::MessageStart { message } if message.id == "msg_1")
        );
        assert!(matches!(&events[1],
            StreamEvent::ContentBlockStart { index: 0, content_block: ContentBlockStartData::Text { text } }
            if text == "一次性完整回复"));
        assert!(matches!(
            &events[2],
            StreamEvent::ContentBlockStop { index: 0 }
        ));
        assert!(matches!(&events[3],
            StreamEvent::ContentBlockStart { index: 1, content_block: ContentBlockStartData::ToolUse { id, name, input } }
            if id == "call_tool" && name == "read_file" && input["path"] == "TODO.md"));
        assert!(matches!(
            &events[4],
            StreamEvent::ContentBlockStop { index: 1 }
        ));
        assert!(matches!(&events[5],
            StreamEvent::MessageDelta { delta, usage: Some(_) } if delta.stop_reason.as_deref() == Some("tool_use")));
        assert!(matches!(&events[6], StreamEvent::MessageStop));
        assert_eq!(events.len(), 7);
    }
}
