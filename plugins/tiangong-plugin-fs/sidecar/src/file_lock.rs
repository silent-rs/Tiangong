//! 文件编辑锁：sidecar 进程级共享锁表。
//!
//! 一份 sidecar 进程内的「文件路径锁表」，以规范化后的绝对路径为唯一标识。
//! 锁只在单次写操作期间持有（加锁+写+解锁一次完成），保证同一调用内多文件
//! 补丁全有或全无，并经 `file_lock_changed` 事件供锁面板显示。
//!
//! ## 语义
//!
//! - **工具调用级**：fs 写工具（`write_file` / `replace_in_file` / `apply_patch`）
//!   执行前对目标路径加锁、执行后释放，对模型完全透明。
//! - **不区分调用方**：文件只要已有锁，任何后续写操作都拒绝——防止并发写
//!   同一文件互相覆盖。
//! - **全有或全无**：`apply_patch` 一次可能涉及多个文件，任一被占用则本次
//!   操作全部不加锁。
//! - **租约兜底**：锁持有方异常卡死时，过期锁（默认 30s）在下次加锁时
//!   静默清理，新操作可重新取得锁。
//! - **操作编号防误删**：旧操作超时后新操作可能取得锁；旧操作随后结束时，
//!   靠 `operation_id` 校验，不会误删新操作的锁。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 锁租约（秒）：超过该时长未被释放的锁视为过期，下次加锁时静默清理。
pub const FILE_LOCK_LEASE_SECS: i64 = 30;

/// 单条锁记录：只保存获取时间与本次操作的唯一编号。
#[derive(Debug, Clone)]
struct LockRecord {
    acquired_at: chrono::NaiveDateTime,
    operation_id: String,
}

/// sidecar 进程级共享锁表句柄（零字段，全静态）。
pub struct FileLockTable;

impl FileLockTable {
    /// 取进程内唯一的锁表（懒初始化）。
    fn shared() -> &'static Mutex<HashMap<PathBuf, LockRecord>> {
        static TABLE: OnceLock<Mutex<HashMap<PathBuf, LockRecord>>> = OnceLock::new();
        TABLE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// 尝试对一组路径原子加锁（全有或全无）。
    ///
    /// 成功返回本次操作的 `operation_id`（调用方需保存，用于后续 [`release`]
    /// 校验）；任一路径已被占用（且未过期）则全部不加锁，返回错误文案。
    /// 加锁过程中会先清理本次涉及路径上的过期记录。
    pub fn acquire(paths: &[PathBuf], now: &chrono::NaiveDateTime) -> Result<String, String> {
        let mut table = Self::shared()
            .lock()
            .map_err(|_| "文件锁状态锁定失败".to_string())?;

        // 先去重 + 清理本次涉及路径上的过期记录。
        let unique = dedup_paths(paths);
        purge_expired_in(&mut table, &unique, now);

        // 任一路径已被占用则整体失败。
        if let Some(path) = unique.iter().find(|p| table.contains_key(*p)) {
            return Err(format!("文件 {} 正被其他写操作占用", path.display()));
        }

        let operation_id = scru128::new().to_string();
        for path in &unique {
            table.insert(
                path.clone(),
                LockRecord {
                    acquired_at: *now,
                    operation_id: operation_id.clone(),
                },
            );
        }
        Ok(operation_id)
    }

    /// 释放一组路径上由 `operation_id` 取得的锁，返回实际被释放的路径列表。
    ///
    /// 仅删除 `operation_id` 匹配的记录——若某路径已被新操作接管（旧操作
    /// 超时后），则该记录的 `operation_id` 不同，不会被误删，也不会出现在
    /// 返回值里。调用方应只对返回的路径发送 `unlocked` 事件。
    pub fn release(paths: &[PathBuf], operation_id: &str) -> Vec<PathBuf> {
        let Ok(mut table) = Self::shared().lock() else {
            return Vec::new();
        };
        let mut released = Vec::new();
        for path in dedup_paths(paths) {
            if let Some(record) = table.get(&path)
                && record.operation_id == operation_id
            {
                table.remove(&path);
                released.push(path);
            }
        }
        released
    }
}

/// 对一组路径去重（保留顺序，按值相等去重）。
fn dedup_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .iter()
        .filter(|p| seen.insert((*p).clone()))
        .cloned()
        .collect()
}

/// 清理指定路径集合中已过期的记录（惰性清理）。
fn purge_expired_in(
    table: &mut HashMap<PathBuf, LockRecord>,
    paths: &[PathBuf],
    now: &chrono::NaiveDateTime,
) {
    for path in paths {
        if let Some(record) = table.get(path)
            && is_expired(&record.acquired_at, now)
        {
            table.remove(path);
        }
    }
}

