//! MCP 旧数据迁移（插件自身行为）。
//!
//! 早期版本把 `mcp.json` 与能力缓存放在存储根下；沙箱收拢写域后存储根只读，
//! 配置改放插件自有数据目录 `data/mcp/`。首次启动把旧文件复制进新目录并写
//! 完成标记，之后直接跳过（幂等）。旧文件原样保留，由用户自行清理。
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 旧数据迁移完成标记文件名（位于插件自有数据目录内）。
const LEGACY_MIGRATION_MARKER: &str = ".legacy-migrated";

/// 一次迁移的结果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct LegacyMigration {
    /// 本次是否已执行过迁移（标记已存在时为 `false`）。
    pub(crate) performed: bool,
    /// 本次复制进新目录的文件（新目录中的路径）。
    pub(crate) copied: Vec<PathBuf>,
}

/// 把存储根下的旧文件复制进插件自有数据目录。
///
/// `files` 为 `(旧文件路径, 新目录内文件名)`。规则：
/// - `target_dir` 内已有完成标记：直接返回，不做任何事；
/// - 旧文件不存在、或新目录已有同名文件：跳过该项（不覆盖新数据）；
/// - 每个文件先写临时文件再原子改名，全部成功后才写完成标记；
///   中途失败返回错误且不写标记，下次调用会继续补齐。
pub(crate) fn migrate_legacy_files(
    target_dir: &Path,
    files: &[(PathBuf, &str)],
) -> Result<LegacyMigration> {
    let marker = target_dir.join(LEGACY_MIGRATION_MARKER);
    if marker.exists() {
        return Ok(LegacyMigration::default());
    }
    std::fs::create_dir_all(target_dir)
        .with_context(|| format!("创建插件数据目录失败: {}", target_dir.display()))?;
    let mut copied = Vec::new();
    for (legacy, name) in files {
        let target = target_dir.join(name);
        if target.exists() {
            continue;
        }
        match std::fs::metadata(legacy) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("读取旧数据失败: {}", legacy.display()));
            }
        }
        let staged = target_dir.join(format!(".{name}.migrating"));
        std::fs::copy(legacy, &staged).with_context(|| {
            format!(
                "复制旧数据失败: {} -> {}",
                legacy.display(),
                staged.display()
            )
        })?;
        std::fs::rename(&staged, &target).with_context(|| {
            format!(
                "落盘迁移数据失败: {} -> {}",
                staged.display(),
                target.display()
            )
        })?;
        tracing::info!(
            from = %legacy.display(),
            to = %target.display(),
            "插件旧数据已复制到自有数据目录"
        );
        copied.push(target);
    }
    let note = files
        .iter()
        .map(|(legacy, name)| format!("{} -> {name}\n", legacy.display()))
        .collect::<String>();
    std::fs::write(&marker, note)
        .with_context(|| format!("写入迁移完成标记失败: {}", marker.display()))?;
    Ok(LegacyMigration {
        performed: true,
        copied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_once_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("mcp.json");
        std::fs::write(&legacy, "old").unwrap();
        let target = root.path().join("data").join("mcp");
        let files = [(legacy.clone(), "mcp.json")];

        let first = migrate_legacy_files(&target, &files).unwrap();
        assert!(first.performed);
        assert_eq!(first.copied, vec![target.join("mcp.json")]);
        assert_eq!(
            std::fs::read_to_string(target.join("mcp.json")).unwrap(),
            "old"
        );
        assert!(target.join(LEGACY_MIGRATION_MARKER).is_file());
        // 旧文件保留。
        assert!(legacy.is_file());

        // 新数据写入后再次调用：不会被旧文件覆盖。
        std::fs::write(target.join("mcp.json"), "new").unwrap();
        std::fs::write(&legacy, "changed-old").unwrap();
        let second = migrate_legacy_files(&target, &files).unwrap();
        assert!(!second.performed);
        assert_eq!(
            std::fs::read_to_string(target.join("mcp.json")).unwrap(),
            "new"
        );
    }

    #[test]
    fn missing_legacy_and_existing_target_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("data").join("mcp");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("cache.json"), "kept").unwrap();
        let legacy_cache = root.path().join("cache.json");
        std::fs::write(&legacy_cache, "old-cache").unwrap();

        let result = migrate_legacy_files(
            &target,
            &[
                (root.path().join("absent.json"), "absent.json"),
                (legacy_cache, "cache.json"),
            ],
        )
        .unwrap();
        assert!(result.performed);
        assert!(result.copied.is_empty());
        assert!(!target.join("absent.json").exists());
        assert_eq!(
            std::fs::read_to_string(target.join("cache.json")).unwrap(),
            "kept"
        );
        assert!(target.join(LEGACY_MIGRATION_MARKER).is_file());
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_legacy_fails_without_marker() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let legacy_dir = root.path().join("legacy");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy = legacy_dir.join("mcp.json");
        std::fs::write(&legacy, "old").unwrap();
        let target = root.path().join("data").join("mcp");
        let original = std::fs::metadata(&legacy_dir).unwrap().permissions();
        let mut restricted = original.clone();
        restricted.set_mode(0o000);
        std::fs::set_permissions(&legacy_dir, restricted).unwrap();
        let result = migrate_legacy_files(&target, &[(legacy, "mcp.json")]);
        std::fs::set_permissions(&legacy_dir, original).unwrap();
        // root 用户不受权限位限制，此时迁移会成功，跳过断言。
        if result.is_ok() {
            return;
        }
        assert!(!target.join(LEGACY_MIGRATION_MARKER).exists());
        assert!(!target.join("mcp.json").exists());
    }
}
