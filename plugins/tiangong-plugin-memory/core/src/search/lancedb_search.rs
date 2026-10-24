//! 嵌入式 LanceDB 向量索引
//!
//! 使用 lancedb 在进程内提供向量检索，无需外部服务。

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use lancedb::arrow::arrow_array::{
    Array, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator, StringArray,
    builder::Float32Builder,
};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, SchemaRef};
use lancedb::database::CreateTableMode;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::{Connection, DistanceType, Table, connect};

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

        // 上次重建中断时原表可能已被删除，必须先恢复再判断是否需要建空表，
        // 否则会用空表顶替、丢失已有向量。
        if let Err(err) = recover_interrupted_rebuild(&db, &entry.table).await {
            tracing::warn!(table = %entry.table, "Memory LanceDB 恢复中断的重建失败: {err:#}");
        }
        let table_names = db
            .table_names()
            .execute()
            .await
            .with_context(|| "获取 LanceDB 表列表失败")?;
        let table = if table_names.contains(&entry.table) {
            // 回收旧版逐条写入遗留的历史版本。失败只告警：不影响本次打开，
            // 下次启动会再次尝试。
            if let Err(err) = reclaim_history(&db, &entry.table).await {
                tracing::warn!(table = %entry.table, "Memory LanceDB 历史版本回收失败: {err:#}");
            }
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

        let stale_tables = meta.activate(&fingerprint);
        // 上一代表保留以便切回模型，平时不会被打开，膨胀只能在这里一并回收。
        let previous_table = meta
            .previous
            .as_ref()
            .and_then(|key| meta.tables.get(key))
            .map(|entry| entry.table.clone())
            .filter(|name| {
                table_names.contains(name) || table_names.contains(&rebuild_table_name(name))
            });
        if let Some(name) = previous_table
            && let Err(err) = reclaim_history(&db, &name).await
        {
            tracing::warn!(table = %name, "Memory LanceDB 历史版本回收失败: {err:#}");
        }

        for stale in stale_tables {
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

/// 触发回收的表版本号阈值。
///
/// 旧版逐条 delete + add 每条向量提交两个版本且从不回收（实测单表 3.6 万
/// 版本、27 GB manifest）。merge_insert 下每批只产生一个版本，正常使用很久
/// 才会达到该量级；重建后版本号从头计数，因此用当前版本号即可判定膨胀，
/// 无需 `list_versions`（它会逐个解析全部 manifest，代价与回收本身相当）。
const RECLAIM_VERSION_THRESHOLD: u64 = if cfg!(test) { 32 } else { 1024 };

/// 重建过程中暂存数据的临时表名。
fn rebuild_table_name(name: &str) -> String {
    format!("{name}__rebuild")
}

/// 一次性回收存量膨胀：版本号达到阈值时按原名重建表，只保留当前数据。
///
/// 不使用 `optimize(Prune)`：它会逐个读取全部历史 manifest，真实数据
/// （3.6 万个、平均 770 KB）实测超过 10 分钟仍未完成。重建只读当前版本：
/// 读出全部行 → 写入临时表 → 删除旧表（整目录删除）→ 按原名写回 → 删除
/// 临时表。任一步骤中断时，[`recover_interrupted_rebuild`] 在下次打开时
/// 用临时表恢复。只在 Leader 进程调用（同一存储根下只有 Leader 持有
/// LanceDB），不存在并发写入方。
async fn reclaim_history(db: &Connection, name: &str) -> Result<()> {
    recover_interrupted_rebuild(db, name).await?;
    let result = rebuild_if_bloated(db, name).await;
    if result.is_err()
        && let Err(err) = recover_interrupted_rebuild(db, name).await
    {
        tracing::warn!(table = %name, "Memory LanceDB 重建失败后恢复失败: {err:#}");
    }
    result
}

async fn rebuild_if_bloated(db: &Connection, name: &str) -> Result<()> {
    let table = db
        .open_table(name)
        .execute()
        .await
        .with_context(|| format!("打开 LanceDB 表失败: {name}"))?;
    let version = table.version().await.context("读取 LanceDB 表版本失败")?;
    if version < RECLAIM_VERSION_THRESHOLD {
        return Ok(());
    }
    let started = std::time::Instant::now();
    tracing::info!(table = %name, version, "Memory LanceDB 历史版本过多，开始重建回收");
    let schema = table.schema().await.context("读取 LanceDB 表结构失败")?;
    let rows = scan_all(&table).await?;
    drop(table);
    let expected: usize = rows.iter().map(RecordBatch::num_rows).sum();

    let tmp = rebuild_table_name(name);
    write_table(db, &tmp, &schema, rows.clone(), expected).await?;
    db.drop_table(name, &[])
        .await
        .with_context(|| format!("删除膨胀的 LanceDB 表失败: {name}"))?;
    write_table(db, name, &schema, rows, expected).await?;
    db.drop_table(&tmp, &[])
        .await
        .with_context(|| format!("删除 LanceDB 临时表失败: {tmp}"))?;
    tracing::info!(
        table = %name,
        version_before = version,
        rows = expected,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "Memory LanceDB 历史版本回收完成"
    );
    Ok(())
}

/// 处理上次重建中断留下的临时表：原表完好则丢弃临时表，否则用临时表恢复原表。
async fn recover_interrupted_rebuild(db: &Connection, name: &str) -> Result<()> {
    let tmp = rebuild_table_name(name);
    let names = db
        .table_names()
        .execute()
        .await
        .context("获取 LanceDB 表列表失败")?;
    if !names.contains(&tmp) {
        return Ok(());
    }
    let original_exists = names.iter().any(|n| n == name);
    let original_intact = original_exists && db.open_table(name).execute().await.is_ok();
    if !original_intact {
        tracing::warn!(table = %name, "Memory LanceDB 上次重建中断，从临时表恢复");
        let staged = db
            .open_table(&tmp)
            .execute()
            .await
            .with_context(|| format!("打开 LanceDB 临时表失败: {tmp}"))?;
        let schema = staged
            .schema()
            .await
            .context("读取 LanceDB 临时表结构失败")?;
        let rows = scan_all(&staged).await?;
        drop(staged);
        let expected = rows.iter().map(RecordBatch::num_rows).sum();
        if original_exists {
            db.drop_table(name, &[])
                .await
                .with_context(|| format!("删除损坏的 LanceDB 表失败: {name}"))?;
        }
        write_table(db, name, &schema, rows, expected).await?;
    }
    db.drop_table(&tmp, &[])
        .await
        .with_context(|| format!("删除 LanceDB 临时表失败: {tmp}"))
}

async fn scan_all(table: &Table) -> Result<Vec<RecordBatch>> {
    table
        .query()
        .execute()
        .await
        .with_context(|| format!("读取 LanceDB 表失败: {}", table.name()))?
        .try_collect::<Vec<_>>()
        .await
        .with_context(|| format!("读取 LanceDB 表数据失败: {}", table.name()))
}

/// 以覆盖模式一次提交写入整表，并校验行数。
async fn write_table(
    db: &Connection,
    name: &str,
    schema: &SchemaRef,
    mut rows: Vec<RecordBatch>,
    expected: usize,
) -> Result<()> {
    rows.retain(|batch| batch.num_rows() > 0);
    if rows.is_empty() {
        rows.push(RecordBatch::new_empty(schema.clone()));
    }
    let table = db
        .create_table(name, rows)
        .mode(CreateTableMode::Overwrite)
        .execute()
        .await
        .with_context(|| format!("写入 LanceDB 表失败: {name}"))?;
    let actual = table
        .count_rows(None)
        .await
        .with_context(|| format!("校验 LanceDB 表行数失败: {name}"))?;
    if actual != expected {
        bail!("LanceDB 表 {name} 行数校验失败: expected={expected} actual={actual}");
    }
    Ok(())
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

    async fn upsert_batch(&self, points: Vec<VectorPoint>) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        if let Some(point) = points.iter().find(|p| p.vector.len() != self.dimension) {
            bail!(
                "LanceDB 向量维度不匹配: expected={} actual={} node_id={}",
                self.dimension,
                point.vector.len(),
                point.node_id
            );
        }

        // merge_insert 一次提交只产生一个表版本；旧实现逐条 delete + add，
        // 每条向量提交两个版本且从不回收，manifest 随版本数平方级膨胀。
        let batch = vector_points_to_batch(&points, &self.schema, self.dimension)?;
        let reader = RecordBatchIterator::new(vec![Ok(batch)], self.schema.clone());
        let mut merge = self.table.merge_insert(&["node_id"]);
        merge
            .when_matched_update_all(None)
            .when_not_matched_insert_all();
        merge
            .execute(Box::new(reader))
            .await
            .with_context(|| format!("LanceDB 写入 {} 条向量失败", points.len()))?;
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

    async fn list_node_ids(&self) -> Result<Vec<String>> {
        let batches = self
            .table
            .query()
            .select(Select::Columns(vec!["node_id".to_string()]))
            .execute()
            .await
            .with_context(|| "LanceDB 读取节点 ID 失败")?
            .try_collect::<Vec<_>>()
            .await
            .with_context(|| "LanceDB 节点 ID 收集失败")?;
        let mut ids = Vec::new();
        for batch in batches {
            let Some(column) = batch
                .column_by_name("node_id")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
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

fn vector_points_to_batch(
    points: &[VectorPoint],
    schema: &SchemaRef,
    dimension: usize,
) -> Result<RecordBatch> {
    let mut vec_builder = Float32Builder::with_capacity(dimension * points.len());
    for point in points {
        vec_builder.append_slice(&point.vector);
    }
    let values = vec_builder.finish();
    let vector = FixedSizeListArray::new(
        Arc::new(Field::new("item", DataType::Float32, true)),
        dimension as i32,
        Arc::new(values),
        None,
    );

    Ok(RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from_iter_values(
                points.iter().map(|p| p.node_id.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                points.iter().map(|p| p.title.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                points.iter().map(|p| p.summary.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                points.iter().map(|p| format!("{:?}", p.kind)),
            )),
            Arc::new(Float32Array::from_iter_values(
                points.iter().map(|p| p.importance as f32),
            )),
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

    const DIM: usize = 4;

    fn point(node_id: &str, title: &str, seed: f32) -> VectorPoint {
        VectorPoint {
            node_id: node_id.to_string(),
            title: title.to_string(),
            summary: format!("{title} 摘要"),
            kind: MemoryKind::Episode,
            importance: 0.5,
            vector: vec![seed, 1.0, 0.0, 0.0],
        }
    }

    async fn open_index(base: &Path, model: &str) -> LanceDbIndex {
        LanceDbIndex::open(base, &VectorIdentity::new(model, DIM))
            .await
            .expect("打开 LanceDB 索引")
            .0
    }

    async fn version_count(index: &LanceDbIndex) -> usize {
        index.table.list_versions().await.unwrap().len()
    }

    async fn titles_by_node(index: &LanceDbIndex) -> std::collections::BTreeMap<String, String> {
        let batches = index
            .table
            .query()
            .execute()
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let mut rows = std::collections::BTreeMap::new();
        for batch in batches {
            let ids = batch
                .column_by_name("node_id")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .clone();
            let titles = batch
                .column_by_name("title")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .clone();
            for i in 0..ids.len() {
                rows.insert(ids.value(i).to_string(), titles.value(i).to_string());
            }
        }
        rows
    }

    /// 一批写入只产生一个表版本；重复 node_id 覆盖原行而不是追加。
    #[tokio::test]
    async fn upsert_batch_commits_once_and_overwrites_by_node_id() {
        let tmp = TempDir::new().unwrap();
        let index = open_index(tmp.path(), "test-model").await;
        let before = version_count(&index).await;

        index
            .upsert_batch(vec![
                point("a", "甲", 0.1),
                point("b", "乙", 0.2),
                point("c", "丙", 0.3),
            ])
            .await
            .unwrap();
        assert_eq!(
            version_count(&index).await,
            before + 1,
            "整批只提交一个版本"
        );

        index
            .upsert_batch(vec![point("b", "乙-更新", 0.4), point("d", "丁", 0.5)])
            .await
            .unwrap();
        assert_eq!(version_count(&index).await, before + 2);

        let rows = titles_by_node(&index).await;
        assert_eq!(rows.len(), 4, "覆盖写不应产生重复行: {rows:?}");
        assert_eq!(rows["b"], "乙-更新");
        assert_eq!(rows["a"], "甲");

        let hits = index.search(vec![0.4, 1.0, 0.0, 0.0], 1).await.unwrap();
        assert_eq!(hits[0].node_id, "b", "更新后的向量参与检索");
    }

    /// `list_node_ids` 只读 node_id 列，覆盖写与删除后结果与表内实际一致。
    #[tokio::test]
    async fn list_node_ids_reflects_upserts_and_deletes() {
        let tmp = TempDir::new().unwrap();
        let index = open_index(tmp.path(), "test-model").await;
        assert!(index.list_node_ids().await.unwrap().is_empty());

        index
            .upsert_batch(vec![point("a", "甲", 0.1), point("b", "乙", 0.2)])
            .await
            .unwrap();
        index
            .upsert_batch(vec![point("b", "乙-更新", 0.3), point("c", "丙", 0.4)])
            .await
            .unwrap();
        index.delete("a").await.unwrap();

        let mut ids = index.list_node_ids().await.unwrap();
        ids.sort();
        assert_eq!(ids, vec!["b".to_string(), "c".to_string()]);
    }

    #[tokio::test]
    async fn upsert_batch_rejects_dimension_mismatch_and_ignores_empty() {
        let tmp = TempDir::new().unwrap();
        let index = open_index(tmp.path(), "test-model").await;
        let before = version_count(&index).await;
        index.upsert_batch(Vec::new()).await.unwrap();
        assert_eq!(version_count(&index).await, before, "空批次不提交");

        let mut bad = point("x", "坏", 0.1);
        bad.vector.push(9.0);
        assert!(index.upsert_batch(vec![bad]).await.is_err());
        assert_eq!(version_count(&index).await, before, "校验失败不提交");
    }

    /// 模拟旧版逐条 delete + add 遗留的版本膨胀：重新打开后一次性回收，
    /// 只保留当前版本且数据完整；上一代表同样被回收。
    #[tokio::test]
    async fn reopen_reclaims_legacy_version_history() {
        let tmp = TempDir::new().unwrap();
        let rounds = RECLAIM_VERSION_THRESHOLD as usize;
        {
            let legacy = open_index(tmp.path(), "old-model").await;
            for i in 0..rounds {
                let id = format!("n{}", i % 8);
                legacy
                    .table
                    .delete(&format!("node_id = '{id}'"))
                    .await
                    .unwrap();
                let batch =
                    vector_points_to_batch(&[point(&id, &id, i as f32)], &legacy.schema, DIM)
                        .unwrap();
                legacy.table.add(batch).execute().await.unwrap();
            }
            assert!(
                version_count(&legacy).await > rounds,
                "构造出膨胀的版本历史"
            );
        }
        {
            let current = open_index(tmp.path(), "new-model").await;
            for i in 0..rounds {
                let id = format!("m{}", i % 8);
                current
                    .table
                    .delete(&format!("node_id = '{id}'"))
                    .await
                    .unwrap();
                let batch =
                    vector_points_to_batch(&[point(&id, &id, i as f32)], &current.schema, DIM)
                        .unwrap();
                current.table.add(batch).execute().await.unwrap();
            }
        }

        // 重新打开 new-model：当前表与上一代 old-model 表都应被回收。
        let reopened = open_index(tmp.path(), "new-model").await;
        assert!(
            version_count(&reopened).await <= 2,
            "当前表历史版本应已回收，实际 {}",
            version_count(&reopened).await
        );
        assert_eq!(titles_by_node(&reopened).await.len(), 8, "回收不丢数据");

        let previous = open_index(tmp.path(), "old-model").await;
        assert!(
            version_count(&previous).await <= 2,
            "上一代表历史版本应已回收，实际 {}",
            version_count(&previous).await
        );
        assert_eq!(titles_by_node(&previous).await.len(), 8);
        assert!(
            !tmp.path()
                .join("lancedb")
                .join(format!(
                    "{}.lance",
                    rebuild_table_name(&VectorIdentity::new("new-model", DIM).table_name())
                ))
                .exists(),
            "重建完成后不残留临时表"
        );
    }

    /// 重建在删除原表后中断：下次打开从临时表恢复原表，数据不丢。
    #[tokio::test]
    async fn interrupted_rebuild_is_recovered_from_staging_table() {
        let tmp = TempDir::new().unwrap();
        let name = VectorIdentity::new("test-model", DIM).table_name();
        {
            let index = open_index(tmp.path(), "test-model").await;
            index
                .upsert_batch(vec![point("a", "甲", 0.1), point("b", "乙", 0.2)])
                .await
                .unwrap();
        }
        // 模拟中断现场：数据已写入临时表，原表已删除。
        {
            let db = connect(tmp.path().join("lancedb").to_str().unwrap())
                .execute()
                .await
                .unwrap();
            let table = db.open_table(&name).execute().await.unwrap();
            let schema = table.schema().await.unwrap();
            let rows = scan_all(&table).await.unwrap();
            drop(table);
            write_table(&db, &rebuild_table_name(&name), &schema, rows, 2)
                .await
                .unwrap();
            db.drop_table(&name, &[]).await.unwrap();
        }
        // 元数据仍登记原表，打开时应先恢复再使用。
        let reopened = open_index(tmp.path(), "test-model").await;
        let rows = titles_by_node(&reopened).await;
        assert_eq!(rows.len(), 2, "从临时表恢复全部数据: {rows:?}");
        let db = connect(tmp.path().join("lancedb").to_str().unwrap())
            .execute()
            .await
            .unwrap();
        let names = db.table_names().execute().await.unwrap();
        assert!(
            !names.contains(&rebuild_table_name(&name)),
            "恢复后清理临时表"
        );
    }

    /// 版本数未达阈值时不触发回收（正常 merge_insert 写入不应反复整理）。
    #[tokio::test]
    async fn reclaim_skips_tables_below_threshold() {
        let tmp = TempDir::new().unwrap();
        {
            let index = open_index(tmp.path(), "test-model").await;
            for i in 0..4 {
                index
                    .upsert_batch(vec![point(&format!("n{i}"), "x", i as f32)])
                    .await
                    .unwrap();
            }
        }
        let reopened = open_index(tmp.path(), "test-model").await;
        assert!(version_count(&reopened).await >= 5, "未达阈值保留版本历史");
    }
}
