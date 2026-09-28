use std::time::Duration;

use anyhow::Context;
use futures_util::Stream;
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::error::LlmError;
use crate::model::ProviderModelInfo;
use crate::rerank::RerankRequest;

use super::config::OpenAiChatConfig;
use super::error::{is_retryable_openai_error, map_openai_error};
use super::mapping::normalize_api_base;

type OpenAiByotStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<Value, async_openai::error::OpenAIError>> + Send>>;

/// 流式请求的响应形态：正常 SSE 流，或服务端忽略 stream 参数返回的一次性 JSON。
pub enum OpenAiStreamResponse {
    Sse(OpenAiByotStream),
    Complete(Value),
}

/// 把 SSE 响应体解析为 JSON 值流，跳过 [DONE] 与 keepalive。
/// Responses 协议与 Chat Completions 的 SSE 帧格式一致，共用此实现。
pub(crate) fn sse_value_stream(
    response: reqwest::Response,
) -> impl Stream<Item = Result<Value, async_openai::error::OpenAIError>> {
    sse_value_stream_from_bytes(response.bytes_stream())
}

/// 同 [sse_value_stream]，但接受任意字节流（用于内容探测后把首块拼回流）。
fn sse_value_stream_from_bytes<S>(
    byte_stream: S,
) -> impl Stream<Item = Result<Value, async_openai::error::OpenAIError>>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    use eventsource_stream::Eventsource;
    use futures_util::StreamExt;
    byte_stream.eventsource().filter_map(|event| async move {
        match event {
            Ok(message) => {
                if message.data == "[DONE]" || message.event == "keepalive" {
                    return None;
                }
                Some(serde_json::from_str::<Value>(&message.data).map_err(|err| {
                    async_openai::error::OpenAIError::JSONDeserialize(err, message.data)
                }))
            }
            Err(err) => Some(Err(async_openai::error::OpenAIError::StreamError(
                Box::new(async_openai::error::StreamError::EventStream(
                    err.to_string(),
                )),
            ))),
        }
    })
}

/// 流式响应体的处置结果：SSE 事件流，或一次性完整 JSON。
pub(crate) enum StreamBody {
    Sse(OpenAiByotStream),
    Complete(Value),
}

/// 等待响应头/读取完整响应体的超时错误。
pub(crate) fn stream_timeout_error(timeout: Duration) -> async_openai::error::OpenAIError {
    async_openai::error::OpenAIError::ApiError(async_openai::error::ApiErrorResponse {
        status_code: reqwest::StatusCode::REQUEST_TIMEOUT,
        api_error: async_openai::error::ApiError {
            message: format!(
                "timeout after {}ms waiting for stream response body",
                timeout.as_millis()
            ),
            r#type: None,
            param: None,
            code: None,
        },
    })
}

/// 判断响应首块是否是 JSON（跳过空白后以 `{` 或 `[` 开头）。
fn looks_like_json(first: &[u8]) -> bool {
    first
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'{' || *byte == b'[')
}

/// 按响应类型与实际内容把流式请求的响应分流为 SSE 或一次性 JSON。
///
/// - 响应类型声明 `text/event-stream` → 按流式解析；
/// - 响应类型声明 JSON → 按一次性完整响应读取；
/// - 类型缺失或不明确（部分网关漏标或错标 `application/octet-stream`）→
///   探测首块内容：`{`/`[` 开头按 JSON，否则仍按流式解析，避免误判真实 SSE 流。
///
/// 一次性响应体的完整读取受 `timeout` 约束；SSE 流本身允许长时间增量生成，
/// 不设总时限。
pub(crate) async fn resolve_stream_body(
    mut response: reqwest::Response,
    timeout: Duration,
) -> Result<StreamBody, async_openai::error::OpenAIError> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.contains("text/event-stream") {
        return Ok(StreamBody::Sse(Box::pin(sse_value_stream(response))));
    }
    if content_type.contains("json") {
        return read_complete_body(response, timeout)
            .await
            .map(StreamBody::Complete);
    }
    // 类型不明确：按首块内容探测真实形态。
    let first = response.chunk().await?.unwrap_or_default();
    if looks_like_json(&first) {
        return read_complete_body_with_prefix(response, first, timeout)
            .await
            .map(StreamBody::Complete);
    }
    // 首块不是 JSON：按 SSE 流解析，把首块拼回流头。
    use futures_util::StreamExt;
    let byte_stream = futures_util::stream::once(async move { Ok::<_, reqwest::Error>(first) })
        .chain(response.bytes_stream());
    Ok(StreamBody::Sse(Box::pin(sse_value_stream_from_bytes(
        byte_stream,
    ))))
}

