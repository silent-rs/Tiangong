use std::time::Duration;

use async_trait::async_trait;
use tiangong_anthropic::types::{EventStream, MessagesCreateRequest, MessagesCreateResponse};
use tiangong_anthropic::{
    AnthropicClient as NativeAnthropicClient, AnthropicConfig as NativeAnthropicConfig,
};

use crate::error::LlmError;
use crate::model::ProviderModelInfo;
use crate::providers::anthropic::config::AnthropicConfig;
use crate::providers::anthropic::error::map_anthropic_error;

#[async_trait]
pub(crate) trait AnthropicTransport: Send + Sync {
    async fn create(
        &self,
        request: MessagesCreateRequest,
    ) -> Result<MessagesCreateResponse, LlmError>;

    async fn create_stream(&self, request: MessagesCreateRequest) -> Result<EventStream, LlmError>;

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError>;
}

#[derive(Clone)]
pub struct NativeAnthropicTransport {
    client: NativeAnthropicClient,
}

impl NativeAnthropicTransport {
    pub fn new(config: &AnthropicConfig) -> Result<Self, LlmError> {
        let mut native = NativeAnthropicConfig::new(config.api_key.clone());
        if let Some(base_url) = config.normalized_base_url() {
            native.base_url = base_url;
        }
        native.timeout = config.timeout;
        native.api_version = config.resolve_api_version();
        native.beta = config.beta.clone();
        native.headers = config.headers.clone();

        let client = NativeAnthropicClient::from_config(native).map_err(map_anthropic_error)?;
        Ok(Self { client })
    }
}

#[async_trait]
impl AnthropicTransport for NativeAnthropicTransport {
    async fn create(
        &self,
        request: MessagesCreateRequest,
    ) -> Result<MessagesCreateResponse, LlmError> {
        self.client
            .create(request)
            .await
            .map_err(map_anthropic_error)
    }

    async fn create_stream(&self, request: MessagesCreateRequest) -> Result<EventStream, LlmError> {
        self.client
            .create_stream(request)
            .await
            .map_err(map_anthropic_error)
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        let response = self
            .client
            .list_models()
            .await
            .map_err(map_anthropic_error)?;
        Ok(response
            .data
            .into_iter()
            .map(|item| ProviderModelInfo {
                id: item.id.clone(),
                display_name: item.display_name.or(Some(item.id)),
                context_window: None,
            })
            .collect())
    }
}

#[derive(Clone)]
pub(crate) struct AnthropicClient<T = NativeAnthropicTransport> {
    transport: T,
    config: AnthropicConfig,
}

impl AnthropicClient<NativeAnthropicTransport> {
    pub fn from_config(config: AnthropicConfig) -> Result<Self, LlmError> {
        let transport = NativeAnthropicTransport::new(&config)?;
        Ok(Self { transport, config })
    }
}

impl<T> AnthropicClient<T>
where
    T: AnthropicTransport,
{
    #[cfg(test)]
    pub fn new(transport: T, config: AnthropicConfig) -> Self {
        Self { transport, config }
    }

    pub async fn create(
        &self,
        request: MessagesCreateRequest,
    ) -> Result<MessagesCreateResponse, LlmError> {
        let model = request.model.clone();
        self.run_with_retry("anthropic_complete", "anthropic", &model, false, || {
            let request = request.clone();
            async move { self.transport.create(request).await }
        })
        .await
    }

    pub async fn create_stream(
        &self,
        request: MessagesCreateRequest,
    ) -> Result<EventStream, LlmError> {
        let model = request.model.clone();
        self.run_with_retry("anthropic_stream", "anthropic", &model, true, || {
            let request = request.clone();
            async move { self.transport.create_stream(request).await }
        })
        .await
    }

    pub async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        self.run_with_retry(
            "anthropic_list_models",
            "anthropic",
            "<list_models>",
            false,
            || async { self.transport.list_models().await },
        )
        .await
    }

    async fn run_with_retry<F, Fut, R>(
        &self,
        operation: &'static str,
        provider: &'static str,
        model: &str,
        stream: bool,
        mut f: F,
    ) -> Result<R, LlmError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<R, LlmError>>,
    {
        let mut attempt = 0u32;
        let mut delay_ms = 1000u64;

        loop {
            let start = std::time::Instant::now();
            tracing::info!(
                operation,
                provider,
                model,
                stream,
                attempt,
                "开始 Anthropic 请求"
            );
            match f().await {
                Ok(result) => {
                    tracing::info!(
                        operation,
                        provider,
                        model,
                        stream,
                        attempt,
                        latency_ms = start.elapsed().as_millis() as u64,
                        "Anthropic 请求完成"
                    );
                    return Ok(result);
                }
                Err(err) if err.is_retryable() && attempt < self.config.max_retries => {
                    attempt += 1;
                    tracing::warn!(
                        operation,
                        provider,
                        model,
                        stream,
                        attempt,
                        delay_ms,
                        error = %err,
                        "Anthropic 请求失败，准备重试"
                    );
                    if let Some(notifier) = &self.config.retry_notifier {
                        notifier(attempt, self.config.max_retries, delay_ms, &err.to_string());
                    }
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms *= 2;
                }
                Err(err) => {
                    tracing::warn!(
                        operation,
                        provider,
                        model,
                        stream,
                        attempt,
                        latency_ms = start.elapsed().as_millis() as u64,
                        error = %err,
                        "Anthropic 请求失败"
                    );
                    return Err(err);
                }
            }
        }
    }
}
