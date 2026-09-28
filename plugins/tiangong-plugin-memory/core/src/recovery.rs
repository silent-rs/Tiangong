use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::db::{MemoryDb, sqlite::is_migration_backup_file};

const RECOVERY_MARKER: &str = ".plugin-data-recovered";

/// 恢复短暂写入插件私有目录的 Memory 数据。
///
/// 仅当来源包含真实记忆、当前标准目录没有节点和其他有效数据时切换。
/// 返回被保留的空目录备份路径；目标原本不存在时返回 `None`。
pub fn recover_plugin_data_dir(source: &Path) -> Result<Option<PathBuf>> {
    recover_plugin_data_dir_to(source, &crate::paths::memory_data_dir())
}

fn recover_plugin_data_dir_to(source: &Path, target: &Path) -> Result<Option<PathBuf>> {
    if source == target
        || source.join(RECOVERY_MARKER).exists()
        || !source.join("metadata.db").is_file()
    {
        return Ok(None);
    }

    let source_count = count_nodes(source)
        .with_context(|| format!("核对待恢复 Memory 数据失败: {}", source.display()))?;
    if source_count == 0 {
        return Ok(None);
    }

    if target.join(RECOVERY_MARKER).exists() {
        return Ok(None);
    }

    let target_count = if target.join("metadata.db").is_file() {
        count_nodes(target)
            .with_context(|| format!("核对当前 Memory 数据失败: {}", target.display()))?
    } else {
        0
    };
    if target_count > 0 || has_meaningful_data(target)? {
        tracing::warn!(
            source = %source.display(),
            target = %target.display(),
            source_count,
            target_count,
            "检测到两处 Memory 数据，保留当前标准目录并跳过自动恢复"
        );
        return Ok(None);
    }

    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Memory 数据目录缺少父目录: {}", target.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("创建 Memory 数据父目录失败: {}", parent.display()))?;

    let transaction_id = scru128::new().to_string();
    let staged = parent.join(format!(".memory-recovery-{transaction_id}"));
    let backup = parent.join(format!("memory.pre-recovery-{transaction_id}"));
    copy_data_directory(source, &staged)?;
    std::fs::write(
        staged.join(RECOVERY_MARKER),
        format!("source={}\n", source.display()),
    )
    .with_context(|| "写入 Memory 数据恢复标记失败")?;

    let backup_path = if target.exists() {
        std::fs::rename(target, &backup).with_context(|| {
            format!(
                "备份当前 Memory 数据目录失败: {} -> {}",
                target.display(),
                backup.display()
            )
        })?;
        Some(backup.clone())
    } else {
        None
    };

    if let Err(error) = std::fs::rename(&staged, target) {
        if let Some(backup) = &backup_path {
            let _ = std::fs::rename(backup, target);
        }
        return Err(error).with_context(|| {
            format!(
                "启用恢复后的 Memory 数据失败: {} -> {}",
                staged.display(),
                target.display()
            )
        });
    }

    if let Err(error) = std::fs::write(
        source.join(RECOVERY_MARKER),
        format!("target={}\n", target.display()),
    ) {
        tracing::warn!(%error, path = %source.display(), "写入 Memory 来源恢复标记失败");
    }

    tracing::info!(
        source = %source.display(),
        target = %target.display(),
        source_count,
        backup = backup_path.as_ref().map(|path| path.display().to_string()),
        "已恢复改造期间分流的 Memory 数据"
    );
    Ok(backup_path)
}

fn count_nodes(data_dir: &Path) -> Result<usize> {
    let database = MemoryDb::open_at_data_dir(data_dir)?;
    database.count_memory_nodes(None, None, None, None)
}

