//! 数据迁移模块
//!
//! ⚠️ 本模块为临时过渡代码，所有用户完成迁移后应整体删除。
//!
//! 当前处理的迁移路径：
//! - Schema 列级迁移（memory_type 列补齐）
//! - SQLite memory_vectors → LanceDB 向量迁移
//!
//! 删除时同步清理：
//! - `schema.rs` 中的 `memory_vectors` 表定义和 `idx_vectors_dimension` 索引
//! - `store.rs` 中对本模块所有函数的调用

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use rusqlite::Connection;

use super::MemoryDb;
use crate::search::lancedb_search::LanceDbIndex;
use crate::search::vector::VectorIndex;
use crate::types::{MemoryKind, VectorPoint};

// ── Schema 迁移 ──────────────────────────────────────────────────

/// 执行 Schema 列级迁移（幂等）。
pub(crate) fn migrate_schema_columns(conn: &Connection) -> Result<()> {
    if let Err(err) = conn.execute(
        "ALTER TABLE memory_nodes ADD COLUMN memory_type TEXT NOT NULL DEFAULT 'factual'",
        [],
    ) {
        let message = err.to_string();
        if !message.contains("duplicate column name") {
            return Err(err.into());
        }
    }

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_nodes_memory_type ON memory_nodes(memory_type);",
    )
    .map_err(|e| anyhow!("{e}"))?;

    Ok(())
}

// ── 向量数据迁移（SQLite → LanceDB） ────────────────────────────

/// 判断是否需要从 SQLite 迁移向量数据到 LanceDB。
///
/// 条件：SQLite memory_vectors 表中存在记录，且 LanceDB 目录尚未创建。
pub(crate) fn needs_vector_migration(db: &MemoryDb, base_dir: &Path) -> bool {
    count_vectors(db).unwrap_or(0) > 0 && !base_dir.join("lancedb").exists()
}

/// 将 SQLite memory_vectors 表中的向量迁移到 LanceDB。
///
/// 迁移成功后自动清空 SQLite 向量表。
pub(crate) async fn migrate_vectors(db: &MemoryDb, index: &LanceDbIndex, dimension: usize) {
    let points = match list_vectors(db, dimension) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!("Memory 向量迁移: 读取 SQLite 向量失败，跳过: {err}");
            return;
        }
    };

    if points.is_empty() {
        return;
    }

    tracing::info!("Memory 开始迁移 {} 条向量到 LanceDB...", points.len());

    let mut migrated = 0usize;
    let mut failed = 0usize;
    for point in &points {
        match index.upsert(point.clone()).await {
            Ok(()) => migrated += 1,
            Err(err) => {
                tracing::warn!(
                    node_id = %point.node_id,
                    "Memory 向量迁移失败（跳过）: {err}"
                );
                failed += 1;
            }
        }
    }

    if migrated > 0 {
        if let Err(err) = clear_vectors(db) {
            tracing::warn!("Memory 向量迁移: 清空 SQLite 向量表失败: {err}");
        } else {
            tracing::info!("Memory 向量迁移完成: {migrated} 条成功, {failed} 条失败");
        }
    }
}

// ── SQLite 向量表访问（迁移专用） ────────────────────────────────

fn list_vectors(db: &MemoryDb, dimension: usize) -> Result<Vec<VectorPoint>> {
    let conn = db.connection();
    let mut stmt = conn.prepare(
        "SELECT node_id, title, summary, kind, importance, vector
         FROM memory_vectors
         WHERE dimension = ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![dimension as i64], |row| {
        let kind: String = row.get(3)?;
        let vector_json: String = row.get(5)?;
        let vector: Vec<f32> = serde_json::from_str(&vector_json).unwrap_or_default();
        Ok(VectorPoint {
            node_id: row.get(0)?,
            title: row.get(1)?,
            summary: row.get(2)?,
            kind: str_to_memory_kind(&kind),
            importance: row.get(4)?,
            vector,
        })
    })?;

    let mut points = Vec::new();
    for row in rows {
        points.push(row.with_context(|| "读取 memory_vectors 行失败")?);
    }
    Ok(points)
}

fn clear_vectors(db: &MemoryDb) -> Result<()> {
    db.connection()
        .execute("DELETE FROM memory_vectors", [])
        .with_context(|| "清空 memory_vectors 表失败")?;
    Ok(())
}

fn count_vectors(db: &MemoryDb) -> Result<usize> {
    let count: i64 =
        db.connection()
            .query_row("SELECT COUNT(*) FROM memory_vectors", [], |row| row.get(0))?;
    Ok(count as usize)
}

fn str_to_memory_kind(kind: &str) -> MemoryKind {
    match kind {
        "entity" => MemoryKind::Entity,
        "decision" => MemoryKind::Decision,
        "evidence" => MemoryKind::Evidence,
        _ => MemoryKind::Episode,
    }
}
