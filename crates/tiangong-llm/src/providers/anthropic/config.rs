use std::sync::Arc;
use std::time::Duration;

use crate::error::LlmError;

pub type RetryNotifier = Arc<dyn Fn(u32, u32, u64, &str) + Send + Sync>;

const DEFAULT_API_VERSION: &str = "2023-06-01";

/// Anthropic provider 配置。
#[derive(Clone)]
pub struct AnthropicConfig {
    pub headers: reqwest::header::HeaderMap,
    pub api_key: String,
    pub base_url: Option<String>,
    pub timeout: Duration,
    pub max_retries: u32,
    pub api_version: Option<String>,
    pub beta: Option<String>,
    pub retry_notifier: Option<RetryNotifier>,
}

impl std::fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicConfig")
            .field("api_key", &(!self.api_key.is_empty()))
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .field("api_version", &self.api_version)
            .field("beta", &self.beta)
            .field("retry_notifier", &self.retry_notifier.is_some())
            .finish()
    }
}

impl AnthropicConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            headers: Default::default(),
            api_key: api_key.into(),
            base_url: None,
            timeout: Duration::from_secs(60),
            max_retries: crate::retry::DEFAULT_MAX_RETRIES,
            api_version: Some(DEFAULT_API_VERSION.to_string()),
            beta: None,
            retry_notifier: None,
        }
    }

    #[allow(dead_code)]
    pub fn from_env() -> Result<Self, LlmError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| LlmError::Configuration("缺少环境变量 ANTHROPIC_API_KEY".to_string()))?;
        let mut config = Self::new(api_key);
        config.base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
        config.api_version = std::env::var("ANTHROPIC_API_VERSION").ok();
        Ok(config)
    }

    pub fn normalized_base_url(&self) -> Option<String> {
        self.base_url.as_ref().map(|base_url| {
            let trimmed = base_url.trim().trim_end_matches('/').to_string();
            trimmed.strip_suffix("/v1").unwrap_or(&trimmed).to_string()
        })
    }

    pub fn resolve_api_version(&self) -> String {
        self.api_version
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(DEFAULT_API_VERSION)
            .to_string()
    }
}
