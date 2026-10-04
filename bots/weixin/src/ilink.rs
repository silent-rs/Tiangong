//! 腾讯微信 iLink Bot HTTP API 客户端。

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use rand::RngCore;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const MEDIA_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
const LONG_POLL_TIMEOUT: Duration = Duration::from_secs(40);
const QR_LONG_POLL_TIMEOUT: Duration = Duration::from_secs(35);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_MAX_RETRIES: usize = 3;
pub const DEFAULT_QR_POLL_INTERVAL: u64 = 1;
pub const DEFAULT_QR_EXPIRES_IN: i64 = 300;
const BOT_TYPE: u32 = 3;

// 与腾讯公开的 openclaw-weixin v2.4.6 协议标识保持一致。
const ILINK_APP_ID: &str = "bot";
const ILINK_APP_CLIENT_VERSION: &str = "132102";
const CHANNEL_VERSION: &str = "2.4.6";
const BOT_AGENT: &str = "Tiangong/0.3.0";

fn build_common_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("ilink-app-id", HeaderValue::from_static(ILINK_APP_ID));
    headers.insert(
        "ilink-app-clientversion",
        HeaderValue::from_static(ILINK_APP_CLIENT_VERSION),
    );
    headers
}

fn build_headers(bot_token: Option<&str>) -> Result<HeaderMap> {
    let mut headers = build_common_headers();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "authorizationtype",
        HeaderValue::from_static("ilink_bot_token"),
    );

    let random_uin: u32 = rand::random();
    let uin = base64::engine::general_purpose::STANDARD.encode(random_uin.to_string());
    headers.insert(
        "x-wechat-uin",
        HeaderValue::from_str(&uin).context("构造 X-WECHAT-UIN 失败")?,
    );

    if let Some(token) = bot_token.map(str::trim).filter(|token| !token.is_empty()) {
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).context("构造 Authorization 失败")?,
        );
    }

    Ok(headers)
}

#[derive(Debug, Serialize)]
struct BaseInfo {
    channel_version: &'static str,
    bot_agent: &'static str,
}

fn base_info() -> BaseInfo {
    BaseInfo {
        channel_version: CHANNEL_VERSION,
        bot_agent: BOT_AGENT,
    }
}

pub fn normalize_base_url(value: &str) -> Result<String> {
    let value = value.trim().trim_end_matches('/');
    let url = Url::parse(value).context("解析 iLink API 地址失败")?;
    if url.scheme() != "https" || url.host_str().is_none() {
        bail!("iLink API 地址必须是有效的 HTTPS 地址");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("iLink API 地址不能包含认证信息、查询参数或片段");
    }
    Ok(value.to_string())
}

pub fn redirect_base_url(redirect_host: &str) -> Result<String> {
    let redirect_host = redirect_host.trim();
    if redirect_host.is_empty() {
        bail!("iLink 扫码重定向缺少目标地址");
    }
    let value = if redirect_host.starts_with("https://") {
        redirect_host.to_string()
    } else {
        format!("https://{redirect_host}")
    };
    normalize_base_url(&value)
}

fn api_url(base_url: &str, path: &str) -> Result<Url> {
    let base = normalize_base_url(base_url)?;
    Url::parse(&format!("{base}/"))
        .context("解析 iLink API 地址失败")?
        .join(path.trim_start_matches('/'))
        .context("拼接 iLink API 地址失败")
}

// -- 扫码登录 --------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct QrCodeResponse {
    #[serde(default)]
    pub qrcode: String,
    #[serde(default)]
    pub qrcode_img_content: Option<String>,
    #[serde(default)]
    pub qrcode_url: Option<String>,
}

impl QrCodeResponse {
    pub fn render_content(&self) -> String {
        if let Some(content) = self
            .qrcode_img_content
            .as_deref()
            .map(str::trim)
            .filter(|content| !content.is_empty())
        {
            return content.to_string();
        }
        if let Some(url) = self
            .qrcode_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            return url.to_string();
        }
        self.qrcode.clone()
    }
}

#[derive(Debug, Deserialize)]
pub struct QrStatusResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub baseurl: Option<String>,
    #[serde(default)]
    pub redirect_host: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrStatus {
    Waiting,
    Scanned,
    Confirmed,
    Expired,
    Canceled,
    Redirect,
    NeedVerifyCode,
    VerifyCodeBlocked,
    AlreadyBound,
    Unknown,
}

