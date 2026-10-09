//! 飞书主动推送目标存储。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Local;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

const TARGETS_FILE: &str = "targets.json";
const TARGETS_LOCK_FILE: &str = "targets.lock";

#[derive(Debug, Default, Serialize, Deserialize)]
struct TargetStore {
    #[serde(default = "store_version")]
    version: u32,
    #[serde(default)]
    targets: Vec<TargetRecord>,
}

fn store_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TargetRecord {
    target_id: String,
    chat_id: String,
    kind: String,
    enabled: bool,
    last_seen_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushTargetView {
    pub target_id: String,
    pub label: String,
    pub kind: String,
    pub enabled: bool,
    pub availability: String,
    pub last_seen_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limitation: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AuthorizedTarget {
    pub target_id: String,
    pub chat_id: String,
}

/// 收到用户消息后发现或刷新目标，并自动授权该会话用于主动推送。
///
/// 返回该会话对应的推送目标编号（与 `chat_id` 一一对应，跨消息稳定）。
pub fn upsert_discovered(directory: &Path, chat_id: &str, kind: &str) -> Result<String> {
    let chat_id = chat_id.trim();
    if chat_id.is_empty() {
        bail!("飞书会话 ID 不能为空");
    }
    let kind = if kind == "group" { "group" } else { "direct" };
    with_exclusive_store(directory, |store| {
        let now = Local::now().naive_local().to_string();
        if let Some(target) = store
            .targets
            .iter_mut()
            .find(|target| target.chat_id == chat_id)
        {
            target.kind = kind.to_string();
            target.enabled = true;
            target.last_seen_at = now;
            Ok(target.target_id.clone())
        } else {
            let target_id = scru128::new().to_string();
            store.targets.push(TargetRecord {
                target_id: target_id.clone(),
                chat_id: chat_id.to_string(),
                kind: kind.to_string(),
                enabled: true,
                last_seen_at: now,
            });
            Ok(target_id)
        }
    })
}

pub fn list_views(directory: &Path) -> Result<Vec<PushTargetView>> {
    let mut targets = read_store(directory)?.targets;
    targets.sort_by(|left, right| right.last_seen_at.cmp(&left.last_seen_at));
    Ok(targets.iter().map(view_from_record).collect())
}

pub fn delete(directory: &Path, target_id: &str) -> Result<()> {
    let target_id = target_id.trim();
    if target_id.is_empty() {
        bail!("推送目标 ID 不能为空");
    }
    with_exclusive_store(directory, |store| {
        let original_len = store.targets.len();
        store.targets.retain(|target| target.target_id != target_id);
        if store.targets.len() == original_len {
            return Err(anyhow!("未找到推送目标：{target_id}"));
        }
        Ok(())
    })
}

pub fn find_authorized(directory: &Path, target_id: &str) -> Result<Option<AuthorizedTarget>> {
    let store = read_store(directory)?;
    Ok(store
        .targets
        .into_iter()
        .find(|target| target.target_id == target_id && target.enabled)
        .map(|target| AuthorizedTarget {
            target_id: target.target_id,
            chat_id: target.chat_id,
        }))
}

fn view_from_record(target: &TargetRecord) -> PushTargetView {
    let kind_label = if target.kind == "group" {
        "飞书群聊"
    } else {
        "飞书私聊"
    };
    PushTargetView {
        target_id: target.target_id.clone(),
        label: format!("{kind_label}，最近使用于 {}", target.last_seen_at),
        kind: target.kind.clone(),
        enabled: target.enabled,
        availability: "ready".to_string(),
        last_seen_at: target.last_seen_at.clone(),
        limitation: None,
    }
}

fn with_exclusive_store<T>(
    directory: &Path,
    update: impl FnOnce(&mut TargetStore) -> Result<T>,
) -> Result<T> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("创建飞书插件数据目录失败：{}", directory.display()))?;
    let lock_path = directory.join(TARGETS_LOCK_FILE);
    let lock = secure_open(&lock_path, false)?;
    lock.lock_exclusive()
        .with_context(|| format!("锁定飞书推送目标失败：{}", lock_path.display()))?;

    let result = (|| {
        let mut store = read_store(directory)?;
        let output = update(&mut store)?;
        write_store(directory, &store)?;
        Ok(output)
    })();
    let unlock_result = FileExt::unlock(&lock)
        .with_context(|| format!("解锁飞书推送目标失败：{}", lock_path.display()));
    match (result, unlock_result) {
        (Ok(output), Ok(())) => Ok(output),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn read_store(directory: &Path) -> Result<TargetStore> {
    let path = directory.join(TARGETS_FILE);
    if !path.exists() {
        return Ok(TargetStore {
            version: store_version(),
            targets: Vec::new(),
        });
    }
    reject_symlink(&path)?;
    let content = std::fs::read(&path)
        .with_context(|| format!("读取飞书推送目标失败：{}", path.display()))?;
    let store: TargetStore = serde_json::from_slice(&content)
        .with_context(|| format!("解析飞书推送目标失败：{}", path.display()))?;
    if store.version != store_version() {
        bail!("飞书推送目标版本不受支持：{}", store.version);
    }
    Ok(store)
}

fn write_store(directory: &Path, store: &TargetStore) -> Result<()> {
    let path = directory.join(TARGETS_FILE);
    reject_symlink(&path)?;
    let temp = path.with_file_name(format!(".targets-{}.tmp", scru128::new()));
    let content = serde_json::to_vec_pretty(store).context("序列化飞书推送目标失败")?;
    let mut file = secure_open(&temp, true)?;
    file.write_all(&content)
        .with_context(|| format!("写入飞书推送目标失败：{}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("同步飞书推送目标失败：{}", temp.display()))?;
    drop(file);
    if let Err(error) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error).with_context(|| format!("替换飞书推送目标失败：{}", path.display()));
    }
    set_private_permissions(&path)?;
    Ok(())
}

fn secure_open(path: &Path, create_new: bool) -> Result<File> {
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("打开飞书推送目标文件失败：{}", path.display()))?;
    set_private_permissions(path)?;
    Ok(file)
}

fn set_private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("设置飞书推送目标权限失败：{}", path.display()))?;
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("拒绝使用符号链接：{}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("检查文件失败：{}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovered_targets_are_stable_and_deletable() {
        let directory = tempfile::tempdir().unwrap();
        let first = upsert_discovered(directory.path(), "chat-1", "group").unwrap();
        let again = upsert_discovered(directory.path(), "chat-1", "group").unwrap();
        assert_eq!(first, again);
        let found = find_authorized(directory.path(), &first).unwrap().unwrap();
        assert_eq!(found.chat_id, "chat-1");
        assert_eq!(list_views(directory.path()).unwrap().len(), 1);
        delete(directory.path(), &first).unwrap();
        assert!(find_authorized(directory.path(), &first).unwrap().is_none());
        assert!(delete(directory.path(), &first).is_err());
    }
}
