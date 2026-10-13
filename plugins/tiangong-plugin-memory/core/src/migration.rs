//! Memory 旧数据迁移（插件自身行为）。
//!
//! 早期版本把全部数据放在存储根下的 `memory/`；沙箱收拢写域后存储根只读，
//! 数据改放插件自有数据目录 `data/memory/`。首次启动时把旧目录逐项复制进
//! 新目录并写完成标记，之后直接跳过（幂等）。旧目录原样保留，由用户确认
//! 无误后自行清理。
//!
//! 内置模型（`models/`，约 1.2G）不在启动时迁移：整树复制在非 APFS 文件系统
//! 上耗时可能超过 sidecar 启动超时，被中断后会反复重来。模型改由下载流程
//! 按需获取——先尝试从旧目录逐文件复制并做 sha256 校验，复制失败（如 Windows
//! 沙箱无旧目录读权限）或校验不符时自动走网络下载（见
//! [`crate::local_model`] 的下载模块）。
//!
//! 安全约束：
//! - 旧目录仍有存活 Leader（旧版本进程还在写库）时拒绝迁移，避免复制到写
//!   了一半的 SQLite / LanceDB；
//! - 新目录已有 `metadata.db` 时不合并（两份库无法安全合并），只记录告警
//!   并写标记，旧目录保留供人工处理；
//! - 每个顶层条目先复制到同级临时名再原子改名；全部成功后才写标记，中途
//!   失败不写标记，调用方必须中止启动，下次启动继续补齐。
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// 旧数据迁移完成标记文件名（位于新数据目录内）。
const LEGACY_MIGRATION_MARKER: &str = ".legacy-migrated";
/// 迁移临时条目前缀。
const STAGING_PREFIX: &str = ".legacy-migrating-";

/// 把存储根下旧 `memory/` 迁入当前数据目录。
pub fn migrate_legacy_storage() -> Result<()> {
    let legacy = crate::paths::storage_root().join("memory");
    migrate_legacy_dir(&legacy, &crate::paths::memory_data_dir())
}

fn migrate_legacy_dir(legacy: &Path, target: &Path) -> Result<()> {
    let marker = target.join(LEGACY_MIGRATION_MARKER);
    if marker.exists() || legacy == target {
        return Ok(());
    }
    std::fs::create_dir_all(target)
        .with_context(|| format!("创建 Memory 数据目录失败: {}", target.display()))?;
    match std::fs::symlink_metadata(legacy) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return write_marker(&marker, "skipped: legacy is not a directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return write_marker(&marker, "skipped: no legacy data");
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取旧 Memory 数据失败: {}", legacy.display()));
        }
    }
    if target.join("metadata.db").exists() {
        tracing::warn!(
            legacy = %legacy.display(),
            target = %target.display(),
            "新 Memory 数据目录已有数据库，跳过旧数据迁移（旧目录保留，需人工处理）"
        );
        return write_marker(
            &marker,
            &format!(
                "skipped: target already had metadata.db; legacy={}",
                legacy.display()
            ),
        );
    }
    ensure_legacy_idle(legacy)?;

    let mut copied = 0usize;
    for entry in std::fs::read_dir(legacy)
        .with_context(|| format!("读取旧 Memory 数据目录失败: {}", legacy.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if is_runtime_entry(&name_text) {
            continue;
        }
        let destination = target.join(&name);
        if destination.exists() {
            continue;
        }
        let staged = target.join(format!("{STAGING_PREFIX}{name_text}"));
        remove_any(&staged)?;
        copy_entry(&entry.path(), &staged)?;
        std::fs::rename(&staged, &destination).with_context(|| {
            format!(
                "落盘迁移数据失败: {} -> {}",
                staged.display(),
                destination.display()
            )
        })?;
        copied += 1;
    }
    write_marker(&marker, &format!("migrated from {}", legacy.display()))?;
    tracing::info!(
        legacy = %legacy.display(),
        target = %target.display(),
        copied,
        "Memory 旧数据迁移完成"
    );
    Ok(())
}

/// 旧目录仍有存活 Leader（旧版本进程还在写库）时拒绝读取其数据。
fn ensure_legacy_idle(legacy: &Path) -> Result<()> {
    if let Ok(Some(info)) = crate::election::read_leader_info_at(&legacy.join("leader.json"))
        && crate::election::is_leader_alive(&info)
        && info.pid != std::process::id()
    {
        bail!(
            "旧 Memory 数据目录仍被进程 {} 使用（{}），请先关闭旧版本 Memory 后重试",
            info.pid,
            legacy.display()
        );
    }
    Ok(())
}

