//! QQ 机器人扫码配置协议。
//!
//! 插件向 QQ 官方服务创建一次性绑定任务，用户扫码选择或创建机器人后，
//! 插件轮询取得加密凭证并自行解密、保存到平台数据目录。

use std::path::{Path, PathBuf};
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::Local;
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

const CREDENTIALS_FILE: &str = "credentials.json";
const CREATE_BIND_TASK_URL: &str = "https://q.qq.com/lite/create_bind_task";
const POLL_BIND_RESULT_URL: &str = "https://q.qq.com/lite/poll_bind_result";
const CONNECT_URL: &str = "https://q.qq.com/qqbot/openclaw/connect.html";
const CONNECT_SOURCE: &str = "tiangong";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_QR_EXPIRES_IN: i64 = 600;
pub const DEFAULT_QR_POLL_INTERVAL: u64 = 2;

const BIND_STATUS_NONE: u8 = 0;
const BIND_STATUS_PENDING: u8 = 1;
const BIND_STATUS_COMPLETED: u8 = 2;
const BIND_STATUS_EXPIRED: u8 = 3;
const AES_GCM_NONCE_LEN: usize = 12;
const AES_GCM_TAG_LEN: usize = 16;

/// 扫码会话（与主程序 `QrSession` 契约一致）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QrSession {
    /// 扫码 URL（前端渲染为二维码）。
    pub qr_url: String,
    /// 过期时间戳（Unix 秒）。
    pub expires_at: i64,
    /// 轮询间隔（秒）。
    pub interval: u64,
    /// bot 自己维护的会话状态，主程序不读取或解释。
    pub state: Value,
}

/// 轮询扫码授权的状态（与主程序 `ProvisionStatus` 契约一致）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProvisionStatus {
    Pending {
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_after: Option<u64>,
    },
    Success,
    Expired,
    Error {
        message: String,
    },
}

/// 扫码会话内部状态（仅 bot 自己使用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct QqSessionState {
    task_id: String,
    /// QQ 使用该密钥加密 AppSecret，只有发起绑定的 bot 能解密。
    decrypt_key: String,
    started_at: i64,
}

/// 扫码所得 / 手工填入的凭证。
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub app_id: String,
    pub app_secret: String,
}

#[derive(Serialize)]
struct CreateBindTaskRequest<'a> {
    key: &'a str,
}

#[derive(Deserialize)]
struct CreateBindTaskData {
    task_id: String,
}

#[derive(Serialize)]
struct PollBindResultRequest<'a> {
    task_id: &'a str,
}

#[derive(Deserialize)]
struct PollBindResultData {
    #[serde(default)]
    status: u8,
    #[serde(default)]
    bot_appid: Value,
    #[serde(default)]
    bot_encrypt_secret: String,
}

#[derive(Deserialize)]
struct QqApiResponse<T> {
    retcode: i64,
    #[serde(default)]
    msg: String,
    data: Option<T>,
}

// ── 凭证加载 ──────────────────────────────────────────────────

/// 读取平台数据目录中保存的凭证。
pub fn load_credentials(directory: &Path) -> Result<Option<Credentials>> {
    crate::platform::read_credentials_file::<Credentials>(&credentials_path(directory))?
        .map(validate_credentials)
        .transpose()
}

pub fn clear_credentials(directory: &Path) -> Result<()> {
    crate::platform::remove_credentials_file(&credentials_path(directory))
}

// ── 扫码流程 ──────────────────────────────────────────────────

/// 启动扫码会话：创建 QQ 官方绑定任务并返回任务专属二维码。
pub async fn begin() -> Result<QrSession> {
    let client = build_http_client()?;
    let decrypt_key = generate_bind_key();
    let task = create_bind_task(&client, &decrypt_key).await?;
    let qr_url = build_connect_url(&task.task_id)?;
    let now = Local::now().timestamp();

    Ok(QrSession {
        qr_url,
        expires_at: now.saturating_add(DEFAULT_QR_EXPIRES_IN),
        interval: DEFAULT_QR_POLL_INTERVAL,
        state: serde_json::to_value(QqSessionState {
            task_id: task.task_id,
            decrypt_key,
            started_at: now,
        })
        .context("保存 QQ 扫码会话失败")?,
    })
}

