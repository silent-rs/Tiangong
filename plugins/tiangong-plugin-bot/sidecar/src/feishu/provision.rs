//! 飞书应用扫码注册协议。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Local;
use reqwest::{Client, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const FEISHU_ACCOUNTS_URL: &str = "https://accounts.feishu.cn";
const LARK_ACCOUNTS_URL: &str = "https://accounts.larksuite.com";
const REGISTRATION_PATH: &str = "/oauth/v1/app/registration";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CREDENTIALS_FILE: &str = "credentials.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QrSession {
    pub qr_url: String,
    pub expires_at: i64,
    pub interval: u64,
    pub state: Value,
}

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

#[derive(Serialize, Deserialize)]
struct FeishuSessionState {
    device_code: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub app_id: String,
    pub app_secret: String,
}

/// 读取插件数据目录中的飞书应用凭据（扫码或手工配置后保存）。
pub fn load_credentials(directory: &Path) -> Result<Option<Credentials>> {
    let path = credentials_path(directory);
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read(&path)
        .with_context(|| format!("读取飞书 bot 配置失败：{}", path.display()))?;
    let credentials = serde_json::from_slice(&content)
        .with_context(|| format!("解析飞书 bot 配置失败：{}", path.display()))?;
    validate_credentials(credentials).map(Some)
}

/// 删除已保存的凭据（解除绑定）。
pub fn clear_credentials(directory: &Path) -> Result<()> {
    match std::fs::remove_file(credentials_path(directory)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("删除飞书 bot 配置失败"),
    }
}

pub async fn begin() -> Result<QrSession> {
    let client = Client::new();
    let (status, init): (_, InitResponse) =
        post(&client, FEISHU_ACCOUNTS_URL, &[("action", "init")]).await?;
    if !status.is_success() {
        bail!("飞书扫码服务请求失败（HTTP {status}）");
    }
    if !init
        .supported_auth_methods
        .iter()
        .any(|method| method == "client_secret")
    {
        bail!("飞书扫码服务暂不支持 App Secret 授权");
    }

    let (status, begin): (_, BeginResponse) = post(
        &client,
        FEISHU_ACCOUNTS_URL,
        &[
            ("action", "begin"),
            ("archetype", "PersonalAgent"),
            ("auth_method", "client_secret"),
            ("request_user_info", "open_id"),
        ],
    )
    .await?;
    if !status.is_success() {
        bail!("飞书扫码服务请求失败（HTTP {status}）");
    }
    if begin.device_code.trim().is_empty() {
        bail!("飞书扫码服务未返回设备码");
    }

    let expires_in = begin.expires_in.unwrap_or(600).max(1);
    Ok(QrSession {
        qr_url: build_qr_url(&begin.verification_uri_complete)?,
        expires_at: Local::now().timestamp().saturating_add(expires_in),
        interval: begin.interval.unwrap_or(5).max(1),
        state: serde_json::to_value(FeishuSessionState {
            device_code: begin.device_code,
        })
        .context("保存飞书扫码会话失败")?,
    })
}

/// 轮询扫码结果；授权成功时把凭据保存到 `directory`。
pub async fn poll(directory: &Path, session: &QrSession) -> Result<ProvisionStatus> {
    if Local::now().timestamp() >= session.expires_at {
        return Ok(ProvisionStatus::Expired);
    }
    let state: FeishuSessionState =
        serde_json::from_value(session.state.clone()).context("扫码会话状态无效")?;
    if state.device_code.trim().is_empty() {
        bail!("扫码会话缺少设备码");
    }

    let client = Client::new();
    let mut response = poll_at(&client, FEISHU_ACCOUNTS_URL, &state.device_code).await?;
    if credentials_from_response(&response).is_none()
        && response
            .user_info
            .as_ref()
            .and_then(|info| info.tenant_brand.as_deref())
            == Some("lark")
    {
        response = poll_at(&client, LARK_ACCOUNTS_URL, &state.device_code).await?;
    }

    if let Some(credentials) = credentials_from_response(&response) {
        save_credentials(directory, &credentials)?;
        return Ok(ProvisionStatus::Success);
    }

    let error = response.error.as_deref().unwrap_or_default();
    match error {
        "authorization_pending" => Ok(ProvisionStatus::Pending { retry_after: None }),
        "slow_down" => Ok(ProvisionStatus::Pending {
            retry_after: Some(session.interval.saturating_add(5)),
        }),
        "expired_token" => Ok(ProvisionStatus::Expired),
        "access_denied" => Ok(ProvisionStatus::Error {
            message: "已取消飞书授权".to_string(),
        }),
        _ => Ok(ProvisionStatus::Error {
            message: response
                .error_description
                .filter(|message| !message.trim().is_empty())
                .unwrap_or_else(|| {
                    if error.is_empty() {
                        "飞书返回了无法识别的授权状态".to_string()
                    } else {
                        format!("飞书授权失败：{error}")
                    }
                }),
        }),
    }
}