/// 增量补迁完成标记文件名（位于新数据目录内）。
const LEGACY_RESYNC_MARKER: &str = ".legacy-resynced";
/// 补迁暂存目录：旧目录在沙箱内只读，SQLite 无法原地打开其 WAL，先复制快照。
const RESYNC_STAGING_DIR: &str = ".legacy-resync-staging";
/// 补迁写入、尚待重建检索索引的节点 ID 列表（由 Store 启动时消费）。
pub(crate) const RESYNC_PENDING_FILE: &str = ".legacy-resync-pending.json";
/// 注入文件目录（agent.md），按修改时间补迁。
const INJECTION_DIRS: [&str; 3] = ["profile", "workspaces", "sessions"];

/// 增量补迁：首次整目录迁移后，旧版本进程仍可能继续写旧目录（升级时旧
/// sidecar 未退出、新版本启动失败期间旧版本继续服务），这部分记忆不在新
/// 目录里。只对完成过整目录迁移（标记为 `migrated from`）的数据执行一次：
/// - SQLite：旧库中新库缺失或 `updated_at` 更新的节点（含扩展表）与关系按行
///   合并进新库，冲突以 `updated_at` 较新者为准，新库独有数据不受影响；
/// - 注入文件：旧目录中缺失或更新的 `agent.md` 覆盖到新目录；
/// - 合并节点的 ID 写入待重建列表，Store 启动后补齐 Tantivy 与向量索引。
///
/// 旧目录仍被存活 Leader 占用时报错，调用方告警后下次启动重试。
pub fn resync_legacy_storage() -> Result<()> {
    let legacy = crate::paths::storage_root().join("memory");
    resync_legacy_dir(&legacy, &crate::paths::memory_data_dir())
}

fn resync_legacy_dir(legacy: &Path, target: &Path) -> Result<()> {
    let marker = target.join(LEGACY_RESYNC_MARKER);
    if marker.exists() || legacy == target || !target.is_dir() {
        return Ok(());
    }
    let migrated =
        std::fs::read_to_string(target.join(LEGACY_MIGRATION_MARKER)).unwrap_or_default();
    if !migrated.starts_with("migrated from") || !legacy.join("metadata.db").is_file() {
        return write_marker(&marker, "skipped: no legacy data to resync");
    }
    ensure_legacy_idle(legacy)?;
    let merged = merge_legacy_database(legacy, target)?;
    let files = sync_injection_files(legacy, target)?;
    if !merged.is_empty() {
        append_resync_pending(target, &merged)?;
    }
    write_marker(
        &marker,
        &format!(
            "resynced from {}; nodes={} files={files}",
            legacy.display(),
            merged.len()
        ),
    )?;
    tracing::info!(
        legacy = %legacy.display(),
        target = %target.display(),
        nodes = merged.len(),
        files,
        "Memory 旧目录增量补迁完成"
    );
    Ok(())
}

/// 把旧库快照合并进新库，返回被写入的节点 ID。
fn merge_legacy_database(legacy: &Path, target: &Path) -> Result<Vec<String>> {
    let staging = target.join(RESYNC_STAGING_DIR);
    remove_any(&staging)?;
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("创建补迁暂存目录失败: {}", staging.display()))?;
    let result = (|| -> Result<Vec<String>> {
        // 连同 WAL 一起复制：旧进程最近的写入可能还未 checkpoint 进主库。
        for name in ["metadata.db", "metadata.db-wal", "metadata.db-shm"] {
            let source = legacy.join(name);
            if source.is_file() {
                std::fs::copy(&source, staging.join(name))
                    .with_context(|| format!("复制旧 Memory 数据库失败: {}", source.display()))?;
            }
        }
        // 按正常流程打开快照：回放 WAL、转换历史加密库并补齐 Schema 列，
        // 保证与新库列一致后再按列名合并。
        drop(crate::db::MemoryDb::open_at_data_dir(&staging)?);
        let db = crate::db::MemoryDb::open_at_data_dir(target)?;
        let conn = db.connection();
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let snapshot = staging.join("metadata.db");
        conn.execute(
            "ATTACH DATABASE ?1 AS legacy",
            [snapshot.to_string_lossy().as_ref()],
        )
        .with_context(|| "挂载旧 Memory 数据库快照失败")?;
        let merged = merge_attached(conn);
        let _ = conn.execute("DETACH DATABASE legacy", []);
        merged
    })();
    let _ = remove_any(&staging);
    result
}

