use std::sync::Arc;
use std::time::Duration;

pub type RetryNotifier = Arc<dyn Fn(u32, u32, u64, &str) + Send + Sync>;

/// DeepSeek provider 配置。
#[derive(Clone)]
pub struct DeepSeekConfig {
    pub headers: reqwest::header::HeaderMap,
    pub api_key: String,
    pub base_url: Option<String>,
    pub timeout: Duration,
    pub max_retries: u32,
    pub retry_notifier: Option<RetryNotifier>,
}

impl std::fmt::Debug for DeepSeekConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeepSeekConfig")
            .field("api_key", &(!self.api_key.is_empty()))
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .field("retry_notifier", &self.retry_notifier.is_some())
            .finish()
    }
}

impl DeepSeekConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            headers: Default::default(),
            api_key: api_key.into(),
            base_url: None,
            timeout: Duration::from_secs(60),
            max_retries: crate::retry::DEFAULT_MAX_RETRIES,
            retry_notifier: None,
        }
    }

    pub fn resolved_base_url(&self) -> String {
        self.base_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .unwrap_or("https://api.deepseek.com")
            .trim_end_matches('/')
            .to_string()
    }
}