async fn poll_at(client: &Client, domain: &str, device_code: &str) -> Result<PollResponse> {
    let (status, response): (_, PollResponse) = post(
        client,
        domain,
        &[("action", "poll"), ("device_code", device_code)],
    )
    .await?;
    if !status.is_success() && response.error.is_none() {
        bail!("飞书扫码服务请求失败（HTTP {status}）");
    }
    Ok(response)
}

async fn post<T>(
    client: &Client,
    domain: &str,
    form: &[(&str, &str)],
) -> Result<(reqwest::StatusCode, T)>
where
    T: DeserializeOwned,
{
    let response = client
        .post(format!("{domain}{REGISTRATION_PATH}"))
        .timeout(REQUEST_TIMEOUT)
        .form(form)
        .send()
        .await
        .context("连接飞书扫码服务失败")?;
    let status = response.status();
    let body = response.bytes().await.context("读取飞书扫码响应失败")?;
    let parsed = serde_json::from_slice(&body).with_context(|| {
        if status.is_success() {
            "解析飞书扫码响应失败".to_string()
        } else {
            format!("飞书扫码服务请求失败（HTTP {status}）")
        }
    })?;
    Ok((status, parsed))
}

#[derive(Deserialize)]
struct InitResponse {
    #[serde(default)]
    supported_auth_methods: Vec<String>,
}

#[derive(Deserialize)]
struct BeginResponse {
    device_code: String,
    verification_uri_complete: String,
    interval: Option<u64>,
    expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct PollResponse {
    client_id: Option<String>,
    client_secret: Option<String>,
    user_info: Option<PollUserInfo>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct PollUserInfo {
    tenant_brand: Option<String>,
}

fn build_qr_url(raw_url: &str) -> Result<String> {
    let mut url = Url::parse(raw_url).context("飞书返回了无效的扫码地址")?;
    if url.scheme() != "https" {
        bail!("飞书返回了不安全的扫码地址");
    }
    url.query_pairs_mut()
        .append_pair("from", "sdk")
        .append_pair("tp", "sdk")
        .append_pair("source", "tiangong");
    Ok(url.to_string())
}

fn credentials_from_response(response: &PollResponse) -> Option<Credentials> {
    let app_id = response.client_id.as_ref()?.trim();
    let app_secret = response.client_secret.as_ref()?.trim();
    if app_id.is_empty() || app_secret.is_empty() {
        return None;
    }

    Some(Credentials {
        app_id: app_id.to_string(),
        app_secret: app_secret.to_string(),
    })
}

pub fn save_credentials(directory: &Path, credentials: &Credentials) -> Result<()> {
    let credentials = validate_credentials(credentials.clone())?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("创建飞书插件数据目录失败：{}", directory.display()))?;
    let path = credentials_path(directory);
    let content = serde_json::to_vec(&credentials).context("序列化飞书 bot 配置失败")?;

    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(&path)
        .with_context(|| format!("保存飞书 bot 配置失败：{}", path.display()))?;
    file.write_all(&content)
        .with_context(|| format!("写入飞书 bot 配置失败：{}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("同步飞书 bot 配置失败：{}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("设置飞书 bot 配置权限失败：{}", path.display()))?;
    }

    Ok(())
}

fn validate_credentials(credentials: Credentials) -> Result<Credentials> {
    if credentials.app_id.trim().is_empty() || credentials.app_secret.trim().is_empty() {
        bail!("飞书 bot 配置缺少 App ID 或 App Secret");
    }
    Ok(credentials)
}

fn credentials_path(directory: &Path) -> PathBuf {
    directory.join(CREDENTIALS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_round_trip_and_clear() {
        let directory = tempfile::tempdir().unwrap();
        assert!(load_credentials(directory.path()).unwrap().is_none());
        let credentials = Credentials {
            app_id: "cli_a".into(),
            app_secret: "secret".into(),
        };
        save_credentials(directory.path(), &credentials).unwrap();
        let loaded = load_credentials(directory.path()).unwrap().unwrap();
        assert_eq!(loaded.app_id, "cli_a");
        assert!(
            save_credentials(
                directory.path(),
                &Credentials {
                    app_id: " ".into(),
                    app_secret: "x".into()
                }
            )
            .is_err()
        );
        clear_credentials(directory.path()).unwrap();
        clear_credentials(directory.path()).unwrap();
        assert!(load_credentials(directory.path()).unwrap().is_none());
    }
}
