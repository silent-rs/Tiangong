//! 天工 QQ bot 制品。
//!
//! 独立编译的二进制，由天工主程序在运行时下载并启动。
//! 通过 QQ 开放平台 WebSocket 网关接收 QQ 消息（C2C 私聊、群组 @提及），
//! 转发到天工本地 embedded server 的 `POST /api/v1/messages`，
//! 再把天工返回的文本/本地文件回复发送回 QQ。
//!
//! 凭证由天工主程序通过环境变量注入，或由 bot 扫码配置自行保存：
//! - `TIANGONG_BOT_QQ_APP_ID` / `TIANGONG_BOT_QQ_APP_SECRET`
//! - `TIANGONG_URL`（默认 `http://127.0.0.1:8080`）
//! - `TIANGONG_TOKEN`（可选，embedded server 认证 token）

use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use md5::{Digest, Md5};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::Sha1;
use tokio::signal;
use tokio::sync::{Mutex, Notify};
use tracing_subscriber::EnvFilter;

mod gateway;
mod mcp;
mod provision;
mod schema;
mod target_store;
#[cfg(test)]
#[path = "../../test_support.rs"]
mod test_support;
mod token;

use gateway::{DispatchEvent, GatewayRunner, INTENT_GROUP_AND_C2C_EVENT};
use token::AccessTokenCache;

const RECENT_MESSAGE_LIMIT: usize = 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MD5_10M_BYTES: usize = 10_002_432;
const OPENAPI_BASE: &str = "https://api.sgroup.qq.com";

