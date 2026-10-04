//! 天工飞书 bot 制品。
//!
//! 独立编译的二进制，由天工主程序在运行时下载并启动。
//! 通过 openlark WebSocket 长连接接收飞书消息，转发到天工本地 embedded
//! server 的 `POST /api/v1/messages`，并把回复通过飞书互动卡片发回。
//!
//! 凭证由天工主程序通过环境变量注入：
//! - `TIANGONG_BOT_FEISHU_APP_ID` / `TIANGONG_BOT_FEISHU_APP_SECRET`
//! - `TIANGONG_URL`（默认 `http://127.0.0.1:8080`）
//! - `TIANGONG_TOKEN`（可选，embedded server 认证 token）

use std::collections::VecDeque;
use std::future::Future;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use openlark_client::CoreConfig as Config;
use openlark_client::ws_client::{EventDispatcherHandler, EventHandler, LarkWsClient};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio::signal;
use tokio::sync::{Mutex, RwLock};
use tracing_subscriber::EnvFilter;

mod mcp;
mod provision;
mod schema;
mod target_store;
#[cfg(test)]
#[path = "../../test_support.rs"]
mod test_support;

// ── 数据结构 ──────────────────────────────────────────────────

struct BotState {
    http: reqwest::Client,
    /// 天工任务允许长时间执行，不设置请求总时限。
    tiangong_http: reqwest::Client,
    app_id: String,
    app_secret: String,
    access_token: RwLock<Option<String>>,
    /// 天工 server 地址，如 http://127.0.0.1:8080
    tiangong_url: String,
    /// 天工 server 认证 token（可选）
    tiangong_token: Option<String>,
    recent_messages: Mutex<RecentMessages>,
}

const RECENT_MESSAGE_LIMIT: usize = 1024;

#[derive(Default)]
struct RecentMessages {
    keys: VecDeque<(String, String)>,
}

impl RecentMessages {
    fn claim(&mut self, channel_id: &str, message_id: &str) -> bool {
        if self
            .keys
            .iter()
            .any(|(channel, message)| channel == channel_id && message == message_id)
        {
            return false;
        }
        self.keys
            .push_back((channel_id.to_string(), message_id.to_string()));
        if self.keys.len() > RECENT_MESSAGE_LIMIT {
            self.keys.pop_front();
        }
        true
    }

    fn release(&mut self, channel_id: &str, message_id: &str) {
        self.keys
            .retain(|(channel, message)| channel != channel_id || message != message_id);
    }
}

#[derive(Debug, Deserialize)]
struct EventEnvelope {
    event: EventBody,
}

#[derive(Debug, Deserialize)]
struct EventBody {
    sender: EventSender,
    message: EventMessage,
}

#[derive(Debug, Deserialize)]
struct EventSender {
    sender_id: EventSenderId,
    #[serde(rename = "senderType", alias = "sender_type")]
    sender_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct EventSenderId {
    #[serde(rename = "openId", alias = "open_id")]
    open_id: Option<String>,
    #[serde(rename = "unionId", alias = "union_id")]
    union_id: Option<String>,
    #[serde(rename = "userId", alias = "user_id")]
    user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventMessage {
    #[serde(rename = "chatId", alias = "chat_id")]
    chat_id: String,
    #[serde(
        rename = "messageType",
        alias = "message_type",
        alias = "msg_type",
        default
    )]
    msg_type: String,
    #[serde(rename = "chatType", alias = "chat_type", default)]
    chat_type: Option<String>,
    #[serde(default)]
    content: String,
    #[serde(rename = "messageId", alias = "message_id")]
    message_id: Option<String>,
}

// ── 天工 Server API 类型 ──────────────────────────────────────

/// POST /api/v1/messages 请求体
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

