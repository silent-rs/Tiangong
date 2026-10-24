//! 向量索引抽象。

use anyhow::Result;
use async_trait::async_trait;

use crate::types::{RecallHit, VectorPoint};

#[async_trait(?Send)]
pub(crate) trait VectorIndex: Send {
    async fn ensure_ready(&self) -> Result<()>;
    /// 按 `node_id` 批量覆盖写入：已存在的节点整行替换，不存在的插入。
    ///
    /// 实现必须把一批写入合并为一次提交（LanceDB 每次提交产生一个表版本），
    /// 调用方应尽量整批调用而不是逐条循环。同一批内 `node_id` 需唯一。
    async fn upsert_batch(&self, points: Vec<VectorPoint>) -> Result<()>;
    async fn search(&self, query_vector: Vec<f32>, limit: usize) -> Result<Vec<RecallHit>>;
    #[allow(dead_code)]
    async fn delete(&self, node_id: &str) -> Result<()>;
    /// 列出已写入向量的全部节点 ID（只读 `node_id` 列，用于覆盖率统计与缺失补写）。
    async fn list_node_ids(&self) -> Result<Vec<String>>;
}
