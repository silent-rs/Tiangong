//! 飞书 Bot 的 stdio MCP 主动推送入口。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Local;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{ErrorData, ServiceExt, tool, tool_router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::target_store::{self, AuthorizedTarget};
use crate::{
    BotState, RecentMessages, detect_image_mime, get_token, post_message, provision, refresh_token,
};

const MAX_TEXT_CHARS: usize = 4000;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 30 * 1024 * 1024;
const MAX_FILE_NAME_CHARS: usize = 255;

#[derive(Serialize)]
pub struct RegistrationConfig {
    schema_version: u32,
    name: String,
    transport: String,
    command: String,
    args: Vec<String>,
    enabled: bool,
    tags: Vec<String>,
}

#[derive(Clone)]
struct FeishuMcp {
    state: Arc<BotState>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct PushTargetListOutput {
    targets: Vec<PushTargetOutput>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct PushTargetOutput {
    target_id: String,
    label: String,
    kind: String,
    availability: String,
    last_seen_at: String,
    limitation: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendTextInput {
    /// `list_push_targets` 返回的目标编号。
    target_id: String,
    /// 要发送的文本内容。
    text: String,
    /// 本次任务稳定编号；同一目标下重复使用不会再次发送。
    idempotency_key: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendLocalMediaInput {
    /// `list_push_targets` 返回的目标编号。
    target_id: String,
    /// 当前工作目录或 `~/.tiangong/media` 内的本地文件路径。
    file_path: String,
    /// 本次任务稳定编号；同一目标下重复使用不会再次发送。
    idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct DeliveryResult {
    delivery_id: String,
    status: String,
    platform_message_id: Option<String>,
    sent_at: String,
    retryable: bool,
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredDelivery {
    target_id: String,
    result: DeliveryResult,
}

enum DeliveryClaim {
    Existing(DeliveryResult),
    New {
        path: PathBuf,
        delivery: StoredDelivery,
    },
}

enum SendFailure {
    Rejected(String),
    Unknown(String),
}

enum TargetResolution {
    Ready(AuthorizedTarget),
    Return(DeliveryResult),
}

enum DeliveryResolution {
    Ready {
        path: PathBuf,
        delivery: StoredDelivery,
    },
    Return(DeliveryResult),
}

struct LocalMedia {
    bytes: Vec<u8>,
    file_name: String,
    mime_type: String,
}

#[derive(Clone, Copy)]
enum UploadKind {
    Image,
    File { file_type: &'static str },
}

enum UploadAttemptError {
    TokenExpired,
    Failure(SendFailure),
}

#[tool_router(server_handler)]
impl FeishuMcp {
    #[tool(description = "列出用户已授权接收飞书主动推送的目标")]
    async fn list_push_targets(&self) -> Result<Json<PushTargetListOutput>, ErrorData> {
        let targets = target_store::list_enabled_views()
            .map_err(internal_error)?
            .into_iter()
            .map(|target| PushTargetOutput {
                target_id: target.target_id,
                label: target.label,
                kind: target.kind,
                availability: target.availability,
                last_seen_at: target.last_seen_at,
                limitation: target.limitation,
            })
            .collect();
        Ok(Json(PushTargetListOutput { targets }))
    }

    #[tool(description = "向一个已授权的飞书目标主动发送文本消息")]
    async fn send_text_message(
        &self,
        Parameters(input): Parameters<SendTextInput>,
    ) -> Result<Json<DeliveryResult>, ErrorData> {
        let target_id = input.target_id.trim();
        let idempotency_key = input.idempotency_key.trim();
        let text = input.text.trim();
        if text.is_empty() || text.chars().count() > MAX_TEXT_CHARS {
            return Ok(Json(rejected("消息不能为空且不能超过 4000 个字符")));
        }

        let target = match resolve_target(target_id, idempotency_key)? {
            TargetResolution::Ready(target) => target,
            TargetResolution::Return(result) => return Ok(Json(result)),
        };
        let (path, delivery) = match resolve_delivery(&target, idempotency_key)? {
            DeliveryResolution::Ready { path, delivery } => (path, delivery),
            DeliveryResolution::Return(result) => return Ok(Json(result)),
        };
        let outcome = send_platform_text(&self.state, &target, text).await;
        Ok(Json(finish_delivery(
            &path,
            delivery,
            outcome,
            "飞书已受理文本消息",
        )))
    }

    #[tool(
        description = "向一个已授权的飞书目标主动发送本地图片；文件必须位于当前工作目录或 ~/.tiangong/media，支持 PNG/JPEG/GIF/WebP，最大 10 MiB"
    )]
    async fn send_image_message(
        &self,
        Parameters(input): Parameters<SendLocalMediaInput>,
    ) -> Result<Json<DeliveryResult>, ErrorData> {
        let target_id = input.target_id.trim();
        let idempotency_key = input.idempotency_key.trim();
        let target = match resolve_target(target_id, idempotency_key)? {
            TargetResolution::Ready(target) => target,
            TargetResolution::Return(result) => return Ok(Json(result)),
        };
        let media = match read_local_media(input.file_path.trim(), MAX_IMAGE_BYTES, true) {
            Ok(media) => media,
            Err(message) => return Ok(Json(rejected(&message))),
        };
        let (path, delivery) = match resolve_delivery(&target, idempotency_key)? {
            DeliveryResolution::Ready { path, delivery } => (path, delivery),
            DeliveryResolution::Return(result) => return Ok(Json(result)),
        };
        let outcome = match upload_platform_media(&self.state, &media, UploadKind::Image).await {
            Ok(image_key) => {
                send_platform_media(&self.state, &target, "image", "image_key", &image_key).await
            }
            Err(error) => Err(error),
        };
        Ok(Json(finish_delivery(
            &path,
            delivery,
            outcome,
            "飞书已受理图片消息",
        )))
    }

    #[tool(
        description = "向一个已授权的飞书目标主动发送本地文件；文件必须位于当前工作目录或 ~/.tiangong/media，最大 30 MiB"
    )]
    async fn send_file_message(
        &self,
        Parameters(input): Parameters<SendLocalMediaInput>,
    ) -> Result<Json<DeliveryResult>, ErrorData> {
        let target_id = input.target_id.trim();
        let idempotency_key = input.idempotency_key.trim();
        let target = match resolve_target(target_id, idempotency_key)? {
            TargetResolution::Ready(target) => target,
            TargetResolution::Return(result) => return Ok(Json(result)),
        };
        let media = match read_local_media(input.file_path.trim(), MAX_FILE_BYTES, false) {
            Ok(media) => media,
            Err(message) => return Ok(Json(rejected(&message))),
        };
        let (path, delivery) = match resolve_delivery(&target, idempotency_key)? {
            DeliveryResolution::Ready { path, delivery } => (path, delivery),
            DeliveryResolution::Return(result) => return Ok(Json(result)),
        };
        let kind = UploadKind::File {
            file_type: feishu_file_type(&media.file_name),
        };
        let outcome = match upload_platform_media(&self.state, &media, kind).await {
            Ok(file_key) => {
                send_platform_media(&self.state, &target, "file", "file_key", &file_key).await
            }
            Err(error) => Err(error),
        };
        Ok(Json(finish_delivery(
            &path,
            delivery,
            outcome,
            "飞书已受理文件消息",
        )))
    }
}

pub async fn serve() -> Result<()> {
    let credentials = provision::load_credentials()?;
    let state = Arc::new(BotState {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?,
        tiangong_http: reqwest::Client::new(),
        app_id: credentials.app_id,
        app_secret: credentials.app_secret,
        access_token: tokio::sync::RwLock::new(None),
        tiangong_url: String::new(),
        tiangong_token: None,
        recent_messages: tokio::sync::Mutex::new(RecentMessages::default()),
    });
    FeishuMcp { state }
        .serve(rmcp::transport::stdio())
        .await
        .context("启动飞书 MCP stdio 服务失败")?
        .waiting()
        .await
        .context("飞书 MCP stdio 服务异常退出")?;
    Ok(())
}

/// 本 Bot 实例注册到天工的 MCP 服务名（`bot-{实例目录名}`）。
///
/// 入站消息的来源说明与 MCP 注册共用该名称，保证模型看到的服务名与实际
/// 注册名一致；多实例时各自不同。
pub fn server_name() -> Result<String> {
    let executable = std::env::current_exe().context("获取飞书 bot 路径失败")?;
    let bot_id = executable
        .parent()
        .and_then(|directory| directory.file_name())
        .and_then(|name| name.to_str())
        .context("飞书 bot 实例目录名称无效")?;
    Ok(format!("bot-{bot_id}"))
}

pub fn registration_config() -> Result<RegistrationConfig> {
    let executable = std::env::current_exe().context("获取飞书 bot 路径失败")?;
    Ok(RegistrationConfig {
        schema_version: 1,
        name: server_name()?,
        transport: "stdio".to_string(),
        command: executable.to_string_lossy().to_string(),
        args: vec!["--mcp".to_string()],
        enabled: true,
        tags: vec!["bot-outbound".to_string()],
    })
}

fn resolve_target(target_id: &str, idempotency_key: &str) -> Result<TargetResolution, ErrorData> {
    if target_id.is_empty() {
        return Ok(TargetResolution::Return(rejected("推送目标 ID 不能为空")));
    }
    if idempotency_key.is_empty() || idempotency_key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
        return Ok(TargetResolution::Return(rejected(
            "幂等键不能为空且不能超过 256 字节",
        )));
    }
    let Some(target) = target_store::find_authorized(target_id).map_err(internal_error)? else {
        return Ok(TargetResolution::Return(rejected(
            "推送目标不存在或尚未授权",
        )));
    };
    Ok(TargetResolution::Ready(target))
}

fn resolve_delivery(
    target: &AuthorizedTarget,
    idempotency_key: &str,
) -> Result<DeliveryResolution, ErrorData> {
    match claim_delivery(&target.target_id, idempotency_key).map_err(internal_error)? {
        DeliveryClaim::Existing(result) => Ok(DeliveryResolution::Return(result)),
        DeliveryClaim::New { path, delivery } => Ok(DeliveryResolution::Ready { path, delivery }),
    }
}

fn finish_delivery(
    path: &Path,
    mut delivery: StoredDelivery,
    outcome: Result<Option<String>, SendFailure>,
    accepted_message: &str,
) -> DeliveryResult {
    match outcome {
        Ok(platform_message_id) => {
            delivery.result.status = "accepted".to_string();
            delivery.result.platform_message_id = platform_message_id;
            delivery.result.sent_at = now_string();
            delivery.result.message = accepted_message.to_string();
        }
        Err(SendFailure::Rejected(message)) => {
            delivery.result.status = "rejected".to_string();
            delivery.result.sent_at = now_string();
            delivery.result.message = message;
        }
        Err(SendFailure::Unknown(message)) => {
            delivery.result.status = "unknown".to_string();
            delivery.result.sent_at = now_string();
            delivery.result.message = message;
        }
    }

    if let Err(error) = replace_delivery(path, &delivery) {
        delivery.result.status = "unknown".to_string();
        delivery.result.retryable = false;
        delivery.result.message = format!("投递结果可能已产生，但保存状态失败：{error}");
    }
    delivery.result
}

fn read_local_media(
    file_path: &str,
    max_bytes: u64,
    image_required: bool,
) -> std::result::Result<LocalMedia, String> {
    if file_path.is_empty() {
        return Err("本地文件路径不能为空".to_string());
    }
    let workspace = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|_| "无法确定当前 MCP 工作目录".to_string())?;
    let candidate = PathBuf::from(file_path);
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        workspace.join(candidate)
    };
    let canonical =
        std::fs::canonicalize(&candidate).map_err(|_| "本地文件不存在或无法访问".to_string())?;
    if !path_is_allowed(&canonical, &workspace) {
        return Err("本地文件必须位于当前工作目录或 ~/.tiangong/media".to_string());
    }
    let metadata = std::fs::metadata(&canonical).map_err(|_| "无法读取本地文件信息".to_string())?;
    if !metadata.is_file() {
        return Err("本地路径必须指向普通文件".to_string());
    }
    if metadata.len() == 0 {
        return Err("不能发送空文件".to_string());
    }
    if metadata.len() > max_bytes {
        return Err(format!("本地文件超过 {} MiB 限制", max_bytes / 1024 / 1024));
    }
    let file_name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "本地文件名无效".to_string())?
        .to_string();
    if file_name.chars().count() > MAX_FILE_NAME_CHARS {
        return Err("本地文件名不能超过 255 个字符".to_string());
    }
    let bytes = std::fs::read(&canonical).map_err(|_| "读取本地文件失败".to_string())?;
    if bytes.is_empty() {
        return Err("不能发送空文件".to_string());
    }
    if bytes.len() as u64 > max_bytes {
        return Err(format!("本地文件超过 {} MiB 限制", max_bytes / 1024 / 1024));
    }
    let mime_type = if image_required {
        detect_image_mime(&bytes)
            .ok_or_else(|| "图片格式不受支持，仅支持 PNG、JPEG、GIF 和 WebP".to_string())?
    } else {
        "application/octet-stream".to_string()
    };
    Ok(LocalMedia {
        bytes,
        file_name,
        mime_type,
    })
}