impl QrStatusResponse {
    pub fn status(&self) -> QrStatus {
        match self.status.to_ascii_lowercase().as_str() {
            "wait" | "waiting" | "pending" | "" => QrStatus::Waiting,
            "scaned" | "scanned" | "scanned_confirm" => QrStatus::Scanned,
            "confirmed" | "success" | "ok" => QrStatus::Confirmed,
            "expired" | "timeout" => QrStatus::Expired,
            "canceled" | "cancelled" | "denied" => QrStatus::Canceled,
            "scaned_but_redirect" => QrStatus::Redirect,
            "need_verifycode" => QrStatus::NeedVerifyCode,
            "verify_code_blocked" => QrStatus::VerifyCodeBlocked,
            "binded_redirect" => QrStatus::AlreadyBound,
            _ => QrStatus::Unknown,
        }
    }
}

#[derive(Serialize)]
struct GetQrCodeRequest {
    local_token_list: Vec<String>,
}

pub async fn get_bot_qrcode(client: &Client, local_tokens: &[String]) -> Result<QrCodeResponse> {
    let mut url = api_url(ILINK_BASE_URL, "ilink/bot/get_bot_qrcode")?;
    url.query_pairs_mut()
        .append_pair("bot_type", &BOT_TYPE.to_string());
    let local_token_list = local_tokens
        .iter()
        .map(|token| token.trim())
        .filter(|token| !token.is_empty())
        .take(10)
        .map(str::to_string)
        .collect();

    let response = client
        .post(url)
        .headers(build_headers(None)?)
        .json(&GetQrCodeRequest { local_token_list })
        .timeout(QR_LONG_POLL_TIMEOUT)
        .send()
        .await
        .context("请求 iLink 二维码失败")?;
    let body = parse_body(response).await?;
    check_ret(&body, "get_bot_qrcode")?;
    serde_json::from_value(body.clone())
        .with_context(|| format!("解析 iLink 二维码响应失败: {}", truncate_json(&body)))
}

/// 返回 `None` 表示一次正常的客户端长轮询超时，调用方应继续等待。
pub async fn get_qrcode_status(
    client: &Client,
    base_url: &str,
    qrcode: &str,
) -> Result<Option<QrStatusResponse>> {
    let mut url = api_url(base_url, "ilink/bot/get_qrcode_status")?;
    url.query_pairs_mut().append_pair("qrcode", qrcode);

    let response = match client
        .get(url)
        .headers(build_common_headers())
        .timeout(QR_LONG_POLL_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) if error.is_timeout() => return Ok(None),
        Err(error) => return Err(error).context("请求 iLink 扫码状态失败"),
    };
    let body = parse_body(response).await?;
    check_ret(&body, "get_qrcode_status")?;
    serde_json::from_value(body.clone())
        .map(Some)
        .with_context(|| format!("解析 iLink 扫码状态响应失败: {}", truncate_json(&body)))
}

// -- 消息协议 --------------------------------------------------

#[derive(Debug, Serialize)]
struct GetUpdatesRequest {
    get_updates_buf: String,
    base_info: BaseInfo,
}

#[derive(Debug, Deserialize)]
pub struct UpdatesResponse {
    #[serde(default)]
    pub get_updates_buf: Option<String>,
    #[serde(default)]
    pub longpolling_timeout_ms: Option<u64>,
    #[serde(default)]
    pub msgs: Vec<WeixinMessage>,
}

#[derive(Debug, Deserialize)]
pub struct WeixinMessage {
    #[serde(default)]
    pub seq: Option<u64>,
    #[serde(default)]
    pub message_id: Option<u64>,
    #[serde(default)]
    pub from_user_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub to_user_id: String,
    #[serde(default)]
    pub message_type: i64,
    #[serde(default)]
    pub context_token: String,
    #[serde(default)]
    pub item_list: Vec<MessageItem>,
    #[serde(default)]
    pub group_id: String,
}

