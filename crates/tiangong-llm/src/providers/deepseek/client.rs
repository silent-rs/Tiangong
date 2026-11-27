use super::config::DeepSeekConfig;
use super::error::map_deepseek_error;
use super::uploads::{self, DeepSeekFileIdTable};
use crate::error::LlmError;
use crate::model::ProviderModelInfo;
use crate::request::ProviderRequest;
use async_trait::async_trait;
use futures_util::Stream;
use std::time::Duration;
use tiangong_deepseek::types::{CreateResponseRequest, ResponsesEventStream};

pub type DeepSeekStream = std::pin::Pin<
    Box<dyn Stream<Item = Result<crate::stream::ProviderStreamEvent, LlmError>> + Send>,
>;

#[async_trait]
pub(crate) trait DeepSeekTransport: Send + Sync {
    async fn prepare_request(&self, request: ProviderRequest) -> Result<ProviderRequest, LlmError> {
        Ok(request)
    }
    async fn create_stream(
        &self,
        request: CreateResponseRequest,
    ) -> Result<ResponsesEventStream, LlmError>;
    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError>;
}

#[derive(Clone)]
pub struct NativeDeepSeekTransport {
    client: tiangong_deepseek::DeepSeekClient,
    /// 禁用代理的客户端：环境变量代理不可达（如 VPN 已关闭）时直连重试（#662）。
    direct_client: tiangong_deepseek::DeepSeekClient,
    /// Files API 上传 + 文档解析可能各需 120 秒，使用独立长超时客户端。
    files_client: tiangong_deepseek::DeepSeekClient,
    direct_files_client: tiangong_deepseek::DeepSeekClient,
    file_ids: DeepSeekFileIdTable,
    file_upload_defaults: serde_json::Map<String, serde_json::Value>,
}

impl NativeDeepSeekTransport {
    pub fn new(config: &DeepSeekConfig) -> Result<Self, LlmError> {
        let builder = tiangong_deepseek::DeepSeekClient::builder(config.api_key.clone())
            .base_url(config.resolved_base_url())
            .timeout(config.timeout)
            .headers(config.headers.clone());
        let client = builder.clone().build().map_err(map_deepseek_error)?;
        let direct_client = builder
            .clone()
            .no_proxy()
            .build()
            .map_err(map_deepseek_error)?;
        let files_builder = builder
            .clone()
            .timeout(config.timeout.max(Duration::from_secs(300)));
        let files_client = files_builder.clone().build().map_err(map_deepseek_error)?;
        let direct_files_client = files_builder
            .no_proxy()
            .build()
            .map_err(map_deepseek_error)?;
        let file_upload_defaults = uploads::file_upload_defaults()?;
        Ok(Self {
            client,
            direct_client,
            files_client,
            direct_files_client,
            file_ids: DeepSeekFileIdTable::persistent(),
            file_upload_defaults,
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
    async fn prepare_request(
        &self,
        mut request: ProviderRequest,
    ) -> Result<ProviderRequest, LlmError> {
        let metadata = request.metadata.get_or_insert_with(Default::default);
        for (key, value) in &self.file_upload_defaults {
            metadata.entry(key.clone()).or_insert_with(|| value.clone());
        }
        uploads::prepare_request(
            request,
            &self.files_client,
            &self.direct_files_client,
            &self.file_ids,
        )
        .await
    }

    async fn create_stream(
        &self,
        request: CreateResponseRequest,
    ) -> Result<ResponsesEventStream, LlmError> {
        self.with_direct_retry(|client| {
            let request = request.clone();
            async move { client.responses().create_stream(request).await }
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
    pub async fn prepare_request(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderRequest, LlmError> {
        self.transport.prepare_request(request).await
    }

    pub async fn create_stream(
        &self,
        request: CreateResponseRequest,
    ) -> Result<ResponsesEventStream, LlmError> {
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
        f: F,
    ) -> Result<R, LlmError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<R, LlmError>>,
    {
        let _ = stream;
        crate::retry::run(
            operation,
            model,
            self.config.max_retries,
            self.config.retry_notifier.as_ref(),
            f,
        )
        .await
    }
}
