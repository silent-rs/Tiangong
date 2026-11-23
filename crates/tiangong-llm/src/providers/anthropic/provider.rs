use async_trait::async_trait;

use crate::error::LlmError;
use crate::model::ProviderModelInfo;
use crate::provider::{LlmProvider, ModelCatalog, ProviderCapabilities};
use crate::providers::anthropic::client::{AnthropicClient, NativeAnthropicTransport};
use crate::providers::anthropic::config::AnthropicConfig;
use crate::providers::anthropic::mapping::to_anthropic_request;
use crate::providers::anthropic::stream::map_anthropic_stream;
use crate::request::ProviderRequest;
use crate::stream::ProviderStream;

#[derive(Clone)]
pub struct AnthropicProvider<T = NativeAnthropicTransport> {
    client: AnthropicClient<T>,
}

impl AnthropicProvider<NativeAnthropicTransport> {
    pub fn from_config(config: AnthropicConfig) -> Result<Self, LlmError> {
        Ok(Self {
            client: AnthropicClient::from_config(config)?,
        })
    }
}

impl<T> AnthropicProvider<T> {
    #[cfg(test)]
    pub(crate) fn new(client: AnthropicClient<T>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl<T> LlmProvider for AnthropicProvider<T>
where
    T: super::client::AnthropicTransport,
{
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            system_prompt: true,
            list_models: true,
        }
    }

    async fn stream(&self, req: ProviderRequest) -> Result<ProviderStream, LlmError> {
        let request = to_anthropic_request(&req)?;
        let stream = self.client.create_stream(request).await?;
        Ok(map_anthropic_stream(stream))
    }
}
#[async_trait]
impl<T: super::client::AnthropicTransport> ModelCatalog for AnthropicProvider<T> {
    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        self.client.list_models().await
    }
}
