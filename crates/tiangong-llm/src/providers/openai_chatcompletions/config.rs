use std::sync::Arc;
use std::time::Duration;

pub type RetryNotifier = Arc<dyn Fn(u32, u32, u64, &str) + Send + Sync>;

#[derive(Clone)]
pub struct OpenAiChatConfig {
    pub headers: reqwest::header::HeaderMap,
    pub api_key: String,
    pub base_url: String,
    pub timeout: Duration,
    pub max_retries: u32,
    pub retry_notifier: Option<RetryNotifier>,
}

impl OpenAiChatConfig {
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            headers: Default::default(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            timeout: Duration::from_secs(60),
            max_retries: crate::retry::DEFAULT_MAX_RETRIES,
            retry_notifier: None,
        }
    }
}

impl std::fmt::Debug for OpenAiChatConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiChatConfig")
            .field("api_key", &(!self.api_key.is_empty()))
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .field("retry_notifier", &self.retry_notifier.is_some())
            .finish()
    }
}
