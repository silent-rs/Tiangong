use std::str::FromStr;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

/// Provider 协议类型。
///
/// OpenAI 官方协议可使用 Responses API（`/responses`）；第三方 OpenAI 兼容端点
/// 默认使用 Chat Completions API（`/chat/completions`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderProtocol {
    /// OpenAI Responses API（`/responses`）。
    OpenAi,
    /// OpenAI Chat Completions API（`/chat/completions`）。
    #[default]
    OpenAiChatCompletions,
    Anthropic,
    DeepSeek,
    /// ChatGPT 账号（Codex 登录）鉴权的 Responses 后端：凭据来自 OAuth 登录，
    /// 不使用 provider 配置里的 api_key。
    Codex,
}

impl Serialize for ProviderProtocol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProviderProtocol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl ProviderProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderProtocol::OpenAi => "openai",
            ProviderProtocol::OpenAiChatCompletions => "openai_chatcompletions",
            ProviderProtocol::Anthropic => "anthropic",
            ProviderProtocol::DeepSeek => "deepseek",
            ProviderProtocol::Codex => "codex",
        }
    }

    /// 是否通过 OAuth 登录态鉴权（无需配置 api_key）。
    pub fn uses_oauth(&self) -> bool {
        matches!(self, ProviderProtocol::Codex)
    }
}

impl FromStr for ProviderProtocol {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "" => Ok(ProviderProtocol::OpenAiChatCompletions),
            "openai_compatible"
            | "open_ai_compatible"
            | "openai_chatcompletions"
            | "openai_chat_completions"
            | "openai_chat"
            | "openai-chat"
            | "chat_completions"
            | "chat-completions" => Ok(ProviderProtocol::OpenAiChatCompletions),
            "openai" | "openai_responses" | "openai-responses" | "responses" => {
                Ok(ProviderProtocol::OpenAi)
            }
            "anthropic" => Ok(ProviderProtocol::Anthropic),
            "deepseek" | "deep_seek" => Ok(ProviderProtocol::DeepSeek),
            "codex" | "chatgpt" | "chatgpt_codex" | "openai_codex" => Ok(ProviderProtocol::Codex),
            other => Err(anyhow!("不支持的 provider 协议：{other}")),
        }
    }
}

/// Provider 暴露的模型信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    /// 服务端声明的上下文窗口（token 数）；未提供时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::ProviderProtocol;

    #[test]
    fn parses_legacy_openai_protocols_as_chat_completions() {
        for protocol in ["", "openai_compatible"] {
            assert_eq!(
                protocol.parse::<ProviderProtocol>().unwrap(),
                ProviderProtocol::OpenAiChatCompletions,
                "{protocol:?} 应兼容解析为 Chat Completions 协议"
            );
        }
    }

    #[test]
    fn parses_openai_responses_protocols() {
        for protocol in ["openai", "openai_responses", "responses"] {
            assert_eq!(
                protocol.parse::<ProviderProtocol>().unwrap(),
                ProviderProtocol::OpenAi,
                "{protocol:?} 应解析为 Responses 协议"
            );
        }
    }
}
