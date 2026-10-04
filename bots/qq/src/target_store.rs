//! QQ 回复窗口推送目标存储。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Local;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::reply_quota::{QuotaPolicy, QuotaState};

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
    recipient_id: String,
    kind: String,
    message_id: String,
    enabled: bool,
    last_seen_at: String,
    /// 当前回复窗口的额度状态；旧记录缺省为空窗口。
    #[serde(default)]
    quota: QuotaState,
}

/// QQ 被动回复规则（官方文档「消息收发概述 · 频率与时效规则」）：
/// 单聊每条消息 60 分钟内可回复 4 次，群聊 5 分钟内可回复 5 次。
/// 有效期扣除安全余量，避免临界时刻发送失败。
pub fn quota_policy(kind: &str) -> QuotaPolicy {
    if kind == "group" {
        QuotaPolicy {
            max_replies: 5,
            window_secs: Some(5 * 60 - 20),
            progress_interval_secs: 30,
            segment_chars: 2000,
            max_pending_progress: 5,
        }
    } else {
        QuotaPolicy {
            max_replies: 4,
            window_secs: Some(60 * 60 - 120),
            progress_interval_secs: 60,
            segment_chars: 2000,
            max_pending_progress: 5,
        }
    }
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
    pub recipient_id: String,
    pub kind: String,
    pub message_id: String,
}

#[derive(Debug, Deserialize)]
pub struct DeleteTargetRequest {
    pub target_id: String,
}

/// 收到用户消息后保存最新回复消息编号，并自动授权该会话用于主动推送。
///
/// 返回该会话对应的推送目标编号（与 `kind + recipient_id` 一一对应，跨消息稳定）。
pub fn upsert_discovered(kind: &str, recipient_id: &str, message_id: &str) -> Result<String> {
    let recipient_id = recipient_id.trim();
    let message_id = message_id.trim();
    if recipient_id.is_empty() || message_id.is_empty() {
        bail!("QQ 推送目标缺少收件人或回复上下文");
    }
    let kind = if kind == "group" { "group" } else { "direct" };
    with_exclusive_store(|store| {
        let now = Local::now().naive_local();
        let now_text = now.to_string();
        if let Some(target) = store
            .targets
            .iter_mut()
            .find(|target| target.kind == kind && target.recipient_id == recipient_id)
        {
            target.message_id = message_id.to_string();
            target.enabled = true;
            target.last_seen_at = now_text;
            target.quota.observe_inbound(message_id, now);
            Ok(target.target_id.clone())
        } else {
            let target_id = scru128::new().to_string();
            let mut quota = QuotaState::default();
            quota.observe_inbound(message_id, now);
            store.targets.push(TargetRecord {
                target_id: target_id.clone(),
                recipient_id: recipient_id.to_string(),
                kind: kind.to_string(),
                message_id: message_id.to_string(),
                enabled: true,
                last_seen_at: now_text,
                quota,
            });
            Ok(target_id)
        }
    })
}

/// 在跨进程文件锁内读取并更新目标的回复窗口额度。
///
/// `plan` 接收当前窗口对应的 `msg_id`、额度策略与可变状态，返回规划结果；
/// 闭包结束后状态立即落盘，保证并发的 MCP 子进程不会重复占用同一额度。
/// 目标不存在、未授权或窗口已被新消息替换时返回 `None`。
pub fn update_quota<T>(
    target_id: &str,
    expected_message_id: &str,
    plan: impl FnOnce(&QuotaPolicy, &mut QuotaState) -> T,
) -> Result<Option<T>> {
    with_exclusive_store(|store| {
        let Some(target) = store
            .targets
            .iter_mut()
            .find(|target| target.target_id == target_id && target.enabled)
        else {
            return Ok(None);
        };
        if target.message_id != expected_message_id {
            return Ok(None);
        }
        let policy = quota_policy(&target.kind);
        Ok(Some(plan(&policy, &mut target.quota)))
    })
}

pub fn list_views() -> Result<Vec<PushTargetView>> {
    let mut targets = read_store()?.targets;
    targets.sort_by(|left, right| right.last_seen_at.cmp(&left.last_seen_at));
    Ok(targets.iter().map(view_from_record).collect())
}

pub fn list_enabled_views() -> Result<Vec<PushTargetView>> {
    Ok(list_views()?
        .into_iter()
        .filter(|target| target.enabled)
        .collect())
}

