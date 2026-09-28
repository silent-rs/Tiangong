//! 渐进式召回（Phase C）
//!
//! 协调 Tantivy BM25 召回（由 MemoryStore 提供）和向量语义召回，融合重排后返回最终结果。
//! RecallEngine 不持有 TantivyIndex，避免同一索引目录双 writer 锁冲突。

use std::sync::Arc;

use tiangong_llm::{EmbeddingProvider, RerankProvider, RerankRequest};

use crate::search::embedding::node_embedding_text;
use crate::search::reranker::{Reranker, analyze_query};
use crate::search::vector::VectorIndex;
use crate::types::{MemoryNode, RecallHit, SearchStrategy, VectorPoint};

/// 召回引擎（可选向量索引，降级为纯 BM25）
///
/// BM25 搜索由外部（MemoryStore）执行后作为 `bm25_hits` 传入；
/// RecallEngine 只负责可选的向量增强与最终融合重排。
pub(crate) struct RecallEngine {
    vector_index: Option<Box<dyn VectorIndex>>,
    embedding: Option<Arc<dyn EmbeddingProvider>>,
    rerank: Option<Arc<dyn RerankProvider>>,
    /// 向量表回填是否完成：未完成时新节点照常写入，但召回不走语义检索，
    /// 避免半量索引拉低结果质量。
    semantic_ready: bool,
}

impl RecallEngine {
    /// 纯 BM25 模式（Phase B 兼容）
    pub(crate) fn bm25_only() -> Self {
        Self {
            vector_index: None,
            embedding: None,
            rerank: None,
            semantic_ready: false,
        }
    }

    /// BM25 + 模型精排模式。
    pub(crate) fn rerank_only(rerank: Arc<dyn RerankProvider>) -> Self {
        Self {
            vector_index: None,
            embedding: None,
            rerank: Some(rerank),
            semantic_ready: false,
        }
    }

    /// 双引擎模式（Tantivy + VectorIndex）
    pub(crate) fn dual(
        vector_index: Box<dyn VectorIndex>,
        embedding: Arc<dyn EmbeddingProvider>,
        rerank: Option<Arc<dyn RerankProvider>>,
        semantic_ready: bool,
    ) -> Self {
        Self {
            vector_index: Some(vector_index),
            embedding: Some(embedding),
            rerank,
            semantic_ready,
        }
    }

    pub(crate) fn set_semantic_ready(&mut self, ready: bool) {
        self.semantic_ready = ready;
    }

    /// 当前 embedding provider（回填在后台线程复用它计算向量）。
    pub(crate) fn embedding_provider(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.embedding.clone()
    }

    /// 把节点的回填文本取出来，交给后台计算 embedding。
    pub(crate) fn backfill_texts(nodes: &[MemoryNode]) -> Vec<String> {
        nodes.iter().map(node_embedding_text).collect()
    }

    /// 写入已在后台算好的回填向量。
    ///
    /// 返回 (成功条数, 失败条数)：单条失败不该让整批回滚，但也不能被忽略，
    /// 否则游标继续推进后这些节点会永久缺索引。
    pub(crate) async fn upsert_nodes_batch(
        &self,
        nodes: &[MemoryNode],
        vectors: Vec<Vec<f32>>,
    ) -> anyhow::Result<(usize, usize)> {
        let Some(vector_index) = self.vector_index.as_ref() else {
            return Ok((0, 0));
        };
        if nodes.is_empty() {
            return Ok((0, 0));
        }
        if vectors.len() != nodes.len() {
            anyhow::bail!(
                "Memory 回填 embedding 数量不一致: expected={} actual={}",
                nodes.len(),
                vectors.len()
            );
        }
        let mut written = 0;
        let mut failed = 0;
        for (node, vector) in nodes.iter().zip(vectors) {
            match vector_index
                .upsert(VectorPoint {
                    node_id: node.id.clone(),
                    title: node.title.clone(),
                    summary: node.summary.clone(),
                    kind: node.kind.clone(),
                    importance: f64::from(node.importance),
                    vector,
                })
                .await
            {
                Ok(()) => written += 1,
                Err(err) => {
                    failed += 1;
                    tracing::warn!(node_id = %node.id, "Memory 回填写入向量失败: {err}");
                }
            }
        }
        Ok((written, failed))
    }

    /// 将节点写入可选语义索引。未启用向量索引时直接跳过。
    pub(crate) async fn upsert_node(&self, node: &MemoryNode) -> anyhow::Result<()> {
        let (vector_index, emb_ref) = match (self.vector_index.as_ref(), self.embedding.as_ref()) {
            (Some(q), Some(e)) => (q, e),
            _ => return Ok(()),
        };

        let text = node_embedding_text(node);
        let mut vectors = emb_ref.embed(vec![text]).await?;
        let Some(vector) = vectors.pop() else {
            anyhow::bail!("Memory node embedding 返回空结果");
        };
        vector_index
            .upsert(VectorPoint {
                node_id: node.id.clone(),
                title: node.title.clone(),
                summary: node.summary.clone(),
                kind: node.kind.clone(),
                importance: f64::from(node.importance),
                vector,
            })
            .await?;
        tracing::debug!(
            node_id = %node.id,
            kind = ?node.kind,
            backend = "vector",
            "Memory vector upsert 完成"
        );
        Ok(())
    }

