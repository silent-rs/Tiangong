//! 直接读取 Memory 向量库（LanceDB）中**已经算好**的向量。
//!
//! Memory 在写入记忆时就按配置的 Embedding 模型把向量算好并落库到
//! `<天工根目录>/data/memory/lancedb/memory_vectors_<模型>_<维度>`，
//! 整理不需要也不应该重新向量化：把表全量读一遍，本地计算相似度即可。
//!
//! 只读打开（不创建、不提交），与正在运行的 Memory 进程互不干扰；
//! 没有向量库（未配置 Embedding 或向量层关闭）时返回 `None`，
//! 由调用方退化为「记忆召回 + 文本相似度」。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use futures_util::TryStreamExt;
use lancedb::arrow::arrow_array::{Array, FixedSizeListArray, Float32Array, StringArray};
use lancedb::connect;
use lancedb::query::ExecutableQuery;

/// 向量库中读出的向量集合。
#[derive(Debug, Clone, Default)]
pub struct StoredVectors {
    /// 表名（如 `memory_vectors_bge_m3_1024`）。
    pub table: String,
    /// 向量维度。
    pub dimension: usize,
    /// node_id → 向量。
    pub vectors: HashMap<String, Vec<f32>>,
}

impl StoredVectors {
    /// 取某条记忆的向量。
    pub fn get(&self, node_id: &str) -> Option<&Vec<f32>> {
        self.vectors.get(node_id)
    }
}

/// 从 Memory 向量库目录（`<数据目录>/lancedb`）读取全部向量；没有向量库时
/// 返回 `None`。
///
/// `model_hint` 为当前配置的 Embedding 模型名（用于在多张向量表中
/// 选择与当前配置一致的那张）。
pub async fn load_stored_vectors(
    lancedb_dir: &Path,
    model_hint: Option<&str>,
) -> Result<Option<StoredVectors>> {
    let dir = lancedb_dir;
    if !dir.is_dir() {
        return Ok(None);
    }
    let db = connect(&connection_uri(dir))
        .execute()
        .await
        .with_context(|| format!("打开向量库失败：{}", dir.display()))?;
    let mut tables: Vec<String> = db
        .table_names()
        .execute()
        .await
        .context("读取向量库表列表失败")?
        .into_iter()
        .filter(|name| name.starts_with("memory_vectors_"))
        .collect();
    tables.sort();
    let Some(table_name) = choose_table(&tables, model_hint) else {
        return Ok(None);
    };
    let table = db
        .open_table(&table_name)
        .execute()
        .await
        .with_context(|| format!("打开向量表失败：{table_name}"))?;
    let total = table.count_rows(None).await.unwrap_or(0);
    let batches = table
        .query()
        .execute()
        .await
        .with_context(|| format!("扫描向量表失败：{table_name}"))?
        .try_collect::<Vec<_>>()
        .await
        .context("收集向量数据失败")?;
    let mut vectors: HashMap<String, Vec<f32>> = HashMap::new();
    let mut dimension = 0;
    for batch in &batches {
        let Some(ids) = batch
            .column_by_name("node_id")
            .and_then(|column| column.as_any().downcast_ref::<StringArray>())
        else {
            continue;
        };
        let Some(values) = batch
            .column_by_name("vector")
            .and_then(|column| column.as_any().downcast_ref::<FixedSizeListArray>())
        else {
            continue;
        };
        dimension = values.value_length() as usize;
        for row in 0..batch.num_rows() {
            if ids.is_null(row) {
                continue;
            }
            let items = values.value(row);
            let Some(items) = items.as_any().downcast_ref::<Float32Array>() else {
                continue;
            };
            let vector: Vec<f32> = (0..items.len())
                .filter(|index| !items.is_null(*index))
                .map(|index| items.value(index))
                .collect();
            if vector.len() != dimension {
                continue;
            }
            vectors.insert(ids.value(row).to_string(), vector);
        }
    }
    if total > 0 && vectors.len() < total {
        tracing::warn!(
            table = %table_name,
            rows = total,
            loaded = vectors.len(),
            "向量表行数与读出的向量数不一致（可能含维度不符的历史行）"
        );
    }
    if vectors.is_empty() {
        return Ok(None);
    }
    Ok(Some(StoredVectors {
        table: table_name,
        dimension,
        vectors,
    }))
}

/// 选择向量表：优先与当前配置模型一致的表，否则取排序最后一张。
fn choose_table(tables: &[String], model_hint: Option<&str>) -> Option<String> {
    if let Some(hint) = model_hint.map(str::trim).filter(|hint| !hint.is_empty()) {
        let slug = slug(hint);
        if let Some(name) = tables.iter().find(|name| name.contains(&slug)) {
            return Some(name.clone());
        }
    }
    tables.last().cloned()
}

/// 模型名转表名片段（与 Memory 的 `VectorIdentity::table_name` 一致）。
fn slug(model: &str) -> String {
    model
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

/// LanceDB 连接 URI：非 Windows 直接用路径，Windows 用 file:// URL。
#[cfg(not(windows))]
pub(crate) fn connection_uri(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg(windows)]
pub(crate) fn connection_uri(path: &Path) -> String {
    format!("file:///{}", path.to_string_lossy().replace('\\', "/"))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_memory_table_naming() {
        assert_eq!(slug("bge-m3"), "bge_m3");
        assert_eq!(slug("text-embedding-3-small"), "text_embedding_3_small");
        assert_eq!(slug("BAAI/bge-m3"), "BAAI_bge_m3");
    }

    #[test]
    fn choose_table_prefers_configured_model() {
        let tables = vec![
            "memory_vectors_bge_small_zh_v1_5_512".to_string(),
            "memory_vectors_bge_m3_1024".to_string(),
        ];
        let chosen = choose_table(&tables, Some("bge-m3")).expect("应选中");
        assert_eq!(chosen, "memory_vectors_bge_m3_1024");
        let fallback = choose_table(&tables, Some("unknown-model")).expect("应回落");
        assert_eq!(fallback, "memory_vectors_bge_m3_1024", "无匹配时取最后一张");
        assert!(choose_table(&[], Some("bge-m3")).is_none());
    }
}