struct BotState {
    http: Client,
    /// 天工任务允许长时间执行，不设置请求总时限。
    tiangong_http: Client,
    token: AccessTokenCache,
    /// 天工 server 地址，如 http://127.0.0.1:8080
    tiangong_url: String,
    /// 天工 server 认证 token（可选）
    tiangong_token: Option<String>,
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

// ── 天工 Server API 类型 ──────────────────────────────────────

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
            tracing::warn!("获取 QQ MCP 服务名失败，回退为同步回复: {error}");
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
        "QQ 群聊"
    } else {
        "QQ 私聊"
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

struct ParsedMessage {
    content: ApiMessageContent,
    media: Vec<MediaAsset>,
}

impl ParsedMessage {
    fn with_source(self, source: &str) -> Self {
        Self {
            content: self.content.with_source(source),
            media: self.media,
        }
    }
}

struct BotReply {
    text: String,
    files: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct UploadPrepareResponse {
    upload_id: String,
    block_size: String,
    parts: Vec<UploadPart>,
}

#[derive(Debug, Deserialize)]
struct UploadPart {
    index: usize,
    presigned_url: String,
}

#[derive(Debug, Deserialize)]
struct MediaUploadResponse {
    file_info: String,
}

// ── QQ 事件载荷 ──────────────────────────────────────────────
//
// QQ 官方事件 payload 的字段不全部被消费，部分字段保留用于协议完整性
// 与未来扩展，因此整个载荷族标记 `#[allow(dead_code)]`。

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GroupAtMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    group_openid: String,
    #[serde(default)]
    message_type: String,
    author: MessageAuthor,
    #[serde(default)]
    attachments: Vec<QqAttachment>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct C2cMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    author: MessageAuthor,
    #[serde(default)]
    attachments: Vec<QqAttachment>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct MessageAuthor {
    #[serde(default)]
    member_openid: String,
    #[serde(default)]
    user_openid: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct QqAttachment {
    #[serde(default)]
    content_type: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    filename: String,
}

// ── 入口 ──────────────────────────────────────────────────────

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

    tracing::info!("QQ 机器人启动中...");
    tracing::info!("天工服务地址: {tiangong_url}");

    let http = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .context("构建 HTTP 客户端失败")?;
    let token = AccessTokenCache::new(http.clone(), credentials.app_id, credentials.app_secret);

    let state = Arc::new(BotState {
        http: http.clone(),
        tiangong_http: Client::builder()
            .build()
            .context("构建天工 HTTP 客户端失败")?,
        token,
        tiangong_url,
        tiangong_token,
        recent_messages: Mutex::new(RecentMessages::default()),
    });

    let shutdown = Arc::new(Notify::new());
    let runner = GatewayRunner::new(
        http,
        state.token.clone(),
        INTENT_GROUP_AND_C2C_EVENT,
        {
            let state = state.clone();
            Arc::new(move |event| {
                let state = state.clone();
                Box::pin(async move { handle_dispatch(&state, event).await })
            })
        },
        shutdown.clone(),
    );

    let gateway_handle = tokio::spawn(async move {
        runner.run().await;
    });

    #[cfg(unix)]
    {
        let mut term = signal::unix::signal(signal::unix::SignalKind::terminate())
            .context("注册 SIGTERM 处理失败")?;
        let mut int = signal::unix::signal(signal::unix::SignalKind::interrupt())
            .context("注册 SIGINT 处理失败")?;
        tokio::select! {
            _ = term.recv() => tracing::info!("收到 SIGTERM，正在退出..."),
            _ = int.recv() => tracing::info!("收到 SIGINT，正在退出..."),
            _ = gateway_handle => tracing::info!("Gateway 主循环已结束"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = signal::ctrl_c() => tracing::info!("收到 Ctrl+C，正在退出..."),
            _ = gateway_handle => tracing::info!("Gateway 主循环已结束"),
        }
    }

    shutdown.notify_waiters();
    Ok(())
}

// ── 事件分派 ──────────────────────────────────────────────────

async fn handle_dispatch(state: &Arc<BotState>, event: DispatchEvent) {
    match event.event_type.as_str() {
        "GROUP_AT_MESSAGE_CREATE" => {
            if let Err(error) = handle_group_message(state, event.data, event.seq).await {
                tracing::error!("处理 QQ 群消息失败: {error}");
            }
        }
        "C2C_MESSAGE_CREATE" => {
            if let Err(error) = handle_c2c_message(state, event.data, event.seq).await {
                tracing::error!("处理 QQ 私聊消息失败: {error}");
            }
        }
        other => {
            tracing::debug!("忽略 QQ 事件类型: {other}");
        }
    }
}

async fn handle_group_message(state: &Arc<BotState>, data: Value, seq: Option<u64>) -> Result<()> {
    let message: GroupAtMessage = serde_json::from_value(data).context("解析 QQ 群消息失败")?;
    let message_id = message.id.trim();
    if message_id.is_empty() {
        tracing::warn!("QQ 群消息缺少 id，跳过");
        return Ok(());
    }
    let channel_id = format!("group:{}", message.group_openid);
    let sender_id = if !message.author.member_openid.is_empty() {
        message.author.member_openid.as_str()
    } else {
        message.author.user_openid.as_str()
    };
    let target_id =
        match target_store::upsert_discovered("group", &message.group_openid, message_id) {
            Ok(target_id) => Some(target_id),
            Err(error) => {
                tracing::warn!("更新 QQ 群聊推送目标失败: {error}");
                None
            }
        };
    if !state
        .recent_messages
        .lock()
        .await
        .claim(&channel_id, message_id)
    {
        tracing::debug!("忽略重复 QQ 群消息 id={message_id}");
        return Ok(());
    }

    let text = strip_at_mention(&message.content);
    let Some(parsed) = parse_text_with_images(state, &text, &message.attachments).await else {
        tracing::debug!("QQ 群消息没有可处理的文本或图片，跳过");
        return Ok(());
    };
    let push = push_channel(target_id);
    let parsed = parsed.with_source(&message_source("group", push.as_ref()));

    let _ = seq;
    tracing::info!(
        "转发 QQ 群消息 group={} sender={sender_id} content_type={} images={}",
        message.group_openid,
        parsed.content.kind(),
        parsed.media.len()
    );
    let group_openid = message.group_openid.clone();
    let reply = match forward_to_tiangong(state, &channel_id, sender_id, message_id, parsed).await {
        Ok(reply) => reply,
        Err(error) => {
            // 转发失败时释放去重占用，允许下次推送重试
            state
                .recent_messages
                .lock()
                .await
                .release(&channel_id, message_id);
            return Err(error);
        }
    };
    if push.is_some() {
        tracing::debug!("QQ 群聊回复由 Agent 经 MCP 发送，忽略同步响应");
        return Ok(());
    }
    send_reply(state, ReplyTarget::Group(group_openid), message_id, reply).await
}

async fn handle_c2c_message(state: &Arc<BotState>, data: Value, seq: Option<u64>) -> Result<()> {
    let message: C2cMessage = serde_json::from_value(data).context("解析 QQ 私聊消息失败")?;
    let message_id = message.id.trim();
    if message_id.is_empty() {
        tracing::warn!("QQ 私聊消息缺少 id，跳过");
        return Ok(());
    }
    let user_openid = message.author.user_openid.trim().to_string();
    if user_openid.is_empty() {
        tracing::warn!("QQ 私聊消息缺少 author.user_openid，跳过");
        return Ok(());
    }
    let channel_id = format!("c2c:{user_openid}");
    let target_id = match target_store::upsert_discovered("direct", &user_openid, message_id) {
        Ok(target_id) => Some(target_id),
        Err(error) => {
            tracing::warn!("更新 QQ 私聊推送目标失败: {error}");
            None
        }
    };
    if !state
        .recent_messages
        .lock()
        .await
        .claim(&channel_id, message_id)
    {
        tracing::debug!("忽略重复 QQ 私聊消息 id={message_id}");
        return Ok(());
    }

    let text = message.content.trim();
    let Some(parsed) = parse_text_with_images(state, text, &message.attachments).await else {
        tracing::debug!("QQ 私聊消息没有可处理的文本或图片，跳过");
        return Ok(());
    };

    if parsed.content.is_empty() {
        return Ok(());
    }
    let push = push_channel(target_id);
    let parsed = parsed.with_source(&message_source("direct", push.as_ref()));
    let _ = seq;
    tracing::info!(
        "转发 QQ 私聊消息 user={} content_type={} images={}",
        user_openid,
        parsed.content.kind(),
        parsed.media.len()
    );
    let forward_result =
        forward_to_tiangong(state, &channel_id, &user_openid, message_id, parsed).await;
    let reply = match forward_result {
        Ok(reply) => reply,
        Err(error) => {
            state
                .recent_messages
                .lock()
                .await
                .release(&channel_id, message_id);
            return Err(error);
        }
    };
    if push.is_some() {
        tracing::debug!("QQ 私聊回复由 Agent 经 MCP 发送，忽略同步响应");
        return Ok(());
    }
    send_reply(state, ReplyTarget::User(user_openid), message_id, reply).await
}

/// 把文本与图片附件解析为 `ParsedMessage`，对齐微信 `handle_message` 的图文组装逻辑。
///
/// - 文本非空 → `Text` + 图片作 `media`
/// - 文本为空 → 首图作 `content`、余图作 `media`
/// - 文本与图片皆空 → 返回 `None`
async fn parse_text_with_images(
    state: &BotState,
    text: &str,
    attachments: &[QqAttachment],
) -> Option<ParsedMessage> {
    let mut images = Vec::new();
    for attachment in attachments {
        if attachment.content_type.starts_with("image/") {
            match download_attachment(state, attachment).await {
                Ok(image) => images.push(image),
                Err(error) => tracing::warn!("下载 QQ 图片失败: {error}"),
            }
        }
    }

    if !text.trim().is_empty() {
        Some(ParsedMessage {
            content: ApiMessageContent::Text {
                text: text.to_string(),
            },
            media: images
                .into_iter()
                .map(|image| MediaAsset::image(image.url, image.mime_type))
                .collect(),
        })
    } else {
        let mut images = images.into_iter();
        let first = images.next()?;
        Some(ParsedMessage {
            content: ApiMessageContent::Image {
                url: first.url,
                caption: None,
            },
            media: images
                .map(|image| MediaAsset::image(image.url, image.mime_type))
                .collect(),
        })
    }
}

// ── 天工 Server 调用 ──────────────────────────────────────────

async fn forward_to_tiangong(
    state: &BotState,
    channel_id: &str,
    sender_id: &str,
    message_id: &str,
    parsed: ParsedMessage,
) -> Result<BotReply> {
    let url = format!(
        "{}/api/v1/messages",
        state.tiangong_url.trim_end_matches('/')
    );
    let request = ConnectorRequest {
        connector: "qq-bot".to_string(),
        channel_id: channel_id.to_string(),
        sender_id: sender_id.to_string(),
        message_id: Some(message_id.to_string()),
        message: None,
        content: Some(parsed.content),
        media: parsed.media,
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
        if !files.iter().any(|existing: &PathBuf| existing == &path) {
            files.push(path);
        }
    }
    BotReply { text, files }
}

// ── 回复 ──────────────────────────────────────────────────────

enum ReplyTarget {
    /// 群消息：用 group_openid
    Group(String),
    /// 私聊消息：用 user_openid
    User(String),
}

async fn send_reply(
    state: &BotState,
    target: ReplyTarget,
    msg_id: &str,
    reply: BotReply,
) -> Result<()> {
    if reply.files.is_empty() {
        if reply.text.trim().is_empty() {
            return Ok(());
        }
        send_text(state, &target, msg_id, 1, &reply.text).await?;
        return Ok(());
    }

    // 有本地文件时，先发送文本（若有），再依次上传文件
    let mut msg_seq = 1;
    if !reply.text.trim().is_empty() {
        send_text(state, &target, msg_id, msg_seq, &reply.text).await?;
        msg_seq += 1;
    }
    for path in &reply.files {
        let caption = "";
        tracing::info!("向 QQ 发送本地文件: {}", path.display());
        if let Err(error) = send_local_file(state, &target, msg_id, msg_seq, path, caption).await {
            tracing::warn!("上传 QQ 本地文件失败: {error}");
        }
        msg_seq += 1;
    }
    Ok(())
}

async fn send_text(
    state: &BotState,
    target: &ReplyTarget,
    msg_id: &str,
    msg_seq: u32,
    text: &str,
) -> Result<()> {
    let access_token = state.token.get().await?;
    let url = target_api_url(target, "messages");
    let body = json!({
        // 0=文本，2=Markdown，7=富媒体
        "msg_type": 0,
        "msg_id": msg_id,
        "msg_seq": msg_seq,
        "content": text,
    });
    let response = state
        .http
        .post(url)
        .header("Authorization", format!("QQBot {access_token}"))
        .json(&body)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("发送 QQ 消息请求失败")?;
    check_send_response(response, "send_text").await?;
    tracing::info!("QQ 文本回复发送成功");
    Ok(())
}

/// 按 QQ 分片上传流程上传本地文件，再发送富媒体消息。
async fn send_local_file(
    state: &BotState,
    target: &ReplyTarget,
    msg_id: &str,
    msg_seq: u32,
    path: &std::path::Path,
    _caption: &str,
) -> Result<()> {
    let access_token = state.token.get().await?;
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("读取待发送文件失败: {}", path.display()))?;
    let file_type = qq_file_type(path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("file");
    let file_md5 = digest_md5(&bytes);
    let prepare_body = json!({
        "file_type": file_type,
        "file_size": bytes.len().to_string(),
        "file_name": file_name,
        "md5": file_md5,
        "sha1": digest_sha1(&bytes),
        "md5_10m": digest_md5(&bytes[..bytes.len().min(MD5_10M_BYTES)]),
    });
    let prepare_response = state
        .http
        .post(target_api_url(target, "upload_prepare"))
        .header("Authorization", format!("QQBot {access_token}"))
        .json(&prepare_body)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("申请 QQ 富媒体分片上传失败")?;
    let mut prepared: UploadPrepareResponse =
        parse_json_response(prepare_response, "申请 QQ 富媒体分片上传").await?;
    let block_size = prepared
        .block_size
        .parse::<usize>()
        .context("QQ 富媒体分片大小无效")?;
    if block_size == 0 || prepared.upload_id.is_empty() || prepared.parts.is_empty() {
        return Err(anyhow!("QQ 富媒体分片上传响应不完整"));
    }

    prepared.parts.sort_by_key(|part| part.index);
    let expected_parts = bytes.len().div_ceil(block_size);
    if prepared.parts.len() != expected_parts {
        return Err(anyhow!(
            "QQ 富媒体分片数量不匹配：期望 {expected_parts}，实际 {}",
            prepared.parts.len()
        ));
    }

    for (part_order, part) in prepared.parts.iter().enumerate() {
        if part.presigned_url.is_empty() {
            return Err(anyhow!("QQ 富媒体分片信息无效"));
        }
        let start = part_order
            .checked_mul(block_size)
            .context("计算 QQ 富媒体分片位置失败")?;
        let end = start.saturating_add(block_size).min(bytes.len());
        let chunk = &bytes[start..end];

        let put_response = state
            .http
            .put(&part.presigned_url)
            .body(chunk.to_vec())
            .timeout(UPLOAD_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("上传 QQ 富媒体分片 {} 失败", part.index))?;
        check_upload_response(put_response, part.index).await?;

        let finish_body = json!({
            "upload_id": prepared.upload_id,
            "part_index": part.index,
            "block_size": chunk.len().to_string(),
            "md5": digest_md5(chunk),
        });
        let finish_response = state
            .http
            .post(target_api_url(target, "upload_part_finish"))
            .header("Authorization", format!("QQBot {access_token}"))
            .json(&finish_body)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("确认 QQ 富媒体分片 {} 失败", part.index))?;
        check_send_response(finish_response, "upload_part_finish").await?;
    }

    let merge_body = json!({
        "file_type": file_type,
        "srv_send_msg": false,
        "file_name": file_name,
        "upload_id": prepared.upload_id,
    });
    let merge_response = state
        .http
        .post(target_api_url(target, "files"))
        .header("Authorization", format!("QQBot {access_token}"))
        .json(&merge_body)
        .timeout(UPLOAD_TIMEOUT)
        .send()
        .await
        .context("合并 QQ 富媒体分片失败")?;
    let uploaded: MediaUploadResponse =
        parse_json_response(merge_response, "合并 QQ 富媒体分片").await?;
    if uploaded.file_info.is_empty() {
        return Err(anyhow!("QQ 富媒体上传响应缺少 file_info"));
    }

    // 以 msg_type=7 发送富媒体消息
    let body = json!({
        "msg_type": 7,
        "msg_id": msg_id,
        "msg_seq": msg_seq,
        "media": {
            "file_info": uploaded.file_info,
        },
    });
    let response = state
        .http
        .post(target_api_url(target, "messages"))
        .header("Authorization", format!("QQBot {access_token}"))
        .json(&body)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("发送 QQ 富媒体消息失败")?;
    check_send_response(response, "send_rich_media").await?;
    tracing::info!("QQ 富媒体回复发送成功");
    Ok(())
}

fn target_api_url(target: &ReplyTarget, endpoint: &str) -> String {
    match target {
        ReplyTarget::Group(group_openid) => {
            format!("{OPENAPI_BASE}/v2/groups/{group_openid}/{endpoint}")
        }
        ReplyTarget::User(user_openid) => {
            format!("{OPENAPI_BASE}/v2/users/{user_openid}/{endpoint}")
        }
    }
}

fn digest_md5(bytes: &[u8]) -> String {
    format!("{:x}", Md5::digest(bytes))
}

fn digest_sha1(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

async fn check_upload_response(response: reqwest::Response, part_index: usize) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(anyhow!(
        "上传 QQ 富媒体分片 {part_index} 失败（HTTP {status}）: {}",
        truncate_str(&body, 256)
    ))
}

async fn parse_json_response<T>(response: reqwest::Response, action: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "{action}失败（HTTP {status}）: {}",
            truncate_str(&body, 256)
        ));
    }
    let value: Value = serde_json::from_str(&body)
        .with_context(|| format!("解析 {action} 响应失败: {}", truncate_str(&body, 256)))?;
    if let Some(code) = value.get("code").and_then(Value::as_i64)
        && code != 0
    {
        let message = value
            .get("message")
            .or_else(|| value.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(anyhow!("{action}业务失败: code={code}, message={message}"));
    }
    serde_json::from_value(value).with_context(|| format!("{action}响应字段无效"))
}

