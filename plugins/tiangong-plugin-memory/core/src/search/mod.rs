//! 全文检索 + 向量检索子模块
//!
//! Phase B: Tantivy BM25 全文检索
//! Phase C: LanceDB 语义向量检索 + 双引擎融合重排

pub(crate) mod embedding;
pub(crate) mod lancedb_search;
pub(crate) mod reranker;
pub(crate) mod tantivy_search;
pub(crate) mod vector;
pub(crate) mod vector_meta;

#[allow(unused_imports)]
pub(crate) use embedding::MemoryEmbeddingClient;
pub(crate) use tantivy_search::TantivyIndex;
