//! Embedding 能力抽象层
//!
//! 提供 `EmbeddingProvider` trait + OpenAI 兼容实现（POST /v1/embeddings）。

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use crate::ProviderProtocol;

/// Embedding 能力抽象 trait
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// 将一批文本嵌入为向量
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>>;

    /// 返回向量维度（用于 Qdrant collection 初始化）
    fn dimension(&self) -> usize;

    /// 返回模型名称
    fn model(&self) -> &str;
}

/// Embedding 端点配置。
///
/// 上层配置系统负责解析模型文件；使用方只需把解析后的端点传给 llm crate，
/// 由 llm crate 选择具体 provider 实现。
#[derive(Debug, Clone)]
pub struct EmbeddingEndpointConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub protocol: ProviderProtocol,
    pub timeout: Duration,
    pub dimension: usize,
}

/// 根据端点配置创建 EmbeddingProvider。
pub fn embedding_provider_from_config(
    config: &EmbeddingEndpointConfig,
) -> Result<Arc<dyn EmbeddingProvider>> {
    match config.protocol {
        ProviderProtocol::OpenAiChatCompletions => {
            Ok(Arc::new(OpenAiEmbeddingProvider::from_config(config)?))
        }
        protocol => anyhow::bail!(
            "Embedding 暂不支持 {} 协议，请使用 OpenAI 兼容端点",
            protocol.as_str()
        ),
    }
}

// ==================== OpenAI 兼容实现 ====================

#[derive(Debug, Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

/// OpenAI 兼容的 Embedding Provider
///
/// 支持任何实现 OpenAI `/v1/embeddings` 接口的服务。
#[derive(Debug, Clone)]
pub struct OpenAiEmbeddingProvider {
    base_url: String,
    api_key: String,
    model: String,
    dimension: usize,
    client: reqwest::Client,
}

impl OpenAiEmbeddingProvider {
    /// 创建一个新的 OpenAI 兼容 Embedding Provider
    ///
    /// # 参数
    /// - `base_url`: API 基础 URL（如 `https://api.openai.com`）
    /// - `api_key`: API 密钥
    /// - `model`: 模型名（如 `text-embedding-3-small`）
    /// - `dimension`: 向量维度（text-embedding-3-small=1536, ada-002=1536）
    pub fn new(base_url: &str, api_key: &str, model: &str, dimension: usize) -> Self {
        let base_url = normalize_embedding_base_url(base_url);
        let client = reqwest::Client::builder()
            .user_agent(crate::headers::TIANGONG_USER_AGENT)
            .build()
            .expect("创建 Embedding HTTP client 失败");
        Self {
            base_url,
            api_key: api_key.to_string(),
            model: model.to_string(),
            dimension,
            client,
        }
    }

    /// 从统一 Embedding 端点配置创建 provider。
    pub fn from_config(config: &EmbeddingEndpointConfig) -> Result<Self> {
        if !matches!(config.protocol, ProviderProtocol::OpenAiChatCompletions) {
            anyhow::bail!(
                "OpenAI Embedding Provider 不支持 {} 协议",
                config.protocol.as_str()
            );
        }
        if config.dimension == 0 {
            anyhow::bail!("Embedding dimension 不能为 0");
        }

        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .user_agent(crate::headers::TIANGONG_USER_AGENT)
            .build()
            .context("创建 Embedding HTTP client 失败")?;
        Ok(Self {
            base_url: normalize_embedding_base_url(&config.base_url),
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            dimension: config.dimension,
            client,
        })
    }

    /// 从环境变量创建（`API_BASE_URL`, `API_AUTH_TOKEN`）
    pub fn from_env(model: &str, dimension: usize) -> Option<Self> {
        let base_url = std::env::var("API_BASE_URL").ok()?;
        let api_key = std::env::var("API_AUTH_TOKEN").ok()?;
        Some(Self::new(&base_url, &api_key, model, dimension))
    }
}

fn normalize_embedding_base_url(base_url: &str) -> String {
    let cleaned = base_url.trim().trim_end_matches('/');
    let cleaned = cleaned.strip_suffix("/chat/completions").unwrap_or(cleaned);
    let cleaned = cleaned.strip_suffix("/embeddings").unwrap_or(cleaned);
    let cleaned = cleaned.strip_suffix("/v1").unwrap_or(cleaned);
    cleaned.to_string()
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbeddingProvider {
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let url = format!("{}/v1/embeddings", self.base_url);
        let req = EmbeddingRequest {
            model: &self.model,
            input: &texts,
        };

        let mut request = self.client.post(&url).json(&req);
        if !self.api_key.trim().is_empty() {
            request = request.bearer_auth(&self.api_key);
        }

        let direct = || {
            reqwest::Client::builder()
                .user_agent(crate::headers::TIANGONG_USER_AGENT)
                .no_proxy()
                .build()
        };
        let resp = crate::http::send_with_direct_retry(request, direct)
            .await
            .with_context(|| format!("Embedding 请求失败: {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Embedding API 返回错误 {status}: {body}");
        }

        let mut result: EmbeddingResponse = resp.json().await.context("解析 Embedding 响应失败")?;

        // 按 index 排序，保证顺序与输入一致
        result.data.sort_by_key(|d| d.index);
        Ok(result.data.into_iter().map(|d| d.embedding).collect())
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn model(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_embedding_base_url;

    #[test]
    fn normalizes_openai_compatible_embedding_base_url() {
        assert_eq!(
            normalize_embedding_base_url("http://127.0.0.1:8000/v1"),
            "http://127.0.0.1:8000"
        );
        assert_eq!(
            normalize_embedding_base_url("http://127.0.0.1:8000/v1/embeddings"),
            "http://127.0.0.1:8000"
        );
        assert_eq!(
            normalize_embedding_base_url("http://127.0.0.1:8000/chat/completions"),
            "http://127.0.0.1:8000"
        );
    }
}
