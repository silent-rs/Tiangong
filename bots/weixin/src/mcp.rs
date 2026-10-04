//! 微信 Bot 的 stdio MCP 回复窗口发送入口。

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

use crate::reply_quota::{MediaPlan, ReplyKind, TextPlan};
use crate::target_store::{self, AuthorizedTarget};
use crate::{BotState, RecentMessages, ilink, provision};

const MAX_TEXT_CHARS: usize = 4000;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 30 * 1024 * 1024;
const MAX_FILE_NAME_CHARS: usize = 255;
const WINDOW_REPLACED: &str =
    "推送目标不存在、已被删除，或用户已发送新消息开启新的回复窗口；请重新确认后再发送";

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
struct WeixinMcp {
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
    /// 是否为本轮最终答复。进展消息填 false：bot 会限流、合并，并始终为最终答复保留回复额度；
    /// 最终答复填 true：超长时自动拆分为多条。
    is_final: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendLocalMediaInput {
    /// `list_push_targets` 返回的目标编号。
    target_id: String,
    /// 当前工作目录或 `~/.tiangong/media` 内的本地文件路径。
    file_path: String,
    /// 本次任务稳定编号；同一目标下重复使用不会再次发送。
    idempotency_key: String,
    /// 是否属于本轮最终答复。false 时 bot 会为最终答复保留至少 1 次回复额度。
    is_final: bool,
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
    path: PathBuf,
}

#[tool_router(server_handler)]
impl WeixinMcp {
    #[tool(description = "列出用户已授权接收微信回复窗口消息的目标")]
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

    #[tool(
        description = "使用最近一条入站消息的回复上下文，向已授权微信目标发送文本。每条入站消息的回复次数有限：进展消息设 is_final=false，bot 会限流合并并保留最终答复额度；最终答复设 is_final=true，超长自动拆分"
    )]
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
        let kind = reply_kind(input.is_final);
        let now = Local::now().naive_local();
        let plan = target_store::update_quota(
            &target.target_id,
            &target.context_token,
            |policy, quota| quota.plan_text(policy, kind, text, now),
        )
        .map_err(internal_error)?
        .unwrap_or_else(|| TextPlan::Rejected(WINDOW_REPLACED.to_string()));
        let segments = match plan {
            TextPlan::Send { segments, .. } => segments,
            TextPlan::Queued { pending } => {
                return Ok(Json(finish_queued(&path, delivery, pending)));
            }
            TextPlan::Rejected(reason) => {
                return Ok(Json(finish_delivery(
                    &path,
                    delivery,
                    Err(SendFailure::Rejected(reason)),
                    "",
                )));
            }
        };

        let total = segments.len();
        let mut outcome = Ok(None);
        for (index, segment) in segments.iter().enumerate() {
            let sent = ilink::send_message(
                &self.state.http,
                &self.state.api_base_url,
                &self.state.bot_token,
                &target.to_user_id,
                &target.context_token,
                segment,
            )
            .await;
            if let Err(error) = sent {
                let failure = classify_platform_error("文本发送", error);
                outcome = Err(partial_failure(failure, index, total));
                break;
            }
        }
        let accepted = if total > 1 {
            format!("微信已受理文本消息（已拆分为 {total} 条发送）")
        } else {
            "微信已受理文本消息（受最近消息回复窗口限制）".to_string()
        };
        Ok(Json(finish_delivery(&path, delivery, outcome, &accepted)))
    }

    #[tool(
        description = "使用最近一条入站消息的回复上下文发送本地图片；文件必须位于当前工作目录或 ~/.tiangong/media，支持 PNG/JPEG/GIF/WebP，最大 10 MiB。每张图片占用 1 次回复额度，属于最终答复时设 is_final=true"
    )]
    async fn send_image_message(
        &self,
        Parameters(input): Parameters<SendLocalMediaInput>,
    ) -> Result<Json<DeliveryResult>, ErrorData> {
        self.send_media(input, true, MAX_IMAGE_BYTES, "图片").await
    }

    #[tool(
        description = "使用最近一条入站消息的回复上下文发送本地文件；文件必须位于当前工作目录或 ~/.tiangong/media，最大 30 MiB。每个文件占用 1 次回复额度，属于最终答复时设 is_final=true"
    )]
    async fn send_file_message(
        &self,
        Parameters(input): Parameters<SendLocalMediaInput>,
    ) -> Result<Json<DeliveryResult>, ErrorData> {
        self.send_media(input, false, MAX_FILE_BYTES, "文件").await
    }
}

