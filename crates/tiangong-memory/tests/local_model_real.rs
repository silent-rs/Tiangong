//! 内置本地推理真实模型冒烟（需联网下载模型，默认忽略）。
//!
//! ```bash
//! TIANGONG_STORAGE_ROOT=/tmp/tg-local \
//!   cargo nextest run -p tiangong-memory --test local_model_real --run-ignored all
//! ```
//!
//! 可用 `TIANGONG_MEMORY_REAL_TIER=low|mid|high` 选择档位（默认 low）。

use tiangong_memory::MemoryLocalTier;
use tiangong_memory::local_model::{load_embedding_for_test, load_rerank_for_test};

fn tier() -> MemoryLocalTier {
    std::env::var("TIANGONG_MEMORY_REAL_TIER")
        .ok()
        .and_then(|value| MemoryLocalTier::parse(&value))
        .unwrap_or(MemoryLocalTier::Low)
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要下载真实模型"]
async fn local_embedding_captures_semantics() {
    let provider = load_embedding_for_test(tier())
        .await
        .expect("加载内置 Embedding");
    let vectors = provider
        .embed(vec![
            "如何修复登录超时的问题".to_string(),
            "登录请求超时后自动重试".to_string(),
            "今天午饭吃了红烧肉".to_string(),
        ])
        .await
        .expect("推理");
    assert_eq!(vectors.len(), 3);
    assert!(vectors.iter().all(|v| v.len() == provider.dimension()));
    let related = cosine(&vectors[0], &vectors[1]);
    let unrelated = cosine(&vectors[0], &vectors[2]);
    println!(
        "model={} dim={} related={related:.3} unrelated={unrelated:.3}",
        provider.model(),
        provider.dimension()
    );
    assert!(related > unrelated + 0.05, "语义相近文本应更相似");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要下载真实模型"]
async fn local_rerank_orders_by_relevance() {
    let provider = load_rerank_for_test(tier()).await.expect("加载内置 Rerank");
    let response = provider
        .rerank(tiangong_llm::RerankRequest {
            query: "登录超时怎么处理".to_string(),
            documents: vec![
                "今天午饭吃了红烧肉".to_string(),
                "登录请求超时后应自动重试并提示用户".to_string(),
                "Rust 的所有权规则".to_string(),
            ],
            top_n: 3,
        })
        .await
        .expect("推理");
    println!("model={} results={:?}", provider.model(), response.results);
    assert_eq!(response.results.len(), 3);
    assert_eq!(response.results[0].index, 1, "最相关文档应排第一");
    assert!(
        response
            .results
            .iter()
            .all(|r| (0.0..=1.0).contains(&r.relevance_score))
    );
}
