//! 平台抽象：飞书、QQ、微信各自实现，服务层统一调度长连接、设置页操作与推送工具。

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::delivery::DeliveryResult;
use crate::tools::{SendLocalMediaInput, SendTextInput};

/// 长连接停止信号（凭据/server 变化或宿主退出时完成）。
pub(crate) type StopSignal = Pin<Box<dyn Future<Output = ()> + Send>>;

/// 本机天工 server 连接信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerEndpoint {
    pub(crate) url: String,
    pub(crate) token: Option<String>,
}

/// 推送目标视图（设置页与 list_push_targets 共用）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TargetView {
    pub(crate) target_id: String,
    pub(crate) platform: &'static str,
    pub(crate) label: String,
    pub(crate) kind: String,
    pub(crate) enabled: bool,
    pub(crate) availability: String,
    pub(crate) last_seen_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) limitation: Option<String>,
}

#[async_trait::async_trait]
pub(crate) trait BotPlatform: Send + Sync + 'static {
    /// 平台标识（数据子目录名、设置页参数），如 `feishu`。
    fn id(&self) -> &'static str;
    /// 展示名，如「飞书」。
    fn name(&self) -> &'static str;
    /// 平台是否只允许一条接收连接（多宿主同时建连会抢消息或游标）。
    /// 为 true 时服务层以文件锁保证同机只有一个持有者建连。
    fn exclusive_connection(&self) -> bool;

    /// 已配置时返回展示用的账号标识（App ID 或脱敏 token），未配置返回 None。
    fn configured_account(&self, dir: &Path) -> Result<Option<String>>;
    /// 发起扫码绑定，返回含 `qr_url` 的会话 JSON（原样回传给 poll）。
    async fn provision_begin(&self, dir: &Path) -> Result<Value>;
    /// 轮询扫码结果；成功时把凭据保存到 `dir`。返回含 `status` 的 JSON。
    async fn provision_poll(&self, dir: &Path, session: Value) -> Result<Value>;
    /// 保存手工填写的凭据。
    fn save_credentials(&self, dir: &Path, credentials: Value) -> Result<()>;
    /// 删除凭据（解除绑定）。
    fn clear_credentials(&self, dir: &Path) -> Result<()>;
    /// 凭据变化后丢弃缓存的平台客户端（令牌缓存等）。
    async fn invalidate(&self);

    fn list_targets(&self, dir: &Path) -> Result<Vec<TargetView>>;
    fn delete_target(&self, dir: &Path, target_id: &str) -> Result<()>;
    /// 推送目标是否属于本平台（不论是否启用）。
    fn owns_target(&self, dir: &Path, target_id: &str) -> Result<bool>;

    /// 运行接收连接直到 `stop` 完成；需要已配置凭据。
    async fn run(&self, dir: &Path, server: ServerEndpoint, stop: StopSignal) -> Result<()>;

    async fn send_text(&self, dir: &Path, input: &SendTextInput) -> Result<DeliveryResult>;
    async fn send_media(
        &self,
        dir: &Path,
        input: &SendLocalMediaInput,
        workspace: Option<&Path>,
        image: bool,
    ) -> Result<DeliveryResult>;
}

/// 推送工具名（运行时以 `{插件 id}__{工具名}` 暴露给模型）。
pub(crate) const SEND_TEXT_TOOL: &str = "bot__send_text_message";
pub(crate) const SEND_IMAGE_TOOL: &str = "bot__send_image_message";
pub(crate) const SEND_FILE_TOOL: &str = "bot__send_file_message";

