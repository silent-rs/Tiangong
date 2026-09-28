//! Memory 侧 Embedding 封装
//!
//! 该模块不直接绑定具体模型提供方，只包装 `tiangong_llm::EmbeddingProvider`，
//! 为 Memory 的 query / node 向量化提供稳定入口。

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tiangong_llm::EmbeddingProvider;

use crate::types::MemoryNode;

const DEFAULT_BATCH_SIZE: usize = 32;

/// Memory 专用 embedding 客户端。
#[allow(dead_code)]
pub(crate) struct MemoryEmbeddingClient {
    provider: Arc<dyn EmbeddingProvider>,
    batch_size: usize,
}

#[allow(dead_code)]
impl MemoryEmbeddingClient {
    pub(crate) fn new(provider: Arc<dyn EmbeddingProvider>) -> Self {
        Self {
            provider,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    pub(crate) fn with_batch_size(provider: Arc<dyn EmbeddingProvider>, batch_size: usize) -> Self {
        Self {
            provider,
            batch_size: batch_size.max(1),
        }
    }

    pub(crate) fn dimension(&self) -> usize {
        self.provider.dimension()
    }

    pub(crate) fn model(&self) -> &str {
        self.provider.model()
    }

    pub(crate) async fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let text = normalize_text(query);
        if text.is_empty() {
            bail!("Memory query embedding 输入不能为空");
        }

        let vectors = self.embed_texts(vec![text]).await?;
        vectors
            .into_iter()
            .next()
            .context("Memory query embedding 返回空结果")
    }

    pub(crate) async fn embed_node(&self, node: &MemoryNode) -> Result<Vec<f32>> {
        let text = node_embedding_text(node);
        let vectors = self.embed_texts(vec![text]).await?;
        vectors
            .into_iter()
            .next()
            .context("Memory node embedding 返回空结果")
    }

    pub(crate) async fn embed_texts(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let normalized = texts
            .into_iter()
            .map(|text| normalize_text(&text))
            .collect::<Vec<_>>();
        if normalized.iter().any(|text| text.is_empty()) {
            bail!("Memory embedding 输入包含空文本");
        }

        let mut all_vectors = Vec::with_capacity(normalized.len());
        for chunk in normalized.chunks(self.batch_size) {
            let vectors = self.provider.embed(chunk.to_vec()).await.with_context(|| {
                format!(
                    "Memory embedding 请求失败: model={} batch={}",
                    self.model(),
                    chunk.len()
                )
            })?;
            if vectors.len() != chunk.len() {
                bail!(
                    "Memory embedding 返回数量不匹配: expected={} actual={}",
                    chunk.len(),
                    vectors.len()
                );
            }
            for vector in vectors {
                validate_vector_dimension(&vector, self.dimension())?;
                all_vectors.push(vector);
            }
        }

        Ok(all_vectors)
    }
}

#[allow(dead_code)]
pub(crate) fn node_embedding_text(node: &MemoryNode) -> String {
    normalize_text(&format!(
        "kind: {:?}\ntitle: {}\nsummary: {}\nkeywords: {}",
        node.kind,
        node.title,
        node.summary,
        node.keywords.join(", ")
    ))
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn validate_vector_dimension(vector: &[f32], expected: usize) -> Result<()> {
    if vector.len() != expected {
        bail!(
            "Memory embedding 向量维度不匹配: expected={} actual={}",
            expected,
            vector.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::types::{MemoryKind, MemoryScopeType, MemoryStatus};

    struct MockEmbeddingProvider {
        dimension: usize,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl MockEmbeddingProvider {
        fn new(dimension: usize) -> Self {
            Self {
                dimension,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().expect("mock lock poisoned").clone()
        }
    }

    #[async_trait]
    impl EmbeddingProvider for MockEmbeddingProvider {
        async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
            self.calls
                .lock()
                .expect("mock lock poisoned")
                .push(texts.clone());
            Ok(texts
                .iter()
                .map(|text| vec![text.len() as f32; self.dimension])
                .collect())
        }

        fn dimension(&self) -> usize {
            self.dimension
        }

        fn model(&self) -> &str {
            "mock-embedding"
        }
    }

    fn make_node() -> MemoryNode {
        MemoryNode {
            id: "node-1".to_string(),
            kind: MemoryKind::Episode,
            memory_type: crate::types::MemoryCognitiveType::Factual,
            scope_type: MemoryScopeType::Workspace,
            scope_id: Some("ws-1".to_string()),
            title: "Fix login timeout".to_string(),
            summary: "Retry request after timeout".to_string(),
            keywords: vec!["login".to_string(), "timeout".to_string()],
            importance: 0.8,
            confidence: 1.0,
            status: MemoryStatus::Active,
            source: Some("session-1".to_string()),
            usage_count: 0,
            last_used_at: None,
            created_at: "2026-04-20 09:00:00".to_string(),
            updated_at: "2026-04-20 09:00:00".to_string(),
        }
    }

    #[tokio::test]
    async fn embed_texts_preserves_order_and_batches() {
        let provider = Arc::new(MockEmbeddingProvider::new(3));
        let client = MemoryEmbeddingClient::with_batch_size(provider.clone(), 2);

        let vectors = client
            .embed_texts(vec![
                " alpha ".to_string(),
                "beta value".to_string(),
                "gamma".to_string(),
            ])
            .await
            .expect("embedding 应成功");

        assert_eq!(vectors.len(), 3);
        assert_eq!(vectors[0], vec![5.0, 5.0, 5.0]);
        assert_eq!(vectors[1], vec![10.0, 10.0, 10.0]);
        assert_eq!(vectors[2], vec![5.0, 5.0, 5.0]);
        assert_eq!(
            provider.calls(),
            vec![
                vec!["alpha".to_string(), "beta value".to_string()],
                vec!["gamma".to_string()],
            ]
        );
    }

    #[tokio::test]
    async fn embed_query_rejects_empty_text() {
        let provider = Arc::new(MockEmbeddingProvider::new(3));
        let client = MemoryEmbeddingClient::new(provider);

        let err = client
            .embed_query("  \n\t")
            .await
            .expect_err("空 query 应返回错误");

        assert!(err.to_string().contains("不能为空"));
    }

    #[tokio::test]
    async fn embed_node_uses_memory_node_fields() {
        let provider = Arc::new(MockEmbeddingProvider::new(2));
        let client = MemoryEmbeddingClient::new(provider);

        let vector = client
            .embed_node(&make_node())
            .await
            .expect("node embedding 应成功");

        assert_eq!(vector.len(), 2);
        assert!(node_embedding_text(&make_node()).contains("Fix login timeout"));
        assert!(node_embedding_text(&make_node()).contains("login, timeout"));
    }
}
