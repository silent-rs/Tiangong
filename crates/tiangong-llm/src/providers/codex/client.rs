use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;

use super::auth::{CODEX_MODELS_CLIENT_VERSION, CODEX_ORIGINATOR};
use crate::error::LlmError;
use crate::model::ProviderModelInfo;
use crate::providers::openai::mapping::normalize_api_base;
use crate::providers::openai::{ResponsesClient, ResponsesStreamResponse};

use super::config::CodexConfig;

/// Codex 后端客户端：注入 OAuth 令牌与账号请求头，复用 Responses 的发送与重试。
#[derive(Clone)]
pub(super) struct CodexClient {
    config: CodexConfig,
}

impl CodexClient {
    pub(super) fn new(config: CodexConfig) -> Self {
        Self { config }
    }

    /// 取访问令牌并补齐 Codex 请求头（originator / 账号 ID / 数据驻留）。
    async fn auth(&self, force_refresh: bool) -> Result<(String, HeaderMap), LlmError> {
        let access = super::auth::access(force_refresh).await?;
        let mut headers = self.config.headers.clone();
        headers.insert("originator", HeaderValue::from_static(CODEX_ORIGINATOR));
        if let Some(account_id) = access.account_id.as_deref()
            && let Ok(mut value) = HeaderValue::from_str(account_id)
        {
            value.set_sensitive(true);
            headers.insert("ChatGPT-Account-Id", value);
        }
        if let Some(residency) = access.residency.as_deref()
            && let Ok(value) = HeaderValue::from_str(residency)
        {
            headers.insert("x-openai-internal-codex-residency", value);
        }
        Ok((access.access_token, headers))
    }

    /// 发起流式请求；鉴权失败（401）时强制续期后重试一次。
    pub(super) async fn stream(
        &self,
        model: &str,
        payload: Value,
    ) -> Result<ResponsesStreamResponse, LlmError> {
        let (token, headers) = self.auth(false).await?;
        let client = ResponsesClient::new(self.config.responses_config(token, headers));
        match client.stream(model, payload.clone()).await {
            Err(LlmError::Authentication(message)) => {
                tracing::info!(error = %message, "Codex 请求鉴权失败，刷新登录后重试");
                let (token, headers) = self.auth(true).await?;
                ResponsesClient::new(self.config.responses_config(token, headers))
                    .stream(model, payload)
                    .await
            }
            other => other,
        }
    }

    /// 模型目录：`GET /models?client_version=`，返回 `models[].slug`。
    pub(super) async fn list_models(&self) -> Result<Vec<ProviderModelInfo>, LlmError> {
        let base = normalize_api_base(&self.config.base_url)
            .map_err(|err| LlmError::Configuration(err.to_string()))?;
        let url = format!("{base}/models?client_version={CODEX_MODELS_CLIENT_VERSION}");
        let client = reqwest::Client::builder()
            .timeout(self.config.timeout)
            .build()
            .map_err(|err| LlmError::Transport(err.to_string()))?;
        let mut force_refresh = false;
        loop {
            let (token, headers) = self.auth(force_refresh).await?;
            let response = client
                .get(&url)
                .headers(headers)
                .bearer_auth(token)
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
            return parse_models(&body);
        }
    }
}

/// 解析 Codex `/models` 响应：仅保留 `visibility=list`（或未标注）的模型，按服务端顺序去重。
pub(super) fn parse_models(body: &str) -> Result<Vec<ProviderModelInfo>, LlmError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|err| LlmError::Serialization(format!("解析 Codex 模型列表失败：{err}")))?;
    let mut seen = std::collections::HashSet::new();
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
                    (!id.is_empty() && seen.insert(id.to_string())).then(|| ProviderModelInfo {
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

#[cfg(test)]
mod tests {
    use super::parse_models;

    #[test]
    fn parses_listed_models_with_context_window() {
        let body = r#"{"models":[
            {"slug":"gpt-5.6-sol","visibility":"list","context_window":272000,"max_context_window":872000},
            {"slug":"gpt-5.5","visibility":"list","context_window":272000},
            {"slug":"gpt-hidden","visibility":"hide","max_context_window":872000},
            {"slug":"gpt-5.5","visibility":"list"},
            {"slug":"gpt-x","visibility":"list"}
        ]}"#;
        let models = parse_models(body).unwrap();
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
