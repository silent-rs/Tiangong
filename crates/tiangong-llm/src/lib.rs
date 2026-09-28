pub mod client;
pub mod embedding;
pub mod endpoint;
pub mod error;
mod headers;
pub mod message;
pub mod model;
pub mod models_config;
pub mod provider;
pub mod provider_client;
pub mod providers;
pub mod request;
pub mod rerank;
pub mod response;
pub mod stream;
pub mod text;
pub mod tool;
pub mod usage;

pub use endpoint::ModelEndpoint;
pub use models_config::{
    ModelCapability, ModelEntry, ModelsConfig, ProviderConfig, ProviderReferences, ResolvedModel,
    RoutingSlot,
};
pub use provider_client::{
    InvalidToolCall, ModelClient, ModelRequest, ModelResponse, ModelStreamChunk, OnRetryCallback,
    SingleProviderClient,
};

pub use client::rerank_provider_from_config;
pub use embedding::{
    EmbeddingEndpointConfig, EmbeddingProvider, OpenAiEmbeddingProvider,
    embedding_provider_from_config,
};
pub use model::{ProviderModelInfo, ProviderProtocol};
pub use request::ReasoningEffort;
pub use rerank::{
    RerankEndpointConfig, RerankProvider, RerankRequest, RerankResponse, RerankResult,
};
pub use response::StopReason;
pub use text::{LlmEndpointConfig, complete_text, complete_text_with_usage};
pub use usage::TokenUsageData;
