use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use serde::Deserialize;

use crate::error::LlmError;
use crate::model::ProviderModelInfo;
use crate::provider::{LlmProvider, ModelCatalog, ProviderCapabilities};
use crate::request::ProviderRequest;
use crate::rerank::{RerankProvider, RerankRequest, RerankResponse, RerankResult};
use crate::stream::ProviderStream;

use super::client::OpenAiClient;
use super::config::OpenAiChatConfig;
use super::error::map_openai_error;
use super::mapping::{build_request_json, parse_complete_response};

#[derive(Clone)]
pub struct OpenAiChatCompletionsProvider {
    client: OpenAiClient,
}

#[derive(Clone)]
pub struct OpenAiChatRerankProvider {
    client: OpenAiClient,
    model: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiRerankResponse {
    #[serde(default)]
    results: Vec<OpenAiRerankResult>,
}

#[derive(Debug, Deserialize)]
struct OpenAiRerankResult {
    index: usize,
    #[serde(alias = "score")]
    relevance_score: f64,
}

impl OpenAiChatCompletionsProvider {
    pub fn new(config: OpenAiChatConfig) -> Self {
        Self {
            client: OpenAiClient::new(config),
        }
    }
}

impl OpenAiChatRerankProvider {
    pub fn new(config: OpenAiChatConfig, model: impl Into<String>) -> Self {
        Self {
            client: OpenAiClient::new(config),
            model: model.into(),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiChatCompletionsProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            system_prompt: true,
            list_models: true,
        }
    }

    async fn stream(&self, req: ProviderRequest) -> Result<ProviderStream, LlmError> {
        let model = req.model.clone();
        let payload = build_request_json(&req, true)
            .map_err(|err| LlmError::InvalidRequest(err.to_string()))?;
        match self.client.stream(&model, payload).await? {
            super::client::OpenAiStreamResponse::Sse(stream) => {
                let mut decoder = super::stream::OpenAiStreamDecoder::default();
                let mapped = stream
                    .map(move |item| match item {
                        Ok(payload) => decoder.parse_payload(&payload),
                        Err(err) => vec![Err(map_openai_error(&err))],
                    })
                    .flat_map(stream::iter);
                Ok(Box::pin(mapped))
            }
            // 服务端忽略 stream 参数返回一次性 JSON：复用非流式解析，
            // 合成等价的流事件序列，消费方无需感知差异。
            super::client::OpenAiStreamResponse::Complete(value) => {
                let response =
                    parse_complete_response(&value).map_err(|err| LlmError::Provider {
                        provider: "openai",
                        message: err.to_string(),
                    })?;
                Ok(Box::pin(stream::iter(
                    crate::stream::complete_response_events(response),
                )))
            }
        }
    }
}
#[async_trait]
impl ModelCatalog for OpenAiChatCompletionsProvider {
    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        self.client.list_models().await
    }
}

#[async_trait]
impl RerankProvider for OpenAiChatRerankProvider {
    async fn rerank(&self, request: RerankRequest) -> anyhow::Result<RerankResponse> {
        let response = self.client.rerank(&self.model, &request).await?;
        let parsed: OpenAiRerankResponse = serde_json::from_value(response)?;
        Ok(RerankResponse {
            results: parsed
                .results
                .into_iter()
                .map(|item| RerankResult {
                    index: item.index,
                    relevance_score: item.relevance_score,
                })
                .collect(),
        })
    }

    fn model(&self) -> &str {
        &self.model
    }
}
