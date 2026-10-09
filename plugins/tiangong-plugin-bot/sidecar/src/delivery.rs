//! 推送投递记录（三平台共用）：幂等键占位、结果落盘与结果构造。
//!
//! 每次发送先以 `target_id + idempotency_key` 摘要为文件名独占创建记录
//! （pending），发送完成后原子替换为最终结果；相同幂等键再次调用直接
//! 返回已记录结果，不重复发送。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Local;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const MAX_TEXT_CHARS: usize = 4000;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DeliveryResult {
    pub(crate) delivery_id: String,
    pub(crate) status: String,
    pub(crate) platform_message_id: Option<String>,
    pub(crate) sent_at: String,
    pub(crate) retryable: bool,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredDelivery {
    target_id: String,
    pub(crate) result: DeliveryResult,
}

pub(crate) enum DeliveryClaim {
    Existing(DeliveryResult),
    New {
        path: PathBuf,
        delivery: StoredDelivery,
    },
}

pub(crate) enum SendFailure {
    Rejected(String),
    Unknown(String),
}

/// 校验幂等键；不合法时返回拒绝结果。
pub(crate) fn check_idempotency_key(idempotency_key: &str) -> Option<DeliveryResult> {
    (idempotency_key.is_empty() || idempotency_key.len() > MAX_IDEMPOTENCY_KEY_BYTES)
        .then(|| rejected("幂等键不能为空且不能超过 256 字节"))
}

pub(crate) fn rejected(message: &str) -> DeliveryResult {
    DeliveryResult {
        delivery_id: scru128::new().to_string(),
        status: "rejected".to_string(),
        platform_message_id: None,
        sent_at: now_string(),
        retryable: false,
        message: message.to_string(),
    }
}

/// 拆分发送中途失败时，说明已送达的段数，避免 Agent 误判为全部未发送。
pub(crate) fn partial_failure(failure: SendFailure, sent: usize, total: usize) -> SendFailure {
    if total <= 1 {
        return failure;
    }
    let note = format!("（共 {total} 段，已发送 {sent} 段）");
    match failure {
        SendFailure::Rejected(message) => SendFailure::Rejected(format!("{message}{note}")),
        SendFailure::Unknown(message) => SendFailure::Unknown(format!("{message}{note}")),
    }
}

/// 进展消息被限流暂存：记录为 queued，相同幂等键重复调用不会再次暂存。
pub(crate) fn finish_queued(
    path: &Path,
    mut delivery: StoredDelivery,
    pending: usize,
) -> DeliveryResult {
    delivery.result.status = "queued".to_string();
    delivery.result.sent_at = now_string();
    delivery.result.message = format!(
        "进展发送过于频繁，已暂存（共 {pending} 条），将合并到下一条进展消息中；最终答复会覆盖未发出的暂存进展"
    );
    if let Err(error) = replace_delivery(path, &delivery) {
        delivery.result.status = "unknown".to_string();
        delivery.result.message = format!("暂存结果保存失败：{error}");
    }
    delivery.result
}

pub(crate) fn finish_delivery(
    path: &Path,
    mut delivery: StoredDelivery,
    outcome: Result<Option<String>, SendFailure>,
    accepted_message: &str,
) -> DeliveryResult {
    match outcome {
        Ok(platform_message_id) => {
            delivery.result.status = "accepted".to_string();
            delivery.result.platform_message_id = platform_message_id;
            delivery.result.message = accepted_message.to_string();
        }
        Err(SendFailure::Rejected(message)) => {
            delivery.result.status = "rejected".to_string();
            delivery.result.message = message;
        }
        Err(SendFailure::Unknown(message)) => {
            delivery.result.status = "unknown".to_string();
            delivery.result.message = message;
        }
    }
    delivery.result.sent_at = now_string();
    if let Err(error) = replace_delivery(path, &delivery) {
        delivery.result.status = "unknown".to_string();
        delivery.result.retryable = false;
        delivery.result.message = format!("投递结果可能已产生，但保存状态失败：{error}");
    }
    delivery.result
}