fn is_expired(locked_at: &chrono::NaiveDateTime, now: &chrono::NaiveDateTime) -> bool {
    (*now - *locked_at).num_seconds() > FILE_LOCK_LEASE_SECS
}

/// 把任意路径规范化为锁表可用的稳定 key。
///
/// - 已存在的路径：直接 `canonicalize`（解析软链接到真实路径）。
/// - 不存在的路径（如 `apply_patch` 的 add 分支）：向上查找最近的已存在父
///   目录 `canonicalize`，再拼回剩余的不存在部分。这样通过软链接访问与
///   通过真实路径访问会得到同一个 key，避免重复加锁。
/// - 最后统一消除 `.` / `..` 分量，防止 `new/../a.txt` 与 `a.txt` 指向同一
///   文件却得到两个不同的 key（绕过锁）。
///
/// `canonicalize` 完全失败时（连父目录都不存在）回退原路径（再消除 `..`）。
pub fn canonicalize_for_lock(path: &Path) -> PathBuf {
    // 已存在：直接 canonicalize（已解析软链接与 ..）。
    if path.exists() {
        return path.canonicalize().unwrap_or_else(|_| normalize_dots(path));
    }

    // 不存在：向上找最近的已存在祖先。
    let mut existing = path.parent();
    while let Some(ancestor) = existing
        && !ancestor.exists()
    {
        existing = ancestor.parent();
    }
    let Some(ancestor) = existing else {
        // 连父目录都不存在：回退原路径并至少消除 .. 。
        return normalize_dots(path);
    };
    let canonical_ancestor = match ancestor.canonicalize() {
        Ok(c) => c,
        Err(_) => return normalize_dots(path),
    };
    // 拼回剩余的不存在部分，再消除拼回过程中残留的 .. 。
    let remaining = path.strip_prefix(ancestor).unwrap_or(path);
    normalize_dots(&canonical_ancestor.join(remaining))
}