async fn read_complete_body(
    response: reqwest::Response,
    timeout: Duration,
) -> Result<Value, async_openai::error::OpenAIError> {
    let bytes = tokio::time::timeout(timeout, response.bytes())
        .await
        .map_err(|_| stream_timeout_error(timeout))??;
    parse_complete_body(&bytes)
}

async fn read_complete_body_with_prefix(
    mut response: reqwest::Response,
    first: bytes::Bytes,
    timeout: Duration,
) -> Result<Value, async_openai::error::OpenAIError> {
    let bytes = tokio::time::timeout(timeout, async {
        let mut buf = first.to_vec();
        while let Some(chunk) = response.chunk().await? {
            buf.extend_from_slice(&chunk);
        }
        Ok::<_, reqwest::Error>(buf)
    })
    .await
    .map_err(|_| stream_timeout_error(timeout))??;
    parse_complete_body(&bytes)
}

pub(crate) fn parse_complete_body(bytes: &[u8]) -> Result<Value, async_openai::error::OpenAIError> {
    serde_json::from_slice(bytes).map_err(|err| {
        async_openai::error::OpenAIError::JSONDeserialize(
            err,
            String::from_utf8_lossy(&bytes[..bytes.len().min(256)]).into_owned(),
        )
    })
}

/// 流式和非流式共用单次发送；重试由调用方统一控制，避免 SDK 再次重试。
pub(crate) async fn send_request(
    url: &str,
    api_key: &str,
    headers: &reqwest::header::HeaderMap,
    payload: &Value,
    request_timeout: Duration,
    stream: bool,
) -> Result<reqwest::Response, async_openai::error::OpenAIError> {
    let client = reqwest::Client::builder()
        .default_headers(headers.clone())
        .retry(reqwest::retry::never())
        .build()?;
    let mut request = client
        .post(url)
        .header(
            reqwest::header::ACCEPT,
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .json(payload);
    if !api_key.trim().is_empty() {
        request = request.bearer_auth(api_key);
    }
    let response = timeout(request_timeout, request.send())
        .await
        .map_err(|_| stream_timeout_error(request_timeout))??;
    let status = response.status();
    if !status.is_success() {
        let body = timeout(request_timeout, response.text())
            .await
            .map_err(|_| stream_timeout_error(request_timeout))?
            .unwrap_or_default();
        return Err(async_openai::error::OpenAIError::ApiError(
            async_openai::error::ApiErrorResponse {
                status_code: status,
                api_error: async_openai::error::ApiError {
                    message: format!("{status}: {body}"),
                    r#type: None,
                    param: None,
                    code: None,
                },
            },
        ));
    }
    Ok(response)
}

const INITIAL_RETRY_DELAY_MS: u64 = 1000;

#[derive(Clone)]
pub struct OpenAiClient {
    config: OpenAiChatConfig,
}

impl OpenAiClient {
    pub fn new(config: OpenAiChatConfig) -> Self {
        Self { config }
    }

    pub async fn complete(&self, model: &str, payload: Value) -> Result<Value, LlmError> {
        let base = normalize_api_base(&self.config.base_url)
            .map_err(|error| LlmError::Configuration(error.to_string()))?;
        let url = format!("{base}/chat/completions");
        timeout(
            self.config.timeout,
            self.with_retry("openai_complete", model, false, || async {
                let response = send_request(
                    &url,
                    &self.config.api_key,
                    &self.config.headers,
                    &payload,
                    self.config.timeout,
                    false,
                )
                .await?;
                parse_complete_body(&response.bytes().await?)
            }),
        )
        .await
        .map_err(|_| LlmError::Timeout(self.config.timeout.as_millis() as u64))?
    }

    pub async fn stream(
        &self,
        model: &str,
        payload: Value,
    ) -> Result<OpenAiStreamResponse, LlmError> {
        let base = normalize_api_base(&self.config.base_url)
            .map_err(|err| LlmError::Configuration(err.to_string()))?;
        let url = format!("{base}/chat/completions");
        let api_key = self.config.api_key.clone();
        let request_timeout = self.config.timeout;
        let headers = self.config.headers.clone();
        self.with_retry("openai_stream", model, true, move || {
            let url = url.clone();
            let api_key = api_key.clone();
            let payload = payload.clone();
            let request_timeout = request_timeout;
            let headers = headers.clone();
            async move {
                let response =
                    send_request(&url, &api_key, &headers, &payload, request_timeout, true).await?;
                match resolve_stream_body(response, request_timeout).await? {
                    StreamBody::Sse(stream) => Ok(OpenAiStreamResponse::Sse(stream)),
                    StreamBody::Complete(value) => {
                        // 服务端忽略 stream 参数返回一次性 JSON：SSE 解析器会把这些
                        // 行全部当未知字段丢弃且不报错，必须在 llm 层按完整响应接住。
                        tracing::info!(
                            operation = "openai_stream",
                            provider = "openai",
                            model,
                            "服务端未按 SSE 流式返回，转按一次性完整响应处理"
                        );
                        Ok(OpenAiStreamResponse::Complete(value))
                    }
                }
            }
        })
        .await
    }

    pub async fn rerank(&self, model: &str, request: &RerankRequest) -> Result<Value, LlmError> {
        if request.documents.is_empty() {
            return Ok(json!({ "results": [] }));
        }

        let base = normalize_rerank_api_base(&self.config.base_url)
            .map_err(|err| LlmError::Configuration(err.to_string()))?;
        let url = format!("{base}/rerank");
        let payload = json!({
            "model": model,
            "query": &request.query,
            "documents": &request.documents,
            "top_n": request.top_n.max(1).min(request.documents.len()),
        });
        let client = reqwest::Client::builder()
            .timeout(self.config.timeout)
            .default_headers(self.config.headers.clone())
            .build()
            .map_err(|err| LlmError::Transport(err.to_string()))?;

        let mut req = client.post(&url).json(&payload);
        if !self.config.api_key.trim().is_empty() {
            req = req.bearer_auth(&self.config.api_key);
        }

        let start = std::time::Instant::now();
        tracing::info!(
            operation = "openai_rerank",
            provider = "openai",
            model,
            stream = false,
            attempt = 0,
            "开始 OpenAI 兼容请求"
        );
        let response = req
            .send()
            .await
            .with_context(|| format!("Rerank 请求失败：{url}"))
            .map_err(|err| LlmError::Transport(err.to_string()))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(
                operation = "openai_rerank",
                provider = "openai",
                model,
                stream = false,
                attempt = 0,
                latency_ms = start.elapsed().as_millis() as u64,
                status = status.as_u16(),
                "OpenAI 兼容请求失败"
            );
            return Err(LlmError::Provider {
                provider: "openai",
                message: format!("Rerank API 返回错误 {status}: {body}"),
            });
        }

        let body = response
            .json::<Value>()
            .await
            .map_err(|err| LlmError::Serialization(err.to_string()))?;
        tracing::info!(
            operation = "openai_rerank",
            provider = "openai",
            model,
            stream = false,
            attempt = 0,
            latency_ms = start.elapsed().as_millis() as u64,
            "OpenAI 兼容请求完成"
        );
        Ok(body)
    }

    pub async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        list_models_via_config(
            &self.config.api_key,
            &self.config.base_url,
            self.config.timeout,
            "openai",
            &self.config.headers,
        )
        .await
    }

    async fn with_retry<F, Fut, T>(
        &self,
        operation: &'static str,
        model: &str,
        stream: bool,
        mut f: F,
    ) -> Result<T, LlmError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, async_openai::error::OpenAIError>>,
    {
        let mut attempt = 0u32;
        let mut delay_ms = INITIAL_RETRY_DELAY_MS;
        let max_retries = self.config.max_retries;
        loop {
            let start = std::time::Instant::now();
            tracing::info!(
                operation,
                provider = "openai",
                model,
                stream,
                attempt,
                "开始 OpenAI 兼容请求"
            );
            match f().await {
                Ok(value) => {
                    tracing::info!(
                        operation,
                        provider = "openai",
                        model,
                        stream,
                        attempt,
                        latency_ms = start.elapsed().as_millis() as u64,
                        "OpenAI 兼容请求完成"
                    );
                    return Ok(value);
                }
                Err(err) if attempt < max_retries && is_retryable_openai_error(&err) => {
                    attempt += 1;
                    if let Some(notifier) = &self.config.retry_notifier {
                        notifier(attempt, max_retries, delay_ms, &err.to_string());
                    }
                    tracing::warn!(
                        operation,
                        provider = "openai",
                        model,
                        stream,
                        attempt,
                        delay_ms,
                        error = %err,
                        "OpenAI 兼容请求失败，准备重试"
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms *= 2;
                }
                Err(err) => {
                    tracing::warn!(
                        operation,
                        provider = "openai",
                        model,
                        stream,
                        attempt,
                        latency_ms = start.elapsed().as_millis() as u64,
                        error = %err,
                        "OpenAI 兼容请求失败"
                    );
                    return Err(map_openai_error(&err));
                }
            }
        }
    }
}