/// 占用幂等键：首次调用创建 pending 记录，重复调用返回已有结果。
pub(crate) fn claim_delivery(
    data_dir: &Path,
    target_id: &str,
    idempotency_key: &str,
) -> Result<DeliveryClaim> {
    let path = delivery_path(data_dir, target_id, idempotency_key)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            let delivery = StoredDelivery {
                target_id: target_id.to_string(),
                result: DeliveryResult {
                    delivery_id: scru128::new().to_string(),
                    status: "pending".to_string(),
                    platform_message_id: None,
                    sent_at: now_string(),
                    retryable: false,
                    message: "正在投递".to_string(),
                },
            };
            let content = serde_json::to_vec(&delivery).context("序列化投递记录失败")?;
            file.write_all(&content)
                .with_context(|| format!("写入投递记录失败：{}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("同步投递记录失败：{}", path.display()))?;
            Ok(DeliveryClaim::New { path, delivery })
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let delivery = read_existing_delivery(&path)?;
            let result = if delivery.result.status == "pending" {
                DeliveryResult {
                    status: "unknown".to_string(),
                    message: "相同幂等键已有投递正在执行或结果未知，未重复发送".to_string(),
                    ..delivery.result
                }
            } else {
                delivery.result
            };
            Ok(DeliveryClaim::Existing(result))
        }
        Err(error) => Err(error).with_context(|| format!("创建投递记录失败：{}", path.display())),
    }
}

fn read_existing_delivery(path: &Path) -> Result<StoredDelivery> {
    for attempt in 0..10 {
        let content =
            std::fs::read(path).with_context(|| format!("读取投递记录失败：{}", path.display()))?;
        if let Ok(delivery) = serde_json::from_slice(&content) {
            return Ok(delivery);
        }
        if attempt < 9 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    bail!("解析投递记录失败：{}", path.display())
}

fn replace_delivery(path: &Path, delivery: &StoredDelivery) -> Result<()> {
    let parent = path.parent().context("投递记录路径缺少父目录")?;
    let temp = parent.join(format!(".delivery-{}.tmp", scru128::new()));
    let content = serde_json::to_vec(delivery).context("序列化投递结果失败")?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .with_context(|| format!("创建投递结果临时文件失败：{}", temp.display()))?;
    file.write_all(&content)
        .with_context(|| format!("写入投递结果失败：{}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("同步投递结果失败：{}", temp.display()))?;
    drop(file);
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error).with_context(|| format!("替换投递记录失败：{}", path.display()));
    }
    Ok(())
}

fn delivery_path(data_dir: &Path, target_id: &str, idempotency_key: &str) -> Result<PathBuf> {
    let directory = data_dir.join("deliveries");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("创建投递记录目录失败：{}", directory.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("设置投递记录目录权限失败：{}", directory.display()))?;
    }
    let mut hasher = Sha256::new();
    hasher.update(target_id.as_bytes());
    hasher.update([0]);
    hasher.update(idempotency_key.as_bytes());
    let name = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(directory.join(format!("{name}.json")))
}

pub(crate) fn now_string() -> String {
    Local::now().naive_local().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_key_reuses_recorded_delivery() {
        let data_dir = tempfile::tempdir().unwrap();
        let DeliveryClaim::New { path, delivery } =
            claim_delivery(data_dir.path(), "t", "k").unwrap()
        else {
            panic!("首次应创建投递记录");
        };
        let finished = finish_delivery(&path, delivery, Ok(Some("m1".into())), "ok");
        assert_eq!(finished.status, "accepted");
        match claim_delivery(data_dir.path(), "t", "k").unwrap() {
            DeliveryClaim::Existing(result) => {
                assert_eq!(result.platform_message_id.as_deref(), Some("m1"))
            }
            DeliveryClaim::New { .. } => panic!("相同幂等键不应重复投递"),
        }
        assert!(matches!(
            claim_delivery(data_dir.path(), "t", "other").unwrap(),
            DeliveryClaim::New { .. }
        ));
    }

    #[test]
    fn pending_claim_reports_unknown_and_keys_are_validated() {
        let data_dir = tempfile::tempdir().unwrap();
        let _ = claim_delivery(data_dir.path(), "t", "k").unwrap();
        match claim_delivery(data_dir.path(), "t", "k").unwrap() {
            DeliveryClaim::Existing(result) => assert_eq!(result.status, "unknown"),
            DeliveryClaim::New { .. } => panic!("pending 记录不应重复发送"),
        }
        assert!(check_idempotency_key("").is_some());
        assert!(check_idempotency_key(&"x".repeat(257)).is_some());
        assert!(check_idempotency_key("ok").is_none());
    }
}