pub async fn poll(directory: &Path, session: &QrSession) -> Result<ProvisionStatus> {
    if Local::now().timestamp() >= session.expires_at {
        return Ok(ProvisionStatus::Expired);
    }

    let state: QqSessionState =
        serde_json::from_value(session.state.clone()).context("扫码会话状态无效")?;
    if state.task_id.trim().is_empty() {
        bail!("扫码会话缺少 QQ 绑定任务编号");
    }

    let client = build_http_client()?;
    let result = match poll_bind_result(&client, &state.task_id).await {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!("轮询 QQ 扫码状态失败，将继续等待: {error:#}");
            return Ok(ProvisionStatus::Pending {
                retry_after: Some(session.interval.max(DEFAULT_QR_POLL_INTERVAL)),
            });
        }
    };

    match result.status {
        BIND_STATUS_NONE | BIND_STATUS_PENDING => Ok(ProvisionStatus::Pending {
            retry_after: Some(session.interval.max(1)),
        }),
        BIND_STATUS_EXPIRED => Ok(ProvisionStatus::Expired),
        BIND_STATUS_COMPLETED => complete_provision(directory, result, &state.decrypt_key),
        status => Ok(ProvisionStatus::Error {
            message: format!("QQ 返回了未知的扫码状态：{status}"),
        }),
    }
}

fn complete_provision(
    directory: &Path,
    result: PollBindResultData,
    decrypt_key: &str,
) -> Result<ProvisionStatus> {
    let app_id = value_as_string(&result.bot_appid);
    if app_id.is_empty() || app_id == "0" || result.bot_encrypt_secret.is_empty() {
        return Ok(ProvisionStatus::Error {
            message: "QQ 未返回完整的机器人凭证，请重新扫码".into(),
        });
    }

    let app_secret = match decrypt_secret(&result.bot_encrypt_secret, decrypt_key) {
        Ok(secret) => secret,
        Err(error) => {
            tracing::warn!("解密 QQ 机器人凭证失败: {error:#}");
            return Ok(ProvisionStatus::Error {
                message: "QQ 机器人凭证解密失败，请重新扫码".into(),
            });
        }
    };

    save_credentials(directory, &Credentials { app_id, app_secret })?;
    tracing::info!("QQ 扫码绑定完成，凭证已安全保存");
    Ok(ProvisionStatus::Success)
}

/// 保存扫码所得或手工填写的凭证。
pub fn save_credentials(directory: &Path, credentials: &Credentials) -> Result<()> {
    let credentials = validate_credentials(credentials.clone())?;
    crate::platform::write_credentials_file(&credentials_path(directory), &credentials)
}

// ── QQ 官方绑定接口 ───────────────────────────────────────────

fn build_http_client() -> Result<Client> {
    Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("构建 QQ 扫码 HTTP 客户端失败")
}

fn generate_bind_key() -> String {
    let key = Aes256Gcm::generate_key(OsRng);
    BASE64_STANDARD.encode(key)
}

async fn create_bind_task(client: &Client, decrypt_key: &str) -> Result<CreateBindTaskData> {
    let task: CreateBindTaskData = post_qq_api(
        client,
        CREATE_BIND_TASK_URL,
        &CreateBindTaskRequest { key: decrypt_key },
        "创建 QQ 绑定任务",
    )
    .await?;
    if task.task_id.trim().is_empty() {
        bail!("创建 QQ 绑定任务响应缺少 task_id");
    }
    Ok(task)
}

async fn poll_bind_result(client: &Client, task_id: &str) -> Result<PollBindResultData> {
    post_qq_api(
        client,
        POLL_BIND_RESULT_URL,
        &PollBindResultRequest { task_id },
        "轮询 QQ 绑定结果",
    )
    .await
}

async fn post_qq_api<B, T>(client: &Client, url: &str, body: &B, operation: &str) -> Result<T>
where
    B: Serialize + ?Sized,
    T: DeserializeOwned,
{
    let response = client
        .post(url)
        .json(body)
        .send()
        .await
        .with_context(|| format!("{operation}请求失败"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("{operation}失败（HTTP {status}）");
    }

    let payload: QqApiResponse<T> = response
        .json()
        .await
        .with_context(|| format!("解析{operation}响应失败"))?;
    if payload.retcode != 0 {
        bail!(
            "{operation}失败：code={} message={}",
            payload.retcode,
            payload.msg
        );
    }
    payload
        .data
        .with_context(|| format!("{operation}响应缺少 data"))
}

fn build_connect_url(task_id: &str) -> Result<String> {
    let mut url = Url::parse(CONNECT_URL).context("QQ 扫码绑定地址无效")?;
    url.query_pairs_mut()
        .append_pair("task_id", task_id)
        .append_pair("source", CONNECT_SOURCE)
        .append_pair("_wv", "2");
    Ok(url.to_string())
}

fn decrypt_secret(encrypted_base64: &str, key_base64: &str) -> Result<String> {
    let key = BASE64_STANDARD
        .decode(key_base64)
        .context("解析 QQ 凭证解密密钥失败")?;
    let encrypted = BASE64_STANDARD
        .decode(encrypted_base64)
        .context("解析 QQ 加密凭证失败")?;
    if encrypted.len() < AES_GCM_NONCE_LEN + AES_GCM_TAG_LEN {
        bail!("QQ 加密凭证长度无效");
    }

    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow!("QQ 凭证解密密钥长度无效"))?;
    let (nonce, ciphertext_and_tag) = encrypted.split_at(AES_GCM_NONCE_LEN);
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext_and_tag)
        .map_err(|_| anyhow!("QQ 加密凭证校验失败"))?;
    String::from_utf8(plaintext).context("QQ 机器人凭证不是有效文本")
}