/// 入站消息前附加的来源说明（issue #572）。
///
/// 推送目标可用时要求 Agent 经本插件的推送工具发送全部回复（含最终答复），
/// 直接输出的文本不会送达移动端；`limited` 平台每条消息回复次数有限，需要
/// 区分进展与最终答复。目标不可用时由 Bot 转发同步响应。
pub(crate) fn message_source(channel: &str, push_target: Option<&str>, limited: bool) -> String {
    let Some(target_id) = push_target else {
        return format!("[来源：{channel}]");
    };
    if limited {
        format!(
            "[来源：{channel}。用户只能收到你经 Bot 插件工具发送的消息，直接输出的文本不会送达：\
             请用 {SEND_TEXT_TOOL}（target_id={target_id}，is_final=true）发送最终答复，图片/文件用 \
             {SEND_IMAGE_TOOL} / {SEND_FILE_TOOL}；任务耗时较长时可先用 is_final=false 发送简短进展。\
             本条消息的回复次数有限，插件会合并过频的进展并为最终答复保留额度。\
             每次发送使用不同的 idempotency_key]"
        )
    } else {
        format!(
            "[来源：{channel}。用户只能收到你经 Bot 插件工具发送的消息，直接输出的文本不会送达：\
             请用 {SEND_TEXT_TOOL}（target_id={target_id}）发送最终答复，图片/文件用 \
             {SEND_IMAGE_TOOL} / {SEND_FILE_TOOL}；任务耗时较长时可先发送简短进展。\
             每次发送使用不同的 idempotency_key]"
        )
    }
}

/// 读取平台数据目录下的 JSON 凭据文件。
pub(crate) fn read_credentials_file<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<Option<T>> {
    use anyhow::Context;
    if !path.exists() {
        return Ok(None);
    }
    let content =
        std::fs::read(path).with_context(|| format!("读取 Bot 凭据失败：{}", path.display()))?;
    serde_json::from_slice(&content)
        .with_context(|| format!("解析 Bot 凭据失败：{}", path.display()))
        .map(Some)
}

/// 以 0600 权限原子写入凭据文件（临时文件 + rename）。
pub(crate) fn write_credentials_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    use anyhow::Context;
    use std::io::Write;
    let directory = path.parent().context("凭据路径缺少父目录")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("创建 Bot 数据目录失败：{}", directory.display()))?;
    let temp = directory.join(format!(".credentials-{}.tmp", scru128::new()));
    let content = serde_json::to_vec(value).context("序列化 Bot 凭据失败")?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .with_context(|| format!("保存 Bot 凭据失败：{}", temp.display()))?;
    file.write_all(&content)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("写入 Bot 凭据失败：{}", temp.display()))?;
    drop(file);
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error).with_context(|| format!("替换 Bot 凭据失败：{}", path.display()));
    }
    Ok(())
}

/// 删除凭据文件（不存在视为成功）。
pub(crate) fn remove_credentials_file(path: &Path) -> Result<()> {
    use anyhow::Context;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("删除 Bot 凭据失败：{}", path.display())),
    }
}

/// 脱敏展示密钥：保留首尾少量字符。
pub(crate) fn mask_secret(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_mentions_tools_only_with_push_target() {
        assert_eq!(message_source("飞书群聊", None, false), "[来源：飞书群聊]");
        let plain = message_source("飞书私聊", Some("t1"), false);
        assert!(plain.contains("bot__send_text_message（target_id=t1）"));
        assert!(plain.contains("直接输出的文本不会送达"));
        let limited = message_source("QQ 私聊", Some("t2"), true);
        assert!(limited.contains("target_id=t2，is_final=true"));
        assert!(limited.contains("回复次数有限"));
    }

    #[test]
    fn credentials_file_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("x").join("credentials.json");
        assert!(read_credentials_file::<Value>(&path).unwrap().is_none());
        write_credentials_file(&path, &serde_json::json!({"a": 1})).unwrap();
        let value: Value = read_credentials_file(&path).unwrap().unwrap();
        assert_eq!(value["a"], 1);
        remove_credentials_file(&path).unwrap();
        remove_credentials_file(&path).unwrap();
        assert!(read_credentials_file::<Value>(&path).unwrap().is_none());
    }

    #[test]
    fn secrets_are_masked() {
        assert_eq!(mask_secret("short"), "****");
        assert_eq!(mask_secret("abcdefghijkl"), "abcd****ijkl");
    }
}
