//! Index 旧数据迁移（插件自身行为）。
//!
//! 早期版本把索引放在存储根下的 `index/`（更早还短暂写过插件安装目录的
//! `data/index/`）；沙箱收拢写域后存储根只读，索引改放插件自有数据目录
//! `data/index/`。首次启动把第一个含索引数据的旧目录下的 `workspaces/`、
//! `sessions/` 复制进来并写完成标记，之后直接跳过（幂等）。旧目录原样保留。
//!
//! Index sidecar 按连接短生命周期拉起，可能并发启动：每个进程复制到自己的
//! 临时目录再原子改名，改名时目标已被其他进程落下则丢弃本进程副本。
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 旧数据迁移完成标记文件名（位于插件自有数据目录内）。
const LEGACY_MIGRATION_MARKER: &str = ".legacy-migrated";
/// 需要迁移的索引子目录。
const INDEX_SUBDIRS: [&str; 2] = ["workspaces", "sessions"];

/// 从 `candidates` 中第一个含索引数据的目录迁移到 `target`，返回来源目录。
///
/// 规则：已有完成标记直接返回 `None`；新目录已有同名子目录时跳过该项
/// （不覆盖）；符号链接来源不迁移；全部成功后才写标记，失败返回错误且
/// 不写标记，调用方必须中止启动，避免新目录先建空索引后旧数据迁不过来。
pub(crate) fn migrate_legacy_index(
    target: &Path,
    candidates: &[PathBuf],
) -> Result<Option<PathBuf>> {
    let marker = target.join(LEGACY_MIGRATION_MARKER);
    if marker.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(target)
        .with_context(|| format!("创建 Index 数据目录失败: {}", target.display()))?;
    let source = candidates.iter().find(|source| {
        source.as_path() != target
            && std::fs::symlink_metadata(source).is_ok_and(|meta| meta.is_dir())
            && INDEX_SUBDIRS.iter().any(|sub| source.join(sub).is_dir())
    });
    if let Some(source) = source {
        for sub in INDEX_SUBDIRS {
            let from = source.join(sub);
            let to = target.join(sub);
            if to.exists() || !std::fs::symlink_metadata(&from).is_ok_and(|meta| meta.is_dir()) {
                continue;
            }
            let staged = target.join(format!(".{sub}.migrating-{}", std::process::id()));
            if staged.exists() {
                std::fs::remove_dir_all(&staged)
                    .with_context(|| format!("清理未完成的迁移失败: {}", staged.display()))?;
            }
            copy_tree(&from, &staged)?;
            if let Err(error) = std::fs::rename(&staged, &to) {
                // 并发启动的其他进程已先落下同名目录：丢弃本进程副本。
                let _ = std::fs::remove_dir_all(&staged);
                if !to.is_dir() {
                    return Err(error).with_context(|| {
                        format!("落盘迁移数据失败: {} -> {}", staged.display(), to.display())
                    });
                }
            }
        }
        tracing::info!(from = %source.display(), to = %target.display(), "Index 旧数据已迁移");
    }
    let note = source
        .map(|s| format!("{}\n", s.display()))
        .unwrap_or_default();
    std::fs::write(&marker, note)
        .with_context(|| format!("写入迁移完成标记失败: {}", marker.display()))?;
    Ok(source.cloned())
}

/// 递归复制目录（只复制普通文件与目录，跳过符号链接与 tantivy 写锁）。
fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)
        .with_context(|| format!("创建目录失败: {}", target.display()))?;
    let entries =
        std::fs::read_dir(source).with_context(|| format!("读取目录失败: {}", source.display()))?;
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let to = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if file_type.is_file() && entry.file_name() != ".tantivy-writer.lock" {
            std::fs::copy(entry.path(), &to).with_context(|| {
                format!(
                    "复制文件失败: {} -> {}",
                    entry.path().display(),
                    to.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_first_candidate_with_data_once() {
        let root = tempfile::tempdir().unwrap();
        let empty = root.path().join("plugin-data-index");
        std::fs::create_dir_all(&empty).unwrap();
        let legacy = root.path().join("index");
        std::fs::create_dir_all(legacy.join("workspaces").join("w1")).unwrap();
        std::fs::write(legacy.join("workspaces").join("w1").join("meta.json"), "{}").unwrap();
        std::fs::write(
            legacy
                .join("workspaces")
                .join("w1")
                .join(".tantivy-writer.lock"),
            "",
        )
        .unwrap();
        let target = root.path().join("data").join("index");

        let source = migrate_legacy_index(&target, &[empty, legacy.clone()]).unwrap();
        assert_eq!(source, Some(legacy.clone()));
        assert!(
            target
                .join("workspaces")
                .join("w1")
                .join("meta.json")
                .is_file()
        );
        assert!(
            !target
                .join("workspaces")
                .join("w1")
                .join(".tantivy-writer.lock")
                .exists()
        );
        assert!(!target.join("sessions").exists());
        assert!(legacy.join("workspaces").is_dir(), "旧数据保留");

        std::fs::remove_dir_all(target.join("workspaces")).unwrap();
        assert_eq!(migrate_legacy_index(&target, &[legacy]).unwrap(), None);
        assert!(!target.join("workspaces").exists(), "已迁移后不再复制");
    }

    #[test]
    fn existing_target_subdir_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("index");
        std::fs::create_dir_all(legacy.join("sessions").join("old")).unwrap();
        let target = root.path().join("data").join("index");
        std::fs::create_dir_all(target.join("sessions").join("kept")).unwrap();
        migrate_legacy_index(&target, &[legacy]).unwrap();
        assert!(target.join("sessions").join("kept").is_dir());
        assert!(!target.join("sessions").join("old").exists());
        assert!(target.join(LEGACY_MIGRATION_MARKER).is_file());
    }
}
