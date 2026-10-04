//! 天工微信 bot 制品。
//!
//! 通过腾讯 iLink 长轮询接收微信消息，转发到天工的统一 Connector API，
//! 再把天工返回的文本回复发送回微信。

use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::signal;
use tokio::sync::{Mutex, RwLock};
use tracing_subscriber::EnvFilter;

mod crypto;
mod ilink;
mod mcp;
mod provision;
mod schema;
mod target_store;
#[cfg(test)]
#[path = "../../test_support.rs"]
mod test_support;

const POLL_ERROR_BACKOFF: Duration = Duration::from_secs(5);
const RECENT_MESSAGE_LIMIT: usize = 1024;

struct BotState {
    http: Client,
    /// 天工任务允许长时间执行，不设置请求总时限。
    tiangong_http: Client,
    bot_token: String,
    api_base_url: String,
    tiangong_url: String,
    tiangong_token: Option<String>,
    cursor: RwLock<String>,
    recent_messages: Mutex<RecentMessages>,
}

#[derive(Default)]
struct RecentMessages {
    keys: VecDeque<String>,
}

impl RecentMessages {
    fn claim(&mut self, channel_id: &str, message_id: &str) -> bool {
        if channel_id.is_empty() || message_id.is_empty() {
            return false;
        }
        let key = format!("{channel_id}:{message_id}");
        if self.keys.iter().any(|existing| existing == &key) {
            return false;
        }
        self.keys.push_back(key);
        if self.keys.len() > RECENT_MESSAGE_LIMIT {
            self.keys.pop_front();
        }
        true
    }

    fn release(&mut self, channel_id: &str, message_id: &str) {
        let key = format!("{channel_id}:{message_id}");
        self.keys.retain(|existing| existing != &key);
    }
}

// -- 天工 Server API -------------------------------------------

#[derive(Debug, Serialize)]
struct ConnectorRequest {
    connector: String,
    channel_id: String,
    sender_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<ApiMessageContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    media: Vec<MediaAsset>,
}

