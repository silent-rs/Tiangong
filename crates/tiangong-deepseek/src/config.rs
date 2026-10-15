use std::time::Duration;

use crate::client::DeepSeekClient;
use crate::error::DeepSeekError;

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// 旧版公开配置，保留以兼容 0.1.x；请改用 [`DeepSeekClient::builder`]。
#[deprecated(since = "0.1.5", note = "请改用 `DeepSeekClient::builder`")]
#[derive(Debug, Clone)]
pub struct DeepSeekConfig {
    pub headers: reqwest::header::HeaderMap,
    pub api_key: String,
    pub base_url: String,
    pub timeout: Duration,
}

#[allow(deprecated)]
impl DeepSeekConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            headers: Default::default(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// 客户端内部配置，不对外暴露。
#[derive(Clone)]
pub(crate) struct ClientOptions {
    pub(crate) headers: reqwest::header::HeaderMap,
    pub(crate) api_key: String,
    pub(crate) base_url: String,
    pub(crate) timeout: Duration,
}

#[allow(deprecated)]
impl From<DeepSeekConfig> for ClientOptions {
    fn from(config: DeepSeekConfig) -> Self {
        Self {
            headers: config.headers,
            api_key: config.api_key,
            base_url: config.base_url,
            timeout: config.timeout,
        }
    }
}

/// [`DeepSeekClient`] 构造器。
///
/// ```no_run
/// # fn main() -> Result<(), tiangong_deepseek::DeepSeekError> {
/// let client = tiangong_deepseek::DeepSeekClient::builder("your-api-key")
///     .base_url("https://api.deepseek.com")
///     .timeout(std::time::Duration::from_secs(30))
///     .build()?;
/// # let _ = client;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct DeepSeekClientBuilder {
    options: ClientOptions,
    no_proxy: bool,
}

impl DeepSeekClientBuilder {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            options: ClientOptions {
                headers: Default::default(),
                api_key: api_key.into(),
                base_url: DEFAULT_BASE_URL.to_string(),
                timeout: DEFAULT_TIMEOUT,
            },
            no_proxy: false,
        }
    }

    /// API 根地址，默认 `https://api.deepseek.com`。
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.options.base_url = base_url.into();
        self
    }

    /// 非流式请求总时限及流式请求建连/首包时限，默认 60 秒。
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.options.timeout = timeout;
        self
    }

    /// 每个请求附带的默认请求头。
    pub fn headers(mut self, headers: reqwest::header::HeaderMap) -> Self {
        self.options.headers = headers;
        self
    }

    /// 禁用全部代理（含 `HTTP(S)_PROXY` / `ALL_PROXY` 环境变量）直连目标。
    pub fn no_proxy(mut self) -> Self {
        self.no_proxy = true;
        self
    }

    pub fn build(self) -> Result<DeepSeekClient, DeepSeekError> {
        DeepSeekClient::from_options(self.options, self.no_proxy)
    }
}

impl std::fmt::Debug for DeepSeekClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeepSeekClientBuilder")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.options.base_url)
            .field("timeout", &self.options.timeout)
            .field("headers", &self.options.headers.keys().collect::<Vec<_>>())
            .field("no_proxy", &self.no_proxy)
            .finish()
    }
}