impl WeixinMcp {
    async fn send_media(
        &self,
        input: SendLocalMediaInput,
        image_required: bool,
        max_bytes: u64,
        label: &str,
    ) -> Result<Json<DeliveryResult>, ErrorData> {
        let target_id = input.target_id.trim();
        let idempotency_key = input.idempotency_key.trim();
        let target = match resolve_target(target_id, idempotency_key)? {
            TargetResolution::Ready(target) => target,
            TargetResolution::Return(result) => return Ok(Json(result)),
        };
        let media = match read_local_media(input.file_path.trim(), max_bytes, image_required) {
            Ok(media) => media,
            Err(message) => return Ok(Json(rejected(&message))),
        };
        let (path, delivery) = match resolve_delivery(&target, idempotency_key)? {
            DeliveryResolution::Ready { path, delivery } => (path, delivery),
            DeliveryResolution::Return(result) => return Ok(Json(result)),
        };
        let kind = reply_kind(input.is_final);
        let now = Local::now().naive_local();
        let plan = target_store::update_quota(
            &target.target_id,
            &target.context_token,
            |policy, quota| quota.plan_media(policy, kind, now),
        )
        .map_err(internal_error)?
        .unwrap_or_else(|| MediaPlan::Rejected(WINDOW_REPLACED.to_string()));
        if let MediaPlan::Rejected(reason) = plan {
            return Ok(Json(finish_delivery(
                &path,
                delivery,
                Err(SendFailure::Rejected(reason)),
                "",
            )));
        }
        let outcome = ilink::send_local_file(
            &self.state.http,
            &self.state.api_base_url,
            &self.state.bot_token,
            &target.to_user_id,
            &target.context_token,
            &media.path,
            "",
        )
        .await
        .map(|()| None)
        .map_err(|error| classify_platform_error(&format!("{label}发送"), error));
        Ok(Json(finish_delivery(
            &path,
            delivery,
            outcome,
            &format!("微信已受理{label}消息（受最近消息回复窗口限制）"),
        )))
    }
}

pub async fn serve() -> Result<()> {
    let credentials = provision::load_credentials()?;
    let state = Arc::new(BotState {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("构建微信 MCP HTTP 客户端失败")?,
        tiangong_http: reqwest::Client::new(),
        bot_token: credentials.bot_token,
        api_base_url: credentials
            .base_url
            .unwrap_or_else(|| ilink::ILINK_BASE_URL.to_string()),
        tiangong_url: String::new(),
        tiangong_token: None,
        cursor: tokio::sync::RwLock::new(String::new()),
        recent_messages: tokio::sync::Mutex::new(RecentMessages::default()),
    });
    WeixinMcp { state }
        .serve(rmcp::transport::stdio())
        .await
        .context("启动微信 MCP stdio 服务失败")?
        .waiting()
        .await
        .context("微信 MCP stdio 服务异常退出")?;
    Ok(())
}

/// 本 Bot 实例注册到天工的 MCP 服务名（`bot-{实例目录名}`）。
///
/// 入站消息的来源说明与 MCP 注册共用该名称，保证模型看到的服务名与实际
/// 注册名一致；多实例时各自不同。
pub fn server_name() -> Result<String> {
    let executable = std::env::current_exe().context("获取微信 bot 路径失败")?;
    let bot_id = executable
        .parent()
        .and_then(|directory| directory.file_name())
        .and_then(|name| name.to_str())
        .context("微信 bot 实例目录名称无效")?;
    Ok(format!("bot-{bot_id}"))
}

pub fn registration_config() -> Result<RegistrationConfig> {
    let executable = std::env::current_exe().context("获取微信 bot 路径失败")?;
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
    if target.to_user_id.trim().is_empty() || target.context_token.trim().is_empty() {
        return Ok(TargetResolution::Return(rejected(
            "缺少最近微信消息的回复上下文，请先从移动端发送一条消息",
        )));
    }
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

fn reply_kind(is_final: bool) -> ReplyKind {
    if is_final {
        ReplyKind::Final
    } else {
        ReplyKind::Progress
    }
}

/// 拆分发送中途失败时，说明已送达的段数，避免 Agent 误判为全部未发送。
fn partial_failure(failure: SendFailure, sent: usize, total: usize) -> SendFailure {
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
fn finish_queued(path: &Path, mut delivery: StoredDelivery, pending: usize) -> DeliveryResult {
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

fn finish_delivery(
    path: &Path,
    mut delivery: StoredDelivery,
    outcome: std::result::Result<Option<String>, SendFailure>,
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

fn classify_platform_error(action: &str, error: anyhow::Error) -> SendFailure {
    let detail = format!("{error:#}");
    tracing::warn!("微信 MCP {action}失败: {detail}");
    if detail.contains("iLink sendmessage 失败")
        || detail.contains("iLink getuploadurl 失败")
        || detail.contains("CDN 上传被拒绝")
        || detail.contains("iLink 请求失败（HTTP 4")
    {
        SendFailure::Rejected(format!(
            "微信平台拒绝{action}，最近消息的回复上下文可能已失效"
        ))
    } else {
        SendFailure::Unknown(format!("微信{action}结果未知，未自动重试"))
    }
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
        .ok_or_else(|| "本地文件名无效".to_string())?;
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
    if image_required
        && (!supported_image_extension(&canonical) || !supported_image_content(&bytes))
    {
        return Err("图片格式不受支持，仅支持 PNG、JPEG、GIF 和 WebP".to_string());
    }
    Ok(LocalMedia { path: canonical })
}

fn supported_image_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp"
    )
}

fn supported_image_content(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47])
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"GIF8")
        || (bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP")
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
