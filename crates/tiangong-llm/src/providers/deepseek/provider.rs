use super::client::{DeepSeekClient, NativeDeepSeekTransport};
use super::config::DeepSeekConfig;
use super::mapping::{from_deepseek_response, to_deepseek_request};
use super::stream::map_deepseek_stream;
use crate::error::LlmError;
use crate::model::ProviderModelInfo;
use crate::provider::{LlmProvider, ProviderCapabilities};
use crate::request::ProviderRequest;
use crate::response::ProviderResponse;
use crate::stream::ProviderStream;
use async_trait::async_trait;

#[derive(Clone)]
pub struct DeepSeekProvider<T = NativeDeepSeekTransport> {
    client: DeepSeekClient<T>,
}

impl DeepSeekProvider<NativeDeepSeekTransport> {
    pub fn from_config(config: DeepSeekConfig) -> Result<Self, LlmError> {
        Ok(Self {
            client: DeepSeekClient::from_config(config)?,
        })
    }
    pub fn new(config: DeepSeekConfig) -> Result<Self, LlmError> {
        Self::from_config(config)
    }
}

#[async_trait]
impl<T> LlmProvider for DeepSeekProvider<T>
where
    T: super::client::DeepSeekTransport,
{
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            system_prompt: true,
            list_models: true,
        }
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, LlmError> {
        let request =
            to_deepseek_request(&req).map_err(|err| LlmError::InvalidRequest(err.to_string()))?;
        let response = self.client.create(request).await?;
        from_deepseek_response(response, !req.tools.is_empty()).map_err(|err| LlmError::Provider {
            provider: "deepseek",
            message: err.to_string(),
        })
    }

    async fn stream(&self, req: ProviderRequest) -> Result<ProviderStream, LlmError> {
        let request =
            to_deepseek_request(&req).map_err(|err| LlmError::InvalidRequest(err.to_string()))?;
        // Responses 模式下 stream 标志由 SDK 在 create_stream 内强制开启
        // （语义化 SSE，终止事件后无 [DONE] 标记），无需在此设置。
        let event_stream = self.client.create_stream(request).await?;
        Ok(map_deepseek_stream(event_stream))
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        self.client.list_models().await
    }
}
