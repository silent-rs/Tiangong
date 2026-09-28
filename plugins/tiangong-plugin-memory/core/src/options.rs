//! Memory 启动参数。
//!
//! Memory 的独立磁盘配置定义在 `config` 模块；这里保留 actor
//! 启动时消费的已解析参数。

use serde::{Deserialize, Serialize};
use tiangong_llm::{EmbeddingEndpointConfig, LlmEndpointConfig, RerankEndpointConfig};

use crate::config::MemoryLocalTier;

#[derive(Debug, Clone, Default)]
pub struct MemoryOptions {
    pub model: Option<LlmEndpointConfig>,
    /// 在线 Embedding 端点；与 `local_embedding` 互斥，在线优先。
    pub embedding: Option<EmbeddingEndpointConfig>,
    /// 在线 Rerank 端点；与 `local_rerank` 互斥，在线优先。
    pub rerank: Option<RerankEndpointConfig>,
    /// 内置本地 Embedding（按档位选择模型，后台下载加载）。
    pub local_embedding: Option<MemoryLocalTier>,
    /// 内置本地 Rerank。
    pub local_rerank: Option<MemoryLocalTier>,
    pub vector_mode: MemoryVectorMode,
}

impl MemoryOptions {
    pub fn new() -> Self {
        Self {
            model: None,
            embedding: None,
            rerank: None,
            local_embedding: None,
            local_rerank: None,
            vector_mode: MemoryVectorMode::default(),
        }
    }

    pub fn with_model(mut self, model: LlmEndpointConfig) -> Self {
        self.model = Some(model);
        self
    }

    pub fn with_embedding(mut self, embedding: EmbeddingEndpointConfig) -> Self {
        self.embedding = Some(embedding);
        self
    }

    pub fn with_rerank(mut self, rerank: RerankEndpointConfig) -> Self {
        self.rerank = Some(rerank);
        self
    }

    pub fn with_local_embedding(mut self, tier: MemoryLocalTier) -> Self {
        self.local_embedding = Some(tier);
        self
    }

    pub fn with_local_rerank(mut self, tier: MemoryLocalTier) -> Self {
        self.local_rerank = Some(tier);
        self
    }

    /// 是否需要内置本地模型。
    pub fn needs_local_models(&self) -> bool {
        (self.embedding.is_none() && self.local_embedding.is_some())
            || (self.rerank.is_none() && self.local_rerank.is_some())
    }

    pub fn with_vector_mode(mut self, vector_mode: MemoryVectorMode) -> Self {
        self.vector_mode = vector_mode;
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryVectorMode {
    /// 有 embedding 配置时默认使用嵌入式 LanceDB 向量索引。
    #[default]
    Auto,
    /// 禁用向量层，仅使用 SQLite + Tantivy。
    Disabled,
    /// 使用嵌入式 LanceDB 向量索引。
    EmbeddedLanceDb,
}
