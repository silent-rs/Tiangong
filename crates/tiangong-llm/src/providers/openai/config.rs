use std::sync::Arc;
use std::time::Duration;

/// 重试通知回调。
pub type RetryNotifier = Arc<dyn Fn(u32, u32, u64, &str) + Send + Sync>;

/// OpenAI Responses provider 配置。
#[derive(Clone)]
pub struct OpenAiResponsesConfig {
    pub headers: reqwest::header::HeaderMap,
    pub api_key: String,
    pub base_url: String,
    pub timeout: Duration,
    pub max_retries: u32,
    pub retry_notifier: Option<RetryNotifier>,
    /// Codex（ChatGPT 账号）模式：鉴权来自 OAuth 登录态，忽略 `api_key`，
    /// 请求体按 Codex 后端约束改写，模型列表走 `/models?client_version=`。
    pub codex: bool,
}

impl OpenAiResponsesConfig {
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            headers: Default::default(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            timeout: Duration::from_secs(60),
            max_retries: 3,
            retry_notifier: None,
            codex: false,
        }
    }
}

impl std::fmt::Debug for OpenAiResponsesConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiResponsesConfig")
            .field("api_key", &(!self.api_key.is_empty()))
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .field("retry_notifier", &self.retry_notifier.is_some())
            .field("codex", &self.codex)
            .finish()
    }
}
