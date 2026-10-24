//! 补写缺失向量（整理工具侧实现，不依赖 Memory 插件新增能力）。
//!
//! Memory 写入记忆时 embedding 未就绪或失败只告警，而向量回填只在新建向量表
//! 时运行一次，漏写的记忆会永久缺语义索引，也就无法参与整理的相似比较。
//! 本模块按真实差集补齐：
//!
//! 1. 经 Memory 公开 API 列出全部活跃记忆；
//! 2. 只读当前模型向量表的 `node_id` 列，求出缺向量的记忆；
//! 3. 用与 Memory 相同的 embedding 模型、相同的节点文本
//!    （[`tiangong_memory::node_embedding_text`]）计算向量，按 `node_id`
//!    merge 写入同一张表（幂等，可重复运行）。
//!
//! 写入直接落 LanceDB，因此调用方必须保证没有其他 Memory 进程同时写该表
//! （整理工具在补写阶段要求自身为 Leader，见 `main.rs`）。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures_util::TryStreamExt;
use lancedb::Table;
use lancedb::arrow::arrow_array::{
    Array, ArrayRef, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator,
    StringArray,
};
use lancedb::arrow::arrow_schema::{DataType, Field};
use lancedb::connect;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use tiangong_llm::EmbeddingProvider;
use tiangong_memory::{MemoryListQuery, MemoryNode, MemoryStatus, node_embedding_text};

use crate::consolidation::HandleProvider;
use crate::vector_store::connection_uri;

/// 单次 embedding 请求的文本数。
const EMBED_BATCH: usize = 32;
/// 单次写入（一个 LanceDB 表版本）的向量数。
const WRITE_BATCH: usize = 256;
/// 分页拉取活跃记忆的页大小（Memory 列表接口上限）。
const LIST_PAGE: usize = 500;

/// 一次补写的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VectorRepairReport {
    /// 向量表名。
    pub table: String,
    /// 活跃记忆总数。
    pub active: usize,
    /// 补写前缺向量的活跃记忆数。
    pub missing: usize,
    /// 成功补写的条数。
    pub repaired: usize,
    /// 补写失败的条数（embedding 或写入失败）。
    pub failed: usize,
    /// 向量表中不属于任何活跃记忆的残留向量数（已归档/删除，仅统计不处理）。
    pub orphaned: usize,
    /// 是否试运行（只统计不写入）。
    pub dry_run: bool,
}

impl VectorRepairReport {
    /// 补写后仍缺向量的条数。
    pub fn remaining(&self) -> usize {
        self.missing.saturating_sub(self.repaired)
    }

    /// 面向用户的单行摘要。
    pub fn summary(&self) -> String {
        let orphan = if self.orphaned > 0 {
            format!(
                "；另有 {} 条残留向量不属于活跃记忆（未处理）",
                self.orphaned
            )
        } else {
            String::new()
        };
        if self.missing == 0 {
            return format!(
                "向量覆盖完整：{} 条活跃记忆均已有向量（{}）{orphan}",
                self.active, self.table
            );
        }
        if self.dry_run {
            return format!(
                "试运行：{} 条活跃记忆中有 {} 条缺向量（{}），正式运行时补写{orphan}",
                self.active, self.missing, self.table
            );
        }
        format!(
            "向量补写完成：缺失 {}，补写 {}，失败 {}，剩余 {}（{}）{orphan}",
            self.missing,
            self.repaired,
            self.failed,
            self.remaining(),
            self.table
        )
    }
}

