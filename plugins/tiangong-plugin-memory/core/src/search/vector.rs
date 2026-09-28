//! 向量索引抽象。

use anyhow::Result;
use async_trait::async_trait;

use crate::types::{RecallHit, VectorPoint};

#[async_trait(?Send)]
pub(crate) trait VectorIndex: Send {
    async fn ensure_ready(&self) -> Result<()>;
    async fn upsert(&self, point: VectorPoint) -> Result<()>;
    async fn search(&self, query_vector: Vec<f32>, limit: usize) -> Result<Vec<RecallHit>>;
    #[allow(dead_code)]
    async fn delete(&self, node_id: &str) -> Result<()>;
}
