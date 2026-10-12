//! Scheduler 旧数据迁移（插件自身行为）。
//!
//! 早期版本把任务与运行记录放在存储根下的 `scheduler/`（`jobs.json` 与
//! `runs/<job_id>.json`）；沙箱收拢写域后存储根只读，数据改放插件自有数据
//! 目录 `data/scheduler/`。首次启动把旧目录逐文件复制进新目录并写完成标记，
//! 之后直接跳过（幂等）。旧目录原样保留，由用户自行清理。
use std::path::Path;

use anyhow::{Context, Result};

/// 旧数据迁移完成标记文件名（位于插件自有数据目录内）。
const LEGACY_MIGRATION_MARKER: &str = ".legacy-migrated";
/// 需要迁移的任务清单文件。
const JOBS_FILE: &str = "jobs.json";
/// 需要迁移的运行记录目录。
const RUNS_DIR: &str = "runs";

/// 把旧 `scheduler/` 目录复制进 `target_dir`，返回本次复制的文件数。
///
/// 规则：
/// - `target_dir` 已有完成标记：直接返回 `None`；
/// - 只复制 `jobs.json` 与 `runs/*.json`，新目录已有同名文件时跳过（不覆盖）；
/// - 每个文件先写临时文件再原子改名，全部成功后才写完成标记；中途失败
///   返回错误且不写标记，调用方必须中止启动，避免在新目录生成空任务清单
///   后旧数据再也迁不过来。
pub(crate) fn migrate_legacy_dir(legacy_dir: &Path, target_dir: &Path) -> Result<Option<usize>> {
    let marker = target_dir.join(LEGACY_MIGRATION_MARKER);
    if marker.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(target_dir)
        .with_context(|| format!("创建插件数据目录失败: {}", target_dir.display()))?;
    let mut copied = 0;
    if legacy_dir.is_dir() && legacy_dir != target_dir {
        if copy_if_absent(&legacy_dir.join(JOBS_FILE), target_dir, JOBS_FILE)? {
            copied += 1;
        }
        let legacy_runs = legacy_dir.join(RUNS_DIR);
        if legacy_runs.is_dir() {
            let target_runs = target_dir.join(RUNS_DIR);
            std::fs::create_dir_all(&target_runs)
                .with_context(|| format!("创建运行记录目录失败: {}", target_runs.display()))?;
            let entries = std::fs::read_dir(&legacy_runs)
                .with_context(|| format!("读取旧运行记录失败: {}", legacy_runs.display()))?;
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if !name.ends_with(".json") || !entry.file_type()?.is_file() {
                    continue;
                }
                if copy_if_absent(&entry.path(), &target_runs, name)? {
                    copied += 1;
                }
            }
        }
    }
    std::fs::write(&marker, format!("{}\n", legacy_dir.display()))
        .with_context(|| format!("写入迁移完成标记失败: {}", marker.display()))?;
    Ok(Some(copied))
}

/// 旧文件存在且目标不存在时复制（临时文件 + 改名），返回是否复制。
fn copy_if_absent(legacy: &Path, target_dir: &Path, name: &str) -> Result<bool> {
    let target = target_dir.join(name);
    if target.exists() {
        return Ok(false);
    }
    match std::fs::metadata(legacy) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
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
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_jobs_and_runs_once() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("scheduler");
        std::fs::create_dir_all(legacy.join("runs")).unwrap();
        std::fs::write(legacy.join("jobs.json"), "[1]").unwrap();
        std::fs::write(legacy.join("runs").join("a.json"), "[]").unwrap();
        std::fs::write(legacy.join("runs").join("a.tmp"), "x").unwrap();
        let target = root.path().join("data").join("scheduler");

        assert_eq!(migrate_legacy_dir(&legacy, &target).unwrap(), Some(2));
        assert_eq!(
            std::fs::read_to_string(target.join("jobs.json")).unwrap(),
            "[1]"
        );
        assert!(target.join("runs").join("a.json").is_file());
        assert!(!target.join("runs").join("a.tmp").exists());
        assert!(legacy.join("jobs.json").is_file(), "旧数据保留");

        // 已迁移后新数据不会被旧数据覆盖。
        std::fs::write(target.join("jobs.json"), "[2]").unwrap();
        assert_eq!(migrate_legacy_dir(&legacy, &target).unwrap(), None);
        assert_eq!(
            std::fs::read_to_string(target.join("jobs.json")).unwrap(),
            "[2]"
        );
    }

    #[test]
    fn existing_target_file_is_kept_and_missing_legacy_marks_done() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("scheduler");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("jobs.json"), "[old]").unwrap();
        let target = root.path().join("data").join("scheduler");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("jobs.json"), "[new]").unwrap();
        assert_eq!(migrate_legacy_dir(&legacy, &target).unwrap(), Some(0));
        assert_eq!(
            std::fs::read_to_string(target.join("jobs.json")).unwrap(),
            "[new]"
        );

        let fresh = root.path().join("data").join("fresh");
        assert_eq!(
            migrate_legacy_dir(&root.path().join("absent"), &fresh).unwrap(),
            Some(0)
        );
        assert!(fresh.join(LEGACY_MIGRATION_MARKER).is_file());
    }
}