fn path_is_allowed(path: &Path, workspace: &Path) -> bool {
    if path.starts_with(workspace) {
        return true;
    }
    home_directory()
        .map(|home| home.join(".tiangong").join("media"))
        .and_then(|media| std::fs::canonicalize(media).ok())
        .is_some_and(|media| path.starts_with(media))
}

fn home_directory() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(profile));
    }
    let mut drive = std::env::var_os("HOMEDRIVE").filter(|value| !value.is_empty())?;
    let path = std::env::var_os("HOMEPATH").filter(|value| !value.is_empty())?;
    drive.push(path);
    Some(PathBuf::from(drive))
}

fn feishu_file_type(file_name: &str) -> &'static str {
    let extension = Path::new(file_name)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("opus") => "opus",
        Some("mp4") => "mp4",
        Some("pdf") => "pdf",
        Some("doc" | "docx") => "doc",
        Some("xls" | "xlsx") => "xls",
        Some("ppt" | "pptx") => "ppt",
        _ => "stream",
    }
}

async fn upload_platform_media(
    state: &Arc<BotState>,
    media: &LocalMedia,
    kind: UploadKind,
) -> std::result::Result<String, SendFailure> {
    let token = get_token(state)
        .await
        .map_err(|error| SendFailure::Unknown(format!("获取飞书令牌失败：{error}")))?;
    match upload_platform_media_request(state, &token, media, kind).await {
        Ok(key) => Ok(key),
        Err(UploadAttemptError::Failure(error)) => Err(error),
        Err(UploadAttemptError::TokenExpired) => {
            let token = refresh_token(state)
                .await
                .map_err(|error| SendFailure::Unknown(format!("刷新飞书令牌失败：{error}")))?;
            match upload_platform_media_request(state, &token, media, kind).await {
                Ok(key) => Ok(key),
                Err(UploadAttemptError::Failure(error)) => Err(error),
                Err(UploadAttemptError::TokenExpired) => Err(SendFailure::Rejected(
                    "飞书身份令牌刷新后仍然无效".to_string(),
                )),
            }
        }
    }
}

