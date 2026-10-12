//! Subagent 旧数据迁移（插件自身行为）。
//!
//! 早期版本把身份目录放在存储根下的 `agents/`、运行态放在 `agents-runtime/`；
//! 沙箱收拢写域后存储根只读，两者改放插件自有数据目录 `data/subagent/` 下的
//! `agents/` 与 `runtime/`。首次启动把旧目录整树复制进来并写完成标记，之后
//! 直接跳过（幂等）。旧目录原样保留，由用户自行清理。
use std::path::Path;

use anyhow::{Context, Result};

/// 旧数据迁移完成标记文件名（位于插件自有数据目录内）。
const LEGACY_MIGRATION_MARKER: &str = ".legacy-migrated";

/// 把 `(旧目录, 新目录名)` 逐项整树复制进 `data_dir`，返回本次复制的目录数。
///
/// 规则：
/// - `data_dir` 已有完成标记：直接返回 `None`；
/// - 旧目录不存在、或新目录已存在：跳过该项（不覆盖新数据）；
/// - 每项先复制到同级临时目录，完成后原子改名；符号链接不跟随、不复制；
/// - 全部成功后才写标记；中途失败返回错误且不写标记，调用方必须拒绝继续
///   读写，避免在新目录生成空数据后旧数据再也迁不过来。
pub(crate) fn migrate_legacy_dirs(
    data_dir: &Path,
    dirs: &[(&Path, &str)],
) -> Result<Option<usize>> {
    let marker = data_dir.join(LEGACY_MIGRATION_MARKER);
    if marker.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("创建插件数据目录失败: {}", data_dir.display()))?;
    let mut copied = 0;
    for (legacy, name) in dirs {
        let target = data_dir.join(name);
        if target.exists() {
            continue;
        }
        match std::fs::symlink_metadata(legacy) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("读取旧数据失败: {}", legacy.display()));
            }
        }
        let staged = data_dir.join(format!(".{name}.migrating"));
        if staged.exists() {
            std::fs::remove_dir_all(&staged)
                .with_context(|| format!("清理上次未完成的迁移失败: {}", staged.display()))?;
        }
        copy_tree(legacy, &staged)?;
        std::fs::rename(&staged, &target).with_context(|| {
            format!(
                "落盘迁移数据失败: {} -> {}",
                staged.display(),
                target.display()
            )
        })?;
        tracing::info!(from = %legacy.display(), to = %target.display(), "Subagent 旧数据已迁移");
        copied += 1;
    }
    let note = dirs
        .iter()
        .map(|(legacy, name)| format!("{} -> {name}\n", legacy.display()))
        .collect::<String>();
    std::fs::write(&marker, note)
        .with_context(|| format!("写入迁移完成标记失败: {}", marker.display()))?;
    Ok(Some(copied))
}

/// 递归复制目录（只复制普通文件与目录，跳过符号链接等特殊文件）。
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
        } else if file_type.is_file() {
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
    fn copies_trees_once_and_keeps_new_data() {
        let root = tempfile::tempdir().unwrap();
        let agents = root.path().join("agents");
        std::fs::create_dir_all(agents.join("a1").join("memory")).unwrap();
        std::fs::write(agents.join("a1").join("agent.toml"), "id='a1'").unwrap();
        std::fs::write(agents.join("a1").join("memory").join("notes.md"), "n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", agents.join("link")).unwrap();
        let data = root.path().join("data").join("subagent");
        let absent = root.path().join("agents-runtime");

        let copied =
            migrate_legacy_dirs(&data, &[(&agents, "agents"), (&absent, "runtime")]).unwrap();
        assert_eq!(copied, Some(1));
        assert_eq!(
            std::fs::read_to_string(
                data.join("agents")
                    .join("a1")
                    .join("memory")
                    .join("notes.md")
            )
            .unwrap(),
            "n"
        );
        assert!(!data.join("agents").join("link").exists(), "符号链接不复制");
        assert!(!data.join("runtime").exists());
        assert!(agents.join("a1").join("agent.toml").is_file(), "旧数据保留");

        std::fs::write(data.join("agents").join("a1").join("agent.toml"), "new").unwrap();
        assert_eq!(
            migrate_legacy_dirs(&data, &[(&agents, "agents")]).unwrap(),
            None
        );
        assert_eq!(
            std::fs::read_to_string(data.join("agents").join("a1").join("agent.toml")).unwrap(),
            "new"
        );
    }

    #[test]
    fn existing_target_dir_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let agents = root.path().join("agents");
        std::fs::create_dir_all(agents.join("old")).unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(data.join("agents").join("kept")).unwrap();
        assert_eq!(
            migrate_legacy_dirs(&data, &[(&agents, "agents")]).unwrap(),
            Some(0)
        );
        assert!(data.join("agents").join("kept").is_dir());
        assert!(!data.join("agents").join("old").exists());
    }
}