// ── 内部工具 ──────────────────────────────────────────────────

fn validate_credentials(credentials: Credentials) -> Result<Credentials> {
    let app_id = credentials.app_id.trim().to_string();
    let app_secret = credentials.app_secret.trim().to_string();
    if app_id.is_empty() || app_secret.is_empty() {
        bail!("QQ bot 配置缺少 AppID 或 ClientSecret");
    }
    Ok(Credentials { app_id, app_secret })
}

fn value_as_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.trim().to_string(),
        Value::Number(value) => value.to_string(),
        _ => String::new(),
    }
}

fn credentials_path(directory: &Path) -> PathBuf {
    directory.join(CREDENTIALS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_url_contains_task_and_source() {
        let url = Url::parse(&build_connect_url("task-123").unwrap()).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.path(), "/qqbot/openclaw/connect.html");
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query.get("task_id").map(String::as_str), Some("task-123"));
        assert_eq!(query.get("source").map(String::as_str), Some("tiangong"));
        assert_eq!(query.get("_wv").map(String::as_str), Some("2"));
    }

    #[test]
    fn decrypts_qq_secret_payload() {
        let key = [7_u8; 32];
        let nonce = [3_u8; AES_GCM_NONCE_LEN];
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), b"secret-value".as_ref())
            .unwrap();
        let mut payload = nonce.to_vec();
        payload.extend(ciphertext);

        let decrypted = decrypt_secret(
            &BASE64_STANDARD.encode(payload),
            &BASE64_STANDARD.encode(key),
        )
        .unwrap();
        assert_eq!(decrypted, "secret-value");
    }

    #[test]
    fn validate_rejects_missing_fields() {
        assert!(
            validate_credentials(Credentials {
                app_id: "".into(),
                app_secret: "secret".into(),
            })
            .is_err()
        );
        assert!(
            validate_credentials(Credentials {
                app_id: "102345".into(),
                app_secret: "  ".into(),
            })
            .is_err()
        );
    }

    #[test]
    fn validate_trims_credentials() {
        let credentials = validate_credentials(Credentials {
            app_id: "  102345  ".into(),
            app_secret: " secret ".into(),
        })
        .unwrap();
        assert_eq!(credentials.app_id, "102345");
        assert_eq!(credentials.app_secret, "secret");
    }

    #[tokio::test]
    async fn poll_reports_expired_after_timeout() {
        let session = QrSession {
            qr_url: CONNECT_URL.into(),
            expires_at: Local::now().timestamp() - 1,
            interval: 1,
            state: serde_json::to_value(QqSessionState {
                task_id: "task-123".into(),
                decrypt_key: BASE64_STANDARD.encode([0_u8; 32]),
                started_at: Local::now().timestamp(),
            })
            .unwrap(),
        };
        match poll(std::path::Path::new("/nonexistent"), &session)
            .await
            .unwrap()
        {
            ProvisionStatus::Expired => {}
            other => panic!("期望 Expired，实际 {other:?}"),
        }
    }

    #[test]
    fn qr_session_serializes_to_contract_shape() {
        let session = QrSession {
            qr_url: "https://example.com".into(),
            expires_at: 1_700_000_000,
            interval: 2,
            state: serde_json::json!({
                "task_id": "task-123",
                "decrypt_key": "key",
                "started_at": 1_700_000_000
            }),
        };
        let value = serde_json::to_value(&session).unwrap();
        assert_eq!(value["qr_url"], "https://example.com");
        assert_eq!(value["interval"], 2);
        assert_eq!(value["state"]["task_id"], "task-123");
    }
}