#[derive(Debug, Deserialize)]
struct ConnectorResponse {
    #[allow(dead_code)]
    session_id: String,
    message: String,
    content: ApiMessageContent,
    #[serde(default)]
    attachments: Vec<ApiMessageContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiMessageContent {
    Text {
        text: String,
    },
    Image {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
    },
    File {
        url: String,
        name: String,
    },
    Audio {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration: Option<u32>,
    },
    Video {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
    },
}

impl ApiMessageContent {
    fn text(&self) -> String {
        match self {
            Self::Text { text } => text.clone(),
            Self::Image { caption, .. } | Self::Video { caption, .. } => {
                caption.clone().unwrap_or_default()
            }
            Self::File { .. } | Self::Audio { .. } => String::new(),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Text { text } => text.trim().is_empty(),
            Self::Image { url, .. }
            | Self::File { url, .. }
            | Self::Audio { url, .. }
            | Self::Video { url, .. } => url.trim().is_empty(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Image { .. } => "image",
            Self::File { .. } => "file",
            Self::Audio { .. } => "audio",
            Self::Video { .. } => "video",
        }
    }

    fn local_file_url(&self) -> Option<&str> {
        match self {
            Self::Image { url, .. }
            | Self::File { url, .. }
            | Self::Audio { url, .. }
            | Self::Video { url, .. } => Some(url),
            Self::Text { .. } => None,
        }
    }

    /// 在消息正文前附加来源说明；纯图片/视频附加到说明文字。
    fn with_source(self, source: &str) -> Self {
        let prefix = |text: Option<String>| match text.filter(|text| !text.trim().is_empty()) {
            Some(text) => format!("{source}\n{text}"),
            None => source.to_string(),
        };
        match self {
            Self::Text { text } => Self::Text {
                text: prefix(Some(text)),
            },
            Self::Image { url, caption } => Self::Image {
                url,
                caption: Some(prefix(caption)),
            },
            Self::Video { url, caption } => Self::Video {
                url,
                caption: Some(prefix(caption)),
            },
            other @ (Self::File { .. } | Self::Audio { .. }) => other,
        }
    }
}

/// 本条消息可用的推送通道：(MCP 服务名, 推送目标编号)。
///
/// 两者都可用时，回复全部交由 Agent 经 MCP 发送（issue #572）；任一缺失时
/// Agent 无法推送，退回由 Bot 转发同步响应。
fn push_channel(target_id: Option<String>) -> Option<(String, String)> {
    let target_id = target_id?;
    match mcp::server_name() {
        Ok(server) => Some((server, target_id)),
        Err(error) => {
            tracing::warn!("获取微信 MCP 服务名失败，回退为同步回复: {error}");
            None
        }
    }
}

/// 生成附加在入站消息前的来源说明（issue #572）。
///
/// 推送通道可用时要求 Agent 经本 Bot 的 MCP 工具发送全部回复（含最终答复），
/// 直接输出的文本不会送达移动端。
fn message_source(target_kind: &str, push: Option<&(String, String)>) -> String {
    let channel = if target_kind == "group" {
        "微信群聊"
    } else {
        "微信私聊"
    };
    match push {
        Some((server, target_id)) => format!(
            "[来源：{channel}。用户只能收到你经 MCP 服务 {server} 发送的消息，直接输出的文本不会送达：\
             请用 send_text_message（target_id={target_id}）发送最终答复，图片/文件用 \
             send_image_message / send_file_message；任务耗时较长时可先发送简短进展。\
             每次发送使用不同的 idempotency_key]"
        ),
        None => format!("[来源：{channel}]"),
    }
}

#[derive(Debug, Clone, Serialize)]
struct MediaAsset {
    kind: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capability: Option<String>,
}

impl MediaAsset {
    fn image(url: String, mime_type: String) -> Self {
        Self {
            kind: "image".to_string(),
            url,
            mime_type: Some(mime_type),
            title: None,
            capability: Some("multimodal".to_string()),
        }
    }
}

struct DownloadedImage {
    url: String,
    mime_type: String,
}

struct ParsedMessage {
    content: ApiMessageContent,
    media: Vec<MediaAsset>,
}

struct BotReply {
    text: String,
    files: Vec<PathBuf>,
}

// -- 入口 -------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // 管理命令必须在日志和常驻运行初始化前处理，stdout 只输出协议 JSON。
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let mode = arguments.first().map(String::as_str);
    match mode {
        Some("--describe") => {
            println!("{}", serde_json::to_string(&schema::describe_output())?);
            return Ok(());
        }
        Some("--provision-begin") => {
            println!("{}", serde_json::to_string(&provision::begin().await?)?);
            return Ok(());
        }
        Some("--provision-poll") => {
            println!(
                "{}",
                serde_json::to_string(&provision::poll_from_stdin().await?)?
            );
            return Ok(());
        }
        Some("--push-target-list") => {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "targets": target_store::list_enabled_views()?
                }))?
            );
            return Ok(());
        }
        Some("--push-target-delete") => {
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .context("读取推送目标删除请求失败")?;
            let request: target_store::DeleteTargetRequest =
                serde_json::from_str(&input).context("解析推送目标删除请求失败")?;
            target_store::delete(&request.target_id)?;
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "target_id": request.target_id,
                    "deleted": true
                }))?
            );
            return Ok(());
        }
        Some("--mcp") if arguments.get(1).map(String::as_str) == Some("generate") => {
            println!("{}", serde_json::to_string(&mcp::registration_config()?)?);
            return Ok(());
        }
        _ => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .with_writer(std::io::stderr)
        .init();

    if mode == Some("--mcp") {
        return mcp::serve().await;
    }

    let credentials = provision::load_credentials()?;
    let tiangong_url =
        std::env::var("TIANGONG_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let tiangong_token = std::env::var("TIANGONG_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());

    let state = Arc::new(BotState {
        http: Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("构建 HTTP 客户端失败")?,
        tiangong_http: Client::builder()
            .build()
            .context("构建天工 HTTP 客户端失败")?,
        bot_token: credentials.bot_token,
        api_base_url: credentials
            .base_url
            .unwrap_or_else(|| ilink::ILINK_BASE_URL.to_string()),
        tiangong_url,
        tiangong_token,
        cursor: RwLock::new(String::new()),
        recent_messages: Mutex::new(RecentMessages::default()),
    });

    tracing::info!("微信机器人启动中...");
    tracing::info!("天工服务地址: {}", state.tiangong_url);
    if let Err(error) =
        ilink::notify_start(&state.http, &state.api_base_url, &state.bot_token).await
    {
        tracing::warn!("通知微信机器人启动失败，将继续运行: {error}");
    }

    let mut poll_handle = tokio::spawn({
        let state = state.clone();
        async move {
            if let Err(error) = run_poll_loop(&state).await {
                tracing::error!("微信长轮询循环退出: {error}");
            }
        }
    });

    let poll_finished: bool;
    #[cfg(unix)]
    {
        let mut term = signal::unix::signal(signal::unix::SignalKind::terminate())
            .context("注册 SIGTERM 处理失败")?;
        let mut int = signal::unix::signal(signal::unix::SignalKind::interrupt())
            .context("注册 SIGINT 处理失败")?;
        poll_finished = tokio::select! {
            _ = term.recv() => {
                tracing::info!("收到 SIGTERM，正在退出...");
                false
            },
            _ = int.recv() => {
                tracing::info!("收到 SIGINT，正在退出...");
                false
            },
            _ = &mut poll_handle => {
                tracing::info!("长轮询循环已结束");
                true
            },
        };
    }
    #[cfg(not(unix))]
    {
        poll_finished = tokio::select! {
            result = signal::ctrl_c() => {
                result.context("等待 Ctrl+C 失败")?;
                tracing::info!("收到 Ctrl+C，正在退出...");
                false
            },
            _ = &mut poll_handle => {
                tracing::info!("长轮询循环已结束");
                true
            },
        };
    }

    if !poll_finished {
        poll_handle.abort();
        let _ = poll_handle.await;
    }
    if let Err(error) = ilink::notify_stop(&state.http, &state.api_base_url, &state.bot_token).await
    {
        tracing::warn!("通知微信机器人停止失败: {error}");
    }
    Ok(())
}

