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
    /// 禁用代理的客户端：环境变量代理不可达（如 VPN 已关闭）时直连重试（#662）。
    direct_client: tiangong_deepseek::DeepSeekClient,
}

impl NativeDeepSeekTransport {
    pub fn new(config: &DeepSeekConfig) -> Result<Self, LlmError> {
        let builder = tiangong_deepseek::DeepSeekClient::builder(config.api_key.clone())
            .base_url(config.resolved_base_url())
            .timeout(config.timeout)
            .headers(config.headers.clone());
        let client = builder.clone().build().map_err(map_deepseek_error)?;
        let direct_client = builder.no_proxy().build().map_err(map_deepseek_error)?;
        Ok(Self {
            client,
            direct_client,
        })
    }

    /// 执行请求；连接阶段失败时以直连客户端重试一次（请求尚未送达服务端，不会重复计费）。
    async fn with_direct_retry<R, Fut>(
        &self,
        operation: impl Fn(tiangong_deepseek::DeepSeekClient) -> Fut,
    ) -> Result<R, LlmError>
    where
        Fut: std::future::Future<Output = Result<R, tiangong_deepseek::DeepSeekError>>,
    {
        match operation(self.client.clone()).await {
            Err(error) if error.is_connect() => {
                tracing::warn!(error = %error, "DeepSeek 连接失败，改用直连重试");
                operation(self.direct_client.clone()).await
            }
            result => result,
        }
        .map_err(map_deepseek_error)
    }
}

#[async_trait]
impl DeepSeekTransport for NativeDeepSeekTransport {
    async fn create(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, LlmError> {
        self.with_direct_retry(|client| {
            let request = request.clone();
            async move { client.chat().create(request).await }
        })
        .await
    }

    async fn create_stream(&self, request: ChatCompletionRequest) -> Result<EventStream, LlmError> {
        self.with_direct_retry(|client| {
            let request = request.clone();
            async move { client.chat().create_stream(request).await }
        })
        .await
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        let response = self
            .with_direct_retry(|client| async move { client.models().list().await })
            .await?;
        Ok(response
            .data
            .into_iter()
            .map(|item| ProviderModelInfo {
                id: item.id.clone(),
                display_name: Some(item.id),
                ..Default::default()
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
