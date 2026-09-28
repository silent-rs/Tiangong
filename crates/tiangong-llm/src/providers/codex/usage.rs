//! ChatGPT 账号的 Codex 用量额度查询（`GET {CODEX_BASE_URL}/usage`）。
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::LlmError;

use super::auth::{CODEX_BASE_URL, CODEX_ORIGINATOR, access, http_client, non_empty_str, now_unix};

/// 单个限流窗口的用量（如 5 小时 / 每周额度）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexUsageWindow {
    /// 已用百分比（0-100）。
    pub used_percent: f64,
    /// 窗口时长（秒），如 18000=5 小时、604800=7 天。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_seconds: Option<u64>,
    /// 额度重置时间（Unix 秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
}

/// 具名额度（如代码审查的独立额度）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexNamedLimit {
    pub name: String,
    #[serde(default)]
    pub windows: Vec<CodexUsageWindow>,
    pub limit_reached: bool,
}

/// ChatGPT 账号的 Codex 用量额度（不含邮箱、账号 ID 等个人信息）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodexUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    /// 当前是否允许继续请求。
    pub allowed: bool,
    /// 是否已触达限额。
    pub limit_reached: bool,
    /// 主额度窗口（primary / secondary，按服务端返回顺序）。
    #[serde(default)]
    pub windows: Vec<CodexUsageWindow>,
    /// 其他额度（代码审查及 additional_rate_limits）。
    #[serde(default)]
    pub extra_limits: Vec<CodexNamedLimit>,
    /// 按量点数余额（字符串原样透传，如 "12.50"）；无点数时为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits_balance: Option<String>,
    /// 点数是否不限量。
    pub credits_unlimited: bool,
}

fn parse_usage_window(value: &Value) -> Option<CodexUsageWindow> {
    let used_percent = value.get("used_percent")?.as_f64()?;
    let now = now_unix();
    Some(CodexUsageWindow {
        used_percent: used_percent.clamp(0.0, 100.0),
        window_seconds: value.get("limit_window_seconds").and_then(Value::as_u64),
        reset_at: value.get("reset_at").and_then(Value::as_i64).or_else(|| {
            value
                .get("reset_after_seconds")
                .and_then(Value::as_i64)
                .map(|secs| now + secs)
        }),
    })
}

fn parse_rate_limit(value: &Value) -> (Vec<CodexUsageWindow>, bool, bool) {
    let windows = ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(parse_usage_window))
        .collect();
    let limit_reached = value
        .get("limit_reached")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let allowed = value
        .get("allowed")
        .and_then(Value::as_bool)
        .unwrap_or(!limit_reached);
    (windows, allowed, limit_reached)
}

