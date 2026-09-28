use std::time::Duration;

use async_trait::async_trait;
use futures_util::Stream;
use tiangong_deepseek::types::{ChatCompletionRequest, ChatCompletionResponse, EventStream};

use crate::error::LlmError;
use crate::model::ProviderModelInfo;
use crate::stream::ProviderStreamEvent;

use super::config::DeepSeekConfig;
use super::error::map_deepseek_error;

pub type DeepSeekStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<ProviderStreamEvent, LlmError>> + Send>>;

#[async_trait]
pub(crate) trait DeepSeekTransport: Send + Sync {
    async fn create(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, LlmError>;

    async fn create_stream(&self, request: ChatCompletionRequest) -> Result<EventStream, LlmError>;

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError>;
}

#[derive(Clone)]
pub struct NativeDeepSeekTransport {
    client: tiangong_deepseek::DeepSeekClient,
}

impl NativeDeepSeekTransport {
    pub fn new(config: &DeepSeekConfig) -> Result<Self, LlmError> {
        let mut native = tiangong_deepseek::DeepSeekConfig::new(config.api_key.clone());
        native.base_url = config.resolved_base_url();
        native.timeout = config.timeout;
        native.headers = config.headers.clone();

        let client =
            tiangong_deepseek::DeepSeekClient::from_config(native).map_err(map_deepseek_error)?;
        Ok(Self { client })
    }
}

#[async_trait]
impl DeepSeekTransport for NativeDeepSeekTransport {
    async fn create(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, LlmError> {
        self.client
            .chat()
            .create(request)
            .await
            .map_err(map_deepseek_error)
    }

    async fn create_stream(&self, request: ChatCompletionRequest) -> Result<EventStream, LlmError> {
        self.client
            .chat()
            .create_stream(request)
            .await
            .map_err(map_deepseek_error)
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        let response = self
            .client
            .models()
            .list()
            .await
            .map_err(map_deepseek_error)?;
        Ok(response
            .data
            .into_iter()
            .map(|item| ProviderModelInfo {
                id: item.id.clone(),
                display_name: Some(item.id),
                context_window: None,
            })
            .collect())
    }
}

#[derive(Clone)]
pub(crate) struct DeepSeekClient<T = NativeDeepSeekTransport> {
    transport: T,
    config: DeepSeekConfig,
}

impl DeepSeekClient<NativeDeepSeekTransport> {
    pub fn from_config(config: DeepSeekConfig) -> Result<Self, LlmError> {
        let transport = NativeDeepSeekTransport::new(&config)?;
        Ok(Self { transport, config })
    }
}

impl<T> DeepSeekClient<T>
where
    T: DeepSeekTransport,
{
    pub async fn create(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, LlmError> {
        let model = request.model.clone();
        self.run_with_retry("deepseek_complete", &model, false, || {
            let request = request.clone();
            async { self.transport.create(request).await }
        })
        .await
    }

    pub async fn create_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<EventStream, LlmError> {
        let model = request.model.clone();
        self.run_with_retry("deepseek_stream", &model, true, || {
            let request = request.clone();
            async { self.transport.create_stream(request).await }
        })
        .await
    }

    pub async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        self.run_with_retry("deepseek_list_models", "<list_models>", false, || async {
            self.transport.list_models().await
        })
        .await
    }

    async fn run_with_retry<F, Fut, R>(
        &self,
        operation: &'static str,
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
                provider = "deepseek",
                model,
                stream,
                attempt,
                "开始 DeepSeek 请求"
            );
            match f().await {
                Ok(result) => {
                    tracing::info!(
                        operation,
                        provider = "deepseek",
                        model,
                        stream,
                        attempt,
                        latency_ms = start.elapsed().as_millis() as u64,
                        "DeepSeek 请求完成"
                    );
                    return Ok(result);
                }
                Err(err) if err.is_retryable() && attempt < self.config.max_retries => {
                    attempt += 1;
                    tracing::warn!(
                        operation,
                        provider = "deepseek",
                        model,
                        stream,
                        attempt,
                        delay_ms,
                        error = %err,
                        "DeepSeek 请求失败，准备重试"
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
                        provider = "deepseek",
                        model,
                        stream,
                        attempt,
                        latency_ms = start.elapsed().as_millis() as u64,
                        error = %err,
                        "DeepSeek 请求失败"
                    );
                    return Err(err);
                }
            }
        }
    }
}
