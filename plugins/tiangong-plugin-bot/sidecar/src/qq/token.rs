//! QQ 开放平台 AppAccessToken 获取与缓存。
//!
//! 官方文档：`POST https://bots.qq.com/app/getAppAccessToken`，
//! 请求体 `{ appId, clientSecret }`，返回 `{ access_token, expires_in }`。
//! `expires_in` 单位为秒，通常约 7200s。为避免边界过期，提前 60s 视为过期。

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::RwLock;

const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
/// 提前量：access_token 在到期前 60s 即视为过期，触发刷新。
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// access_token 缓存。线程安全，过期自动刷新。
#[derive(Clone)]
pub struct AccessTokenCache {
    http: Client,
    app_id: String,
    app_secret: String,
    inner: Arc<RwLock<Option<CachedToken>>>,
}

#[derive(Clone)]
struct CachedToken {
    token: String,
    /// 绝对过期时刻。
    expires_at: SystemTime,
}

impl AccessTokenCache {
    pub fn new(http: Client, app_id: String, app_secret: String) -> Self {
        Self {
            http,
            app_id,
            app_secret,
            inner: Arc::new(RwLock::new(None)),
        }
    }

    /// 返回有效的 access_token；若缓存过期则刷新。
    pub async fn get(&self) -> Result<String> {
        if let Some(token) = self.inner.read().await.as_ref()
            && token.expires_at > SystemTime::now()
        {
            return Ok(token.token.clone());
        }

        self.refresh().await
    }

    /// 强制刷新 access_token。
    pub async fn refresh(&self) -> Result<String> {
        let response = fetch_token(&self.http, &self.app_id, &self.app_secret).await?;
        let expires_in = Duration::from_secs(response.expires_in.max(1));
        let expires_at = SystemTime::now() + expires_in.saturating_sub(EXPIRY_MARGIN);

        let token = response.access_token;
        *self.inner.write().await = Some(CachedToken {
            token: token.clone(),
            expires_at,
        });
        tracing::info!("QQ access_token 已刷新，有效期约 {}s", expires_in.as_secs());
        Ok(token)
    }
}

#[derive(Debug, Serialize)]
#[allow(non_snake_case)]
struct TokenRequest {
    appId: String,
    clientSecret: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(deserialize_with = "deserialize_expires_in")]
    expires_in: u64,
}

fn deserialize_expires_in<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ExpiresIn {
        Number(u64),
        String(String),
    }

    match ExpiresIn::deserialize(deserializer)? {
        ExpiresIn::Number(value) => Ok(value),
        ExpiresIn::String(value) => value
            .parse::<u64>()
            .map_err(|_| serde::de::Error::custom("expires_in 不是有效的非负整数")),
    }
}

async fn fetch_token(http: &Client, app_id: &str, app_secret: &str) -> Result<TokenResponse> {
    let response = http
        .post(TOKEN_URL)
        .json(&TokenRequest {
            appId: app_id.to_string(),
            clientSecret: app_secret.to_string(),
        })
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("请求 QQ access_token 失败")?;

    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .context("读取 QQ access_token 响应失败")?;
    if !status.is_success() {
        return Err(anyhow!("QQ access_token 请求失败（HTTP {status}）"));
    }

    serde_json::from_slice::<TokenResponse>(&bytes).context("解析 QQ access_token 响应失败")
}

/// 判断给定 expires_in（秒）是否在提前量内会被视为过期（用于测试与诊断）。
#[cfg(test)]
#[allow(dead_code)]
pub fn is_within_margin(expires_in_secs: u64) -> bool {
    Duration::from_secs(expires_in_secs) <= EXPIRY_MARGIN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_margin_detects_short_expiry() {
        assert!(is_within_margin(30));
        assert!(is_within_margin(60));
        assert!(!is_within_margin(61));
        assert!(!is_within_margin(7200));
    }

    #[test]
    fn token_response_accepts_number_and_string_expiry() {
        for raw in [
            r#"{"access_token":"abc","expires_in":7200}"#,
            r#"{"access_token":"abc","expires_in":"7200"}"#,
        ] {
            let parsed: TokenResponse = serde_json::from_str(raw).unwrap();
            assert_eq!(parsed.access_token, "abc");
            assert_eq!(parsed.expires_in, 7200);
        }
    }

    #[test]
    fn token_request_uses_camel_case_keys() {
        let request = TokenRequest {
            appId: "102345".into(),
            clientSecret: "s3cr3t".into(),
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["appId"], "102345");
        assert_eq!(value["clientSecret"], "s3cr3t");
    }
}
