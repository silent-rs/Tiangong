use futures_util::Stream;
use serde_json::Value;

use crate::error::LlmError;
use crate::model::ProviderModelInfo;

use super::config::OpenAiResponsesConfig;
use super::error::map_responses_error;
use super::mapping::normalize_api_base;

pub(crate) type ResponsesByotStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<Value, async_openai::error::OpenAIError>> + Send>>;

/// 流式请求的响应形态：正常 SSE 流，或服务端忽略 stream 参数返回的一次性 JSON。
pub enum ResponsesStreamResponse {
    Sse(ResponsesByotStream),
    Complete(Value),
}

#[derive(Clone)]
pub struct ResponsesClient {
    http: Result<reqwest::Client, String>,
    config: OpenAiResponsesConfig,
}

impl ResponsesClient {
    pub fn new(config: OpenAiResponsesConfig) -> Self {
        Self {
            http: crate::providers::openai_chatcompletions::client::build_http_client(
                &config.headers,
            )
            .map_err(|error| error.to_string()),
            config,
        }
    }

    pub async fn stream(
        &self,
        model: &str,
        payload: Value,
    ) -> Result<ResponsesStreamResponse, LlmError> {
        let base = normalize_api_base(&self.config.base_url)
            .map_err(|err| LlmError::Configuration(err.to_string()))?;
        let url = format!("{base}/responses");
        let api_key = self.config.api_key.clone();
        let request_timeout = self.config.timeout;
        let headers = self.config.headers.clone();
        let http = self
            .http
            .as_ref()
            .map_err(|error| LlmError::Configuration(error.clone()))?
            .clone();
        self.with_retry("openai_stream", model, true, move || {
            let url = url.clone();
            let api_key = api_key.clone();
            let payload = payload.clone();
            let request_timeout = request_timeout;
            let headers = headers.clone();
            let http = http.clone();
            async move {
                use crate::providers::openai_chatcompletions::client::{
                    StreamBody, resolve_stream_body,
                };
                let response = crate::providers::openai_chatcompletions::client::send_request(
                    &http,
                    &url,
                    &api_key,
                    &headers,
                    &payload,
                    request_timeout,
                    true,
                )
                .await?;
                match resolve_stream_body(response, request_timeout).await? {
                    StreamBody::Sse(stream) => Ok(ResponsesStreamResponse::Sse(stream)),
                    StreamBody::Complete(value) => {
                        // 服务端忽略 stream 参数返回一次性 JSON：SSE 解析器会把这些
                        // 行全部当未知字段丢弃且不报错，必须在 llm 层按完整响应接住。
                        tracing::info!(
                            operation = "openai_stream",
                            provider = "openai",
                            model,
                            "服务端未按 SSE 流式返回，转按一次性完整响应处理"
                        );
                        Ok(ResponsesStreamResponse::Complete(value))
                    }
                }
            }
        })
        .await
    }

    pub async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        // Responses 与 Chat 共用 /models 端点，复用 Chat Completions 的实现。
        crate::providers::openai_chatcompletions::client::list_models_via_config(
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
        let _ = stream;
        crate::retry::run(
            operation,
            model,
            self.config.max_retries,
            self.config.retry_notifier.as_ref(),
            || {
                let future = f();
                async move { future.await.map_err(|error| map_responses_error(&error)) }
            },
        )
        .await
    }
}