// -- 长轮询 -----------------------------------------------------

async fn run_poll_loop(state: &Arc<BotState>) -> Result<()> {
    tracing::info!("正在连接 iLink 长轮询...");

    loop {
        let cursor = state.cursor.read().await.clone();
        let updates =
            match ilink::get_updates(&state.http, &state.api_base_url, &state.bot_token, &cursor)
                .await
            {
                Ok(updates) => updates,
                Err(error) => {
                    tracing::warn!(
                        "iLink 长轮询出错，{} 秒后重试: {error}",
                        POLL_ERROR_BACKOFF.as_secs()
                    );
                    tokio::time::sleep(POLL_ERROR_BACKOFF).await;
                    continue;
                }
            };

        if let Some(timeout) = updates.longpolling_timeout_ms {
            tracing::debug!("iLink 建议长轮询超时: {timeout}ms");
        }

        let mut batch_failed = false;
        for message in &updates.msgs {
            let message_id = message.stable_message_id();
            let channel_id = stable_channel_id(message);
            if let Some(message_id) = message_id.as_deref()
                && !state
                    .recent_messages
                    .lock()
                    .await
                    .claim(channel_id, message_id)
            {
                tracing::debug!("忽略重复微信消息 message_id={message_id}");
                continue;
            }

            if let Err(error) = handle_message(state, message, message_id.as_deref()).await {
                tracing::error!("处理微信消息失败: {error}");
                if let Some(message_id) = message_id.as_deref() {
                    state
                        .recent_messages
                        .lock()
                        .await
                        .release(channel_id, message_id);
                }
                batch_failed = true;
            }
        }

        if batch_failed {
            tokio::time::sleep(POLL_ERROR_BACKOFF).await;
            continue;
        }
        if let Some(new_cursor) = updates
            .get_updates_buf
            .as_deref()
            .filter(|cursor| !cursor.is_empty())
        {
            *state.cursor.write().await = new_cursor.to_string();
        }
    }
}

// -- 消息处理 ---------------------------------------------------