#[derive(Debug, Deserialize)]
pub struct MessageItem {
    #[serde(default, rename = "type")]
    pub kind: i64,
    #[serde(default)]
    pub msg_id: Option<String>,
    #[serde(default)]
    pub text_item: Option<TextItem>,
    #[serde(default)]
    pub image_item: Option<ImageItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextItem {
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct CdnMedia {
    #[serde(default)]
    pub encrypt_query_param: Option<String>,
    #[serde(default)]
    pub aes_key: Option<String>,
    #[serde(default)]
    pub full_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ImageItem {
    #[serde(default)]
    pub media: Option<CdnMedia>,
    #[serde(default)]
    pub aeskey: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

impl WeixinMessage {
    pub fn is_from_user(&self) -> bool {
        self.message_type == 1
    }

    pub fn text_content(&self) -> String {
        self.item_list
            .iter()
            .filter(|item| item.kind == 1)
            .filter_map(|item| item.text_item.as_ref())
            .map(|item| item.text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn images(&self) -> impl Iterator<Item = &ImageItem> {
        self.item_list
            .iter()
            .filter(|item| item.kind == 2)
            .filter_map(|item| item.image_item.as_ref())
    }

    pub fn stable_message_id(&self) -> Option<String> {
        self.message_id
            .map(|id| id.to_string())
            .or_else(|| {
                self.item_list
                    .iter()
                    .find_map(|item| item.msg_id.as_deref())
                    .map(str::to_string)
            })
            .or_else(|| self.seq.map(|seq| format!("seq-{seq}")))
    }
}

#[derive(Debug, Serialize)]
struct SendMessageRequest {
    msg: OutboundMessage,
    base_info: BaseInfo,
}

#[derive(Debug, Serialize)]
struct OutboundMessage {
    from_user_id: String,
    to_user_id: String,
    client_id: String,
    message_type: i64,
    message_state: i64,
    context_token: String,
    item_list: Vec<OutboundItem>,
}

#[derive(Debug, Serialize)]
struct OutboundItem {
    #[serde(rename = "type")]
    kind: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    text_item: Option<TextItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_item: Option<OutboundImageItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_item: Option<OutboundFileItem>,
}

#[derive(Debug, Serialize)]
struct OutboundCdnMedia {
    encrypt_query_param: String,
    aes_key: String,
    encrypt_type: i64,
}

#[derive(Debug, Serialize)]
struct OutboundImageItem {
    media: OutboundCdnMedia,
    mid_size: usize,
}

#[derive(Debug, Serialize)]
struct OutboundFileItem {
    media: OutboundCdnMedia,
    file_name: String,
    len: String,
}

#[derive(Debug, Serialize)]
struct GetUploadUrlRequest {
    filekey: String,
    media_type: i64,
    to_user_id: String,
    rawsize: usize,
    rawfilemd5: String,
    filesize: usize,
    no_need_thumb: bool,
    aeskey: String,
    base_info: BaseInfo,
}

#[derive(Debug, Deserialize)]
struct GetUploadUrlResponse {
    #[serde(default)]
    upload_param: Option<String>,
    #[serde(default)]
    upload_full_url: Option<String>,
}

struct UploadedFile {
    download_param: String,
    aes_key_hex: String,
    plaintext_size: usize,
    ciphertext_size: usize,
}

pub async fn send_message(
    client: &Client,
    base_url: &str,
    bot_token: &str,
    to_user_id: &str,
    context_token: &str,
    text: &str,
) -> Result<()> {
    send_outbound_item(
        client,
        base_url,
        bot_token,
        to_user_id,
        context_token,
        OutboundItem {
            kind: 1,
            text_item: Some(TextItem {
                text: text.to_string(),
            }),
            image_item: None,
            file_item: None,
        },
    )
    .await
}

/// 上传本地文件并发送到微信。图片使用图片消息，其余类型使用文件附件消息。
pub async fn send_local_file(
    client: &Client,
    base_url: &str,
    bot_token: &str,
    to_user_id: &str,
    context_token: &str,
    path: &Path,
    caption: &str,
) -> Result<()> {
    let is_image = is_image_path(path);
    let media_type = if is_image { 1 } else { 3 };
    let uploaded =
        upload_local_file(client, base_url, bot_token, to_user_id, path, media_type).await?;

    if !caption.trim().is_empty() {
        send_message(
            client,
            base_url,
            bot_token,
            to_user_id,
            context_token,
            caption,
        )
        .await?;
    }

    let media = OutboundCdnMedia {
        encrypt_query_param: uploaded.download_param,
        // 腾讯公开实现发送十六进制密钥文本的 base64，入站解密同时兼容该格式。
        aes_key: base64::engine::general_purpose::STANDARD.encode(uploaded.aes_key_hex.as_bytes()),
        encrypt_type: 1,
    };
    let item = if is_image {
        OutboundItem {
            kind: 2,
            text_item: None,
            image_item: Some(OutboundImageItem {
                media,
                mid_size: uploaded.ciphertext_size,
            }),
            file_item: None,
        }
    } else {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        OutboundItem {
            kind: 4,
            text_item: None,
            image_item: None,
            file_item: Some(OutboundFileItem {
                media,
                file_name,
                len: uploaded.plaintext_size.to_string(),
            }),
        }
    };
    send_outbound_item(client, base_url, bot_token, to_user_id, context_token, item).await
}

async fn send_outbound_item(
    client: &Client,
    base_url: &str,
    bot_token: &str,
    to_user_id: &str,
    context_token: &str,
    item: OutboundItem,
) -> Result<()> {
    let url = api_url(base_url, "ilink/bot/sendmessage")?;
    let request = SendMessageRequest {
        msg: OutboundMessage {
            from_user_id: String::new(),
            to_user_id: to_user_id.to_string(),
            client_id: scru128::new().to_string(),
            message_type: 2,
            message_state: 2,
            context_token: context_token.to_string(),
            item_list: vec![item],
        },
        base_info: base_info(),
    };

    let response = client
        .post(url)
        .headers(build_headers(Some(bot_token))?)
        .json(&request)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("发送微信消息请求失败")?;
    let body = parse_body(response).await?;
    check_ret(&body, "sendmessage")
}

async fn upload_local_file(
    client: &Client,
    base_url: &str,
    bot_token: &str,
    to_user_id: &str,
    path: &Path,
    media_type: i64,
) -> Result<UploadedFile> {
    let plaintext = tokio::fs::read(path)
        .await
        .with_context(|| format!("读取待发送文件失败: {}", path.display()))?;
    let plaintext_size = plaintext.len();
    let rawfilemd5 = format!("{:x}", md5::compute(&plaintext));

    let mut file_key = [0u8; 16];
    let mut aes_key = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut file_key);
    rand::rngs::OsRng.fill_bytes(&mut aes_key);
    let filekey = hex::encode(file_key);
    let aes_key_hex = hex::encode(aes_key);
    let ciphertext = crate::crypto::encrypt_media(&aes_key, &plaintext)?;
    let ciphertext_size = ciphertext.len();

    let response = client
        .post(api_url(base_url, "ilink/bot/getuploadurl")?)
        .headers(build_headers(Some(bot_token))?)
        .json(&GetUploadUrlRequest {
            filekey: filekey.clone(),
            media_type,
            to_user_id: to_user_id.to_string(),
            rawsize: plaintext_size,
            rawfilemd5,
            filesize: ciphertext_size,
            no_need_thumb: true,
            aeskey: aes_key_hex.clone(),
            base_info: base_info(),
        })
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("申请微信文件上传地址失败")?;
    let body = parse_body(response).await?;
    check_ret(&body, "getuploadurl")?;
    let upload: GetUploadUrlResponse = serde_json::from_value(body.clone())
        .with_context(|| format!("解析微信文件上传地址失败: {}", truncate_json(&body)))?;
    let upload_url = resolve_upload_url(upload, &filekey)?;
    let download_param = upload_ciphertext(client, upload_url, &ciphertext).await?;

    Ok(UploadedFile {
        download_param,
        aes_key_hex,
        plaintext_size,
        ciphertext_size,
    })
}

fn resolve_upload_url(upload: GetUploadUrlResponse, filekey: &str) -> Result<Url> {
    if let Some(full_url) = upload
        .upload_full_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        let url = Url::parse(full_url).context("解析微信文件上传地址失败")?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("微信文件上传地址协议无效");
        }
        return Ok(url);
    }

    let upload_param = upload
        .upload_param
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("微信文件上传响应缺少上传地址")?;
    let mut url = Url::parse(&format!("{MEDIA_CDN_BASE_URL}/upload"))
        .context("解析微信媒体 CDN 上传地址失败")?;
    url.query_pairs_mut()
        .append_pair("encrypted_query_param", upload_param)
        .append_pair("filekey", filekey);
    Ok(url)
}

async fn upload_ciphertext(client: &Client, url: Url, ciphertext: &[u8]) -> Result<String> {
    let mut last_error = None;
    for attempt in 1..=UPLOAD_MAX_RETRIES {
        let result = client
            .post(url.clone())
            .header("content-type", "application/octet-stream")
            .body(ciphertext.to_vec())
            .timeout(UPLOAD_TIMEOUT)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {
                let parameter = response
                    .headers()
                    .get("x-encrypted-param")
                    .and_then(|value| value.to_str().ok())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .context("微信 CDN 上传响应缺少下载参数")?;
                return Ok(parameter.to_string());
            }
            Ok(response) if response.status().is_client_error() => {
                bail!("微信 CDN 上传被拒绝: HTTP {}", response.status());
            }
            Ok(response) => {
                last_error = Some(anyhow!("微信 CDN 上传失败: HTTP {}", response.status()));
            }
            Err(error) => {
                last_error = Some(anyhow!(error).context("微信 CDN 上传请求失败"));
            }
        }
        if attempt < UPLOAD_MAX_RETRIES {
            tracing::warn!("微信 CDN 上传失败，正在进行第 {} 次重试", attempt + 1);
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("微信 CDN 上传失败")))
}

fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "avif"
    )
}

