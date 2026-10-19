//! QQ 开放平台 WebSocket 网关：接收 C2C 私聊与群 @ 消息转发到天工 server，
//! 并发送同步回复或供推送工具使用的文本/富媒体发送能力。
//!
//! QQ 网关同一机器人多会话的投递行为未核实，插件按单连接运行（文件锁持有者）。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use md5::{Digest, Md5};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::Sha1;
use tokio::sync::{Mutex, Notify};

use super::gateway::{DispatchEvent, GatewayRunner, INTENT_GROUP_AND_C2C_EVENT};
use super::target_store;
use super::token::AccessTokenCache;
use crate::platform::{InboundSource, StopSignal};
#[cfg(test)]
use crate::test_support;

const RECENT_MESSAGE_LIMIT: usize = 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MD5_10M_BYTES: usize = 10_002_432;
const OPENAPI_BASE: &str = "https://api.sgroup.qq.com";

pub(crate) struct BotState {
    pub(crate) http: Client,
    /// 天工任务允许长时间执行，不设置请求总时限。
    pub(crate) tiangong_http: Client,
    pub(crate) token: AccessTokenCache,
    /// 天工 server 地址，如 http://127.0.0.1:8080（推送实例为空）。
    pub(crate) tiangong_url: String,
    /// 天工 server 认证 token（可选）
    pub(crate) tiangong_token: Option<String>,
    pub(crate) recent_messages: Mutex<RecentMessages>,
    /// 平台数据目录（凭据、推送目标、投递记录）。
    pub(crate) data_dir: PathBuf,
}

#[derive(Default)]
pub(crate) struct RecentMessages {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
    /// 用户正文，只发图片时为空。
    message: String,
    /// 全部附件，按用户发送顺序排列。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    media: Vec<MediaAsset>,
    /// 只给模型看的来源标签。
    #[serde(skip_serializing_if = "Option::is_none")]
    instruction: Option<String>,
    /// 界面渲染声明（本插件 `im-message` 视图）。
    #[serde(skip_serializing_if = "Option::is_none")]
    render: Option<serde_json::Value>,
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

    fn local_file_url(&self) -> Option<&str> {
        match self {
            Self::Image { url, .. }
            | Self::File { url, .. }
            | Self::Audio { url, .. }
            | Self::Video { url, .. } => Some(url),
            Self::Text { .. } => None,
        }
    }
}

