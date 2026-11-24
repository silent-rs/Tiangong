//! 旧版 skills 目录迁入插件数据目录（插件自身行为）。
//!
//! 早期版本把技能放在存储根下的 `skills/`。沙箱收拢写域后那里只读，数据改放
//! 插件自有数据目录 `data/skill/skills/`。首次启动把旧目录逐项复制进新目录并写
//! 完成标记，之后直接跳过（幂等）。旧目录原样保留，由用户清理。

use std::path::Path;

use anyhow::{Context, Result};

/// 迁移完成标记文件名（位于插件数据目录内）。
pub const LEGACY_MIGRATION_MARKER: &str = ".legacy-skills-migrated";

/// 把旧 `skills/` 目录复制进 `target_dir`，返回本次复制的文件数。
///
/// 规则：
/// - `marker_dir` 已有完成标记：直接返回 `None`；
/// - 新目录已有的文件跳过（不覆盖），以新目录里的内容为准；子目录只补缺失的文件；
/// - 符号链接不迁移：目标不明，交由用户处理；
/// - 文件先写临时文件再原子改名；中途失败返回错误且不写标记，调用方中止启动，
///   避免在新目录生成不完整的技能清单后旧数据再也迁不过来；
/// - 旧目录不存在时同样写标记（全新安装无需迁移）。
pub(crate) fn migrate_legacy_dir(
    legacy_dir: &Path,
    target_dir: &Path,
    marker_dir: &Path,
) -> Result<Option<usize>> {
    let marker = marker_dir.join(LEGACY_MIGRATION_MARKER);
    if marker.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(target_dir)
        .with_context(|| format!("创建 skills 数据目录失败: {}", target_dir.display()))?;
    let mut copied = 0;
    if legacy_dir.is_dir() && !is_same_dir(legacy_dir, target_dir) {
        copy_tree(legacy_dir, target_dir, &mut copied)?;
    }
    std::fs::write(&marker, format!("{}\n", legacy_dir.display()))
        .with_context(|| format!("写入迁移完成标记失败: {}", marker.display()))?;
    Ok(Some(copied))
}

/// 两个路径是否指向同一目录（都能解析时比较真实路径，否则按字面比较）。
fn is_same_dir(first: &Path, second: &Path) -> bool {
    match (std::fs::canonicalize(first), std::fs::canonicalize(second)) {
        (Ok(first), Ok(second)) => first == second,
        _ => first == second,
    }
}

fn copy_tree(source: &Path, target: &Path, copied: &mut usize) -> Result<()> {
    for entry in std::fs::read_dir(source)
        .with_context(|| format!("读取旧 skills 目录失败: {}", source.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let path = entry.path();
        let destination = target.join(&name);
        let file_type = entry
            .file_type()
            .with_context(|| format!("读取旧 skills 目录项失败: {}", path.display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            // 已存在的子目录继续进入，只补缺失的文件：中途失败后重试不会留下
            // 不完整的技能目录。
            std::fs::create_dir_all(&destination).with_context(|| {
                format!("创建 skills 数据子目录失败: {}", destination.display())
            })?;
            copy_tree(&path, &destination, copied)?;
        } else if file_type.is_file() && copy_if_absent(&path, &destination)? {
            *copied += 1;
        }
    }
    Ok(())
}

fn copy_if_absent(source: &Path, destination: &Path) -> Result<bool> {
    if destination.exists() {
        return Ok(false);
    }
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let index = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .context("迁移目标文件名无效")?;
    let temp = destination.with_file_name(format!(
        ".{file_name}.migrating-{}-{index}",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        std::fs::copy(source, &temp)
            .with_context(|| format!("复制旧 skills 文件失败: {}", source.display()))?;
        std::fs::rename(&temp, destination)
            .with_context(|| format!("落盘迁移文件失败: {}", destination.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_fixture(root: &Path) -> std::path::PathBuf {
        let legacy = root.join("legacy-skills");
        std::fs::create_dir_all(legacy.join("demo-skill")).unwrap();
        std::fs::write(legacy.join("demo-skill/skill.toml"), "id = 'demo'").unwrap();
        std::fs::write(legacy.join("demo-skill/SKILL.md"), "# demo").unwrap();
        std::fs::write(legacy.join("mcp-lock.json"), "{}").unwrap();
        legacy
    }

    #[test]
    fn copies_legacy_tree_once_and_keeps_new_content() {
        let root = tempfile::tempdir().unwrap();
        let legacy = legacy_fixture(root.path());
        let data = root.path().join("data/skill");
        let target = data.join("skills");
        // 新目录里已有同名文件：以新目录为准，不覆盖。
        std::fs::create_dir_all(target.join("demo-skill")).unwrap();
        std::fs::write(target.join("demo-skill/SKILL.md"), "# newer").unwrap();

        assert_eq!(
            migrate_legacy_dir(&legacy, &target, &data).unwrap(),
            Some(2)
        );
        assert_eq!(
            std::fs::read_to_string(target.join("demo-skill/skill.toml")).unwrap(),
            "id = 'demo'"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("demo-skill/SKILL.md")).unwrap(),
            "# newer"
        );
        assert!(target.join("mcp-lock.json").is_file());
        // 完成标记落在新目录之外：注册表扫描不会把它当成技能。
        assert!(data.join(LEGACY_MIGRATION_MARKER).is_file());
        assert!(!target.join(LEGACY_MIGRATION_MARKER).exists());
        // 旧目录原样保留；再次调用幂等。
        assert!(legacy.join("demo-skill/skill.toml").is_file());
        assert_eq!(migrate_legacy_dir(&legacy, &target, &data).unwrap(), None);
    }

    #[test]
    fn absent_legacy_dir_only_writes_marker() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data/skill");
        let target = data.join("skills");
        assert_eq!(
            migrate_legacy_dir(&root.path().join("absent"), &target, &data).unwrap(),
            Some(0)
        );
        assert!(data.join(LEGACY_MIGRATION_MARKER).is_file());
    }

    #[test]
    fn failure_leaves_no_marker_so_next_start_retries() {
        let root = tempfile::tempdir().unwrap();
        let legacy = legacy_fixture(root.path());
        // 数据目录位置被一个普通文件占住：创建目录必然失败。
        let data = root.path().join("data");
        std::fs::write(&data, "not a directory").unwrap();
        let target = data.join("skill/skills");
        assert!(migrate_legacy_dir(&legacy, &target, &data).is_err());
        assert!(!data.join(LEGACY_MIGRATION_MARKER).exists());
    }
}