fn has_meaningful_data(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    if !path.is_dir() {
        bail!("Memory 数据路径不是目录: {}", path.display());
    }

    for entry in std::fs::read_dir(path)
        .with_context(|| format!("读取 Memory 数据目录失败: {}", path.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // 节点计数可能刚刚转换过空加密库。迁移备份不是当前有效数据，
        // 但会随整个目标目录移入 pre-recovery 备份，不能删除。
        if entry.file_type()?.is_file() && is_migration_backup_file(&name) {
            continue;
        }
        if matches!(
            name.as_ref(),
            "metadata.db"
                | "metadata.db-wal"
                | "metadata.db-shm"
                | "leader.json"
                | "leader.lock"
                | "tantivy_index"
        ) || name.starts_with(".leader.json.")
        {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

fn copy_data_directory(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir(destination).with_context(|| {
        format!(
            "创建 Memory 数据恢复临时目录失败: {}",
            destination.display()
        )
    })?;

    for entry in std::fs::read_dir(source)
        .with_context(|| format!("读取待恢复 Memory 数据失败: {}", source.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if matches!(
            name_text.as_ref(),
            "leader.json" | "leader.lock" | "runtime" | RECOVERY_MARKER
        ) || name_text.starts_with(".leader.json.")
        {
            continue;
        }
        copy_entry(&entry.path(), &destination.join(name))?;
    }
    Ok(())
}

fn copy_entry(source: &Path, destination: &Path) -> Result<()> {
    let file_type = std::fs::symlink_metadata(source)?.file_type();
    if file_type.is_symlink() {
        bail!("Memory 数据目录不允许符号链接: {}", source.display());
    }
    if file_type.is_dir() {
        std::fs::create_dir(destination)
            .with_context(|| format!("创建 Memory 恢复目录失败: {}", destination.display()))?;
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
    bail!("Memory 数据包含不支持的文件类型: {}", source.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::test_helpers::create_empty_encrypted_database;
    use crate::types::{Episode, EpisodeOutcome};

    #[test]
    fn migration_backups_are_distinct_from_other_data() {
        let root = tempfile::tempdir().unwrap();
        for prefix in [".metadata.db.pre-plaintext-", ".metadata.db.pre-key-v2-"] {
            for suffix in ["", "-wal", "-shm", "-journal"] {
                let path = root
                    .path()
                    .join(format!("{prefix}{}.bak{suffix}", scru128::new()));
                std::fs::write(path, b"preserved backup").unwrap();
            }
        }
        assert!(!has_meaningful_data(root.path()).unwrap());
        for name in [
            "notes.txt",
            ".metadata.db.pre-plaintext-user.bak",
            ".metadata.db.pre-key-v2-user.bak",
        ] {
            let path = root.path().join(name);
            std::fs::write(&path, b"user data").unwrap();
            assert!(has_meaningful_data(root.path()).unwrap());
            std::fs::remove_file(path).unwrap();
        }
        let directory = root
            .path()
            .join(format!(".metadata.db.pre-plaintext-{}.bak", scru128::new()));
        std::fs::create_dir(directory).unwrap();
        assert!(has_meaningful_data(root.path()).unwrap());
    }

    #[test]
    fn plugin_history_recovers_over_empty_encrypted_target_without_losing_backups() {
        for use_v2 in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("plugin-private");
            let target = root.path().join("memory");
            let episode = Episode::new(
                "recovery-session".into(),
                "历史记忆".into(),
                "必须完整恢复".into(),
                EpisodeOutcome::Success,
                vec![],
                vec![],
                0.8,
            );
            let db = MemoryDb::open_at_data_dir(&source).unwrap();
            db.insert_episode(&episode, Some("workspace")).unwrap();
            drop(db);
            std::fs::create_dir(source.join("lancedb")).unwrap();
            std::fs::write(source.join("lancedb/preserved"), b"vector-data").unwrap();
            create_empty_encrypted_database(&target, use_v2);
            let original = std::fs::read(target.join("metadata.db")).unwrap();
            let backup = recover_plugin_data_dir_to(&source, &target)
                .unwrap()
                .expect("空加密库不应阻止历史恢复");
            assert_eq!(count_nodes(&target).unwrap(), 1);
            assert_eq!(count_nodes(&source).unwrap(), 1);
            assert_eq!(
                std::fs::read(target.join("lancedb/preserved")).unwrap(),
                b"vector-data"
            );
            let encrypted_backup = std::fs::read_dir(&backup)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".metadata.db.pre-plaintext-")
                })
                .unwrap();
            assert_eq!(std::fs::read(encrypted_backup).unwrap(), original);
            assert_eq!(count_nodes(&backup).unwrap(), 0);
            assert!(
                recover_plugin_data_dir_to(&source, &target)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(count_nodes(&target).unwrap(), 1);
        }
    }
}