pub async fn get_updates(
    client: &Client,
    base_url: &str,
    bot_token: &str,
    cursor: &str,
) -> Result<UpdatesResponse> {
    let url = api_url(base_url, "ilink/bot/getupdates")?;
    let request = GetUpdatesRequest {
        get_updates_buf: cursor.to_string(),
        base_info: base_info(),
    };
    let response = client
        .post(url)
        .headers(build_headers(Some(bot_token))?)
        .json(&request)
        .timeout(LONG_POLL_TIMEOUT)
        .send()
        .await
        .context("iLink 长轮询请求失败")?;
    let body = parse_body(response).await?;
    check_ret(&body, "getupdates")?;
    serde_json::from_value(body.clone())
        .with_context(|| format!("解析 iLink 消息响应失败: {}", truncate_json(&body)))
}

pub async fn download_image_bytes(client: &Client, image: &ImageItem) -> Result<Vec<u8>> {
    let url = build_media_download_url(image)?;
    let response = client
        .get(url)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("下载微信图片请求失败")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("下载微信图片失败: {status}, {}", truncate_str(&body, 256));
    }
    response
        .bytes()
        .await
        .map(Vec::from)
        .context("读取微信图片数据失败")
}

fn build_media_download_url(image: &ImageItem) -> Result<Url> {
    let media = image.media.as_ref();
    if let Some(full_url) = media
        .and_then(|media| media.full_url.as_deref())
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .or_else(|| {
            image
                .url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
        })
    {
        let url = Url::parse(full_url).context("解析微信图片下载地址失败")?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("微信图片下载地址协议无效");
        }
        return Ok(url);
    }

    let parameter = media
        .and_then(|media| media.encrypt_query_param.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("微信图片缺少下载参数")?;
    let mut url = Url::parse(&format!("{MEDIA_CDN_BASE_URL}/download"))
        .context("解析微信媒体 CDN 地址失败")?;
    url.query_pairs_mut()
        .append_pair("encrypted_query_param", parameter);
    Ok(url)
}

