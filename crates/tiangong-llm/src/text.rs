//! 简单文本完成工具。
//!
//! 供上层 crate 复用统一 provider 构建与纯文本调用，避免各业务 crate
//! 重复实现 OpenAI/Anthropic 分支。

use std::time::Duration;

use crate::endpoint::ModelEndpoint;
use crate::error::LlmError;
use crate::model::ProviderProtocol;
use crate::provider_client::SingleProviderClient;
use crate::request::ReasoningEffort;
use tiangong_types::{Message, MessageRole};

#[derive(Debug, Clone)]
pub struct LlmEndpointConfig {
    pub source_provider: Option<String>,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub protocol: ProviderProtocol,
    pub timeout: Duration,
    pub max_retries: u32,
}

impl LlmEndpointConfig {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            source_provider: None,
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
            protocol: ProviderProtocol::default(),
            timeout: Duration::from_secs(60),
            max_retries: crate::retry::DEFAULT_MAX_RETRIES,
        }
    }

    pub fn source_provider_label(&self) -> &str {
        self.source_provider
            .as_deref()
            .map(str::trim)
            .filter(|provider| !provider.is_empty())
            .unwrap_or_else(|| self.protocol.as_str())
    }
}

pub async fn complete_text(
    config: &LlmEndpointConfig,
    system: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<String, LlmError> {
    let (text, _) = complete_text_with_usage(config, system, prompt, max_tokens).await?;
    Ok(text)
}

/// 带 token 用量信息的文本完成。
pub async fn complete_text_with_usage(
    config: &LlmEndpointConfig,
    system: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<(String, Option<crate::usage::TokenUsageData>), LlmError> {
    let timeout_ms = config.timeout.as_millis().min(u64::MAX as u128) as u64;
    let client = SingleProviderClient::new(ModelEndpoint {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: config.model.clone(),
        protocol: config.protocol,
        timeout_ms,
        ..Default::default()
    })
    .with_options(crate::Options {
        reasoning_effort: ReasoningEffort::None,
        max_output_tokens: max_tokens,
        temperature: Some(0.2),
        timeout_ms: Some(timeout_ms),
        max_retries: config.max_retries,
        ..Default::default()
    });
    let messages = vec![
        Message::new(MessageRole::System, system),
        Message::new(MessageRole::User, prompt),
    ];
    client
        .stream(messages)
        .map_err(map_error)?
        .collect_text_with_usage()
        .await
        .map_err(map_error)
}
fn map_error(error: anyhow::Error) -> LlmError {
    error
        .downcast_ref::<LlmError>()
        .cloned()
        .unwrap_or_else(|| LlmError::InvalidRequest(error.to_string()))
}
