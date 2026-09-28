//! 嵌入式 LanceDB 向量索引
//!
//! 使用 lancedb 在进程内提供向量检索，无需外部服务。

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use lancedb::arrow::arrow_array::{
    Array, FixedSizeListArray, Float32Array, RecordBatch, StringArray, builder::Float32Builder,
};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, SchemaRef};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{DistanceType, Table, connect};

use crate::search::vector::VectorIndex;
use crate::search::vector_meta::{
    LEGACY_TABLE_NAME, LEGACY_UNKNOWN_MODEL, VectorIdentity, VectorIndexMeta, VectorTableMeta,
    meta_path,
};
use crate::types::{MemoryKind, RecallHit, VectorPoint};

pub(crate) struct LanceDbIndex {
    table: Table,
    dimension: usize,
    schema: SchemaRef,
}

/// 打开后的索引状态。
#[derive(Debug, Clone)]
pub(crate) struct VectorTableState {
    pub(crate) fingerprint: String,
    /// 回填已完成，可用于语义召回。
    pub(crate) complete: bool,
    /// 回填续传游标。
    pub(crate) cursor: Option<String>,
    pub(crate) meta_path: std::path::PathBuf,
}

impl LanceDbIndex {
    /// 按模型身份打开（或创建）对应的向量表。
    ///
    /// - 旧版无元数据的 `memory_vectors` 表：维度与当前模型一致时直接接管为当前指纹
    ///   （不重算）；不一致时登记为未知模型的上一代表保留。
    /// - 新指纹：建表并标记未完成，由调用方后台回填。
    /// - 只保留当前与上一代两张表，更早的表被删除。
    pub(crate) async fn open(
        base_dir: &Path,
        identity: &VectorIdentity,
    ) -> Result<(Self, VectorTableState)> {
        let lancedb_path = base_dir.join("lancedb");
        std::fs::create_dir_all(&lancedb_path)
            .with_context(|| format!("创建 LanceDB 目录失败: {}", lancedb_path.display()))?;

        // 自愈旧版 LanceDB 遗留的 namespace manifest：旧版会把 auto_cleanup
        // 配置写进 __manifest，与 lance 8.0.0 不兼容（触发 debug_assert，
        // 且 release 下可能令 namespace 刷新逻辑不一致）。检测到即删除整个
        // __manifest，由当前版本重建一个干净的；向量数据 memory_vectors.lance
        // 不受影响。失败不中断启动，交由上层降级为 BM25-only。
        if let Err(err) = heal_legacy_manifest(&lancedb_path) {
            tracing::warn!("Memory LanceDB __manifest 自愈失败，继续尝试打开: {err}");
        }

        let connection_uri = lancedb_connection_uri(&lancedb_path)?;
        let db = connect(&connection_uri)
            .execute()
            .await
            .with_context(|| "连接 LanceDB 失败")?;

        let dimension = identity.dimension;
        let schema = build_schema(dimension);
        let fingerprint = identity.fingerprint();
        let meta_file = meta_path(&lancedb_path);
        let mut meta = VectorIndexMeta::load(&meta_file).unwrap_or_else(|err| {
            tracing::warn!("Memory 向量索引元数据损坏，按空元数据重建: {err}");
            VectorIndexMeta::default()
        });

        let table_names = db
            .table_names()
            .execute()
            .await
            .with_context(|| "获取 LanceDB 表列表失败")?;

        // 旧版单表（无元数据）无法确认生成它的模型：维度相同也可能来自不同
        // 模型，语义空间不通用，直接当成当前模型的索引会让查询向量与存量向量
        // 不匹配，且不会再触发回填。因此一律登记为来源未知的上一代，当前指纹
        // 另建新表并回填；旧表保留（不删），回填完成前语义召回降级。
        let legacy_registered = meta
            .tables
            .values()
            .any(|entry| entry.table == LEGACY_TABLE_NAME);
        if !legacy_registered && table_names.iter().any(|name| name == LEGACY_TABLE_NAME) {
            let legacy = db
                .open_table(LEGACY_TABLE_NAME)
                .execute()
                .await
                .with_context(|| "打开旧版 LanceDB 表失败")?;
            let legacy_dimension = vector_dimension(&legacy).await;
            let legacy_identity =
                VectorIdentity::new(LEGACY_UNKNOWN_MODEL, legacy_dimension.unwrap_or(0));
            tracing::info!(
                legacy_dimension = ?legacy_dimension,
                dimension,
                fingerprint = %fingerprint,
                "Memory 发现旧版向量表但无法确认来源模型，保留为上一代并为当前模型新建索引"
            );
            meta.tables
                .entry(legacy_identity.fingerprint())
                .or_insert_with(|| {
                    VectorTableMeta::new(LEGACY_TABLE_NAME.to_string(), &legacy_identity, true)
                });
            meta.activate(&legacy_identity.fingerprint());
        }

        let entry = meta
            .tables
            .entry(fingerprint.clone())
            .or_insert_with(|| VectorTableMeta::new(identity.table_name(), identity, false))
            .clone();

        let table = if table_names.contains(&entry.table) {
            db.open_table(&entry.table)
                .execute()
                .await
                .with_context(|| format!("打开 LanceDB 表失败: {}", entry.table))?
        } else {
            db.create_empty_table(&entry.table, schema.clone())
                .execute()
                .await
                .with_context(|| format!("创建 LanceDB 表失败: {}", entry.table))?
        };

        for stale in meta.activate(&fingerprint) {
            match db.drop_table(&stale, &[]).await {
                Ok(()) => {
                    meta.mark_dropped(&stale);
                    tracing::info!(table = %stale, "Memory 已清理过期向量表");
                }
                // 删除失败保留在 pending_drop 中，下次打开时继续重试。
                Err(err) => tracing::warn!(table = %stale, "Memory 清理过期向量表失败: {err}"),
            }
        }
        meta.save(&meta_file)?;

        Ok((
            Self {
                table,
                dimension,
                schema,
            },
            VectorTableState {
                fingerprint,
                complete: entry.complete,
                cursor: entry.cursor,
                meta_path: meta_file,
            },
        ))
    }
}