#[derive(Serialize)]
struct NotificationRequest {
    base_info: BaseInfo,
}

pub async fn notify_start(client: &Client, base_url: &str, bot_token: &str) -> Result<()> {
    notify(client, base_url, bot_token, "ilink/bot/msg/notifystart").await
}

pub async fn notify_stop(client: &Client, base_url: &str, bot_token: &str) -> Result<()> {
    notify(client, base_url, bot_token, "ilink/bot/msg/notifystop").await
}

async fn notify(client: &Client, base_url: &str, bot_token: &str, path: &str) -> Result<()> {
    let response = client
        .post(api_url(base_url, path)?)
        .headers(build_headers(Some(bot_token))?)
        .json(&NotificationRequest {
            base_info: base_info(),
        })
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("通知 iLink 运行状态失败")?;
    let body = parse_body(response).await?;
    check_ret(&body, path)
}

// -- 响应处理 --------------------------------------------------

async fn parse_body(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let bytes = response.bytes().await.context("读取 iLink 响应失败")?;
    if !status.is_success() {
        bail!(
            "iLink 请求失败（HTTP {status}）: {}",
            truncate_str(&String::from_utf8_lossy(&bytes), 512)
        );
    }
    if bytes.is_empty() {
        bail!("iLink 响应为空（HTTP {status}）");
    }
    serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "解析 iLink 响应 JSON 失败（HTTP {status}）: {}",
            truncate_str(&String::from_utf8_lossy(&bytes), 512)
        )
    })
}