/// 解析 `GET {CODEX_BASE_URL}/usage` 响应，只保留额度相关字段。
fn parse_usage(body: &Value) -> CodexUsage {
    let (windows, allowed, limit_reached) = body
        .get("rate_limit")
        .filter(|v| v.is_object())
        .map(parse_rate_limit)
        .unwrap_or((Vec::new(), true, false));
    let mut extra_limits = Vec::new();
    if let Some(review) = body.get("code_review_rate_limit").filter(|v| v.is_object()) {
        let (windows, _, reached) = parse_rate_limit(review);
        extra_limits.push(CodexNamedLimit {
            name: "代码审查".to_string(),
            windows,
            limit_reached: reached,
        });
    }
    // additional_rate_limits 可能是数组或以名称为键的对象，两种都兼容。
    let additional: Vec<(String, &Value)> = match body.get("additional_rate_limits") {
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let name = ["limit_name", "name", "metered_feature"]
                    .iter()
                    .find_map(|k| non_empty_str(item.get(*k)))
                    .unwrap_or_else(|| format!("额度 {}", i + 1));
                (name, item.get("rate_limit").unwrap_or(item))
            })
            .collect(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(name, item)| (name.clone(), item.get("rate_limit").unwrap_or(item)))
            .collect(),
        _ => Vec::new(),
    };
    for (name, limit) in additional {
        let (windows, _, reached) = parse_rate_limit(limit);
        if !windows.is_empty() || reached {
            extra_limits.push(CodexNamedLimit {
                name,
                windows,
                limit_reached: reached,
            });
        }
    }
    let credits = body.get("credits");
    let has_credits = credits
        .and_then(|c| c.get("has_credits"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    CodexUsage {
        plan_type: non_empty_str(body.get("plan_type")),
        allowed,
        limit_reached,
        windows,
        extra_limits,
        credits_balance: has_credits
            .then(|| non_empty_str(credits.and_then(|c| c.get("balance"))))
            .flatten(),
        credits_unlimited: credits
            .and_then(|c| c.get("unlimited"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// 查询 ChatGPT 账号的 Codex 用量额度（只读 GET，不消耗额度）。
///
/// 供宿主进程（模型管理 / CLI）调用：沿用 [`access`] 的自动续期，
/// 遇 401 强制刷新后重试一次。
pub async fn usage() -> Result<CodexUsage, LlmError> {
    let client = http_client()?;
    let url = format!("{CODEX_BASE_URL}/usage");
    let mut force_refresh = false;
    loop {
        let access = access(force_refresh).await?;
        let mut request = client
            .get(&url)
            .bearer_auth(&access.access_token)
            .header("originator", CODEX_ORIGINATOR);
        if let Some(account_id) = access.account_id.as_deref() {
            request = request.header("ChatGPT-Account-Id", account_id);
        }
        if let Some(residency) = access.residency.as_deref() {
            request = request.header("x-openai-internal-codex-residency", residency);
        }
        let response = request
            .send()
            .await
            .map_err(|err| LlmError::Transport(format!("查询 ChatGPT 用量失败：{err}")))?;
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
                    message: format!("查询用量失败 {status}: {preview}"),
                }
            });
        }
        let value: Value = serde_json::from_str(&body)
            .map_err(|err| LlmError::Serialization(format!("解析 ChatGPT 用量失败：{err}")))?;
        return Ok(parse_usage(&value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_usage_extracts_windows_without_personal_fields() {
        let body = serde_json::json!({
            "user_id": "u", "account_id": "a", "email": "x@y.z",
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true, "limit_reached": false,
                "primary_window": {"used_percent": 12.5, "limit_window_seconds": 18000, "reset_at": 1_791_000_000},
                "secondary_window": {"used_percent": 150, "limit_window_seconds": 604800, "reset_after_seconds": 60}
            },
            "code_review_rate_limit": {"limit_reached": true, "primary_window": {"used_percent": 100}},
            "additional_rate_limits": [
                {"limit_name": "gpt-6-astra", "rate_limit": {"primary_window": {"used_percent": 3}}}
            ],
            "credits": {"has_credits": true, "unlimited": false, "balance": "12.50"}
        });
        let usage = parse_usage(&body);
        assert_eq!(usage.plan_type.as_deref(), Some("pro"));
        assert!(usage.allowed && !usage.limit_reached);
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].used_percent, 12.5);
        assert_eq!(usage.windows[0].window_seconds, Some(18000));
        assert_eq!(usage.windows[0].reset_at, Some(1_791_000_000));
        // 超过 100 夹紧；只有 reset_after_seconds 时换算为绝对时间
        assert_eq!(usage.windows[1].used_percent, 100.0);
        assert!(usage.windows[1].reset_at.unwrap() >= now_unix() + 59);
        assert_eq!(usage.extra_limits.len(), 2);
        assert_eq!(usage.extra_limits[0].name, "代码审查");
        assert!(usage.extra_limits[0].limit_reached);
        assert_eq!(usage.extra_limits[1].name, "gpt-6-astra");
        assert_eq!(usage.credits_balance.as_deref(), Some("12.50"));
        let json = serde_json::to_string(&usage).unwrap();
        assert!(!json.contains("x@y.z") && !json.contains("account_id"));
    }

    #[test]
    fn parse_usage_tolerates_nulls() {
        let body = serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {"allowed": true, "limit_reached": false,
                "primary_window": {"used_percent": 1, "limit_window_seconds": 604800, "reset_at": 1},
                "secondary_window": null},
            "code_review_rate_limit": null,
            "additional_rate_limits": null,
            "credits": {"has_credits": false, "unlimited": false, "balance": "0E-10"}
        });
        let usage = parse_usage(&body);
        assert_eq!(usage.windows.len(), 1);
        assert!(usage.extra_limits.is_empty());
        assert_eq!(usage.credits_balance, None);
    }
}
