use std::time::Duration;

use crate::client::AnthropicClient;
use crate::error::AnthropicError;

/// 客户端内部配置，不对外暴露；通过 [`AnthropicClient::builder`] 构造。
#[derive(Clone)]
pub(crate) struct ClientOptions {
    pub(crate) headers: reqwest::header::HeaderMap,
    pub(crate) api_key: String,
    pub(crate) base_url: String,
    pub(crate) timeout: Duration,
    pub(crate) api_version: String,
    pub(crate) beta: Option<String>,
}

/// [`AnthropicClient`] 构造器。
#[derive(Clone)]
pub struct AnthropicClientBuilder {
    options: ClientOptions,
    no_proxy: bool,
}

impl AnthropicClientBuilder {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            options: ClientOptions {
                headers: Default::default(),
                api_key: api_key.into(),
                base_url: "https://api.anthropic.com".to_string(),
                timeout: Duration::from_secs(60),
                api_version: "2023-06-01".to_string(),
                beta: None,
            },
            no_proxy: false,
        }
    }

    /// API 根地址，默认 `https://api.anthropic.com`。
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

    /// `anthropic-version` 请求头，默认 `2023-06-01`。
    pub fn api_version(mut self, api_version: impl Into<String>) -> Self {
        self.options.api_version = api_version.into();
        self
    }

    /// `anthropic-beta` 请求头，默认不发送。
    pub fn beta(mut self, beta: Option<String>) -> Self {
        self.options.beta = beta;
        self
    }

    /// 禁用全部代理（含 `HTTP(S)_PROXY` / `ALL_PROXY` 环境变量）直连目标。
    pub fn no_proxy(mut self) -> Self {
        self.no_proxy = true;
        self
    }

    pub fn build(self) -> Result<AnthropicClient, AnthropicError> {
        AnthropicClient::from_options(self.options, self.no_proxy)
    }
}

impl std::fmt::Debug for AnthropicClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicClientBuilder")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.options.base_url)
            .field("timeout", &self.options.timeout)
            .field("headers", &self.options.headers.keys().collect::<Vec<_>>())
            .field("api_version", &self.options.api_version)
            .field("beta", &self.options.beta)
            .field("no_proxy", &self.no_proxy)
            .finish()
    }
}