/// 读取表中 vector 列的维度。
async fn vector_dimension(table: &Table) -> Option<usize> {
    let schema = table.schema().await.ok()?;
    match schema.field_with_name("vector").ok()?.data_type() {
        DataType::FixedSizeList(_, size) => usize::try_from(*size).ok(),
        _ => None,
    }
}

#[cfg(windows)]
fn lancedb_connection_uri(path: &Path) -> Result<String> {
    url::Url::from_directory_path(path)
        .map(String::from)
        .map_err(|_| anyhow::anyhow!("LanceDB 目录无法转换为 file URI: {}", path.display()))
}

#[cfg(not(windows))]
fn lancedb_connection_uri(path: &Path) -> Result<String> {
    Ok(path.to_string_lossy().to_string())
}

#[async_trait(?Send)]
impl VectorIndex for LanceDbIndex {
    async fn ensure_ready(&self) -> Result<()> {
        Ok(())
    }

    async fn upsert(&self, point: VectorPoint) -> Result<()> {
        if point.vector.len() != self.dimension {
            bail!(
                "LanceDB 向量维度不匹配: expected={} actual={}",
                self.dimension,
                point.vector.len()
            );
        }

        // LanceDB 没有原生 upsert，先删后增
        self.table
            .delete(&format!("node_id = '{}'", point.node_id))
            .await
            .ok(); // 忽略不存在的删除

        let batch = vector_point_to_batch(&point, &self.schema, self.dimension)?;
        self.table
            .add(batch)
            .execute()
            .await
            .with_context(|| "LanceDB 写入向量失败")?;

        Ok(())
    }

    async fn search(&self, query_vector: Vec<f32>, limit: usize) -> Result<Vec<RecallHit>> {
        if query_vector.len() != self.dimension {
            bail!(
                "LanceDB 查询向量维度不匹配: expected={} actual={}",
                self.dimension,
                query_vector.len()
            );
        }

        let batches = self
            .table
            .query()
            .nearest_to(query_vector)?
            .distance_type(DistanceType::Cosine)
            .limit(limit)
            .execute()
            .await
            .with_context(|| "LanceDB 向量搜索失败")?
            .try_collect::<Vec<_>>()
            .await
            .with_context(|| "LanceDB 搜索结果收集失败")?;

        Ok(record_batches_to_recall_hits(batches))
    }

