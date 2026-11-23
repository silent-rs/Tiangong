use async_trait::async_trait;

use crate::error::LlmError;
use crate::model::ProviderModelInfo;
use crate::request::ProviderRequest;
use crate::stream::ProviderStream;

/// Provider 能力说明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub system_prompt: bool,
    pub list_models: bool,
}

/// 统一 LLM provider trait。
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn capabilities(&self) -> ProviderCapabilities;

    async fn stream(&self, req: ProviderRequest) -> Result<ProviderStream, LlmError>;
}
#[async_trait]
pub trait ModelCatalog: Send + Sync {
    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError>;
}