async fn check_send_response(response: reqwest::Response, api: &str) -> Result<()> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "QQ {api} 失败（HTTP {status}）: {}",
            truncate_str(&body, 256)
        ));
    }
    // QQ OpenAPI 在 HTTP 200 下用 code 表示业务结果（0=成功）
    if let Ok(value) = serde_json::from_str::<Value>(&body)
        && let Some(code) = value.get("code").and_then(Value::as_i64)
        && code != 0
    {
        let message = value
            .get("message")
            .or_else(|| value.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(anyhow!("QQ {api} 业务失败: code={code}, message={message}"));
    }
    Ok(())
}

// ── 图片下载 ──────────────────────────────────────────────────

struct DownloadedImage {
    url: String,
    mime_type: String,
}

async fn download_attachment(
    state: &BotState,
    attachment: &QqAttachment,
) -> Result<DownloadedImage> {
    // QQ 富媒体 URL 可能需要补全 https 前缀
    let raw_url = attachment.url.trim();
    let url = if raw_url.starts_with("http://") || raw_url.starts_with("https://") {
        raw_url.to_string()
    } else {
        format!("{OPENAPI_BASE}{raw_url}")
    };

    let access_token = state.token.get().await?;
    let response = state
        .http
        .get(&url)
        .header("Authorization", format!("QQBot {access_token}"))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| anyhow!("下载 QQ 图片请求失败: {error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "下载 QQ 图片失败（HTTP {status}）: {}",
            truncate_str(&body, 256)
        ));
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .filter(|value| value.starts_with("image/"))
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| anyhow!("读取 QQ 图片数据失败: {error}"))?;
    if bytes.is_empty() {
        return Err(anyhow!("QQ 图片下载数据为空"));
    }
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
    Ok(DownloadedImage {
        url: format!("data:{content_type};base64,{encoded}"),
        mime_type: content_type,
    })
}