    /// 从可选语义索引删除节点。未启用向量索引时直接跳过。
    pub(crate) async fn delete_node(&self, node_id: &str) -> anyhow::Result<()> {
        let Some(vector_index) = self.vector_index.as_ref() else {
            return Ok(());
        };
        vector_index.delete(node_id).await?;
        tracing::debug!(
            node_id = %node_id,
            backend = "vector",
            "Memory vector delete 完成"
        );
        Ok(())
    }

    /// 执行召回：接受已完成的 BM25 结果，可选地用向量检索增强后融合重排
    ///
    /// `strategy` 为外部（Core/LLM）传入的检索策略，为 None 时内部自动判断。
    pub(crate) async fn recall(
        &self,
        bm25_hits: Vec<RecallHit>,
        query: &str,
        limit: usize,
        strategy: Option<&SearchStrategy>,
    ) -> Vec<RecallHit> {
        if limit == 0 {
            return Vec::new();
        }

        let reranker = match strategy {
            Some(s) => Reranker::from_strategy(s),
            None => {
                let intent = analyze_query(query);
                Reranker::from_intent(intent)
            }
        };

        // 若没有向量索引或回填未完成，退化为纯 BM25
        let (vector_index, emb_ref) = match (self.vector_index.as_ref(), self.embedding.as_ref()) {
            (Some(q), Some(e)) if self.semantic_ready => (q, e),
            _ => {
                tracing::debug!(
                    query = %query,
                    backend = "bm25",
                    hit_count = bm25_hits.len().min(limit),
                    "Memory recall 使用 BM25-only 后端"
                );
                return self
                    .apply_model_rerank(query, bm25_hits, limit, "bm25")
                    .await;
            }
        };

        // 向量化 query
        let vectors = match emb_ref.embed(vec![query.to_string()]).await {
            Ok(v) if !v.is_empty() => v,
            Ok(_) => {
                return self
                    .apply_model_rerank(query, bm25_hits, limit, "bm25_fallback")
                    .await;
            }
            Err(e) => {
                tracing::warn!("Embedding 失败，退化为 BM25 召回: {}", e);
                tracing::debug!(
                    query = %query,
                    backend = "bm25_fallback",
                    hit_count = bm25_hits.len().min(limit),
                    "Memory recall 向量化失败后降级"
                );
                return self
                    .apply_model_rerank(query, bm25_hits, limit, "bm25_fallback")
                    .await;
            }
        };

        // 向量语义召回
        let semantic_hits = match vector_index.search(vectors[0].clone(), limit * 2).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("向量搜索失败，退化为 BM25 召回: {}", e);
                tracing::debug!(
                    query = %query,
                    backend = "bm25_fallback",
                    hit_count = bm25_hits.len().min(limit),
                    "Memory recall 向量搜索失败后降级"
                );
                return self
                    .apply_model_rerank(query, bm25_hits, limit, "bm25_fallback")
                    .await;
            }
        };

        // 融合重排
        let bm25_count = bm25_hits.len();
        let semantic_count = semantic_hits.len();
        let fused = reranker.fuse(bm25_hits, semantic_hits, limit.saturating_mul(2).max(limit));
        let hit_count_before_rerank = fused.len();
        let fused = self.apply_model_rerank(query, fused, limit, "hybrid").await;
        tracing::debug!(
            query = %query,
            backend = "hybrid",
            bm25_hit_count = bm25_count,
            semantic_hit_count = semantic_count,
            candidate_count = hit_count_before_rerank,
            hit_count = fused.len(),
            "Memory recall 使用混合后端"
        );
        fused
    }

    async fn apply_model_rerank(
        &self,
        query: &str,
        hits: Vec<RecallHit>,
        limit: usize,
        backend: &str,
    ) -> Vec<RecallHit> {
        if limit == 0 {
            return Vec::new();
        }

        let Some(rerank) = self.rerank.as_ref() else {
            return hits.into_iter().take(limit).collect();
        };
        if hits.len() <= 1 || query.trim().is_empty() {
            return hits.into_iter().take(limit).collect();
        }

        let documents = hits
            .iter()
            .map(|hit| format!("{}\n{}", hit.title, hit.summary))
            .collect::<Vec<_>>();
        let response = match rerank
            .rerank(RerankRequest {
                query: query.to_string(),
                documents,
                top_n: limit.max(1).min(hits.len()),
            })
            .await
        {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(
                    query = %query,
                    backend,
                    model = %rerank.model(),
                    "Memory rerank 失败，使用规则排序: {err}"
                );
                return hits.into_iter().take(limit).collect();
            }
        };

        if response.results.is_empty() {
            return hits.into_iter().take(limit).collect();
        }

        let mut used = std::collections::HashSet::new();
        let mut reranked = Vec::new();
        for result in response.results {
            if result.index >= hits.len() || !used.insert(result.index) {
                continue;
            }
            let mut hit = hits[result.index].clone();
            hit.score = result.relevance_score;
            reranked.push(hit);
            if reranked.len() >= limit {
                break;
            }
        }

        if reranked.is_empty() {
            return hits.into_iter().take(limit).collect();
        }

        if reranked.len() < limit {
            for (index, hit) in hits.into_iter().enumerate() {
                if used.insert(index) {
                    reranked.push(hit);
                    if reranked.len() >= limit {
                        break;
                    }
                }
            }
        }

        tracing::debug!(
            query = %query,
            backend,
            model = %rerank.model(),
            hit_count = reranked.len(),
            "Memory recall 使用 tiangong-llm rerank 精排"
        );
        reranked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MemoryKind;
    use anyhow::Result;
    use async_trait::async_trait;
    use tiangong_llm::{RerankResponse, RerankResult};

    struct ReverseRerankProvider;
    struct PartialRerankProvider;

    #[async_trait]
    impl RerankProvider for ReverseRerankProvider {
        async fn rerank(&self, request: RerankRequest) -> Result<RerankResponse> {
            let results = (0..request.documents.len())
                .rev()
                .map(|index| RerankResult {
                    index,
                    relevance_score: index as f64,
                })
                .collect();
            Ok(RerankResponse { results })
        }

        fn model(&self) -> &str {
            "reverse-rerank"
        }
    }

    #[async_trait]
    impl RerankProvider for PartialRerankProvider {
        async fn rerank(&self, _request: RerankRequest) -> Result<RerankResponse> {
            Ok(RerankResponse {
                results: vec![RerankResult {
                    index: 2,
                    relevance_score: 0.9,
                }],
            })
        }

        fn model(&self) -> &str {
            "partial-rerank"
        }
    }

    fn make_hits(count: usize) -> Vec<RecallHit> {
        (0..count)
            .map(|i| RecallHit {
                node_id: format!("node-{i}"),
                title: format!("标题 {i}"),
                summary: format!("摘要内容 {i}"),
                score: 1.0 - (i as f64 * 0.05),
                kind: MemoryKind::Episode,
                importance: 0.5,
                depth1_loaded: false,
            })
            .collect()
    }

    #[tokio::test]
    async fn bm25_only_limits_results_to_requested_count() {
        let engine = RecallEngine::bm25_only();
        let hits = make_hits(10);
        let result = engine.recall(hits, "测试查询", 5, None).await;
        assert_eq!(result.len(), 5);
    }

    #[tokio::test]
    async fn bm25_only_returns_all_when_limit_exceeds_hits() {
        let engine = RecallEngine::bm25_only();
        let hits = make_hits(3);
        let result = engine.recall(hits, "测试查询", 10, None).await;
        assert_eq!(result.len(), 3, "结果数不应超过实际命中数");
    }

    #[tokio::test]
    async fn bm25_only_returns_empty_for_empty_input() {
        let engine = RecallEngine::bm25_only();
        let result = engine.recall(vec![], "测试查询", 5, None).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn bm25_only_preserves_order_by_score() {
        let engine = RecallEngine::bm25_only();
        let hits = make_hits(5); // score 从高到低
        let result = engine.recall(hits, "测试查询", 5, None).await;
        // 保持输入顺序（无向量引擎时不重排）
        for (i, hit) in result.iter().enumerate() {
            assert_eq!(hit.node_id, format!("node-{i}"));
        }
    }

    #[tokio::test]
    async fn rerank_only_uses_tiangong_llm_rerank_provider() {
        let engine = RecallEngine::rerank_only(Arc::new(ReverseRerankProvider));
        let hits = make_hits(4);
        let result = engine.recall(hits, "测试查询", 3, None).await;

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].node_id, "node-3");
        assert_eq!(result[1].node_id, "node-2");
        assert_eq!(result[2].node_id, "node-1");
    }

    #[tokio::test]
    async fn rerank_fills_missing_hits_with_rule_order() {
        let engine = RecallEngine::rerank_only(Arc::new(PartialRerankProvider));
        let hits = make_hits(4);
        let result = engine.recall(hits, "测试查询", 3, None).await;

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].node_id, "node-2");
        assert_eq!(result[1].node_id, "node-0");
        assert_eq!(result[2].node_id, "node-1");
    }

    #[tokio::test]
    async fn recall_returns_empty_when_limit_is_zero() {
        let engine = RecallEngine::rerank_only(Arc::new(ReverseRerankProvider));
        let result = engine.recall(make_hits(4), "测试查询", 0, None).await;

        assert!(result.is_empty());
    }
}
