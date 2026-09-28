use std::time::Duration;

use futures_util::Stream;
use serde_json::Value;
use tokio::time::timeout;

use crate::error::LlmError;
use crate::model::ProviderModelInfo;

use super::config::OpenAiResponsesConfig;
use super::error::{is_retryable_responses_error, map_responses_error};
use super::mapping::normalize_api_base;

type ResponsesByotStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<Value, async_openai::error::OpenAIError>> + Send>>;

/// 流式请求的响应形态：正常 SSE 流，或服务端忽略 stream 参数返回的一次性 JSON。
pub enum ResponsesStreamResponse {
    Sse(ResponsesByotStream),
    Complete(Value),
}

const INITIAL_RETRY_DELAY_MS: u64 = 1000;

/// 解析 Codex `/models` 响应：仅保留 `visibility=list`（或未标注）的模型。
fn parse_codex_models(body: &str) -> Result<Vec<ProviderModelInfo>, LlmError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|err| LlmError::Serialization(format!("解析 Codex 模型列表失败：{err}")))?;
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item.get("visibility")
                        .and_then(Value::as_str)
                        .is_none_or(|visibility| visibility == "list")
                })
                .filter_map(|item| {
                    let id = item.get("slug").and_then(Value::as_str)?.trim();
                    (!id.is_empty()).then(|| ProviderModelInfo {
                        id: id.to_string(),
                        display_name: item
                            .get("display_name")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        // 优先取可用上限 max_context_window（如 872k），缺省回落 context_window。
                        context_window: ["max_context_window", "context_window"]
                            .iter()
                            .find_map(|key| item.get(*key).and_then(Value::as_u64))
                            .filter(|window| *window > 0)
                            .map(|window| window as usize),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(models)
}

#[derive(Clone)]
pub struct ResponsesClient {
    config: OpenAiResponsesConfig,
}

impl ResponsesClient {
    pub fn new(config: OpenAiResponsesConfig) -> Self {
        Self { config }
    }

    pub async fn complete(&self, model: &str, payload: Value) -> Result<Value, LlmError> {
        use crate::providers::openai_chatcompletions::client::{parse_complete_body, send_request};
        let base = normalize_api_base(&self.config.base_url)
            .map_err(|error| LlmError::Configuration(error.to_string()))?;
        let url = format!("{base}/responses");
        timeout(
            self.config.timeout,
            self.with_retry("openai_complete", model, false, || async {
                let response = send_request(
                    &url,
                    &self.config.api_key,
                    &self.config.headers,
                    &payload,
                    self.config.timeout,
                    false,
                )
                .await?;
                parse_complete_body(&response.bytes().await?)
            }),
        )
        .await
        .map_err(|_| LlmError::Timeout(self.config.timeout.as_millis() as u64))?
    }

    pub async fn stream(
        &self,
        model: &str,
        payload: Value,
    ) -> Result<ResponsesStreamResponse, LlmError> {
        if !self.config.codex {
            return self
                .stream_with(
                    model,
                    payload,
                    self.config.api_key.clone(),
                    self.config.headers.clone(),
                )
                .await;
        }
        // Codex：鉴权来自 OAuth 登录态；401 时强制刷新令牌后重试一次。
        let (api_key, headers) = self.codex_auth(false).await?;
        match self
            .stream_with(model, payload.clone(), api_key, headers)
            .await
        {
            Err(LlmError::Authentication(message)) => {
                tracing::info!(error = %message, "Codex 请求鉴权失败，刷新登录后重试");
                let (api_key, headers) = self.codex_auth(true).await?;
                self.stream_with(model, payload, api_key, headers).await
            }
            other => other,
        }
    }

    /// Codex 模式下取访问令牌并补齐账号请求头。
    async fn codex_auth(
        &self,
        force_refresh: bool,
    ) -> Result<(String, reqwest::header::HeaderMap), LlmError> {
        let access = crate::codex_auth::access(force_refresh).await?;
        let mut headers = self.config.headers.clone();
        headers.insert(
            "originator",
            reqwest::header::HeaderValue::from_static(crate::codex_auth::CODEX_ORIGINATOR),
        );
        if let Some(account_id) = access.account_id.as_deref()
            && let Ok(mut value) = reqwest::header::HeaderValue::from_str(account_id)
        {
            value.set_sensitive(true);
            headers.insert("ChatGPT-Account-Id", value);
        }
        if let Some(residency) = access.residency.as_deref()
            && let Ok(value) = reqwest::header::HeaderValue::from_str(residency)
        {
            headers.insert("x-openai-internal-codex-residency", value);
        }
        Ok((access.access_token, headers))
    }

    async fn stream_with(
        &self,
        model: &str,
        payload: Value,
        api_key: String,
        headers: reqwest::header::HeaderMap,
    ) -> Result<ResponsesStreamResponse, LlmError> {
        let base = normalize_api_base(&self.config.base_url)
            .map_err(|err| LlmError::Configuration(err.to_string()))?;
        let url = format!("{base}/responses");
        let request_timeout = self.config.timeout;
        self.with_retry("openai_stream", model, true, move || {
            let url = url.clone();
            let api_key = api_key.clone();
            let payload = payload.clone();
            let request_timeout = request_timeout;
            let headers = headers.clone();
            async move {
                use crate::providers::openai_chatcompletions::client::{
                    StreamBody, resolve_stream_body,
                };
                let response = crate::providers::openai_chatcompletions::client::send_request(
                    &url,
                    &api_key,
                    &headers,
                    &payload,
                    request_timeout,
                    true,
                )
                .await?;
                match resolve_stream_body(response, request_timeout).await? {
                    StreamBody::Sse(stream) => Ok(ResponsesStreamResponse::Sse(stream)),
                    StreamBody::Complete(value) => {
                        // 服务端忽略 stream 参数返回一次性 JSON：SSE 解析器会把这些
                        // 行全部当未知字段丢弃且不报错，必须在 llm 层按完整响应接住。
                        tracing::info!(
                            operation = "openai_stream",
                            provider = "openai",
                            model,
                            "服务端未按 SSE 流式返回，转按一次性完整响应处理"
                        );
                        Ok(ResponsesStreamResponse::Complete(value))
                    }
                }
            }
        })
        .await
    }

    pub async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        if self.config.codex {
            return self.list_codex_models().await;
        }
        // Responses 与 Chat 共用 /models 端点，复用 Chat Completions 的实现。
        crate::providers::openai_chatcompletions::client::list_models_via_config(
            &self.config.api_key,
            &self.config.base_url,
            self.config.timeout,
            "openai",
            &self.config.headers,
        )
        .await
    }

    /// Codex 后端的模型目录：`GET /models?client_version=`，返回 `models[].slug`。
    async fn list_codex_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        let base = normalize_api_base(&self.config.base_url)
            .map_err(|err| LlmError::Configuration(err.to_string()))?;
        let url = format!(
            "{base}/models?client_version={}",
            crate::codex_auth::CODEX_MODELS_CLIENT_VERSION
        );
        let client = reqwest::Client::builder()
            .timeout(self.config.timeout)
            .build()
            .map_err(|err| LlmError::Transport(err.to_string()))?;
        let mut force_refresh = false;
        loop {
            let (api_key, headers) = self.codex_auth(force_refresh).await?;
            let response = client
                .get(&url)
                .headers(headers)
                .bearer_auth(api_key)
                .send()
                .await
                .map_err(|err| LlmError::Transport(format!("请求 Codex 模型列表失败：{err}")))?;
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && !force_refresh {
                force_refresh = true;
                continue;
            }
            let body = response
                .text()
                .await
                .map_err(|err| LlmError::Transport(err.to_string()))?;
            if !status.is_success() {
                let preview: String = body.chars().take(300).collect();
                return Err(if status == reqwest::StatusCode::UNAUTHORIZED {
                    LlmError::Authentication(format!("{status}: {preview}"))
                } else {
                    LlmError::Provider {
                        provider: "codex",
                        message: format!("获取模型列表失败 {status}: {preview}"),
                    }
                });
            }
            return parse_codex_models(&body);
        }
    }

    async fn with_retry<F, Fut, T>(
        &self,
        operation: &'static str,
        model: &str,
        stream: bool,
        mut f: F,
    ) -> Result<T, LlmError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, async_openai::error::OpenAIError>>,
    {
        let mut attempt = 0u32;
        let mut delay_ms = INITIAL_RETRY_DELAY_MS;
        let max_retries = self.config.max_retries;
        loop {
            let start = std::time::Instant::now();
            tracing::info!(
                operation,
                provider = "openai",
                model,
                stream,
                attempt,
                "开始 OpenAI Responses 请求"
            );
            match f().await {
                Ok(value) => {
                    tracing::info!(
                        operation,
                        provider = "openai",
                        model,
                        stream,
                        attempt,
                        latency_ms = start.elapsed().as_millis() as u64,
                        "OpenAI Responses 请求完成"
                    );
                    return Ok(value);
                }
                Err(err) if attempt < max_retries && is_retryable_responses_error(&err) => {
                    attempt += 1;
                    if let Some(notifier) = &self.config.retry_notifier {
                        notifier(attempt, max_retries, delay_ms, &err.to_string());
                    }
                    tracing::warn!(
                        operation,
                        provider = "openai",
                        model,
                        stream,
                        attempt,
                        delay_ms,
                        error = %err,
                        "OpenAI Responses 请求失败，准备重试"
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms *= 2;
                }
                Err(err) => {
                    tracing::warn!(
                        operation,
                        provider = "openai",
                        model,
                        stream,
                        attempt,
                        latency_ms = start.elapsed().as_millis() as u64,
                        error = %err,
                        "OpenAI Responses 请求失败"
                    );
                    return Err(map_responses_error(&err));
                }
            }
        }
    }
}

#[cfg(test)]
mod codex_models_tests {
    use super::parse_codex_models;

    #[test]
    fn parses_listed_models_with_context_window() {
        let body = r#"{"models":[
            {"slug":"gpt-5.6-sol","visibility":"list","context_window":272000,"max_context_window":872000},
            {"slug":"gpt-5.5","visibility":"list","context_window":272000},
            {"slug":"gpt-hidden","visibility":"hide","max_context_window":872000},
            {"slug":"gpt-x","visibility":"list"}
        ]}"#;
        let models = parse_codex_models(body).unwrap();
        let got: Vec<_> = models
            .iter()
            .map(|m| (m.id.as_str(), m.context_window))
            .collect();
        assert_eq!(
            got,
            vec![
                ("gpt-5.6-sol", Some(872_000)),
                ("gpt-5.5", Some(272_000)),
                ("gpt-x", None),
            ]
        );
    }
}