fn message_source(target_kind: &str, push_target: Option<&str>) -> InboundSource {
    let channel = if target_kind == "group" {
        "QQ 群聊"
    } else {
        "QQ 私聊"
    };
    InboundSource::new("qq", channel, push_target, true)
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
    text: String,
    media: Vec<MediaAsset>,
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

// ── 接收连接 ──────────────────────────────────────────────────

/// 运行 QQ 网关连接直到 `stop` 完成。
pub(crate) async fn run_connection(state: Arc<BotState>, stop: StopSignal) -> Result<()> {
    tracing::info!("QQ 机器人启动中，天工服务地址: {}", state.tiangong_url);
    let shutdown = Arc::new(Notify::new());
    let runner = GatewayRunner::new(
        state.http.clone(),
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
    let mut gateway = tokio::spawn(async move { runner.run().await });
    tokio::select! {
        _ = stop => tracing::info!("QQ 接收连接收到停止信号"),
        _ = &mut gateway => tracing::info!("QQ Gateway 主循环已结束"),
    }
    shutdown.notify_waiters();
    gateway.abort();
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
    let target_id = match target_store::upsert_discovered(
        &state.data_dir,
        "group",
        &message.group_openid,
        message_id,
    ) {
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
    let push = target_id;
    let source = message_source("group", push.as_deref());

    let _ = seq;
    tracing::info!(
        "转发 QQ 群消息 group={} sender={sender_id} text_len={} images={}",
        message.group_openid,
        parsed.text.len(),
        parsed.media.len()
    );
    let group_openid = message.group_openid.clone();
    let reply = match forward_to_tiangong(state, &channel_id, message_id, parsed, &source).await {
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
        tracing::debug!("QQ 群聊回复由 Agent 经推送工具发送，忽略同步响应");
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
    let target_id = match target_store::upsert_discovered(
        &state.data_dir,
        "direct",
        &user_openid,
        message_id,
    ) {
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

    let push = target_id;
    let source = message_source("direct", push.as_deref());
    let _ = seq;
    tracing::info!(
        "转发 QQ 私聊消息 user={} text_len={} images={}",
        user_openid,
        parsed.text.len(),
        parsed.media.len()
    );
    let forward_result = forward_to_tiangong(state, &channel_id, message_id, parsed, &source).await;
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
        tracing::debug!("QQ 私聊回复由 Agent 经推送工具发送，忽略同步响应");
        return Ok(());
    }
    send_reply(state, ReplyTarget::User(user_openid), message_id, reply).await
}

/// 把文本与图片附件解析为 `ParsedMessage`：正文原样保留（可为空），图片全部
/// 按顺序放入 `media`；文本与图片皆空时返回 `None`。
async fn parse_text_with_images(
    state: &BotState,
    text: &str,
    attachments: &[QqAttachment],
) -> Option<ParsedMessage> {
    let mut media = Vec::new();
    for attachment in attachments {
        if attachment.content_type.starts_with("image/") {
            match download_attachment(state, attachment).await {
                Ok(image) => media.push(MediaAsset::image(image.url, image.mime_type)),
                Err(error) => tracing::warn!("下载 QQ 图片失败: {error}"),
            }
        }
    }
    compose_message(text, media)
}

fn compose_message(text: &str, media: Vec<MediaAsset>) -> Option<ParsedMessage> {
    if text.trim().is_empty() && media.is_empty() {
        return None;
    }
    Some(ParsedMessage {
        text: text.to_string(),
        media,
    })
}

// ── 天工 Server 调用 ──────────────────────────────────────────

async fn forward_to_tiangong(
    state: &BotState,
    channel_id: &str,
    message_id: &str,
    parsed: ParsedMessage,
    source: &InboundSource,
) -> Result<BotReply> {
    let url = format!(
        "{}/api/v1/messages",
        state.tiangong_url.trim_end_matches('/')
    );
    let request = ConnectorRequest {
        connector: "qq-bot".to_string(),
        channel_id: channel_id.to_string(),
        message_id: Some(message_id.to_string()),
        message: parsed.text,
        media: parsed.media,
        instruction: Some(source.instruction()),
        render: Some(source.render()),
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

pub(crate) enum ReplyTarget {
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

pub(crate) async fn send_text(
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
pub(crate) async fn send_local_file(
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
    fn source_mentions_push_target_only_when_available() {
        assert_eq!(
            message_source("group", None).instruction(),
            "[来源：QQ 群聊]"
        );
        let with_push = message_source("direct", Some("target-1"));
        assert_eq!(
            with_push.instruction(),
            "[来源：QQ 私聊，经 Bot 推送工具回复 target_id=target-1，回复次数有限]"
        );
        assert_eq!(with_push.render()["data"]["platform"], "qq");
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
            data_dir: std::env::temp_dir(),
        };

        let started = std::time::Instant::now();
        let reply = forward_to_tiangong(
            &state,
            "mock-channel",
            "mock-message",
            ParsedMessage {
                text: "mock-request".to_string(),
                media: Vec::new(),
            },
            &message_source("direct", None),
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
            message_id: Some("msg-1".to_string()),
            message: "你好".into(),
            media: vec![MediaAsset::image(
                "data:image/png;base64,AA==".into(),
                "image/png".into(),
            )],
            instruction: Some(message_source("group", Some("t1")).instruction()),
            render: Some(message_source("group", Some("t1")).render()),
        };
        let value = serde_json::to_value(&request).unwrap();
        assert!(value.get("content").is_none());
        assert_eq!(value["message"], "你好", "正文不再拼接来源标签");
        assert!(
            value["instruction"]
                .as_str()
                .unwrap()
                .contains("target_id=t1")
        );
        assert_eq!(value["render"]["view"], "im-message");
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
        assert_eq!(parsed.text, "你好");
        assert!(parsed.media.is_empty());
    }

    fn png(url: &str) -> MediaAsset {
        MediaAsset::image(url.into(), "image/png".into())
    }

    fn request_value(parsed: ParsedMessage) -> serde_json::Value {
        serde_json::to_value(ConnectorRequest {
            connector: "qq-bot".to_string(),
            channel_id: "c2c:user".to_string(),
            message_id: Some("msg".to_string()),
            message: parsed.text,
            media: parsed.media,
            instruction: None,
            render: None,
        })
        .unwrap()
    }

    #[test]
    fn image_only_message_sends_every_image_as_media() {
        let parsed = compose_message("", vec![png("first"), png("second")]).unwrap();
        let value = request_value(parsed);
        assert!(value.get("content").is_none());
        assert_eq!(value["message"], "");
        assert_eq!(value["media"].as_array().unwrap().len(), 2);
        assert_eq!(value["media"][0]["url"], "first");
        assert_eq!(value["media"][1]["url"], "second");
    }

    #[test]
    fn text_with_images_keeps_text_and_every_image() {
        let parsed = compose_message("看图", vec![png("first"), png("second")]).unwrap();
        let value = request_value(parsed);
        assert!(value.get("content").is_none());
        assert_eq!(value["message"], "看图");
        assert_eq!(value["media"][0]["url"], "first");
        assert_eq!(value["media"][1]["url"], "second");
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
            data_dir: std::env::temp_dir(),
        }
    }
}