const NODE_COLUMNS: &str = "id, kind, memory_type, scope_type, scope_id, title, summary, keywords, \
     importance, confidence, status, source, usage_count, last_used_at, created_at, updated_at";
const RELATION_COLUMNS: [&str; 8] = [
    "id",
    "from_node_id",
    "to_node_id",
    "relation_kind",
    "weight",
    "note",
    "created_at",
    "updated_at",
];

fn merge_attached(conn: &rusqlite::Connection) -> Result<Vec<String>> {
    let relation_columns = RELATION_COLUMNS.join(", ");
    let relation_select = RELATION_COLUMNS
        .iter()
        .map(|column| format!("l.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(&format!(
        "CREATE TEMP TABLE IF NOT EXISTS resync_nodes (id TEXT PRIMARY KEY);
         DELETE FROM resync_nodes;
         INSERT INTO resync_nodes (id)
           SELECT l.id FROM legacy.memory_nodes l
           LEFT JOIN main.memory_nodes m ON m.id = l.id
           WHERE m.id IS NULL OR l.updated_at > m.updated_at;
         INSERT OR REPLACE INTO main.memory_nodes ({NODE_COLUMNS})
           SELECT {NODE_COLUMNS} FROM legacy.memory_nodes
           WHERE id IN (SELECT id FROM resync_nodes);
         INSERT OR REPLACE INTO main.episodes (id, session_id, outcome, tool_calls, full_content)
           SELECT id, session_id, outcome, tool_calls, full_content FROM legacy.episodes
           WHERE id IN (SELECT id FROM resync_nodes);
         INSERT OR REPLACE INTO main.entities
           (id, entity_type, file_path, related_episodes, full_content)
           SELECT id, entity_type, file_path, related_episodes, full_content FROM legacy.entities
           WHERE id IN (SELECT id FROM resync_nodes);
         INSERT OR REPLACE INTO main.decisions
           (id, context, alternatives, chosen, reasons, episode_ids, full_content)
           SELECT id, context, alternatives, chosen, reasons, episode_ids, full_content
           FROM legacy.decisions WHERE id IN (SELECT id FROM resync_nodes);
         INSERT OR REPLACE INTO main.evidence (id, evidence_path, byte_size)
           SELECT id, evidence_path, byte_size FROM legacy.evidence
           WHERE id IN (SELECT id FROM resync_nodes);
         INSERT OR REPLACE INTO main.memory_relations ({relation_columns})
           SELECT {relation_select} FROM legacy.memory_relations l
           LEFT JOIN main.memory_relations m
             ON m.from_node_id = l.from_node_id AND m.to_node_id = l.to_node_id
            AND m.relation_kind = l.relation_kind
           WHERE m.id IS NULL OR l.updated_at > m.updated_at;"
    ))
    .with_context(|| "合并旧 Memory 数据失败")?;
    let ids = {
        let mut stmt = tx.prepare("SELECT id FROM resync_nodes ORDER BY id")?;
        stmt.query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    tx.execute("DROP TABLE resync_nodes", [])?;
    tx.commit().with_context(|| "提交旧 Memory 数据合并失败")?;
    Ok(ids)
}

/// 注入文件按修改时间补迁，返回覆盖的文件数。
fn sync_injection_files(legacy: &Path, target: &Path) -> Result<usize> {
    let mut copied = 0;
    for dir in INJECTION_DIRS {
        copied += sync_newer_files(&legacy.join(dir), &target.join(dir))?;
    }
    Ok(copied)
}

fn sync_newer_files(source: &Path, destination: &Path) -> Result<usize> {
    let entries = match std::fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("读取旧注入目录失败: {}", source.display()));
        }
    };
    let mut copied = 0;
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let to = destination.join(entry.file_name());
        if file_type.is_dir() {
            copied += sync_newer_files(&entry.path(), &to)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let source_modified = entry.metadata()?.modified()?;
        let newer = match std::fs::metadata(&to) {
            Ok(meta) => meta.modified()? < source_modified,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(error.into()),
        };
        if !newer {
            continue;
        }
        std::fs::create_dir_all(destination)
            .with_context(|| format!("创建注入目录失败: {}", destination.display()))?;
        let staged = destination.join(format!(
            "{STAGING_PREFIX}{}",
            entry.file_name().to_string_lossy()
        ));
        std::fs::copy(entry.path(), &staged)
            .with_context(|| format!("复制注入文件失败: {}", entry.path().display()))?;
        std::fs::rename(&staged, &to)
            .with_context(|| format!("落盘注入文件失败: {}", to.display()))?;
        copied += 1;
    }
    Ok(copied)
}