async fn handle_message(
    state: &Arc<BotState>,
    message: &ilink::WeixinMessage,
    message_id: Option<&str>,
) -> Result<()> {
    if !message.is_from_user() {
        return Ok(());
    }

    let sender_id = message.from_user_id.trim();
    if sender_id.is_empty() {
        tracing::warn!("微信消息缺少发送者，跳过");
        return Ok(());
    }
    let context_token = message.context_token.trim();
    if context_token.is_empty() {
        tracing::warn!("微信消息缺少 context_token，无法回复，跳过");
        return Ok(());
    }
    let channel_id = stable_channel_id(message);
    let target_kind = if message.group_id.trim().is_empty() {
        "direct"
    } else {
        "group"
    };
    let target_id =
        match target_store::upsert_discovered(channel_id, sender_id, target_kind, context_token) {
            Ok(target_id) => Some(target_id),
            Err(error) => {
                tracing::warn!("更新微信推送目标失败: {error}");
                None
            }
        };

    let text = message.text_content();
    let mut images = Vec::new();
    for image in message.images() {
        images.push(download_image(state, image).await?);
    }

    let parsed = if !text.trim().is_empty() {
        ParsedMessage {
            content: ApiMessageContent::Text { text },
            media: images
                .into_iter()
                .map(|image| MediaAsset::image(image.url, image.mime_type))
                .collect(),
        }
    } else {
        let mut images = images.into_iter();
        let Some(first) = images.next() else {
            tracing::debug!("微信消息没有可处理的文本或图片，跳过");
            return Ok(());
        };
        ParsedMessage {
            content: ApiMessageContent::Image {
                url: first.url,
                caption: None,
            },
            media: images
                .map(|image| MediaAsset::image(image.url, image.mime_type))
                .collect(),
        }
    };

    if parsed.content.is_empty() {
        return Ok(());
    }
    let push = push_channel(target_id);
    let source = message_source(target_kind, push.as_ref());
    let parsed = ParsedMessage {
        content: parsed.content.with_source(&source),
        media: parsed.media,
    };
    tracing::info!(
        "转发微信消息 sender={sender_id} content_type={} images={}",
        parsed.content.kind(),
        parsed.media.len()
    );

    let reply = forward_to_tiangong(
        state,
        channel_id,
        sender_id,
        message_id,
        parsed.content,
        parsed.media,
    )
    .await?;
    if push.is_some() {
        tracing::debug!("微信回复由 Agent 经 MCP 发送，忽略同步响应");
        return Ok(());
    }
    send_reply(state, sender_id, context_token, reply).await
}

async fn send_reply(
    state: &BotState,
    to_user_id: &str,
    context_token: &str,
    reply: BotReply,
) -> Result<()> {
    if reply.files.is_empty() {
        if reply.text.trim().is_empty() {
            return Ok(());
        }
        ilink::send_message(
            &state.http,
            &state.api_base_url,
            &state.bot_token,
            to_user_id,
            context_token,
            &reply.text,
        )
        .await?;
        return Ok(());
    }

    for (index, path) in reply.files.iter().enumerate() {
        let caption = if index == 0 { reply.text.as_str() } else { "" };
        tracing::info!("向微信发送本地文件: {}", path.display());
        ilink::send_local_file(
            &state.http,
            &state.api_base_url,
            &state.bot_token,
            to_user_id,
            context_token,
            path,
            caption,
        )
        .await?;
    }
    Ok(())
}

fn stable_channel_id(message: &ilink::WeixinMessage) -> &str {
    let group_id = message.group_id.trim();
    if group_id.is_empty() {
        message.from_user_id.trim()
    } else {
        group_id
    }
}

async fn download_image(
    state: &Arc<BotState>,
    image: &ilink::ImageItem,
) -> Result<DownloadedImage> {
    let encrypted = ilink::download_image_bytes(&state.http, image).await?;
    if encrypted.is_empty() {
        return Err(anyhow!("微信图片下载数据为空"));
    }

    let plaintext = if let Some(key) = image
        .aeskey
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        crypto::decrypt_media_hex(key, &encrypted)?
    } else if let Some(key) = image
        .media
        .as_ref()
        .and_then(|media| media.aes_key.as_deref())
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        crypto::decrypt_media_base64(key, &encrypted)?
    } else {
        encrypted
    };

    let mime_type = detect_image_mime(&plaintext).to_string();
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &plaintext);
    Ok(DownloadedImage {
        url: format!("data:{mime_type};base64,{encoded}"),
        mime_type,
    })
}

