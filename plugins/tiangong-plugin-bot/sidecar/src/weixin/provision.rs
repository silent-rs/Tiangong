//! 微信 iLink 扫码登录协议。
//!
//! 由 Bot 插件设置页经 sidecar 调用，`state` 字段只由本模块解释。
//!
//! 流程：
//! 1. `begin()`：请求 iLink 获取二维码 URL + 会话标识
//! 2. `poll()`：长轮询扫码状态，成功时保存 `bot_token` 到平台数据目录

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Local;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ilink;

const CREDENTIALS_FILE: &str = "credentials.json";

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
struct WeixinSessionState {
    /// iLink 返回的 qrcode 标识（用于轮询扫码状态）。
    qrcode: String,
}

/// 扫码所得凭证。
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub bot_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
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

/// 启动扫码会话：请求 iLink 获取二维码。
pub async fn begin(directory: &Path) -> Result<QrSession> {
    let client = Client::builder().build().context("构建 HTTP 客户端失败")?;

    let local_tokens = load_credentials(directory)?
        .map(|credentials| vec![credentials.bot_token])
        .unwrap_or_default();
    let qr = ilink::get_bot_qrcode(&client, &local_tokens).await?;
    if qr.qrcode.trim().is_empty() {
        bail!("iLink 未返回二维码标识");
    }

    let expires_at = Local::now()
        .timestamp()
        .saturating_add(ilink::DEFAULT_QR_EXPIRES_IN);

    Ok(QrSession {
        qr_url: qr.render_content(),
        expires_at,
        interval: ilink::DEFAULT_QR_POLL_INTERVAL,
        state: serde_json::to_value(WeixinSessionState { qrcode: qr.qrcode })
            .context("保存微信扫码会话失败")?,
    })
}

/// 轮询扫码状态。
pub async fn poll(directory: &Path, session: &QrSession) -> Result<ProvisionStatus> {
    if Local::now().timestamp() >= session.expires_at {
        return Ok(ProvisionStatus::Expired);
    }

    let state: WeixinSessionState =
        serde_json::from_value(session.state.clone()).context("扫码会话状态无效")?;
    if state.qrcode.trim().is_empty() {
        bail!("扫码会话缺少二维码标识");
    }

    let client = Client::builder().build().context("构建 HTTP 客户端失败")?;
    let mut base_url = ilink::ILINK_BASE_URL.to_string();

    for _ in 0..3 {
        let Some(response) = ilink::get_qrcode_status(&client, &base_url, &state.qrcode).await?
        else {
            return Ok(ProvisionStatus::Pending { retry_after: None });
        };

        match response.status() {
            ilink::QrStatus::Confirmed => {
                let bot_token = response
                    .bot_token
                    .filter(|token| !token.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("iLink 授权成功但未返回 bot_token"))?;
                let response_base_url = response
                    .baseurl
                    .filter(|url| !url.trim().is_empty())
                    .or_else(|| (base_url != ilink::ILINK_BASE_URL).then_some(base_url));
                save_credentials(
                    directory,
                    &Credentials {
                        bot_token,
                        base_url: response_base_url,
                    },
                )?;
                tracing::info!("微信扫码授权成功，凭证已保存");
                return Ok(ProvisionStatus::Success);
            }
            ilink::QrStatus::Waiting | ilink::QrStatus::Scanned => {
                return Ok(ProvisionStatus::Pending { retry_after: None });
            }
            ilink::QrStatus::Expired => return Ok(ProvisionStatus::Expired),
            ilink::QrStatus::Canceled => {
                return Ok(ProvisionStatus::Error {
                    message: "已取消微信授权".to_string(),
                });
            }
            ilink::QrStatus::Redirect => {
                let redirect_host = response.redirect_host.as_deref().unwrap_or_default();
                base_url = ilink::redirect_base_url(redirect_host)?;
            }
            ilink::QrStatus::NeedVerifyCode => {
                return Ok(ProvisionStatus::Error {
                    message: "当前微信账号需要输入手机上显示的配对码，请重新扫码或改用 Bot Token 手工配置"
                        .to_string(),
                });
            }
            ilink::QrStatus::VerifyCodeBlocked => {
                return Ok(ProvisionStatus::Error {
                    message: "微信配对码验证次数过多，请稍后重新生成二维码".to_string(),
                });
            }
            ilink::QrStatus::AlreadyBound => {
                return Ok(if load_credentials(directory)?.is_some() {
                    ProvisionStatus::Success
                } else {
                    ProvisionStatus::Error {
                        message: "该微信账号已绑定，但本地没有可用凭证，请改用 Bot Token 手工配置"
                            .to_string(),
                    }
                });
            }
            ilink::QrStatus::Unknown => {
                return Ok(ProvisionStatus::Error {
                    message: format!("微信返回了无法识别的扫码状态：{}", response.status),
                });
            }
        }
    }

    Ok(ProvisionStatus::Error {
        message: "微信扫码服务连续重定向次数过多，请重新生成二维码".to_string(),
    })
}

// ── 凭证持久化 ────────────────────────────────────────────────

/// 保存扫码所得或手工填写的凭证。
pub fn save_credentials(directory: &Path, credentials: &Credentials) -> Result<()> {
    let credentials = validate_credentials(credentials.clone())?;
    crate::platform::write_credentials_file(&credentials_path(directory), &credentials)
}

fn validate_credentials(credentials: Credentials) -> Result<Credentials> {
    let bot_token = credentials.bot_token.trim().to_string();
    if bot_token.is_empty() {
        bail!("微信 bot 配置缺少 bot_token");
    }
    let base_url = credentials
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(ilink::normalize_base_url)
        .transpose()?;
    Ok(Credentials {
        bot_token,
        base_url,
    })
}

fn credentials_path(directory: &Path) -> PathBuf {
    directory.join(CREDENTIALS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_round_trip_and_validation() {
        let directory = tempfile::tempdir().unwrap();
        assert!(load_credentials(directory.path()).unwrap().is_none());
        save_credentials(
            directory.path(),
            &Credentials {
                bot_token: " token ".into(),
                base_url: None,
            },
        )
        .unwrap();
        let loaded = load_credentials(directory.path()).unwrap().unwrap();
        assert_eq!(loaded.bot_token, "token");
        assert!(
            save_credentials(
                directory.path(),
                &Credentials {
                    bot_token: "  ".into(),
                    base_url: None,
                },
            )
            .is_err()
        );
        clear_credentials(directory.path()).unwrap();
        assert!(load_credentials(directory.path()).unwrap().is_none());
    }

    #[tokio::test]
    async fn poll_reports_expired_session() {
        let directory = tempfile::tempdir().unwrap();
        let session = QrSession {
            qr_url: "https://example.invalid".into(),
            expires_at: 0,
            interval: 1,
            state: serde_json::json!({ "qrcode": "code" }),
        };
        assert!(matches!(
            poll(directory.path(), &session).await.unwrap(),
            ProvisionStatus::Expired
        ));
    }
}