// ── 工具 ──────────────────────────────────────────────────────

/// 去除 QQ 群 @机器人 的前缀（QQ 群消息 content 形如 `<@!123> 实际文本`）。
fn strip_at_mention(content: &str) -> String {
    let mut remaining = content.trim();
    while remaining.starts_with('<') {
        if let Some(end) = remaining.find('>') {
            remaining = remaining[end + 1..].trim_start();
        } else {
            break;
        }
    }
    remaining.to_string()
}

fn qq_file_type(path: &std::path::Path) -> u32 {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => 1,
        "mp4" | "mov" | "avi" | "mkv" => 2,
        "mp3" | "wav" | "m4a" | "aac" | "flac" => 3,
        _ => 4,
    }
}

fn truncate_str(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_is_prefixed_and_mentions_push_target_only_when_available() {
        let parsed = ParsedMessage {
            content: ApiMessageContent::Text {
                text: "你好".into(),
            },
            media: Vec::new(),
        }
        .with_source("[来源：QQ 群聊]");
        assert!(matches!(
            parsed.content,
            ApiMessageContent::Text { ref text } if text == "[来源：QQ 群聊]\n你好"
        ));

        assert_eq!(message_source("group", None), "[来源：QQ 群聊]");
        let push = ("bot-qq".to_string(), "target-1".to_string());
        let with_push = message_source("direct", Some(&push));
        assert!(with_push.starts_with("[来源：QQ 私聊"));
        assert!(with_push.contains("bot-qq"));
        assert!(with_push.contains("send_text_message（target_id=target-1）"));
        assert!(with_push.contains("直接输出的文本不会送达"));
        assert!(push_channel(None).is_none());
    }

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
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap();
        let state = BotState {
            http: http.clone(),
            tiangong_http: Client::builder().build().unwrap(),
            token: AccessTokenCache::new(http, String::new(), String::new()),
            tiangong_url,
            tiangong_token: None,
            recent_messages: Mutex::new(RecentMessages::default()),
        };

        let started = std::time::Instant::now();
        let reply = forward_to_tiangong(
            &state,
            "mock-channel",
            "mock-sender",
            "mock-message",
            ParsedMessage {
                content: ApiMessageContent::Text {
                    text: "mock-request".to_string(),
                },
                media: Vec::new(),
            },
        )
        .await
        .expect("天工长任务响应不应在 120 秒时超时");

        assert!(started.elapsed() >= LONG_MOCK_DELAY);
        assert_eq!(reply.text, "mock-ok");
        server.await.expect("长时 Mock 服务异常退出");
    }

    #[test]
    fn recent_messages_deduplicates_and_releases() {
        let mut messages = RecentMessages::default();
        assert!(messages.claim("group:1", "msg-1"));
        assert!(!messages.claim("group:1", "msg-1"));
        assert!(messages.claim("c2c:2", "msg-1"));
        messages.release("group:1", "msg-1");
        assert!(messages.claim("group:1", "msg-1"));
    }

    #[test]
    fn strip_at_mention_removes_robot_prefix() {
        assert_eq!(strip_at_mention("<@!102000> 你好"), "你好");
        assert_eq!(strip_at_mention("  <@!1><@!2> 两段"), "两段");
        assert_eq!(strip_at_mention("普通消息"), "普通消息");
    }

    #[test]
    fn qq_file_type_classifies_known_extensions() {
        assert_eq!(qq_file_type(std::path::Path::new("/tmp/a.png")), 1);
        assert_eq!(qq_file_type(std::path::Path::new("/tmp/a.mp4")), 2);
        assert_eq!(qq_file_type(std::path::Path::new("/tmp/a.mp3")), 3);
        assert_eq!(qq_file_type(std::path::Path::new("/tmp/a.txt")), 4);
    }

    #[test]
    fn connector_request_preserves_message_id_and_images() {
        let request = ConnectorRequest {
            connector: "qq-bot".to_string(),
            channel_id: "group:abc".to_string(),
            sender_id: "member-1".to_string(),
            message_id: Some("msg-1".to_string()),
            message: None,
            content: Some(ApiMessageContent::Text {
                text: "你好".into(),
            }),
            media: vec![MediaAsset::image(
                "data:image/png;base64,AA==".into(),
                "image/png".into(),
            )],
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["connector"], "qq-bot");
        assert_eq!(value["channel_id"], "group:abc");
        assert_eq!(value["message_id"], "msg-1");
        assert_eq!(value["media"][0]["capability"], "multimodal");
    }

    #[test]
    fn reply_extracts_local_files_and_caption() {
        let reply = reply_from_connector_response(ConnectorResponse {
            session_id: "s".into(),
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

    #[test]
    fn reply_ignores_remote_file_urls() {
        let reply = reply_from_connector_response(ConnectorResponse {
            session_id: "s".into(),
            message: "仅文本".into(),
            content: ApiMessageContent::Text {
                text: "仅文本".into(),
            },
            attachments: vec![ApiMessageContent::Image {
                url: "https://example.com/x.png".into(),
                caption: None,
            }],
        });
        assert!(reply.files.is_empty());
    }

    #[test]
    fn c2c_message_parses_attachments() {
        let raw = r#"{
            "id":"msg-9",
            "content":"看图",
            "author":{"member_openid":"","user_openid":"user-1"},
            "attachments":[{"content_type":"image/png","url":"/path","filename":"a.png"}]
        }"#;
        let message: C2cMessage = serde_json::from_str(raw).unwrap();
        assert_eq!(message.id, "msg-9");
        assert_eq!(message.author.user_openid, "user-1");
        assert_eq!(message.attachments.len(), 1);
        assert_eq!(message.attachments[0].content_type, "image/png");
    }

    #[test]
    fn group_message_parses_basic_fields() {
        let raw = r#"{
            "id":"msg-1",
            "content":"<@!1> 你好",
            "group_openid":"group-1",
            "message_type":"group",
            "author":{"member_openid":"member-1","user_openid":""}
        }"#;
        let message: GroupAtMessage = serde_json::from_str(raw).unwrap();
        assert_eq!(message.group_openid, "group-1");
        assert_eq!(message.author.member_openid, "member-1");
    }

    #[test]
    fn group_message_decodes_attachments() {
        let raw = r#"{
            "id":"msg-2",
            "content":"看图",
            "group_openid":"group-1",
            "author":{"member_openid":"member-1","user_openid":""},
            "attachments":[{"content_type":"image/png","url":"/path","filename":"a.png"}]
        }"#;
        let message: GroupAtMessage = serde_json::from_str(raw).unwrap();
        assert_eq!(message.attachments.len(), 1);
        assert_eq!(message.attachments[0].content_type, "image/png");
        assert_eq!(message.attachments[0].filename, "a.png");
    }

    #[tokio::test]
    async fn parse_text_with_images_text_only() {
        let state = bot_state_for_test();
        let parsed = parse_text_with_images(&state, "你好", &[])
            .await
            .expect("应解析文本");
        assert!(matches!(parsed.content, ApiMessageContent::Text { ref text } if text == "你好"));
        assert!(parsed.media.is_empty());
    }

    #[tokio::test]
    async fn parse_text_with_images_empty_returns_none() {
        let state = bot_state_for_test();
        // 无文本且无图片附件 → None（跳过）
        assert!(parse_text_with_images(&state, "   ", &[]).await.is_none());
    }

    fn bot_state_for_test() -> BotState {
        BotState {
            http: Client::new(),
            tiangong_http: Client::new(),
            // 测试只覆盖文本/空附件路径，不会触发 token 刷新，故用空凭证
            token: AccessTokenCache::new(Client::new(), String::new(), String::new()),
            tiangong_url: String::new(),
            tiangong_token: None,
            recent_messages: Mutex::new(RecentMessages::default()),
        }
    }
}