pub fn delete(target_id: &str) -> Result<()> {
    let target_id = target_id.trim();
    if target_id.is_empty() {
        bail!("推送目标 ID 不能为空");
    }
    with_exclusive_store(|store| {
        let original_len = store.targets.len();
        store.targets.retain(|target| target.target_id != target_id);
        if store.targets.len() == original_len {
            return Err(anyhow!("未找到推送目标：{target_id}"));
        }
        Ok(())
    })
}

pub fn find_authorized(target_id: &str) -> Result<Option<AuthorizedTarget>> {
    let store = read_store()?;
    Ok(store
        .targets
        .into_iter()
        .find(|target| target.target_id == target_id && target.enabled)
        .map(|target| AuthorizedTarget {
            target_id: target.target_id,
            recipient_id: target.recipient_id,
            kind: target.kind,
            message_id: target.message_id,
        }))
}

fn view_from_record(target: &TargetRecord) -> PushTargetView {
    let kind_label = if target.kind == "group" {
        "QQ 群聊"
    } else {
        "QQ 私聊"
    };
    let has_context =
        !target.recipient_id.trim().is_empty() && !target.message_id.trim().is_empty();
    PushTargetView {
        target_id: target.target_id.clone(),
        label: format!("{kind_label}，最近使用于 {}", target.last_seen_at),
        kind: target.kind.clone(),
        enabled: target.enabled,
        availability: if has_context {
            "reply_window".to_string()
        } else {
            "unavailable".to_string()
        },
        last_seen_at: target.last_seen_at.clone(),
        limitation: Some(if has_context {
            "只能使用最近一条 QQ 消息的回复上下文，是否仍有效由 QQ 平台决定".to_string()
        } else {
            "缺少最近 QQ 消息的回复上下文，请先从移动端发送一条消息".to_string()
        }),
    }
}

fn with_exclusive_store<T>(update: impl FnOnce(&mut TargetStore) -> Result<T>) -> Result<T> {
    let directory = runtime_directory()?;
    let lock_path = directory.join(TARGETS_LOCK_FILE);
    let lock = secure_open(&lock_path, false)?;
    lock.lock_exclusive()
        .with_context(|| format!("锁定 QQ 推送目标失败：{}", lock_path.display()))?;

    let result = (|| {
        let mut store = read_store()?;
        let output = update(&mut store)?;
        write_store(&store)?;
        Ok(output)
    })();
    let unlock_result = FileExt::unlock(&lock)
        .with_context(|| format!("解锁 QQ 推送目标失败：{}", lock_path.display()));
    match (result, unlock_result) {
        (Ok(output), Ok(())) => Ok(output),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn read_store() -> Result<TargetStore> {
    let path = targets_path()?;
    if !path.exists() {
        return Ok(TargetStore {
            version: store_version(),
            targets: Vec::new(),
        });
    }
    reject_symlink(&path)?;
    let content = std::fs::read(&path)
        .with_context(|| format!("读取 QQ 推送目标失败：{}", path.display()))?;
    let store: TargetStore = serde_json::from_slice(&content)
        .with_context(|| format!("解析 QQ 推送目标失败：{}", path.display()))?;
    if store.version != store_version() {
        bail!("QQ 推送目标版本不受支持：{}", store.version);
    }
    Ok(store)
}

fn write_store(store: &TargetStore) -> Result<()> {
    let path = targets_path()?;
    reject_symlink(&path)?;
    let temp = path.with_file_name(format!(".targets-{}.tmp", scru128::new()));
    let content = serde_json::to_vec_pretty(store).context("序列化 QQ 推送目标失败")?;
    let mut file = secure_open(&temp, true)?;
    file.write_all(&content)
        .with_context(|| format!("写入 QQ 推送目标失败：{}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("同步 QQ 推送目标失败：{}", temp.display()))?;
    drop(file);
    if let Err(error) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error).with_context(|| format!("替换 QQ 推送目标失败：{}", path.display()));
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
        .with_context(|| format!("打开 QQ 推送目标文件失败：{}", path.display()))?;
    set_private_permissions(path)?;
    Ok(file)
}

fn set_private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("设置 QQ 推送目标权限失败：{}", path.display()))?;
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

pub fn runtime_directory() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("获取 QQ bot 路径失败")?;
    executable
        .parent()
        .map(Path::to_path_buf)
        .context("QQ bot 路径缺少父目录")
}

fn targets_path() -> Result<PathBuf> {
    Ok(runtime_directory()?.join(TARGETS_FILE))
}