    async fn delete(&self, node_id: &str) -> Result<()> {
        self.table
            .delete(&format!("node_id = '{node_id}'"))
            .await
            .with_context(|| format!("LanceDB 删除节点失败: {node_id}"))?;
        Ok(())
    }
}

fn build_schema(dimension: usize) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("node_id", DataType::Utf8, false),
        Field::new("title", DataType::Utf8, false),
        Field::new("summary", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("importance", DataType::Float32, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dimension as i32,
            ),
            true,
        ),
    ]))
}

fn vector_point_to_batch(
    point: &VectorPoint,
    schema: &Schema,
    dimension: usize,
) -> Result<RecordBatch> {
    let mut vec_builder = Float32Builder::with_capacity(dimension);
    for &val in &point.vector {
        vec_builder.append_value(val);
    }
    let values = vec_builder.finish();
    let vector = FixedSizeListArray::new(
        Arc::new(Field::new("item", DataType::Float32, true)),
        dimension as i32,
        Arc::new(values),
        None,
    );

    Ok(RecordBatch::try_new(
        Arc::new(schema.clone()),
        vec![
            Arc::new(StringArray::from(vec![point.node_id.clone()])),
            Arc::new(StringArray::from(vec![point.title.clone()])),
            Arc::new(StringArray::from(vec![point.summary.clone()])),
            Arc::new(StringArray::from(vec![format!("{:?}", point.kind)])),
            Arc::new(Float32Array::from(vec![point.importance as f32])),
            Arc::new(vector),
        ],
    )?)
}

fn record_batches_to_recall_hits(batches: Vec<RecordBatch>) -> Vec<RecallHit> {
    let mut hits = Vec::new();
    for batch in batches {
        let Some(node_ids) = batch
            .column_by_name("node_id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        else {
            continue;
        };
        let titles = batch
            .column_by_name("title")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let summaries = batch
            .column_by_name("summary")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let kinds = batch
            .column_by_name("kind")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let importances = batch
            .column_by_name("importance")
            .and_then(|c| c.as_any().downcast_ref::<Float32Array>());
        let distances = batch
            .column_by_name("_distance")
            .and_then(|c| c.as_any().downcast_ref::<Float32Array>());

        for i in 0..node_ids.len() {
            let distance: f32 = distances.map(|d| d.value(i)).unwrap_or(0.0);
            let score = (1.0 - distance / 2.0).max(0.0_f32) as f64;
            hits.push(RecallHit {
                node_id: node_ids.value(i).to_string(),
                title: titles.map(|t| t.value(i).to_string()).unwrap_or_default(),
                summary: summaries
                    .map(|s| s.value(i).to_string())
                    .unwrap_or_default(),
                score,
                kind: kinds
                    .map(|k| parse_kind(k.value(i)))
                    .unwrap_or(MemoryKind::Episode),
                importance: importances.map(|imp| imp.value(i) as f64).unwrap_or(0.5),
                depth1_loaded: false,
            });
        }
    }
    hits
}

fn parse_kind(s: &str) -> MemoryKind {
    match s.to_lowercase().as_str() {
        "entity" => MemoryKind::Entity,
        "decision" => MemoryKind::Decision,
        "evidence" => MemoryKind::Evidence,
        _ => MemoryKind::Episode,
    }
}

/// LanceDB namespace manifest 配置键前缀。
///
/// lance 8.0.0 要求 namespace 内部的 `__manifest` 管理数据集**不得**启用
/// old-version cleanup（见 `lance-namespace-impls` 的 `DatasetConsistencyWrapper`
/// 构造断言）。旧版 LanceDB 创建 `__manifest` 时会写入这些键，升级后即不兼容。
const LEGACY_AUTO_CLEANUP_MARKER: &[u8] = b"lance.auto_cleanup.";