fn append_resync_pending(target: &Path, ids: &[String]) -> Result<()> {
    let path = target.join(RESYNC_PENDING_FILE);
    let mut all = load_resync_pending(target);
    all.extend(ids.iter().cloned());
    all.sort();
    all.dedup();
    std::fs::write(&path, serde_json::to_vec(&all)?)
        .with_context(|| format!("写入补迁待重建列表失败: {}", path.display()))
}

/// 读取待重建索引的节点 ID（文件缺失或损坏时返回空）。
pub(crate) fn load_resync_pending(target: &Path) -> Vec<String> {
    std::fs::read(target.join(RESYNC_PENDING_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// 索引补齐后清除待重建列表。
pub(crate) fn clear_resync_pending(target: &Path) {
    let path = target.join(RESYNC_PENDING_FILE);
    if let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, path = %path.display(), "清除补迁待重建列表失败");
    }
}
/// 旧模型目录名（不随启动迁移，由下载流程按需复用）。
pub(crate) const LEGACY_MODELS_DIR: &str = "models";

/// 不迁移的条目：进程运行态（新目录由新进程重新生成）与内置模型（按需复用）。
fn is_runtime_entry(name: &str) -> bool {
    matches!(
        name,
        "leader.json"
            | "leader.lock"
            | "runtime"
            | "daemon.json"
            | LEGACY_MODELS_DIR
            | LEGACY_MIGRATION_MARKER
            | LEGACY_RESYNC_MARKER
            | RESYNC_PENDING_FILE
            | RESYNC_STAGING_DIR
    ) || name.starts_with(".leader.json.")
        || name.starts_with(STAGING_PREFIX)
}

fn write_marker(marker: &Path, note: &str) -> Result<()> {
    std::fs::write(marker, format!("{note}\n"))
        .with_context(|| format!("写入迁移完成标记失败: {}", marker.display()))
}

fn remove_any(path: &PathBuf) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => Err(error),
    }
    .with_context(|| format!("清理未完成的迁移失败: {}", path.display()))
}