/// 补写缺失向量。
///
/// - `lancedb_dir`：`<天工根目录>/data/memory/lancedb`；
/// - `table`：当前模型的向量表名（[`tiangong_memory::vector_table_name`]）；
/// - `embedding`：与 Memory 相同的 embedding 模型；`None` 表示试运行只统计。
pub async fn repair_missing_vectors(
    provider: HandleProvider<'_>,
    lancedb_dir: &Path,
    table: &str,
    embedding: Option<Arc<dyn EmbeddingProvider>>,
    progress: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> Result<VectorRepairReport> {
    let table_handle = open_table(lancedb_dir, table).await?;
    let dimension = table_dimension(&table_handle).await?;
    if let Some(embedding) = embedding.as_ref()
        && embedding.dimension() != dimension
    {
        bail!(
            "embedding 模型 {} 的维度 {} 与向量表 {table} 的维度 {dimension} 不一致，拒绝写入",
            embedding.model(),
            embedding.dimension()
        );
    }
    let indexed = indexed_node_ids(&table_handle).await?;
    let active = list_active_nodes(provider).await;
    let mut report = VectorRepairReport {
        table: table.to_string(),
        active: active.len(),
        orphaned: indexed
            .iter()
            .filter(|id| !active.contains_key(id.as_str()))
            .count(),
        dry_run: embedding.is_none(),
        ..Default::default()
    };
    // 按 ID 排序，保证分批顺序稳定、日志可复现。
    let mut missing: Vec<&MemoryNode> = active
        .values()
        .filter(|node| !indexed.contains(&node.id))
        .collect();
    missing.sort_by(|left, right| left.id.cmp(&right.id));
    report.missing = missing.len();
    let Some(embedding) = embedding else {
        return Ok(report);
    };
    for chunk in missing.chunks(WRITE_BATCH) {
        let mut nodes = Vec::with_capacity(chunk.len());
        let mut vectors = Vec::with_capacity(chunk.len());
        for sub in chunk.chunks(EMBED_BATCH) {
            let texts = sub.iter().map(|node| node_embedding_text(node)).collect();
            match embedding.embed(texts).await {
                Ok(embedded) if embedded.len() == sub.len() => {
                    for (node, vector) in sub.iter().zip(embedded) {
                        if vector.len() == dimension {
                            nodes.push(*node);
                            vectors.push(vector);
                        } else {
                            report.failed += 1;
                        }
                    }
                }
                Ok(embedded) => {
                    report.failed += sub.len();
                    tracing::warn!(
                        expected = sub.len(),
                        actual = embedded.len(),
                        "embedding 返回数量不一致，跳过该批"
                    );
                }
                Err(err) => {
                    report.failed += sub.len();
                    tracing::warn!(batch = sub.len(), "embedding 计算失败，跳过该批: {err:#}");
                }
            }
        }
        if !nodes.is_empty() {
            match write_vectors(&table_handle, &nodes, vectors, dimension).await {
                Ok(()) => report.repaired += nodes.len(),
                Err(err) => {
                    report.failed += nodes.len();
                    tracing::warn!(batch = nodes.len(), "写入向量失败: {err:#}");
                }
            }
        }
        if let Some(callback) = progress {
            callback(&format!(
                "补写向量 [{}/{}]（失败 {}）",
                report.repaired, report.missing, report.failed
            ));
        }
    }
    if report.repaired == 0 && report.failed > 0 {
        bail!(
            "补写向量全部失败（{} 条），请检查 embedding 模型状态",
            report.failed
        );
    }
    Ok(report)
}

async fn open_table(lancedb_dir: &Path, table: &str) -> Result<Table> {
    if !lancedb_dir.is_dir() {
        bail!("向量库目录不存在：{}", lancedb_dir.display());
    }
    let db = connect(&connection_uri(lancedb_dir))
        .execute()
        .await
        .with_context(|| format!("打开向量库失败：{}", lancedb_dir.display()))?;
    db.open_table(table)
        .execute()
        .await
        .with_context(|| format!("打开向量表 {table} 失败（Memory 尚未为当前模型建表？）"))
}

/// 向量列维度（从表结构读取）。
async fn table_dimension(table: &Table) -> Result<usize> {
    let schema = table.schema().await.context("读取向量表结构失败")?;
    let field = schema
        .field_with_name("vector")
        .context("向量表缺少 vector 列")?;
    match field.data_type() {
        DataType::FixedSizeList(_, size) if *size > 0 => Ok(*size as usize),
        other => bail!("向量表 vector 列类型异常：{other:?}"),
    }
}

/// 只读 `node_id` 列。
async fn indexed_node_ids(table: &Table) -> Result<HashSet<String>> {
    let batches = table
        .query()
        .select(Select::Columns(vec!["node_id".to_string()]))
        .execute()
        .await
        .context("读取向量表 node_id 失败")?
        .try_collect::<Vec<_>>()
        .await
        .context("收集向量表 node_id 失败")?;
    let mut ids = HashSet::new();
    for batch in batches {
        let Some(column) = batch
            .column_by_name("node_id")
            .and_then(|column| column.as_any().downcast_ref::<StringArray>())
        else {
            continue;
        };
        ids.extend(
            (0..column.len())
                .filter(|row| !column.is_null(*row))
                .map(|row| column.value(row).to_string()),
        );
    }
    Ok(ids)
}

/// 经 Memory 公开 API 分页列出全部活跃记忆（按 ID 去重）。
async fn list_active_nodes(provider: HandleProvider<'_>) -> HashMap<String, MemoryNode> {
    let mut nodes = HashMap::new();
    let mut offset = 0;
    loop {
        let page = provider()
            .list_nodes(MemoryListQuery {
                status: Some(MemoryStatus::Active),
                limit: LIST_PAGE,
                offset,
                ..Default::default()
            })
            .await;
        if page.is_empty() {
            break;
        }
        offset += page.len();
        let short = page.len() < LIST_PAGE;
        for node in page {
            nodes.insert(node.id.clone(), node);
        }
        if short {
            break;
        }
    }
    nodes
}

/// 按表结构构造批次并 merge 写入（`node_id` 已存在则覆盖，否则插入）。
async fn write_vectors(
    table: &Table,
    nodes: &[&MemoryNode],
    vectors: Vec<Vec<f32>>,
    dimension: usize,
) -> Result<()> {
    let schema = table.schema().await.context("读取向量表结构失败")?;
    let item_field = match schema.field_with_name("vector")?.data_type() {
        DataType::FixedSizeList(item, _) => item.clone(),
        other => bail!("向量表 vector 列类型异常：{other:?}"),
    };
    let values: Vec<f32> = vectors.into_iter().flatten().collect();
    let vector = FixedSizeListArray::try_new(
        item_field,
        dimension as i32,
        Arc::new(Float32Array::from(values)),
        None,
    )
    .context("构造向量列失败")?;
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        columns.push(build_column(field, nodes, &vector)?);
    }
    let batch = RecordBatch::try_new(schema.clone(), columns).context("构造写入批次失败")?;
    let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
    let mut merge = table.merge_insert(&["node_id"]);
    merge
        .when_matched_update_all(None)
        .when_not_matched_insert_all();
    merge
        .execute(Box::new(reader))
        .await
        .with_context(|| format!("LanceDB 写入 {} 条向量失败", nodes.len()))?;
    Ok(())
}