/// POST /api/v1/messages 响应体
#[derive(Debug, Deserialize)]
struct ConnectorResponse {
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    connector: String,
    #[allow(dead_code)]
    channel_id: String,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
    },
    #[allow(dead_code)]
    File {
        url: String,
        name: String,
    },
    #[allow(dead_code)]
    Audio {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration: Option<u32>,
    },
    #[allow(dead_code)]
    Video {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
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
            Self::Image { url, .. } => url.trim().is_empty(),
            Self::File { url, name } => url.trim().is_empty() || name.trim().is_empty(),
            Self::Audio { url, .. } | Self::Video { url, .. } => url.trim().is_empty(),
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

/// 生成附加在入站消息前的来源说明（issue #572）。
///
/// 告诉模型消息来自哪个渠道；推送目标可用时说明如何经本 Bot 的 MCP 发送工具
/// 向用户推送中间进展。最终答复仍走同步响应，避免重复发送。
/// 本条消息可用的推送通道：(MCP 服务名, 推送目标编号)。
///
/// 两者都可用时，回复全部交由 Agent 经 MCP 发送（issue #572）；任一缺失时
/// Agent 无法推送，退回由 Bot 转发同步响应。
fn push_channel(target_id: Option<String>) -> Option<(String, String)> {
    let target_id = target_id?;
    match mcp::server_name() {
        Ok(server) => Some((server, target_id)),
        Err(error) => {
            tracing::warn!("获取飞书 MCP 服务名失败，回退为同步回复: {error}");
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
        "飞书群聊"
    } else {
        "飞书私聊"
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
    fn image(url: String) -> Self {
        Self {
            kind: "image".to_string(),
            url,
            mime_type: None,
            title: None,
            capability: Some("multimodal".to_string()),
        }
    }
}

struct ParsedMessage {
    content: ApiMessageContent,
    media: Vec<MediaAsset>,
}

/// 待发送的图片数据（已解析为字节，飞书上传前需要原始字节）
struct OutboundImage {
    bytes: Vec<u8>,
    mime_type: String,
    name: Option<String>,
}

/// 天工响应摊开后的飞书回复：文本 + 一组图片
struct BotReply {
    text: String,
    images: Vec<OutboundImage>,
}

impl BotReply {
    fn text(text: &str) -> Self {
        Self {
            text: text.to_string(),
            images: Vec::new(),
        }
    }
}

/// 天工响应 → 飞书回复
///
/// 对齐微信 `reply_from_connector_response`：遍历 `content` 与 `attachments`，
/// 文本取 `content.text()`（空则回退 `message`），图片收集为 `OutboundImage`。
async fn build_reply(state: &Arc<BotState>, body: ConnectorResponse) -> BotReply {
    let text = {
        let content_text = body.content.text();
        if content_text.trim().is_empty() {
            body.message
        } else {
            content_text
        }
    };

    let mut images = Vec::new();
    for content in std::iter::once(&body.content).chain(body.attachments.iter()) {
        match content {
            ApiMessageContent::Image { url, .. } => {
                if let Some(img) = resolve_outbound_image(state, url).await {
                    images.push(img);
                }
            }
            ApiMessageContent::File { .. }
            | ApiMessageContent::Audio { .. }
            | ApiMessageContent::Video { .. } => {
                tracing::warn!("飞书暂不支持发送 {} 附件，跳过", content.kind());
            }
            ApiMessageContent::Text { .. } => {}
        }
    }

    BotReply { text, images }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MessageHandling {
    Forwarded,
    ReplyFailed,
    Ignored,
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
            install_rustls_provider()?;
            println!("{}", serde_json::to_string(&provision::begin().await?)?);
            return Ok(());
        }
        Some("--provision-poll") => {
            install_rustls_provider()?;
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

    // 初始化日志（输出到 stderr，主程序捕获 tail）
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .with_writer(std::io::stderr)
        .init();

    // 安装 rustls 加密后端
    install_rustls_provider()?;

    if mode == Some("--mcp") {
        return mcp::serve().await;
    }

    let credentials = provision::load_credentials()?;
    let app_id = credentials.app_id;
    let app_secret = credentials.app_secret;
    let tiangong_url =
        std::env::var("TIANGONG_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let tiangong_token = std::env::var("TIANGONG_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());

    tracing::info!("飞书机器人启动中...");
    tracing::info!("天工服务地址: {tiangong_url}");

    let state = Arc::new(BotState {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?,
        tiangong_http: reqwest::Client::builder().build()?,
        app_id,
        app_secret,
        access_token: RwLock::new(None),
        tiangong_url,
        tiangong_token,
        recent_messages: Mutex::new(RecentMessages::default()),
    });

    // 注册事件处理器
    let handler = FeishuHandler {
        state: state.clone(),
    };
    let dispatcher = EventDispatcherHandler::builder()
        .register_raw("im.message.receive_v1", handler)
        .map_err(|e| anyhow!("注册事件处理器失败: {e}"))?
        .build();

    // 构建配置
    let config = Config::builder()
        .app_id(&state.app_id)
        .app_secret(&state.app_secret)
        .build();

    tracing::info!("正在连接飞书 WebSocket 长连接...");

    // 每次调用重新执行 LarkWsClient::open：内部会重新拉取飞书 WS 端点，
    // 因此重连时无需手工刷新连接地址。
    let connect = {
        let config = Arc::new(config);
        move || {
            let (config, dispatcher) = (config.clone(), dispatcher.clone());
            async move {
                let started = Instant::now();
                let outcome = match LarkWsClient::open(config, dispatcher).await {
                    Ok(()) => ConnectOutcome::Closed,
                    Err(error) => {
                        tracing::warn!(error = %error, "飞书 WebSocket 连接断开");
                        ConnectOutcome::Failed
                    }
                };
                (outcome, started.elapsed())
            }
        }
    };

    let stop: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(wait_for_stop());
    reconnect_loop(ReconnectPolicy::default(), connect, stop).await;

    tracing::info!("飞书机器人已停止");
    Ok(())
}

// ── WebSocket 重连 ───────────────────────────────────────────

/// 单次 `LarkWsClient::open` 的结果分类。
///
/// `open` 在会话终止时几乎总是返回 `Err`，正常关闭也以
/// `Err(ConnectionClosed)` 体现；这里把「正常关闭」与「异常失败」区分开，
/// 便于日志分级与后续策略扩展。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConnectOutcome {
    /// 会话正常结束（`open` 返回 `Ok`）。
    Closed,
    /// 连接或会话异常退出（`open` 返回 `Err`）。
    Failed,
}

/// 重连退避策略。
#[derive(Clone, Copy, Debug)]
struct ReconnectPolicy {
    /// 首次重连前的等待时间。
    initial_backoff: Duration,
    /// 退避时间的上限。
    max_backoff: Duration,
    /// 单次连接稳定运行超过该阈值后，下一次退避重置为 `initial_backoff`。
    stable_threshold: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            stable_threshold: Duration::from_secs(60),
        }
    }
}

/// 根据上一次连接的持续时长，计算下一次重连前的退避时间。
///
/// - 连接稳定运行超过 `stable_threshold`：退避重置为 `initial_backoff`；
/// - 否则在 `current` 基础上翻倍，但不超过 `max_backoff`。
fn next_backoff(policy: &ReconnectPolicy, current: Duration, lasted: Duration) -> Duration {
    if lasted >= policy.stable_threshold {
        policy.initial_backoff
    } else {
        std::cmp::min(current.saturating_mul(2), policy.max_backoff)
    }
}

/// 为退避时间叠加少量随机抖动，避免多实例集中重连。
///
/// 抖动上限为 `backoff / 4`，`seed` 决定具体取值；注入 seed 便于单测断言。
fn backoff_with_jitter(backoff: Duration, seed: u128) -> Duration {
    if backoff.is_zero() {
        return backoff;
    }
    let jitter_cap = backoff / 4;
    if jitter_cap.is_zero() {
        return backoff;
    }
    let jitter = Duration::from_nanos((seed % jitter_cap.as_nanos().max(1)) as u64);
    backoff + jitter
}

/// 收集一个用于抖动的种子，来源为系统时钟纳秒。
fn jitter_seed() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// 重连驱动。
///
/// 每一轮：先 `select` 同时等待 `connect()` 与停止信号；连接结束后按策略计算
/// 下一次退避时间，再 `select` 等待退避与停止信号。停止信号在连接与退避
/// 两个阶段都会被监听，确保收到 SIGTERM/SIGINT 时立即退出且不再重连。
async fn reconnect_loop<F, Fut>(
    policy: ReconnectPolicy,
    mut connect: F,
    mut stop: Pin<Box<dyn Future<Output = ()> + Send>>,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = (ConnectOutcome, Duration)>,
{
    let mut backoff = policy.initial_backoff;
    let mut attempt: u64 = 0;

    loop {
        attempt += 1;

        let (outcome, lasted) = tokio::select! {
            biased;
            _ = &mut stop => {
                tracing::info!("收到停止信号，不再重连 attempt={attempt}");
                return;
            }
            pair = connect() => pair,
        };

        let lasted_secs = lasted.as_secs_f64();
        match outcome {
            ConnectOutcome::Closed => tracing::info!(
                lasted_secs = format!("{lasted_secs:.3}"),
                "飞书 WebSocket 长连接已结束"
            ),
            ConnectOutcome::Failed => tracing::warn!(
                lasted_secs = format!("{lasted_secs:.3}"),
                "飞书 WebSocket 长连接异常退出"
            ),
        }

        backoff = next_backoff(&policy, backoff, lasted);
        let delay = backoff_with_jitter(backoff, jitter_seed());
        tracing::info!(
            attempt,
            delay_secs = format!("{:.3}", delay.as_secs_f64()),
            backoff_secs = format!("{:.3}", backoff.as_secs_f64()),
            "等待后重连"
        );

        tokio::select! {
            biased;
            _ = &mut stop => {
                tracing::info!("收到停止信号，不再重连 attempt={attempt}");
                return;
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// 等待进程终止信号（SIGTERM/SIGINT/Ctrl+C），主程序 stop 时发送。
async fn wait_for_stop() {
    #[cfg(unix)]
    {
        let term = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .context("注册 SIGTERM 处理失败")
                .expect("注册 SIGTERM 处理失败")
                .recv()
                .await;
            tracing::info!("收到 SIGTERM，正在退出...");
        };
        let int = async {
            signal::unix::signal(signal::unix::SignalKind::interrupt())
                .context("注册 SIGINT 处理失败")
                .expect("注册 SIGINT 处理失败")
                .recv()
                .await;
            tracing::info!("收到 SIGINT，正在退出...");
        };
        tokio::select! {
            _ = term => {}
            _ = int => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = signal::ctrl_c().await;
        tracing::info!("收到 Ctrl+C，正在退出...");
    }
}

fn install_rustls_provider() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|e| anyhow!("安装 rustls provider 失败: {e:?}"))
}

// ── 事件处理器 ────────────────────────────────────────────────

struct FeishuHandler {
    state: Arc<BotState>,
}

impl EventHandler for FeishuHandler {
    fn handle(&self, event: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let state = self.state.clone();
        let payload = event.to_vec();

        tokio::spawn(async move {
            if let Err(e) = handle_event(&state, &payload).await {
                tracing::error!("处理飞书消息失败: {e}");
            }
        });

        Ok(())
    }
}

// ── 事件处理 ──────────────────────────────────────────────────

async fn handle_event(state: &Arc<BotState>, payload: &[u8]) -> Result<()> {
    let raw = String::from_utf8_lossy(payload);
    let envelope: EventEnvelope = serde_json::from_str(&raw).map_err(|e| {
        tracing::error!("解析事件 JSON 失败: {e}\n原始数据: {raw}");
        anyhow!("解析事件 JSON 失败: {e}")
    })?;

    // 只处理用户消息，忽略机器人自身消息
    if envelope.event.sender.sender_type.as_deref() != Some("user") {
        return Ok(());
    }

    let chat_id = &envelope.event.message.chat_id;
    let target_kind = if envelope.event.message.chat_type.as_deref() == Some("group") {
        "group"
    } else {
        "direct"
    };
    let target_id = match target_store::upsert_discovered(chat_id, target_kind) {
        Ok(target_id) => Some(target_id),
        Err(error) => {
            tracing::warn!("更新飞书推送目标失败: {error}");
            None
        }
    };
    let push = push_channel(target_id);
    let source = message_source(target_kind, push.as_ref());
    let reply_by_agent = push.is_some();
    let msg_type = &envelope.event.message.msg_type;
    let message_id = envelope.event.message.message_id.clone();
    let sender_id = envelope
        .event
        .sender
        .sender_id
        .open_id
        .clone()
        .or_else(|| envelope.event.sender.sender_id.user_id.clone())
        .unwrap_or_default();

    tracing::info!("收到飞书消息 chat_id={chat_id} msg_type={msg_type} sender={sender_id}");

    // 如果 msg_type 为空，打印原始 JSON 用于调试
    if msg_type.is_empty() {
        tracing::warn!("msg_type 为空，原始事件数据:\n{raw}");
    }

    if let Some(message_id) = message_id.as_deref()
        && !state
            .recent_messages
            .lock()
            .await
            .claim(chat_id, message_id)
    {
        tracing::info!("忽略重复飞书消息 chat_id={chat_id} message_id={message_id}");
        return Ok(());
    }

    // 给用户消息添加已接收标记
    let mut salute_reaction_id = String::new();
    if let Some(ref msg_id) = message_id {
        match add_reaction(state, msg_id, "SALUTE").await {
            Ok(rid) => salute_reaction_id = rid,
            Err(e) => tracing::warn!("添加接收标记失败: {e}"),
        }
    }

    let handling = process_user_message(
        state,
        &envelope.event.message,
        chat_id,
        &sender_id,
        &message_id,
        &source,
        reply_by_agent,
    )
    .await;

    let retryable_failure = handling.is_err();
    if let Err(error) = &handling {
        tracing::error!("飞书消息处理失败: {error}");
        if let Err(reply_error) = send_reply(
            state,
            chat_id,
            &BotReply::text("抱歉，处理消息时出现了错误，请稍后重试。"),
        )
        .await
        {
            tracing::warn!("发送失败提示失败: {reply_error}");
        }
    }

    finish_message_reaction(
        state,
        message_id.as_deref(),
        &salute_reaction_id,
        matches!(&handling, Ok(MessageHandling::Forwarded)),
    )
    .await;

    if retryable_failure && let Some(message_id) = message_id.as_deref() {
        state
            .recent_messages
            .lock()
            .await
            .release(chat_id, message_id);
    }

    handling.map(|_| ())
}

async fn process_user_message(
    state: &Arc<BotState>,
    message: &EventMessage,
    chat_id: &str,
    sender_id: &str,
    message_id: &Option<String>,
    source: &str,
    reply_by_agent: bool,
) -> Result<MessageHandling> {
    let parsed = match message.msg_type.as_str() {
        "text" => {
            let text = extract_text_from_message(&message.content)?;
            ParsedMessage {
                content: ApiMessageContent::Text { text },
                media: Vec::new(),
            }
        }
        "image" => {
            let image_key = extract_image_key(&message.content)?;
            tracing::info!("收到图片消息 image_key={image_key}");
            let image_url = download_image_as_data_uri(state, message_id, &image_key).await?;
            ParsedMessage {
                content: ApiMessageContent::Image {
                    url: image_url,
                    caption: None,
                },
                media: Vec::new(),
            }
        }
        "post" => {
            tracing::info!(
                "开始解析 post 消息 content={}",
                truncate_str(&message.content, 200)
            );
            let (text, images, failed_images) =
                extract_post_with_images(&message.content, state, message_id).await?;
            tracing::info!(
                "post 解析完成 text={} images={} failed_images={failed_images}",
                truncate_str(&text, 100),
                images.len()
            );
            if text.trim().is_empty() && images.is_empty() && failed_images > 0 {
                return Err(anyhow!("post 消息中的图片全部下载失败"));
            }
            compose_post_message(text, images)
        }
        _ => {
            tracing::warn!("不支持的消息类型: {}", message.msg_type);
            return Ok(MessageHandling::Ignored);
        }
    };

    if parsed.content.is_empty() {
        tracing::info!("忽略空消息 msg_type={}", message.msg_type);
        return Ok(MessageHandling::Ignored);
    }

    tracing::info!(
        "转发到天工 chat_id={chat_id} content_type={} media={}",
        parsed.content.kind(),
        parsed.media.len()
    );

    match forward_to_tiangong(
        state,
        chat_id,
        sender_id,
        message_id,
        parsed.content.with_source(source),
        parsed.media,
    )
    .await
    {
        Ok(reply) => {
            if reply_by_agent {
                tracing::debug!("飞书回复由 Agent 经 MCP 发送，忽略同步响应 chat_id={chat_id}");
                return Ok(MessageHandling::Forwarded);
            }
            tracing::info!(
                "天工回复 chat_id={chat_id} text_len={} images={}",
                reply.text.len(),
                reply.images.len()
            );
            match send_reply(state, chat_id, &reply).await {
                Ok(()) => Ok(MessageHandling::Forwarded),
                Err(error) => {
                    tracing::error!("发送飞书回复失败: {error}");
                    Ok(MessageHandling::ReplyFailed)
                }
            }
        }
        Err(error) => {
            tracing::error!("天工调用失败: {error}");
            Err(error)
        }
    }
}

async fn finish_message_reaction(
    state: &Arc<BotState>,
    message_id: Option<&str>,
    salute_reaction_id: &str,
    completed: bool,
) {
    if let Some(message_id) = message_id {
        if !salute_reaction_id.is_empty()
            && let Err(error) = remove_reaction(state, message_id, salute_reaction_id).await
        {
            tracing::warn!("移除接收标记失败: {error}");
        }
        if completed && let Err(error) = add_reaction(state, message_id, "OK").await {
            tracing::warn!("添加完成标记失败: {error}");
        }
    }
}

// ── 消息解析 ──────────────────────────────────────────────────

fn extract_text_from_message(content: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(content).map_err(|e| anyhow!("解析消息 content 失败: {e}"))?;

    // text 类型: {"text": "xxx"}
    if let Some(text) = value["text"].as_str() {
        return Ok(text.to_string());
    }

    Ok(value.to_string())
}

fn extract_image_key(content: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(content).map_err(|e| anyhow!("解析图片消息失败: {e}"))?;

    let key = value["image_key"]
        .as_str()
        .ok_or_else(|| anyhow!("图片消息缺少 image_key"))?;

    Ok(key.to_string())
}

fn compose_post_message(text: String, images: Vec<String>) -> ParsedMessage {
    if images.is_empty() {
        return ParsedMessage {
            content: ApiMessageContent::Text { text },
            media: Vec::new(),
        };
    }

    if text.trim().is_empty() {
        let mut images = images.into_iter();
        let first_image = images.next().expect("已确认富文本至少包含一张图片");
        return ParsedMessage {
            content: ApiMessageContent::Image {
                url: first_image,
                caption: None,
            },
            media: images.map(MediaAsset::image).collect(),
        };
    }

    ParsedMessage {
        content: ApiMessageContent::Text { text },
        media: images.into_iter().map(MediaAsset::image).collect(),
    }
}

/// 解析 post 类型消息，提取文本和图片（图片下载为 data URI）
async fn extract_post_with_images(
    content: &str,
    state: &Arc<BotState>,
    message_id: &Option<String>,
) -> Result<(String, Vec<String>, usize)> {
    let value: serde_json::Value =
        serde_json::from_str(content).map_err(|e| anyhow!("解析 post 消息失败: {e}"))?;

    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    let mut failed_images = 0;

    // 飞书 post 有两种格式：
    // 1. 扁平格式: {"title":"","content":[[...]]}
    // 2. 多语言格式: {"zh_cn":{"title":"","content":[[...]]}}
    let paragraphs = if let Some(arr) = value["content"].as_array() {
        tracing::debug!("post 扁平格式，content 数组长度={}", arr.len());
        arr.clone()
    } else {
        // 多语言格式，遍历语言键
        tracing::debug!(
            "post 尝试多语言格式，顶层 keys={:?}",
            value.as_object().map(|o| o.keys().collect::<Vec<_>>())
        );
        let mut result = vec![];
        if let Some(obj) = value.as_object() {
            for (lang, body) in obj {
                if let Some(arr) = body["content"].as_array() {
                    tracing::debug!("post 语言={lang} content 数组长度={}", arr.len());
                    result.extend(arr.clone());
                }
            }
        }
        result
    };

    tracing::debug!("post paragraphs 数量={}", paragraphs.len());

    for (pi, para) in paragraphs.iter().enumerate() {
        if let Some(elems) = para.as_array() {
            tracing::debug!("post paragraph[{pi}] elements={}", elems.len());
            for (ei, elem) in elems.iter().enumerate() {
                let tag = elem["tag"].as_str().unwrap_or("");
                tracing::debug!("post paragraph[{pi}][{ei}] tag={tag}");
                match tag {
                    "text" => {
                        if let Some(t) = elem["text"].as_str() {
                            text_parts.push(t.to_string());
                        }
                    }
                    "at" => {
                        if let Some(uid) = elem["user_id"].as_str() {
                            text_parts.push(format!("@{uid}"));
                        }
                    }
                    "img" => {
                        if let Some(key) = elem["image_key"].as_str() {
                            tracing::info!("下载 post 内嵌图片 image_key={key}");
                            match download_image_as_data_uri(state, message_id, key).await {
                                Ok(url) => images.push(url),
                                Err(e) => {
                                    failed_images += 1;
                                    tracing::warn!("下载图片失败 key={key}: {e}");
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok((text_parts.join(""), images, failed_images))
}

// ── 图片下载 ──────────────────────────────────────────────────

async fn download_image_as_data_uri(
    state: &Arc<BotState>,
    message_id: &Option<String>,
    image_key: &str,
) -> Result<String> {
    let token = get_token(state).await?;
    let msg_id = message_id
        .as_deref()
        .ok_or_else(|| anyhow!("缺少 message_id，无法下载图片"))?;

    let resp = state
        .http
        .get(format!(
            "https://open.feishu.cn/open-apis/im/v1/messages/{msg_id}/resources/{image_key}"
        ))
        .query(&[("type", "image")])
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| anyhow!("下载图片请求失败: {e}"))?;

    let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        // token 过期，刷新重试
        let new_token = refresh_token(state).await?;
        state
            .http
            .get(format!(
                "https://open.feishu.cn/open-apis/im/v1/messages/{msg_id}/resources/{image_key}"
            ))
            .query(&[("type", "image")])
            .bearer_auth(&new_token)
            .send()
            .await
            .map_err(|e| anyhow!("重试下载图片失败: {e}"))?
    } else {
        resp
    };

    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .filter(|value| value.starts_with("image/"))
        .unwrap_or("image/jpeg")
        .to_string();
    tracing::debug!("图片下载响应 status={status} content_type={content_type}");

    if !status.is_success() {
        let error_body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "图片下载失败 status={status} body={}",
            truncate_str(&error_body, 512)
        ));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| anyhow!("读取图片数据失败: {e}"))?;
    if bytes.is_empty() {
        return Err(anyhow!("图片下载结果为空 image_key={image_key}"));
    }

    tracing::info!(
        "图片下载完成 image_key={image_key} size={}KB",
        bytes.len() / 1024
    );

    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
    Ok(format!("data:{content_type};base64,{b64}"))
}

// ── 天工 Server 调用 ──────────────────────────────────────────

async fn forward_to_tiangong(
    state: &Arc<BotState>,
    chat_id: &str,
    sender_id: &str,
    message_id: &Option<String>,
    content: ApiMessageContent,
    media: Vec<MediaAsset>,
) -> Result<BotReply> {
    let url = format!(
        "{}/api/v1/messages",
        state.tiangong_url.trim_end_matches('/')
    );

    let req_body = ConnectorRequest {
        connector: "feishu-bot".to_string(),
        channel_id: chat_id.to_string(),
        sender_id: sender_id.to_string(),
        message_id: message_id.clone(),
        message: None,
        content: Some(content),
        media,
    };

    let mut req = state.tiangong_http.post(&url);
    if let Some(ref token) = state.tiangong_token {
        req = req.bearer_auth(token);
    }

    let resp = req
        .json(&req_body)
        .send()
        .await
        .map_err(|e| anyhow!("天工 server 请求失败: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("天工 server 错误: {status}, {body}"));
    }

    let body: ConnectorResponse = resp
        .json()
        .await
        .map_err(|e| anyhow!("解析天工响应失败: {e}"))?;

    Ok(build_reply(state, body).await)
}

// ── 飞书 token 管理 ───────────────────────────────────────────

async fn refresh_token(state: &Arc<BotState>) -> Result<String> {
    let resp = state
        .http
        .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
        .json(&serde_json::json!({
            "app_id": &state.app_id,
            "app_secret": &state.app_secret,
        }))
        .send()
        .await
        .map_err(|e| anyhow!("获取 token 请求失败: {e}"))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("解析 token 响应失败: {e}"))?;

    let code = body["code"].as_i64().unwrap_or(-1);
    if code != 0 {
        let msg = body["msg"].as_str().unwrap_or("未知错误");
        return Err(anyhow!("获取 token 失败: code={code}, msg={msg}"));
    }

    let token = body["tenant_access_token"]
        .as_str()
        .ok_or_else(|| anyhow!("响应缺少 tenant_access_token"))?
        .to_string();

    *state.access_token.write().await = Some(token.clone());
    tracing::info!("飞书 access_token 已刷新");
    Ok(token)
}

async fn get_token(state: &Arc<BotState>) -> Result<String> {
    if let Some(t) = state.access_token.read().await.as_ref() {
        return Ok(t.clone());
    }
    refresh_token(state).await
}

// ── 发送飞书消息 ──────────────────────────────────────────────

/// 飞书互动卡片（Markdown 渲染）。token 过期自动刷新重试。
async fn send_card(state: &Arc<BotState>, chat_id: &str, text: &str) -> Result<()> {
    let card_content = serde_json::json!({
        "config": { "wide_screen_mode": true },
        "elements": [ { "tag": "markdown", "content": text } ]
    })
    .to_string();

    let payload = serde_json::json!({
        "receive_id": chat_id,
        "msg_type": "interactive",
        "content": card_content,
    });

    let body = post_message(state, payload).await?;
    let code = body["code"].as_i64().unwrap_or(-1);
    if code == 0 {
        return Ok(());
    }
    let msg = body["msg"].as_str().unwrap_or("未知错误");
    Err(anyhow!("发送卡片消息失败: code={code}, msg={msg}"))
}

/// 上传图片到飞书获取 `image_key`（`image_type=message`）。
async fn upload_image(state: &Arc<BotState>, image: &OutboundImage) -> Result<String> {
    let token = get_token(state).await?;
    let filename = image
        .name
        .clone()
        .unwrap_or_else(|| format!("image.{}", mime_extension(&image.mime_type)));

    let image_key = upload_image_request(state, &token, filename.clone(), &image.bytes).await;
    let image_key = match image_key {
        Ok(key) => key,
        Err(UploadError::TokenExpired) => {
            tracing::warn!("上传图片时 token 过期，刷新后重试...");
            let new_token = refresh_token(state).await?;
            upload_image_request(state, &new_token, filename, &image.bytes)
                .await
                .map_err(|e| anyhow!("重试上传图片失败: {e}"))?
        }
        Err(UploadError::Other(e)) => return Err(anyhow!("上传图片失败: {e}")),
    };

    tracing::info!(
        "图片上传完成 image_key={image_key} size={}KB",
        image.bytes.len() / 1024
    );
    Ok(image_key)
}

enum UploadError {
    TokenExpired,
    Other(String),
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TokenExpired => write!(f, "token 过期"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

async fn upload_image_request(
    state: &Arc<BotState>,
    token: &str,
    filename: String,
    bytes: &[u8],
) -> Result<String, UploadError> {
    let part = reqwest::multipart::Part::bytes(bytes.to_vec())
        .file_name(filename)
        .mime_str("application/octet-stream")
        .map_err(|e| UploadError::Other(format!("构造 multipart 失败: {e}")))?;
    let form = reqwest::multipart::Form::new()
        .text("image_type", "message")
        .part("image", part);

    let resp = state
        .http
        .post("https://open.feishu.cn/open-apis/im/v1/images")
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| UploadError::Other(format!("上传图片请求失败: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| UploadError::Other(format!("解析上传响应失败: {e}")))?;

    let code = body["code"].as_i64().unwrap_or(-1);
    if code == 99991663 || code == 99991668 {
        return Err(UploadError::TokenExpired);
    }
    if code != 0 {
        let msg = body["msg"].as_str().unwrap_or("未知错误");
        return Err(UploadError::Other(format!("code={code}, msg={msg}")));
    }

    body["data"]["image_key"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| UploadError::Other("响应缺少 image_key".to_string()))
}

/// 发送 `msg_type=image` 消息（单图）。
async fn send_image_message(state: &Arc<BotState>, chat_id: &str, image_key: &str) -> Result<()> {
    let payload = serde_json::json!({
        "receive_id": chat_id,
        "msg_type": "image",
        "content": serde_json::to_string(&serde_json::json!({ "image_key": image_key }))?,
    });
    let body = post_message(state, payload).await?;
    let code = body["code"].as_i64().unwrap_or(-1);
    if code == 0 {
        return Ok(());
    }
    let msg = body["msg"].as_str().unwrap_or("未知错误");
    Err(anyhow!("发送图片消息失败: code={code}, msg={msg}"))
}

/// 向 `im/v1/messages` 投递消息体，处理 token 过期刷新重试。
async fn post_message(
    state: &Arc<BotState>,
    payload: serde_json::Value,
) -> Result<serde_json::Value> {
    let send = |token: &str| {
        state
            .http
            .post("https://open.feishu.cn/open-apis/im/v1/messages")
            .bearer_auth(token)
            .query(&[("receive_id_type", "chat_id")])
            .json(&payload)
            .send()
    };

    let token = get_token(state).await?;
    let resp = send(&token)
        .await
        .map_err(|e| anyhow!("发送消息请求失败: {e}"))?;
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("解析发送响应失败: {e}"))?;

    let code = body["code"].as_i64().unwrap_or(-1);
    if code != 0 && (code == 99991663 || code == 99991668) {
        tracing::warn!("token 过期，刷新后重试...");
        let new_token = refresh_token(state).await?;
        let retry = send(&new_token)
            .await
            .map_err(|e| anyhow!("重试发送失败: {e}"))?;
        return retry
            .json()
            .await
            .map_err(|e| anyhow!("解析重试响应失败: {e}"));
    }
    Ok(body)
}

/// 解析待发送图片 URL 为字节，支持 data URI、本地路径、http(s)。
async fn resolve_outbound_image(state: &Arc<BotState>, url: &str) -> Option<OutboundImage> {
    if let Some(rest) = url.strip_prefix("data:") {
        // data:{mime};base64,{data}
        let (mime, data) = match rest.split_once(',') {
            Some((meta, data)) => {
                let mime = meta
                    .split(';')
                    .next()
                    .filter(|m| !m.is_empty())
                    .unwrap_or("image/png");
                (mime.to_string(), data)
            }
            None => return None,
        };
        match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data.trim()) {
            Ok(bytes) => {
                return Some(OutboundImage {
                    bytes,
                    mime_type: mime,
                    name: None,
                });
            }
            Err(e) => {
                tracing::warn!("data URI base64 解码失败: {e}");
                return None;
            }
        }
    }

    let (path, needs_download) = match url.strip_prefix("file://") {
        Some(p) => (PathBuf::from(p), false),
        None if PathBuf::from(url).is_absolute() => (PathBuf::from(url), false),
        None if url.starts_with("http://") || url.starts_with("https://") => {
            (PathBuf::from(url), true)
        }
        _ => {
            tracing::warn!("忽略非本地/可下载形式的图片 URL: {url}");
            return None;
        }
    };

    if needs_download {
        let resp = state.http.get(url).send().await;
        match resp {
            Ok(resp) if resp.status().is_success() => {
                let mime = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.split(';').next())
                    .filter(|v| v.starts_with("image/"))
                    .unwrap_or("image/png")
                    .to_string();
                match resp.bytes().await {
                    Ok(bytes) if !bytes.is_empty() => {
                        return Some(OutboundImage {
                            bytes: bytes.to_vec(),
                            mime_type: mime,
                            name: path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .map(|s| s.to_string()),
                        });
                    }
                    Ok(_) => tracing::warn!("图片下载结果为空: {url}"),
                    Err(e) => tracing::warn!("读取图片数据失败: {url} {e}"),
                }
            }
            Ok(resp) => tracing::warn!("图片下载失败: {url} status={}", resp.status()),
            Err(e) => tracing::warn!("图片下载请求失败: {url} {e}"),
        }
        return None;
    }

    match std::fs::read(&path) {
        Ok(bytes) => {
            let mime = detect_image_mime(&bytes).unwrap_or_else(|| "image/png".to_string());
            Some(OutboundImage {
                bytes,
                mime_type: mime,
                name: path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string()),
            })
        }
        Err(e) => {
            tracing::warn!("读取本地图片失败: {} {e}", path.display());
            None
        }
    }
}

/// 发送天工回复：先发文本卡片，再逐张上传并发送图片。
async fn send_reply(state: &Arc<BotState>, chat_id: &str, reply: &BotReply) -> Result<()> {
    if !reply.text.trim().is_empty()
        && let Err(error) = send_card(state, chat_id, &reply.text).await
    {
        tracing::error!("发送飞书文本回复失败: {error}");
        return Err(error);
    }

    for image in &reply.images {
        match upload_image(state, image).await {
            Ok(image_key) => {
                if let Err(error) = send_image_message(state, chat_id, &image_key).await {
                    tracing::warn!("发送图片消息失败 image_key={image_key}: {error}");
                }
            }
            Err(error) => {
                tracing::warn!("上传图片失败 size={}KB: {error}", image.bytes.len() / 1024)
            }
        }
    }

    Ok(())
}

// ── 消息表情回应 ──────────────────────────────────────────────

async fn add_reaction(state: &Arc<BotState>, message_id: &str, emoji: &str) -> Result<String> {
    let token = get_token(state).await?;
    let url = format!("https://open.feishu.cn/open-apis/im/v1/messages/{message_id}/reactions");

    let resp = state
        .http
        .post(&url)
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "reaction_type": { "emoji_type": emoji }
        }))
        .send()
        .await
        .map_err(|e| anyhow!("添加表情请求失败: {e}"))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("解析表情响应失败: {e}"))?;

    let code = body["code"].as_i64().unwrap_or(-1);
    if code != 0 {
        let msg = body["msg"].as_str().unwrap_or("未知错误");
        return Err(anyhow!("添加表情失败: code={code}, msg={msg}"));
    }

    // 返回 reaction_id
    let reaction_id = body["data"]["reaction_id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    Ok(reaction_id)
}

async fn remove_reaction(state: &Arc<BotState>, message_id: &str, reaction_id: &str) -> Result<()> {
    if reaction_id.is_empty() {
        return Ok(());
    }
    let token = get_token(state).await?;
    let url = format!(
        "https://open.feishu.cn/open-apis/im/v1/messages/{message_id}/reactions/{reaction_id}"
    );

    let resp = state
        .http
        .delete(&url)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| anyhow!("移除表情请求失败: {e}"))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("解析表情响应失败: {e}"))?;

    let code = body["code"].as_i64().unwrap_or(-1);
    if code != 0 {
        let msg = body["msg"].as_str().unwrap_or("未知错误");
        return Err(anyhow!("移除表情失败: code={code}, msg={msg}"));
    }

    Ok(())
}

// ── 工具函数 ──────────────────────────────────────────────────

fn truncate_str(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

/// 按 magic bytes 嗅探图片 MIME，与微信 `detect_image_mime` 对齐。
fn detect_image_mime(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        Some("image/png".to_string())
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg".to_string())
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif".to_string())
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp".to_string())
    } else {
        None
    }
}

/// MIME → 文件扩展名，用于上传图片时的文件名。
fn mime_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_is_prefixed_and_mentions_push_target_only_when_available() {
        let text = ApiMessageContent::Text {
            text: "你好".into(),
        }
        .with_source("[来源：飞书群聊]");
        assert!(matches!(
            text,
            ApiMessageContent::Text { ref text } if text == "[来源：飞书群聊]\n你好"
        ));

        assert_eq!(message_source("group", None), "[来源：飞书群聊]");
        let push = ("bot-feishu".to_string(), "target-1".to_string());
        let with_push = message_source("direct", Some(&push));
        assert!(with_push.starts_with("[来源：飞书私聊"));
        assert!(with_push.contains("bot-feishu"));
        assert!(with_push.contains("send_text_message（target_id=target-1）"));
        assert!(with_push.contains("直接输出的文本不会送达"));
        assert!(push_channel(None).is_none());
    }

    const LONG_MOCK_DELAY: Duration = Duration::from_secs(125);

    fn request_from(parsed: ParsedMessage) -> ConnectorRequest {
        ConnectorRequest {
            connector: "feishu-bot".to_string(),
            channel_id: "chat".to_string(),
            sender_id: "sender".to_string(),
            message_id: Some("message".to_string()),
            message: None,
            content: Some(parsed.content),
            media: parsed.media,
        }
    }

    #[test]
    fn content_validity_depends_on_its_resource() {
        assert!(ApiMessageContent::Text { text: "  ".into() }.is_empty());
        assert!(
            ApiMessageContent::Image {
                url: String::new(),
                caption: None,
            }
            .is_empty()
        );
        assert!(
            ApiMessageContent::File {
                url: "data:file".into(),
                name: String::new(),
            }
            .is_empty()
        );
        assert!(
            !ApiMessageContent::Image {
                url: "data:image/png;base64,AA==".into(),
                caption: None,
            }
            .is_empty()
        );
    }

    #[test]
    fn single_image_is_sent_as_structured_content() {
        let parsed = ParsedMessage {
            content: ApiMessageContent::Image {
                url: "data:image/png;base64,first".into(),
                caption: None,
            },
            media: Vec::new(),
        };
        let value = serde_json::to_value(request_from(parsed)).unwrap();

        assert_eq!(value["content"]["type"], "image");
        assert_eq!(value["content"]["url"], "data:image/png;base64,first");
        assert!(value.get("media").is_none());
    }

    #[test]
    fn image_only_post_keeps_every_image_in_order() {
        let parsed = compose_post_message(
            String::new(),
            vec!["first".into(), "second".into(), "third".into()],
        );
        let value = serde_json::to_value(request_from(parsed)).unwrap();

        assert_eq!(value["content"]["type"], "image");
        assert_eq!(value["content"]["url"], "first");
        assert_eq!(value["media"].as_array().unwrap().len(), 2);
        assert_eq!(value["media"][0]["url"], "second");
        assert_eq!(value["media"][1]["url"], "third");
    }

    #[test]
    fn text_and_images_post_keeps_text_and_every_image() {
        let parsed = compose_post_message("正文".into(), vec!["first".into(), "second".into()]);
        let value = serde_json::to_value(request_from(parsed)).unwrap();

        assert_eq!(value["content"]["type"], "text");
        assert_eq!(value["content"]["text"], "正文");
        assert_eq!(value["media"].as_array().unwrap().len(), 2);
        assert_eq!(value["media"][0]["url"], "first");
        assert_eq!(value["media"][1]["url"], "second");
    }

    #[test]
    fn unicode_log_truncation_does_not_split_characters() {
        assert_eq!(truncate_str("飞书图片消息", 4), "飞书图片...");
        assert_eq!(truncate_str("飞书", 4), "飞书");
    }

    #[test]
    fn duplicate_message_claim_is_scoped_by_channel_and_releasable() {
        let mut messages = RecentMessages::default();
        assert!(messages.claim("chat-a", "message-1"));
        assert!(!messages.claim("chat-a", "message-1"));
        assert!(messages.claim("chat-b", "message-1"));

        messages.release("chat-a", "message-1");
        assert!(messages.claim("chat-a", "message-1"));
    }

    fn bot_state_for_test() -> Arc<BotState> {
        Arc::new(BotState {
            http: reqwest::Client::new(),
            tiangong_http: reqwest::Client::new(),
            app_id: String::new(),
            app_secret: String::new(),
            access_token: RwLock::new(None),
            tiangong_url: "http://127.0.0.1:8080".to_string(),
            tiangong_token: None,
            recent_messages: Mutex::new(RecentMessages::default()),
        })
    }

    #[tokio::test]
    #[ignore = "长时回归测试会等待 125 秒"]
    async fn forward_to_tiangong_waits_beyond_120_seconds() {
        let body = serde_json::json!({
            "session_id": "mock-session",
            "connector": "feishu-bot",
            "channel_id": "mock-channel",
            "message": "mock-ok",
            "content": { "type": "text", "text": "mock-ok" },
            "attachments": []
        })
        .to_string();
        let (tiangong_url, server) =
            test_support::spawn_delayed_json_response(LONG_MOCK_DELAY, body).await;
        let state = Arc::new(BotState {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap(),
            tiangong_http: reqwest::Client::builder().build().unwrap(),
            app_id: String::new(),
            app_secret: String::new(),
            access_token: RwLock::new(None),
            tiangong_url,
            tiangong_token: None,
            recent_messages: Mutex::new(RecentMessages::default()),
        });

        let started = std::time::Instant::now();
        let reply = forward_to_tiangong(
            &state,
            "mock-channel",
            "mock-sender",
            &Some("mock-message".to_string()),
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

    fn image_content(url: &str) -> ApiMessageContent {
        ApiMessageContent::Image {
            url: url.to_string(),
            caption: None,
        }
    }

    #[tokio::test]
    async fn build_reply_collects_content_and_attachment_images() {
        let state = bot_state_for_test();
        // data URI 不触发网络请求，可安全在单测中解析
        let body = ConnectorResponse {
            session_id: "s".into(),
            connector: "feishu-bot".into(),
            channel_id: "c".into(),
            message: "回退文本".into(),
            content: ApiMessageContent::Image {
                url: "data:image/png;base64,iVBORw0KGgo=".into(),
                caption: Some("图片说明".into()),
            },
            attachments: vec![
                image_content("data:image/jpeg;base64,/9j/4AAQSkZJRg=="),
                image_content("relative/path.png"), // 非本地/可下载 → 跳过
            ],
        };

        let reply = build_reply(&state, body).await;
        // content.caption 作为文本回退
        assert_eq!(reply.text, "图片说明");
        assert_eq!(reply.images.len(), 2);
        assert_eq!(reply.images[0].mime_type, "image/png");
        assert_eq!(reply.images[1].mime_type, "image/jpeg");
    }

    #[tokio::test]
    async fn build_reply_falls_back_to_message_when_content_has_no_text() {
        let state = bot_state_for_test();
        let body = ConnectorResponse {
            session_id: "s".into(),
            connector: "feishu-bot".into(),
            channel_id: "c".into(),
            message: "天工默认消息".into(),
            content: image_content("data:image/png;base64,iVBORw0KGgo="),
            attachments: Vec::new(),
        };

        let reply = build_reply(&state, body).await;
        assert_eq!(reply.text, "天工默认消息");
        assert_eq!(reply.images.len(), 1);
    }

    #[tokio::test]
    async fn resolve_outbound_image_decodes_data_uri() {
        let state = bot_state_for_test();
        // 1x1 透明 PNG
        let url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
        let img = resolve_outbound_image(&state, url)
            .await
            .expect("应解析成功");
        assert_eq!(img.mime_type, "image/png");
        assert!(img.bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]));
    }

    #[tokio::test]
    async fn resolve_outbound_image_ignores_unsupported_url() {
        let state = bot_state_for_test();
        assert!(
            resolve_outbound_image(&state, "relative/path.png")
                .await
                .is_none()
        );
        assert!(
            resolve_outbound_image(&state, "data:image/png;base64,!!!invalid")
                .await
                .is_none()
        );
    }

    #[test]
    fn detect_image_mime_matches_common_formats() {
        assert_eq!(
            detect_image_mime(&[0x89, 0x50, 0x4e, 0x47, 0x0d]).as_deref(),
            Some("image/png")
        );
        assert_eq!(
            detect_image_mime(&[0xff, 0xd8, 0xff, 0xe0]).as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            detect_image_mime(b"RIFF\x00\x00\x00\x00WEBPVP8 ").as_deref(),
            Some("image/webp")
        );
        assert_eq!(detect_image_mime(b"GIF89a").as_deref(), Some("image/gif"));
        assert!(detect_image_mime(b"unknown").is_none());
    }

    #[test]
    fn mime_extension_maps_common_types() {
        assert_eq!(mime_extension("image/png"), "png");
        assert_eq!(mime_extension("image/jpeg"), "jpg");
        assert_eq!(mime_extension("image/gif"), "gif");
        assert_eq!(mime_extension("image/webp"), "webp");
        // 未知 MIME 回退为 png
        assert_eq!(mime_extension("application/octet-stream"), "png");
    }

    fn policy() -> ReconnectPolicy {
        ReconnectPolicy {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            stable_threshold: Duration::from_secs(60),
        }
    }

    #[test]
    fn backoff_doubles_and_caps_at_max() {
        let policy = policy();
        let mut current = policy.initial_backoff;

        // 每次短暂断开后退避翻倍：1 → 2 → 4 → 8 → 16 → 32 → 60（封顶）
        let expected = [
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
            Duration::from_secs(32),
            Duration::from_secs(60),
            Duration::from_secs(60),
        ];
        for &want in &expected[1..] {
            current = next_backoff(&policy, current, Duration::from_secs(1));
            assert_eq!(current, want);
        }
    }

    #[test]
    fn stable_connection_resets_backoff() {
        let policy = policy();
        let current = Duration::from_secs(32);
        // 持续 90s ≥ 60s 阈值 → 重置为 initial_backoff
        let next = next_backoff(&policy, current, Duration::from_secs(90));
        assert_eq!(next, policy.initial_backoff);
    }

    #[test]
    fn backoff_jitter_stays_within_quarter_and_is_deterministic() {
        let backoff = Duration::from_secs(8);
        // 同一 seed 必须得到同一抖动
        let a = backoff_with_jitter(backoff, 123);
        let b = backoff_with_jitter(backoff, 123);
        assert_eq!(a, b);

        // 抖动落在 [0, backoff/4] 区间内
        for seed in [0u128, 1, 7, 999, 1_000_000, u64::MAX as u128] {
            let v = backoff_with_jitter(backoff, seed);
            assert!(v >= backoff, "{seed} 抖动低于退避基准");
            assert!(v <= backoff + backoff / 4, "{seed} 抖动超过上限");
        }
    }

    #[test]
    fn backoff_jitter_zero_backoff_is_zero() {
        assert_eq!(backoff_with_jitter(Duration::ZERO, 5), Duration::ZERO);
    }

    #[tokio::test]
    async fn reconnect_loop_retries_until_stop_and_respects_signal() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        use tokio::sync::Notify;

        // 前 2 次失败、第 3 次起挂起（模拟连接成功后长期占用）。
        let calls = Arc::new(AtomicU32::new(0));
        let third_block = Arc::new(Notify::new());
        let calls_for_connect = calls.clone();
        let block_for_connect = third_block.clone();

        let mut connect = move || {
            let calls = calls_for_connect.clone();
            let block = block_for_connect.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    return (ConnectOutcome::Failed, Duration::from_millis(1));
                }
                // 模拟稳定连接：让 select 同时挂在 connect 与 stop 上，
                // 直到被 stop 打断。
                block.notified().await;
                (ConnectOutcome::Closed, Duration::from_secs(0))
            }
        };

        // 用 fast 策略压缩退避：initial=1ms, max=4ms。
        let fast = ReconnectPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(4),
            stable_threshold: Duration::from_secs(60),
        };

        let calls_for_stop = calls.clone();
        let stop = async move {
            // 等待第 3 次连接开始（calls==3）再完成 stop future，
            // 让重连循环在 select 处感知到停止。
            loop {
                if calls_for_stop.load(Ordering::SeqCst) >= 3 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        };
        let stop: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(stop);

        reconnect_loop(fast, &mut connect, stop).await;

        // 停止后不应再发起第 4 次连接
        let total = calls.load(Ordering::SeqCst);
        assert!(total < 4, "停止信号后不应继续重连，calls={total}");
        assert!(total >= 3, "至少应重连到第 3 次，calls={total}");
    }
}