fn check_ret(body: &Value, api: &str) -> Result<()> {
    for field in ["ret", "errcode"] {
        if let Some(code) = body.get(field).and_then(Value::as_i64)
            && code != 0
        {
            let message = body
                .get("err_msg")
                .or_else(|| body.get("errmsg"))
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            bail!("iLink {api} 失败: {field}={code}, errmsg={message}");
        }
    }
    Ok(())
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

fn truncate_json(value: &Value) -> String {
    truncate_str(&value.to_string(), 256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_render_content_uses_server_link() {
        let qr = QrCodeResponse {
            qrcode: "qr-id".into(),
            qrcode_img_content: Some("https://example.com/qr".into()),
            qrcode_url: None,
        };
        assert_eq!(qr.render_content(), "https://example.com/qr");
    }

    #[test]
    fn qr_status_matches_current_protocol_values() {
        for (status, expected) in [
            ("wait", QrStatus::Waiting),
            ("scaned", QrStatus::Scanned),
            ("confirmed", QrStatus::Confirmed),
            ("scaned_but_redirect", QrStatus::Redirect),
            ("need_verifycode", QrStatus::NeedVerifyCode),
            ("binded_redirect", QrStatus::AlreadyBound),
        ] {
            let response = QrStatusResponse {
                status: status.into(),
                bot_token: None,
                baseurl: None,
                redirect_host: None,
            };
            assert_eq!(response.status(), expected);
        }
    }

    #[test]
    fn check_ret_rejects_ret_and_errcode() {
        assert!(check_ret(&serde_json::json!({ "ret": 0 }), "test").is_ok());
        assert!(check_ret(&serde_json::json!({ "ret": 1 }), "test").is_err());
        assert!(check_ret(&serde_json::json!({ "errcode": -14 }), "test").is_err());
    }

    #[test]
    fn message_parses_current_image_shape() {
        let response: UpdatesResponse = serde_json::from_value(serde_json::json!({
            "get_updates_buf": "cursor-123",
            "msgs": [{
                "seq": 7,
                "message_id": 42,
                "from_user_id": "u@im.wechat",
                "message_type": 1,
                "context_token": "ctx",
                "item_list": [
                    { "type": 1, "text_item": { "text": "hello" } },
                    { "type": 2, "image_item": { "media": {
                        "encrypt_query_param": "a&b=c",
                        "aes_key": "AA=="
                    } } }
                ]
            }]
        }))
        .unwrap();
        let message = &response.msgs[0];
        assert_eq!(message.text_content(), "hello");
        assert_eq!(message.stable_message_id().as_deref(), Some("42"));
        let image = message.images().next().unwrap();
        let url = build_media_download_url(image).unwrap();
        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key == "encrypted_query_param")
                .map(|(_, value)| value.into_owned())
                .as_deref(),
            Some("a&b=c")
        );
    }

    #[test]
    fn base_url_rejects_non_https_values() {
        assert!(normalize_base_url(ILINK_BASE_URL).is_ok());
        assert!(normalize_base_url("http://127.0.0.1").is_err());
        assert!(normalize_base_url("not-a-url").is_err());
    }

    #[test]
    fn outbound_image_uses_current_ilink_media_shape() {
        let item = OutboundItem {
            kind: 2,
            text_item: None,
            image_item: Some(OutboundImageItem {
                media: OutboundCdnMedia {
                    encrypt_query_param: "download-param".into(),
                    aes_key: "YWVzLWtleQ==".into(),
                    encrypt_type: 1,
                },
                mid_size: 32,
            }),
            file_item: None,
        };
        let value = serde_json::to_value(item).unwrap();
        assert_eq!(value["type"], 2);
        assert_eq!(
            value["image_item"]["media"]["encrypt_query_param"],
            "download-param"
        );
        assert_eq!(value["image_item"]["media"]["encrypt_type"], 1);
        assert_eq!(value["image_item"]["mid_size"], 32);
        assert!(value.get("text_item").is_none());
    }

    #[test]
    fn upload_url_fallback_preserves_signed_parameter() {
        let url = resolve_upload_url(
            GetUploadUrlResponse {
                upload_param: Some("a&b=c".into()),
                upload_full_url: None,
            },
            "file-key",
        )
        .unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            query.get("encrypted_query_param").map(|v| v.as_ref()),
            Some("a&b=c")
        );
        assert_eq!(query.get("filekey").map(|v| v.as_ref()), Some("file-key"));
    }
}