/// 手动消除路径中的 `.` 与 `..` 分量（不触碰文件系统）。
fn normalize_dots(path: &Path) -> PathBuf {
    use std::path::Component;

    fn is_poppable(result: &Path) -> bool {
        result
            .components()
            .next_back()
            .is_some_and(|last| matches!(last, Component::Normal(_)))
    }

    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if is_poppable(&result) {
                    result.pop();
                } else {
                    result.push("..");
                }
            }
            Component::CurDir => {}
            c => result.push(c.as_os_str()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, NaiveDate};

    use super::*;

    fn instant() -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 7, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    #[test]
    fn acquire_releases_single_path() {
        let now = instant();
        let path = PathBuf::from("/tmp/fs-lock-test-single.txt");
        FileLockTable::release(std::slice::from_ref(&path), "stale");

        let op = FileLockTable::acquire(std::slice::from_ref(&path), &now).unwrap();
        let err = FileLockTable::acquire(std::slice::from_ref(&path), &now).unwrap_err();
        assert!(err.contains("正被其他写操作占用"), "{err}");

        FileLockTable::release(std::slice::from_ref(&path), &op);
        let op2 = FileLockTable::acquire(std::slice::from_ref(&path), &now).unwrap();
        FileLockTable::release(std::slice::from_ref(&path), &op2);
    }

    #[test]
    fn acquire_is_all_or_nothing() {
        let now = instant();
        let a = PathBuf::from("/tmp/fs-lock-test-all-a.txt");
        let b = PathBuf::from("/tmp/fs-lock-test-all-b.txt");
        FileLockTable::release(&[a.clone(), b.clone()], "stale");

        let op_hold = FileLockTable::acquire(std::slice::from_ref(&a), &now).unwrap();
        let err = FileLockTable::acquire(&[a.clone(), b.clone()], &now).unwrap_err();
        assert!(err.contains("正被其他写操作占用"));

        FileLockTable::release(std::slice::from_ref(&a), &op_hold);
        let op_both = FileLockTable::acquire(&[a.clone(), b.clone()], &now).unwrap();
        FileLockTable::release(&[a.clone(), b.clone()], &op_both);
    }

    #[test]
    fn expired_lock_can_be_reacquired() {
        let now = instant();
        let path = PathBuf::from("/tmp/fs-lock-test-expired.txt");
        FileLockTable::release(std::slice::from_ref(&path), "stale");

        let op_old = FileLockTable::acquire(std::slice::from_ref(&path), &now).unwrap();
        assert!(
            FileLockTable::acquire(std::slice::from_ref(&path), &(now + Duration::seconds(10)))
                .is_err()
        );

        let later = now + Duration::seconds(31);
        let op_new = FileLockTable::acquire(std::slice::from_ref(&path), &later).unwrap();

        // 旧操作结束后不应误删新操作的锁。
        FileLockTable::release(std::slice::from_ref(&path), &op_old);
        assert!(FileLockTable::acquire(std::slice::from_ref(&path), &later).is_err());

        FileLockTable::release(std::slice::from_ref(&path), &op_new);
    }

    #[test]
    fn canonicalize_unifies_symlink_and_real_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join("real.txt");
        std::fs::write(&real, "x").unwrap();
        #[cfg(unix)]
        let link = dir.path().join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        let link = real.clone();

        let key_real = canonicalize_for_lock(&real);
        let key_link = canonicalize_for_lock(&link);
        assert_eq!(key_real, key_link, "软链接与真实路径应归一为同一锁 key");
    }

    #[test]
    fn canonicalize_handles_nonexistent_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("sub").join("missing.txt");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();

        let key = canonicalize_for_lock(&missing);
        let canonical_parent = dir.path().canonicalize().unwrap();
        assert!(
            key.starts_with(&canonical_parent),
            "key {key:?} 应以已存在父目录 canonical 路径 {:?} 为前缀",
            canonical_parent
        );
        assert!(key.ends_with("sub/missing.txt"));
    }

    #[test]
    fn same_agent_concurrent_write_is_rejected() {
        let now = instant();
        let path = PathBuf::from("/tmp/fs-lock-test-same-agent.txt");
        FileLockTable::release(std::slice::from_ref(&path), "stale");

        let op = FileLockTable::acquire(std::slice::from_ref(&path), &now).unwrap();
        let err = FileLockTable::acquire(std::slice::from_ref(&path), &now).unwrap_err();
        assert!(err.contains("正被其他写操作占用"));
        FileLockTable::release(std::slice::from_ref(&path), &op);
    }

    #[test]
    fn canonicalize_collapses_dotdot_to_prevent_lock_bypass() {
        let dir = tempfile::TempDir::new().unwrap();
        let direct = dir.path().join("a.txt");
        let via_dotdot = dir.path().join("new").join("..").join("a.txt");

        let key_direct = canonicalize_for_lock(&direct);
        let key_dotdot = canonicalize_for_lock(&via_dotdot);
        assert_eq!(
            key_direct, key_dotdot,
            "`a.txt` 与 `new/../a.txt` 应归一为同一锁 key（direct={key_direct:?}, dotdot={key_dotdot:?}）"
        );

        let now = instant();
        FileLockTable::release(std::slice::from_ref(&key_direct), "stale");
        let op = FileLockTable::acquire(std::slice::from_ref(&key_direct), &now).unwrap();
        let err = FileLockTable::acquire(std::slice::from_ref(&key_dotdot), &now).unwrap_err();
        assert!(err.contains("正被其他写操作占用"));
        FileLockTable::release(std::slice::from_ref(&key_direct), &op);
    }

    #[test]
    fn release_returns_only_actually_released_paths() {
        let now = instant();
        let path = PathBuf::from("/tmp/fs-lock-test-release-return.txt");
        FileLockTable::release(std::slice::from_ref(&path), "stale");

        let op_old = FileLockTable::acquire(std::slice::from_ref(&path), &now).unwrap();
        let later = now + Duration::seconds(31);
        let op_new = FileLockTable::acquire(std::slice::from_ref(&path), &later).unwrap();

        let released = FileLockTable::release(std::slice::from_ref(&path), &op_old);
        assert!(
            released.is_empty(),
            "旧操作不应释放已被新操作接管的锁，却返回 {released:?}"
        );

        let released = FileLockTable::release(std::slice::from_ref(&path), &op_new);
        assert_eq!(released.len(), 1);
        assert_eq!(released[0], path);
    }

    #[test]
    fn release_skips_unknown_operation_id() {
        let now = instant();
        let path = PathBuf::from("/tmp/fs-lock-test-release-unknown.txt");
        FileLockTable::release(std::slice::from_ref(&path), "stale");

        let released = FileLockTable::release(std::slice::from_ref(&path), "never-acquired");
        assert!(released.is_empty());

        let op = FileLockTable::acquire(std::slice::from_ref(&path), &now).unwrap();
        let released = FileLockTable::release(std::slice::from_ref(&path), &op);
        assert_eq!(released.len(), 1);
        let released_again = FileLockTable::release(std::slice::from_ref(&path), &op);
        assert!(released_again.is_empty());
    }
}
