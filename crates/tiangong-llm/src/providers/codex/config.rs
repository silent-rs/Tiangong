use std::time::Duration;

use reqwest::header::HeaderMap;

pub(crate) use crate::providers::openai::RetryNotifier;

/// Codex 后端配置。鉴权不在配置中：每次请求从 ChatGPT 登录态取令牌。
#[derive(Clone)]
pub struct CodexConfig {
    pub base_url: String,
    pub headers: HeaderMap,
    pub timeout: Duration,
    pub max_retries: u32,
    pub retry_notifier: Option<RetryNotifier>,
}

impl CodexConfig {
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        Self {
            base_url: if base_url.trim().is_empty() {
                super::auth::CODEX_BASE_URL.to_string()
            } else {
                base_url
            },
            headers: HeaderMap::new(),
            timeout: Duration::from_secs(60),
            max_retries: 3,
            retry_notifier: None,
        }
    }
}

impl std::fmt::Debug for CodexConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexConfig")
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .field("retry_notifier", &self.retry_notifier.is_some())
            .finish()
    }
}
