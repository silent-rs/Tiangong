//! Memory 旧数据迁移（插件自身行为）。
//!
//! 早期版本把全部数据放在存储根下的 `memory/`；沙箱收拢写域后存储根只读，
//! 数据改放插件自有数据目录 `data/memory/`。首次启动时把旧目录逐项复制进
//! 新目录并写完成标记，之后直接跳过（幂等）。旧目录原样保留，由用户确认
//! 无误后自行清理。
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

/// 进程运行态条目：不迁移（新目录由新进程重新生成）。
fn is_runtime_entry(name: &str) -> bool {
    matches!(
        name,
        "leader.json" | "leader.lock" | "runtime" | "daemon.json" | LEGACY_MIGRATION_MARKER
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