/// 扫描并自愈旧版 LanceDB 遗留的 namespace manifest。
///
/// 检测到 `__manifest` 的任意版本文件包含 `lance.auto_cleanup.` 配置时，
/// 删除整个 `__manifest` 目录，交由当前版本重建。读取/判断/删除均为纯字节
/// 操作，不依赖 lance 内部结构，对未来格式变动鲁棒。
fn heal_legacy_manifest(lancedb_dir: &Path) -> Result<()> {
    let manifest_dir = lancedb_dir.join("__manifest");
    if !manifest_dir.is_dir() {
        return Ok(());
    }

    // manifest 版本文件位于 _versions/ 下；兼容个别布局直接放在 __manifest/ 根。
    let versions_dir = manifest_dir.join("_versions");
    let scan_dirs = [versions_dir.as_path(), manifest_dir.as_path()];

    let mut tainted = false;
    for dir in scan_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("manifest") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if bytes
                .windows(LEGACY_AUTO_CLEANUP_MARKER.len())
                .any(|w| w == LEGACY_AUTO_CLEANUP_MARKER)
            {
                tainted = true;
                break;
            }
        }
        if tainted {
            break;
        }
    }

    if tainted {
        tracing::warn!(
            "Memory 检测到旧版 LanceDB __manifest 含 auto_cleanup 配置（与 lance 8.0.0 不兼容），正在删除以重建"
        );
        std::fs::remove_dir_all(&manifest_dir)
            .with_context(|| format!("删除遗留 __manifest 失败: {}", manifest_dir.display()))?;
        tracing::info!("Memory LanceDB __manifest 已清理，将由当前版本重建");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// 在 `root` 下写入一个 `__manifest` 版本文件，返回其路径。
    fn write_manifest(root: &Path, name: &str, content: &[u8]) -> PathBuf {
        let dir = root.join("__manifest").join("_versions");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.manifest"));
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn clean_manifest_is_not_touched() {
        let tmp = TempDir::new().unwrap();
        let path = write_manifest(tmp.path(), "1", b"plain manifest without cleanup config");
        // 不含 auto_cleanup 标记时不应删除。
        heal_legacy_manifest(tmp.path()).unwrap();
        assert!(path.exists(), "干净的 manifest 不应被删除");
        assert!(tmp.path().join("__manifest").exists());
    }

    #[test]
    fn dirty_manifest_is_removed() {
        let tmp = TempDir::new().unwrap();
        let path = write_manifest(
            tmp.path(),
            "1",
            b"some bytes lance.auto_cleanup.older_than 14days trailing",
        );
        // 命中标记时应删除整个 __manifest。
        heal_legacy_manifest(tmp.path()).unwrap();
        assert!(!path.exists(), "受污染的 manifest 应被删除");
        assert!(
            !tmp.path().join("__manifest").exists(),
            "整个 __manifest 目录应被删除"
        );
    }

    #[test]
    fn no_manifest_is_noop() {
        let tmp = TempDir::new().unwrap();
        // 无 __manifest 时应正常返回、无副作用。
        heal_legacy_manifest(tmp.path()).unwrap();
        assert!(!tmp.path().join("__manifest").exists());
    }

    #[test]
    fn any_tainted_file_triggers_removal() {
        let tmp = TempDir::new().unwrap();
        let clean = write_manifest(tmp.path(), "1", b"clean content");
        // 第二个文件命中标记，即便第一个干净也应整体删除。
        write_manifest(tmp.path(), "2", b"xxx lance.auto_cleanup.interval 20 yyy");
        heal_legacy_manifest(tmp.path()).unwrap();
        assert!(!clean.exists());
        assert!(!tmp.path().join("__manifest").exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_lancedb_path_uses_file_uri() {
        let uri = lancedb_connection_uri(Path::new(r"C:\Users\Test User\.tiangong\memory\lancedb"))
            .unwrap();
        assert!(uri.starts_with("file:///C:/"), "unexpected URI: {uri}");
        assert!(uri.contains("Test%20User"), "unexpected URI: {uri}");
    }
}