fn detect_image_mime(data: &[u8]) -> &'static str {
    if data.len() >= 4 {
        match &data[..4] {
            [0x89, 0x50, 0x4e, 0x47] => return "image/png",
            [0xff, 0xd8, 0xff, _] => return "image/jpeg",
            [0x47, 0x49, 0x46, 0x38] => return "image/gif",
            _ => {}
        }
    }
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return "image/webp";
    }
    "image/jpeg"
}

async fn forward_to_tiangong(
    state: &BotState,
    channel_id: &str,
    sender_id: &str,
    message_id: Option<&str>,
    content: ApiMessageContent,
    media: Vec<MediaAsset>,
) -> Result<BotReply> {
    let url = format!(
        "{}/api/v1/messages",
        state.tiangong_url.trim_end_matches('/')
    );
    let request = ConnectorRequest {
        connector: "weixin-bot".to_string(),
        channel_id: channel_id.to_string(),
        sender_id: sender_id.to_string(),
        message_id: message_id.map(str::to_string),
        message: None,
        content: Some(content),
        media,
    };

    let mut builder = state.tiangong_http.post(url);
    if let Some(token) = &state.tiangong_token {
        builder = builder.bearer_auth(token);
    }
    let response = builder
        .json(&request)
        .send()
        .await
        .context("天工 server 请求失败")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("天工 server 错误: {status}, {body}"));
    }

    let body: ConnectorResponse = response.json().await.context("解析天工响应失败")?;
    Ok(reply_from_connector_response(body))
}

