use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use super::auth::{CODEX_MODELS_CLIENT_VERSION, CODEX_ORIGINATOR};
use crate::error::LlmError;
use crate::model::ProviderModelInfo;
use crate::providers::openai::mapping::normalize_api_base;

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

    /// 取访问令牌并补齐 Codex 请求头（originator / 账号 ID / 数据驻留 / UA）。
    async fn auth(&self, force_refresh: bool) -> Result<(String, HeaderMap), LlmError> {
        let access = super::auth::access(force_refresh).await?;
        let mut headers = self.config.headers.clone();
        headers.insert("originator", HeaderValue::from_static(CODEX_ORIGINATOR));
        // 真实客户端（Codex CLI、opencode）都会自报 UA；缺 UA 是非常规形态的信号。
        if !headers.contains_key(reqwest::header::USER_AGENT)
            && let Ok(value) = HeaderValue::from_str(crate::headers::TIANGONG_USER_AGENT)
        {
            headers.insert(reqwest::header::USER_AGENT, value);
        }
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

    /// 组装 WS 传输的请求上下文：`(url, 头列表)`。
    ///
    /// 与 HTTP 路径同一套鉴权与身份头（`auth()` 结果 + 调用方注入的 session
    /// 亲和头），Authorization 随 WS 握手发送；URL 为 `/responses` 的 wss 形态。
    pub(super) async fn ws_request_context(
        &self,
        session_headers: HeaderMap,
    ) -> Result<(String, Vec<(String, String)>), LlmError> {
        let (token, mut headers) = self.auth(false).await?;
        for (name, value) in session_headers.iter() {
            headers.insert(name, value.clone());
        }
        let mut pairs: Vec<(String, String)> = headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_string(), v.to_string()))
            })
            .collect();
        pairs.push(("Authorization".to_string(), format!("Bearer {token}")));
        let base = self.config.base_url.trim_end_matches('/');
        let url = format!("{base}/responses").replacen("https://", "wss://", 1);
        Ok((url, pairs))
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
            let request = client.get(&url).headers(headers).bearer_auth(token);
            let direct = || {
                reqwest::Client::builder()
                    .timeout(self.config.timeout)
                    .no_proxy()
                    .build()
            };
            let response = crate::http::send_with_direct_retry(request, direct)
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

/// 为 `/responses` 请求注入会话亲和头。
///
/// ChatGPT Codex 后端按 session 头路由 prompt cache：opencode 以 `session-id`
/// 头获得缓存路由（源码注释 "ChatGPT routes its prompt cache on this
/// header"），sub2api / Codex CLI 使用下划线形态 `session_id`。两种形态同值
/// 注入，值取本会话 id——append-only 会话内全程稳定，是块级前缀缓存命中的
/// 路由键。仅流式请求路径调用，模型目录等低频接口不需要。
pub(super) fn with_session_headers(mut headers: HeaderMap, session_id: Option<&str>) -> HeaderMap {
    if let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty())
        && let Ok(value) = HeaderValue::from_str(session_id)
    {
        headers.insert(HeaderName::from_static("session-id"), value.clone());
        headers.insert(HeaderName::from_static("session_id"), value);
    }
    headers
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
    use super::{HeaderMap, parse_models, with_session_headers};

    #[test]
    fn session_headers_injected_in_both_forms() {
        let headers = with_session_headers(HeaderMap::new(), Some("03h605565vhaxu3a591ictc93"));
        assert_eq!(
            headers.get("session-id").and_then(|v| v.to_str().ok()),
            Some("03h605565vhaxu3a591ictc93")
        );
        assert_eq!(
            headers.get("session_id").and_then(|v| v.to_str().ok()),
            Some("03h605565vhaxu3a591ictc93")
        );
    }

    #[test]
    fn session_headers_skipped_for_missing_or_blank() {
        for session_id in [None, Some(""), Some("   ")] {
            let headers = with_session_headers(HeaderMap::new(), session_id);
            assert!(headers.get("session-id").is_none());
            assert!(headers.get("session_id").is_none());
        }
    }

    #[test]
    fn session_headers_keep_existing_headers() {
        let mut base = HeaderMap::new();
        base.insert(
            reqwest::header::HeaderName::from_static("originator"),
            reqwest::header::HeaderValue::from_static("tiangong"),
        );
        let headers = with_session_headers(base, Some("sid-1"));
        assert_eq!(
            headers.get("originator"),
            Some(&"tiangong".parse().unwrap())
        );
        assert_eq!(headers.get("session-id"), Some(&"sid-1".parse().unwrap()));
    }

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