async fn upload_platform_media_request(
    state: &Arc<BotState>,
    token: &str,
    media: &LocalMedia,
    kind: UploadKind,
) -> std::result::Result<String, UploadAttemptError> {
    let label = match kind {
        UploadKind::Image => "图片",
        UploadKind::File { .. } => "文件",
    };
    let part = reqwest::multipart::Part::bytes(media.bytes.clone())
        .file_name(media.file_name.clone())
        .mime_str(&media.mime_type)
        .map_err(|error| {
            UploadAttemptError::Failure(SendFailure::Rejected(format!(
                "构造飞书{label}上传请求失败：{error}"
            )))
        })?;
    let (url, form, key_field) = match kind {
        UploadKind::Image => (
            "https://open.feishu.cn/open-apis/im/v1/images",
            reqwest::multipart::Form::new()
                .text("image_type", "message")
                .part("image", part),
            "image_key",
        ),
        UploadKind::File { file_type } => (
            "https://open.feishu.cn/open-apis/im/v1/files",
            reqwest::multipart::Form::new()
                .text("file_type", file_type)
                .text("file_name", media.file_name.clone())
                .part("file", part),
            "file_key",
        ),
    };
    let response = state
        .http
        .post(url)
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .map_err(|error| {
            UploadAttemptError::Failure(SendFailure::Unknown(format!(
                "飞书{label}上传结果未知：{error}"
            )))
        })?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.map_err(|error| {
        let failure = if status.is_client_error() {
            SendFailure::Rejected(format!("飞书拒绝{label}上传：HTTP {status}"))
        } else {
            SendFailure::Unknown(format!("解析飞书{label}上传响应失败：{error}"))
        };
        UploadAttemptError::Failure(failure)
    })?;
    let code = body["code"].as_i64().unwrap_or(-1);
    if code == 99991663 || code == 99991668 {
        return Err(UploadAttemptError::TokenExpired);
    }
    if code != 0 {
        let message = body["msg"].as_str().unwrap_or("未知错误");
        return Err(UploadAttemptError::Failure(SendFailure::Rejected(format!(
            "飞书拒绝{label}上传：code={code}, message={message}"
        ))));
    }
    body["data"][key_field]
        .as_str()
        .map(ToString::to_string)
        .ok_or_else(|| {
            UploadAttemptError::Failure(SendFailure::Unknown(format!(
                "飞书{label}上传成功但响应缺少结果标识"
            )))
        })
}

