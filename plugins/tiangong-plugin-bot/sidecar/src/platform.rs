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

/// 入站消息来源（issue #572）。
///
/// 转发时不再拼进正文：来源标签作为只给模型看的 `instruction`，界面经
/// `render` 声明由本插件的 `im-message` 视图渲染为渠道卡片。标签只携带
/// 本条消息特有的信息（渠道、推送目标、回复额度是否受限）；“经推送工具
/// 回复、直接输出不会送达、幂等键”等通用规则由插件清单 `prompt` 注入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InboundSource {
    /// 平台标识：`feishu` / `qq` / `weixin`。
    pub(crate) platform: &'static str,
    /// 渠道名，如「微信私聊」。
    pub(crate) channel: &'static str,
    /// 推送目标编号；有值时 Agent 需经推送工具回复。
    pub(crate) target_id: Option<String>,
    /// 回复次数是否受限（QQ / 微信）。
    pub(crate) limited: bool,
}

/// 本插件渲染入站消息的 `session.message-item` 贡献 ID。
pub(crate) const MESSAGE_VIEW: &str = "im-message";

impl InboundSource {
    pub(crate) fn new(
        platform: &'static str,
        channel: &'static str,
        target_id: Option<&str>,
        limited: bool,
    ) -> Self {
        Self {
            platform,
            channel,
            target_id: target_id.map(str::to_string),
            limited,
        }
    }

    /// 只给模型看的来源标签。
    pub(crate) fn instruction(&self) -> String {
        let channel = self.channel;
        let Some(target_id) = self.target_id.as_deref() else {
            return format!("[来源：{channel}]");
        };
        let quota = if self.limited {
            "，回复次数有限"
        } else {
            ""
        };
        format!("[来源：{channel}，经 Bot 推送工具回复 target_id={target_id}{quota}]")
    }

    /// 界面渲染声明（`POST /api/v1/messages` 的 `render` 字段）。
    pub(crate) fn render(&self) -> Value {
        serde_json::json!({
            "plugin": crate::PLUGIN_ID,
            "view": MESSAGE_VIEW,
            "data": {
                "platform": self.platform,
                "channel": self.channel,
                "target_id": self.target_id,
                "limited": self.limited,
            },
        })
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
    fn source_tag_is_short_and_carries_target() {
        assert_eq!(
            InboundSource::new("feishu", "飞书群聊", None, false).instruction(),
            "[来源：飞书群聊]"
        );
        assert_eq!(
            InboundSource::new("feishu", "飞书私聊", Some("t1"), false).instruction(),
            "[来源：飞书私聊，经 Bot 推送工具回复 target_id=t1]"
        );
        assert_eq!(
            InboundSource::new("qq", "QQ 私聊", Some("t2"), true).instruction(),
            "[来源：QQ 私聊，经 Bot 推送工具回复 target_id=t2，回复次数有限]"
        );
    }

    #[test]
    fn render_points_to_message_view() {
        let render = InboundSource::new("weixin", "微信私聊", Some("t3"), true).render();
        assert_eq!(render["plugin"], crate::PLUGIN_ID);
        assert_eq!(render["view"], MESSAGE_VIEW);
        assert_eq!(render["data"]["platform"], "weixin");
        assert_eq!(render["data"]["channel"], "微信私聊");
        assert_eq!(render["data"]["target_id"], "t3");
        assert_eq!(render["data"]["limited"], true);
        let group = InboundSource::new("qq", "QQ 群聊", None, true).render();
        assert!(group["data"]["target_id"].is_null());
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