/// 递归复制（只复制普通文件与目录；符号链接视为篡改拒绝）。
///
/// macOS APFS 上 `std::fs::copy` 走 clonefile，模型等大文件近乎零成本。
fn copy_entry(source: &Path, destination: &Path) -> Result<()> {
    let file_type = std::fs::symlink_metadata(source)?.file_type();
    if file_type.is_symlink() {
        bail!("Memory 数据目录不允许符号链接: {}", source.display());
    }
    if file_type.is_dir() {
        std::fs::create_dir(destination)
            .with_context(|| format!("创建目录失败: {}", destination.display()))?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if file_type.is_file() {
        std::fs::copy(source, destination).with_context(|| {
            format!(
                "复制 Memory 数据失败: {} -> {}",
                source.display(),
                destination.display()
            )
        })?;
        return Ok(());
    }
    // socket / fifo 等特殊文件不属于持久数据。
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_legacy(root: &Path) -> PathBuf {
        let legacy = root.join("memory");
        std::fs::create_dir_all(legacy.join("lancedb").join("t.lance")).unwrap();
        std::fs::create_dir_all(legacy.join("runtime")).unwrap();
        std::fs::create_dir_all(legacy.join("models").join("bge-m3")).unwrap();
        std::fs::write(legacy.join("models").join("bge-m3").join("m.onnx"), b"m").unwrap();
        std::fs::write(legacy.join("metadata.db"), b"db").unwrap();
        std::fs::write(legacy.join("metadata.db-wal"), b"wal").unwrap();
        std::fs::write(legacy.join("config.json"), b"{}").unwrap();
        std::fs::write(legacy.join("lancedb").join("t.lance").join("v1"), b"v").unwrap();
        std::fs::write(legacy.join("leader.lock"), b"").unwrap();
        legacy
    }

    #[test]
    fn copies_persistent_data_once_and_skips_runtime() {
        let root = tempfile::tempdir().unwrap();
        let legacy = seed_legacy(root.path());
        let target = root.path().join("data").join("memory");

        migrate_legacy_dir(&legacy, &target).unwrap();
        assert_eq!(std::fs::read(target.join("metadata.db")).unwrap(), b"db");
        assert_eq!(
            std::fs::read(target.join("metadata.db-wal")).unwrap(),
            b"wal"
        );
        assert!(target.join("lancedb").join("t.lance").join("v1").is_file());
        assert!(!target.join("leader.lock").exists());
        assert!(!target.join("runtime").exists());
        assert!(!target.join("models").exists(), "模型由下载流程按需复用");
        assert!(target.join(LEGACY_MIGRATION_MARKER).is_file());
        assert!(legacy.join("metadata.db").is_file(), "旧数据保留");

        // 已迁移：旧目录后续变化不再影响新目录。
        std::fs::write(target.join("config.json"), b"new").unwrap();
        std::fs::write(legacy.join("config.json"), b"changed").unwrap();
        migrate_legacy_dir(&legacy, &target).unwrap();
        assert_eq!(std::fs::read(target.join("config.json")).unwrap(), b"new");
    }

    #[test]
    fn existing_target_database_is_never_merged() {
        let root = tempfile::tempdir().unwrap();
        let legacy = seed_legacy(root.path());
        let target = root.path().join("data").join("memory");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("metadata.db"), b"current").unwrap();

        migrate_legacy_dir(&legacy, &target).unwrap();
        assert_eq!(
            std::fs::read(target.join("metadata.db")).unwrap(),
            b"current"
        );
        assert!(!target.join("lancedb").exists(), "不混入旧向量库");
        assert!(target.join(LEGACY_MIGRATION_MARKER).is_file());
    }

    #[test]
    fn interrupted_staging_is_replaced_on_retry() {
        let root = tempfile::tempdir().unwrap();
        let legacy = seed_legacy(root.path());
        let target = root.path().join("data").join("memory");
        let stale = target.join(format!("{STAGING_PREFIX}lancedb"));
        std::fs::create_dir_all(stale.join("partial")).unwrap();

        migrate_legacy_dir(&legacy, &target).unwrap();
        assert!(!stale.exists());
        assert!(target.join("lancedb").join("t.lance").join("v1").is_file());
        assert!(!target.join("lancedb").join("partial").exists());
    }

    #[test]
    fn missing_legacy_only_writes_marker() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("data").join("memory");
        migrate_legacy_dir(&root.path().join("memory"), &target).unwrap();
        assert!(target.join(LEGACY_MIGRATION_MARKER).is_file());
        assert!(!target.join("metadata.db").exists());
    }

    fn create_db(dir: &Path) -> crate::db::MemoryDb {
        crate::db::MemoryDb::open_at_data_dir(dir).unwrap()
    }

    fn insert_node(db: &crate::db::MemoryDb, id: &str, title: &str, updated_at: &str) {
        db.connection()
            .execute(
                "INSERT OR REPLACE INTO memory_nodes
                 (id, kind, scope_type, title, summary, created_at, updated_at)
                 VALUES (?1, 'episode', 'global', ?2, '摘要', ?3, ?3)",
                rusqlite::params![id, title, updated_at],
            )
            .unwrap();
        db.connection()
            .execute(
                "INSERT OR REPLACE INTO episodes (id, session_id, outcome, full_content)
                 VALUES (?1, 's', 'success', ?2)",
                rusqlite::params![id, title],
            )
            .unwrap();
    }

    fn node_title(db: &crate::db::MemoryDb, id: &str) -> Option<String> {
        db.connection()
            .query_row(
                "SELECT title FROM memory_nodes WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .ok()
    }

    #[test]
    fn resync_merges_rows_written_to_legacy_after_migration() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("memory");
        let target = root.path().join("data").join("memory");
        {
            let old = create_db(&legacy);
            insert_node(&old, "a", "旧版本更新后", "2026-10-13 18:00:00");
            insert_node(&old, "b", "旧版本新增", "2026-10-13 18:00:00");
            insert_node(&old, "c", "旧版本过时", "2026-10-13 10:00:00");
        }
        {
            let new = create_db(&target);
            insert_node(&new, "a", "迁移时的版本", "2026-10-13 16:00:00");
            insert_node(&new, "c", "新版本较新", "2026-10-13 17:00:00");
            insert_node(&new, "d", "新版本独有", "2026-10-13 17:00:00");
        }
        std::fs::create_dir_all(legacy.join("sessions").join("s1")).unwrap();
        std::fs::write(legacy.join("sessions/s1/agent.md"), "旧注入").unwrap();
        write_marker(&target.join(LEGACY_MIGRATION_MARKER), "migrated from test").unwrap();

        resync_legacy_dir(&legacy, &target).unwrap();

        let new = create_db(&target);
        assert_eq!(node_title(&new, "a").as_deref(), Some("旧版本更新后"));
        assert_eq!(node_title(&new, "b").as_deref(), Some("旧版本新增"));
        assert_eq!(
            node_title(&new, "c").as_deref(),
            Some("新版本较新"),
            "新库较新不被覆盖"
        );
        assert_eq!(node_title(&new, "d").as_deref(), Some("新版本独有"));
        let episode: String = new
            .connection()
            .query_row(
                "SELECT full_content FROM episodes WHERE id = 'b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(episode, "旧版本新增", "扩展表随节点合并");
        assert_eq!(
            std::fs::read_to_string(target.join("sessions/s1/agent.md")).unwrap(),
            "旧注入"
        );
        assert_eq!(load_resync_pending(&target), vec!["a", "b"]);
        assert!(target.join(LEGACY_RESYNC_MARKER).is_file());
        assert!(!target.join(RESYNC_STAGING_DIR).exists());
        assert!(legacy.join("metadata.db").is_file(), "旧目录保留");

        // 只执行一次：标记存在后旧目录变化不再合并。
        insert_node(
            &create_db(&legacy),
            "e",
            "标记后写入",
            "2026-10-13 19:00:00",
        );
        resync_legacy_dir(&legacy, &target).unwrap();
        assert_eq!(node_title(&create_db(&target), "e"), None);
    }

    #[test]
    fn resync_skips_when_target_was_not_migrated_from_legacy() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("memory");
        let target = root.path().join("data").join("memory");
        insert_node(&create_db(&legacy), "a", "旧", "2026-10-13 18:00:00");
        create_db(&target);
        write_marker(
            &target.join(LEGACY_MIGRATION_MARKER),
            "skipped: target already had metadata.db",
        )
        .unwrap();
        resync_legacy_dir(&legacy, &target).unwrap();
        assert_eq!(
            node_title(&create_db(&target), "a"),
            None,
            "两份独立库不合并"
        );
        assert!(target.join(LEGACY_RESYNC_MARKER).is_file());
    }

    #[test]
    #[cfg(unix)]
    fn alive_legacy_leader_blocks_resync() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("memory");
        let target = root.path().join("data").join("memory");
        insert_node(&create_db(&legacy), "a", "旧", "2026-10-13 18:00:00");
        create_db(&target);
        write_marker(&target.join(LEGACY_MIGRATION_MARKER), "migrated from test").unwrap();
        let info = crate::election::LeaderInfo {
            pid: std::os::unix::process::parent_id(),
            ..crate::election::LeaderInfo::new(
                crate::election::ProcessType::Sidecar,
                "memory".into(),
            )
        };
        std::fs::write(
            legacy.join("leader.json"),
            serde_json::to_vec(&info).unwrap(),
        )
        .unwrap();
        assert!(resync_legacy_dir(&legacy, &target).is_err());
        assert!(!target.join(LEGACY_RESYNC_MARKER).exists(), "下次启动重试");
    }

    #[test]
    #[cfg(unix)]
    fn alive_legacy_leader_blocks_migration() {
        let root = tempfile::tempdir().unwrap();
        let legacy = seed_legacy(root.path());
        // 父进程（cargo test runner）一定存活且不是本进程。
        let parent = std::os::unix::process::parent_id();
        let info = crate::election::LeaderInfo {
            pid: parent,
            ..crate::election::LeaderInfo::new(
                crate::election::ProcessType::Sidecar,
                "memory".into(),
            )
        };
        std::fs::write(
            legacy.join("leader.json"),
            serde_json::to_vec(&info).unwrap(),
        )
        .unwrap();
        let target = root.path().join("data").join("memory");
        let error = migrate_legacy_dir(&legacy, &target).unwrap_err();
        assert!(error.to_string().contains("仍被进程"), "{error:#}");
        assert!(!target.join(LEGACY_MIGRATION_MARKER).exists());
        assert!(!target.join("metadata.db").exists());
    }
}