/// 按列名构造与 Memory 写入一致的列（`kind` 与 Memory 一样取 Debug 形式）。
fn build_column(
    field: &Field,
    nodes: &[&MemoryNode],
    vector: &FixedSizeListArray,
) -> Result<ArrayRef> {
    let column: ArrayRef = match field.name().as_str() {
        "node_id" => Arc::new(StringArray::from_iter_values(
            nodes.iter().map(|node| node.id.as_str()),
        )),
        "title" => Arc::new(StringArray::from_iter_values(
            nodes.iter().map(|node| node.title.as_str()),
        )),
        "summary" => Arc::new(StringArray::from_iter_values(
            nodes.iter().map(|node| node.summary.as_str()),
        )),
        "kind" => Arc::new(StringArray::from_iter_values(
            nodes.iter().map(|node| format!("{:?}", node.kind)),
        )),
        "importance" => Arc::new(Float32Array::from_iter_values(
            nodes.iter().map(|node| node.importance),
        )),
        "vector" => Arc::new(vector.clone()),
        other => bail!("向量表包含未知列 {other}，拒绝写入（表结构与当前版本不兼容）"),
    };
    Ok(column)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(missing: usize, repaired: usize, dry_run: bool) -> VectorRepairReport {
        VectorRepairReport {
            table: "memory_vectors_bge_m3_1024".to_string(),
            active: 10,
            missing,
            repaired,
            failed: missing - repaired,
            orphaned: 0,
            dry_run,
        }
    }

    #[test]
    fn summary_reflects_state() {
        assert!(report(0, 0, false).summary().contains("覆盖完整"));
        assert!(report(3, 0, true).summary().contains("试运行"));
        let partial = report(3, 2, false);
        assert_eq!(partial.remaining(), 1);
        assert!(partial.summary().contains("剩余 1"));
    }
}