/// 通过 `/models` 端点拉取模型列表。
///
/// OpenAI Chat Completions 与 Responses 共用此端点，故抽为 crate 内公共函数。
pub(crate) async fn list_models_via_config(
    api_key: &str,
    base_url: &str,
    timeout: Duration,
    provider_label: &'static str,
    headers: &reqwest::header::HeaderMap,
) -> Result<Vec<ProviderModelInfo>, LlmError> {
    let operation = "openai_list_models";
    let start = std::time::Instant::now();
    tracing::info!(
        operation,
        provider = provider_label,
        model = "<list_models>",
        stream = false,
        attempt = 0,
        "开始 OpenAI 兼容请求"
    );
    let base =
        normalize_api_base(base_url).map_err(|err| LlmError::Configuration(err.to_string()))?;
    let url = format!("{base}/models");
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|err| LlmError::Transport(err.to_string()))?;
    let response = client
        .get(&url)
        .headers(headers.clone())
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await
        .with_context(|| format!("请求模型列表失败：{url}"))
        .map_err(|err| LlmError::Transport(err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        tracing::warn!(
            operation,
            provider = provider_label,
            model = "<list_models>",
            stream = false,
            attempt = 0,
            latency_ms = start.elapsed().as_millis() as u64,
            status = status.as_u16(),
            "OpenAI 兼容请求失败"
        );
        let message = format!("获取模型列表失败：HTTP {status}，响应：{body}");
        if status.as_u16() == 429
            || status.as_u16() == 529
            || body.to_ascii_lowercase().contains("rate limit")
            || body.to_ascii_lowercase().contains("too many requests")
            || body.to_ascii_lowercase().contains("overloaded_error")
        {
            return Err(LlmError::RateLimited(message));
        }
        return Err(LlmError::Provider {
            provider: provider_label,
            message,
        });
    }
    #[derive(serde::Deserialize)]
    struct ModelEntry {
        id: String,
    }
    #[derive(serde::Deserialize)]
    struct ModelsResponse {
        data: Vec<ModelEntry>,
    }
    let body = response
        .text()
        .await
        .map_err(|err| LlmError::Transport(err.to_string()))?;
    let parsed: ModelsResponse = serde_json::from_str(&body).map_err(|err| LlmError::Provider {
        provider: provider_label,
        message: format!("failed to deserialize api response: {err}: {body}"),
    })?;
    tracing::info!(
        operation,
        provider = provider_label,
        model = "<list_models>",
        stream = false,
        attempt = 0,
        latency_ms = start.elapsed().as_millis() as u64,
        "OpenAI 兼容请求完成"
    );
    Ok(parsed
        .data
        .into_iter()
        .map(|entry| ProviderModelInfo {
            id: entry.id,
            display_name: None,
            ..Default::default()
        })
        .collect())
}

fn normalize_rerank_api_base(base_url: &str) -> anyhow::Result<String> {
    let base = normalize_api_base(base_url)?;
    let cleaned = base.trim_end_matches('/');
    let cleaned = cleaned.strip_suffix("/rerank").unwrap_or(cleaned);
    Ok(cleaned.to_string())
}

#[cfg(test)]
mod tests {
    use super::normalize_rerank_api_base;

    #[test]
    fn normalizes_openai_compatible_rerank_api_base() {
        assert_eq!(
            normalize_rerank_api_base("http://127.0.0.1:8000/v1").unwrap(),
            "http://127.0.0.1:8000/v1"
        );
        assert_eq!(
            normalize_rerank_api_base("http://127.0.0.1:8000/v1/rerank").unwrap(),
            "http://127.0.0.1:8000/v1"
        );
        assert_eq!(
            normalize_rerank_api_base("http://127.0.0.1:8000/rerank").unwrap(),
            "http://127.0.0.1:8000"
        );
    }
}