async fn send_platform_text(
    state: &Arc<BotState>,
    target: &AuthorizedTarget,
    text: &str,
) -> Result<Option<String>, SendFailure> {
    let card_content = serde_json::json!({
        "config": { "wide_screen_mode": true },
        "elements": [{ "tag": "markdown", "content": text }]
    })
    .to_string();
    let payload = serde_json::json!({
        "receive_id": target.chat_id,
        "msg_type": "interactive",
        "content": card_content,
    });
    send_platform_payload(state, payload).await
}

async fn send_platform_media(
    state: &Arc<BotState>,
    target: &AuthorizedTarget,
    message_type: &str,
    key_name: &str,
    platform_key: &str,
) -> Result<Option<String>, SendFailure> {
    let mut content = serde_json::Map::new();
    content.insert(
        key_name.to_string(),
        serde_json::Value::String(platform_key.to_string()),
    );
    let payload = serde_json::json!({
        "receive_id": target.chat_id,
        "msg_type": message_type,
        "content": serde_json::Value::Object(content).to_string(),
    });
    send_platform_payload(state, payload).await
}

async fn send_platform_payload(
    state: &Arc<BotState>,
    payload: serde_json::Value,
) -> Result<Option<String>, SendFailure> {
    let body = post_message(state, payload)
        .await
        .map_err(|error| SendFailure::Unknown(format!("飞书请求结果未知：{error}")))?;
    let code = body["code"].as_i64().unwrap_or(-1);
    if code != 0 {
        let message = body["msg"].as_str().unwrap_or("未知错误");
        return Err(SendFailure::Rejected(format!(
            "飞书拒绝消息：code={code}, message={message}"
        )));
    }
    let message_id = body["data"]["message_id"]
        .as_str()
        .or_else(|| body["data"]["message"]["message_id"].as_str())
        .map(ToString::to_string);
    Ok(message_id)
}

fn rejected(message: &str) -> DeliveryResult {
    DeliveryResult {
        delivery_id: scru128::new().to_string(),
        status: "rejected".to_string(),
        platform_message_id: None,
        sent_at: now_string(),
        retryable: false,
        message: message.to_string(),
    }
}

fn claim_delivery(target_id: &str, idempotency_key: &str) -> Result<DeliveryClaim> {
    let path = delivery_path(target_id, idempotency_key)?;
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

fn delivery_path(target_id: &str, idempotency_key: &str) -> Result<PathBuf> {
    let directory = target_store::runtime_directory()?.join("deliveries");
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
    Ok(directory.join(format!("{:x}.json", hasher.finalize())))
}

fn internal_error(error: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}

fn now_string() -> String {
    Local::now().naive_local().to_string()
}