fn reply_from_connector_response(body: ConnectorResponse) -> BotReply {
    let text = {
        let content_text = body.content.text();
        if content_text.trim().is_empty() {
            body.message
        } else {
            content_text
        }
    };
    let mut files = Vec::new();
    for content in std::iter::once(&body.content).chain(body.attachments.iter()) {
        let Some(url) = content.local_file_url() else {
            continue;
        };
        let path = PathBuf::from(url.strip_prefix("file://").unwrap_or(url));
        if !path.is_absolute() {
            tracing::warn!("忽略非本地文件形式的天工附件: {url}");
            continue;
        }
        if !files.iter().any(|existing| existing == &path) {
            files.push(path);
        }
    }
    BotReply { text, files }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG_MOCK_DELAY: Duration = Duration::from_secs(125);

    #[tokio::test]
    #[ignore = "长时回归测试会等待 125 秒"]
    async fn forward_to_tiangong_waits_beyond_120_seconds() {
        let body = serde_json::json!({
            "session_id": "mock-session",
            "message": "mock-ok",
            "content": { "type": "text", "text": "mock-ok" },
            "attachments": []
        })
        .to_string();
        let (tiangong_url, server) =
            test_support::spawn_delayed_json_response(LONG_MOCK_DELAY, body).await;
        let state = BotState {
            http: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap(),
            tiangong_http: Client::builder().build().unwrap(),
            bot_token: String::new(),
            api_base_url: String::new(),
            tiangong_url,
            tiangong_token: None,
            cursor: RwLock::new(String::new()),
            recent_messages: Mutex::new(RecentMessages::default()),
        };

        let started = std::time::Instant::now();
        let reply = forward_to_tiangong(
            &state,
            "mock-channel",
            "mock-sender",
            Some("mock-message"),
            ApiMessageContent::Text {
                text: "mock-request".to_string(),
            },
            Vec::new(),
        )
        .await
        .expect("天工长任务响应不应在 120 秒时超时");

        assert!(started.elapsed() >= LONG_MOCK_DELAY);
        assert_eq!(reply.text, "mock-ok");
        server.await.expect("长时 Mock 服务异常退出");
    }

    #[test]
    fn source_is_prefixed_to_text_and_media_caption() {
        let source = "[来源：微信私聊]";
        let text = ApiMessageContent::Text {
            text: "总结群聊".into(),
        }
        .with_source(source);
        assert!(
            matches!(text, ApiMessageContent::Text { ref text } if text == "[来源：微信私聊]\n总结群聊")
        );

        let image = ApiMessageContent::Image {
            url: "data:image/png;base64,AA==".into(),
            caption: None,
        }
        .with_source(source);
        assert!(matches!(
            image,
            ApiMessageContent::Image { ref caption, .. } if caption.as_deref() == Some(source)
        ));
    }

    #[test]
    fn source_requires_mcp_reply_only_when_push_is_available() {
        assert_eq!(message_source("group", None), "[来源：微信群聊]");

        let push = ("bot-weixin".to_string(), "target-1".to_string());
        let with_push = message_source("direct", Some(&push));
        assert!(with_push.starts_with("[来源：微信私聊"));
        assert!(with_push.contains("bot-weixin"));
        assert!(with_push.contains("send_text_message（target_id=target-1）"));
        assert!(with_push.contains("直接输出的文本不会送达"));
        assert!(with_push.contains("不同的 idempotency_key"));
        assert!(push_channel(None).is_none());
    }

    #[test]
    fn recent_messages_can_retry_failed_message() {
        let mut messages = RecentMessages::default();
        assert!(messages.claim("chat-1", "msg-1"));
        assert!(!messages.claim("chat-1", "msg-1"));
        assert!(messages.claim("chat-2", "msg-1"));
        messages.release("chat-1", "msg-1");
        assert!(messages.claim("chat-1", "msg-1"));
    }

    #[test]
    fn channel_id_is_stable_for_direct_and_group_messages() {
        let mut message = ilink::WeixinMessage {
            seq: None,
            message_id: None,
            from_user_id: "user@im.wechat".into(),
            to_user_id: "bot@im.bot".into(),
            message_type: 1,
            context_token: "per-message-token".into(),
            item_list: Vec::new(),
            group_id: String::new(),
        };
        assert_eq!(stable_channel_id(&message), "user@im.wechat");
        message.group_id = "group-1".into();
        assert_eq!(stable_channel_id(&message), "group-1");
    }

    #[test]
    fn connector_request_keeps_message_id_and_image_metadata() {
        let request = ConnectorRequest {
            connector: "weixin-bot".to_string(),
            channel_id: "user@im.wechat".to_string(),
            sender_id: "user@im.wechat".to_string(),
            message_id: Some("42".to_string()),
            message: None,
            content: Some(ApiMessageContent::Text {
                text: "你好".into(),
            }),
            media: vec![MediaAsset::image(
                "data:image/png;base64,AA==".into(),
                "image/png".into(),
            )],
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["message_id"], "42");
        assert_eq!(value["content"]["type"], "text");
        assert_eq!(value["media"][0]["mime_type"], "image/png");
        assert_eq!(value["media"][0]["capability"], "multimodal");
    }

    #[test]
    fn content_validation_and_mime_detection() {
        assert!(ApiMessageContent::Text { text: "  ".into() }.is_empty());
        assert!(
            !ApiMessageContent::Image {
                url: "data:image/png;base64,AA==".into(),
                caption: None,
            }
            .is_empty()
        );
        assert_eq!(
            detect_image_mime(&[0x89, 0x50, 0x4e, 0x47, 0x0d]),
            "image/png"
        );
        assert_eq!(detect_image_mime(&[0xff, 0xd8, 0xff, 0xe0]), "image/jpeg");
        assert_eq!(
            detect_image_mime(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            "image/webp"
        );
    }

    #[test]
    fn connector_response_keeps_caption_and_all_local_files() {
        let reply = reply_from_connector_response(ConnectorResponse {
            session_id: "session-1".into(),
            message: "图片已生成".into(),
            content: ApiMessageContent::Image {
                url: "/tmp/image.jpg".into(),
                caption: Some("图片已生成".into()),
            },
            attachments: vec![ApiMessageContent::File {
                url: "/tmp/report.txt".into(),
                name: "report.txt".into(),
            }],
        });

        assert_eq!(reply.text, "图片已生成");
        assert_eq!(
            reply.files,
            vec![
                PathBuf::from("/tmp/image.jpg"),
                PathBuf::from("/tmp/report.txt")
            ]
        );
    }
}
