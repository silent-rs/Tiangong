use std::time::Duration;

use async_trait::async_trait;
use tiangong_anthropic::AnthropicClient as NativeAnthropicClient;
use tiangong_anthropic::types::{EventStream, MessagesCreateRequest, MessagesCreateResponse};

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
    /// 禁用代理的客户端：环境变量代理不可达（如 VPN 已关闭）时直连重试（#662）。
    direct_client: NativeAnthropicClient,
}

impl NativeAnthropicTransport {
    pub fn new(config: &AnthropicConfig) -> Result<Self, LlmError> {
        let mut builder = NativeAnthropicClient::builder(config.api_key.clone())
            .timeout(config.timeout)
            .api_version(config.resolve_api_version())
            .beta(config.beta.clone())
            .headers(config.headers.clone());
        if let Some(base_url) = config.normalized_base_url() {
            builder = builder.base_url(base_url);
        }
        let client = builder.clone().build().map_err(map_anthropic_error)?;
        let direct_client = builder.no_proxy().build().map_err(map_anthropic_error)?;
        Ok(Self {
            client,
            direct_client,
        })
    }

    /// 执行请求；连接阶段失败时以直连客户端重试一次（请求尚未送达服务端，不会重复计费）。
    async fn with_direct_retry<R, Fut>(
        &self,
        operation: impl Fn(NativeAnthropicClient) -> Fut,
    ) -> Result<R, LlmError>
    where
        Fut: std::future::Future<Output = Result<R, tiangong_anthropic::AnthropicError>>,
    {
        match operation(self.client.clone()).await {
            Err(error) if error.is_connect() => {
                tracing::warn!(error = %error, "Anthropic 连接失败，改用直连重试");
                operation(self.direct_client.clone()).await
            }
            result => result,
        }
        .map_err(map_anthropic_error)
    }
}

#[async_trait]
impl AnthropicTransport for NativeAnthropicTransport {
    async fn create(
        &self,
        request: MessagesCreateRequest,
    ) -> Result<MessagesCreateResponse, LlmError> {
        self.with_direct_retry(|client| {
            let request = request.clone();
            async move { client.create(request).await }
        })
        .await
    }

    async fn create_stream(&self, request: MessagesCreateRequest) -> Result<EventStream, LlmError> {
        self.with_direct_retry(|client| {
            let request = request.clone();
            async move { client.create_stream(request).await }
        })
        .await
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        let response = self
            .with_direct_retry(|client| async move { client.list_models().await })
            .await?;
        Ok(response
            .data
            .into_iter()
            .map(|item| ProviderModelInfo {
                id: item.id.clone(),
                display_name: item.display_name.or(Some(item.id)),
                ..Default::default()
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
