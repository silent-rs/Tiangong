//! Agent 运行时配置。
//!
//! AgentConfig 仅保留 agent runtime 配置；扩展能力配置由各插件自管。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    /// 当前权限信任模式（可按当前会话调整）
    #[serde(default)]
    pub trust_mode: crate::permission::TrustMode,
    /// 新对话默认权限信任模式
    #[serde(default)]
    pub default_trust_mode: crate::permission::TrustMode,
    /// 思考强度设置：None 关闭思考，默认 Medium
    #[serde(default = "default_reasoning_effort")]
    #[serde(deserialize_with = "tiangong_llm::request::deserialize_reasoning_effort_flexible")]
    pub reasoning_effort: tiangong_llm::ReasoningEffort,
}

fn default_reasoning_effort() -> tiangong_llm::ReasoningEffort {
    tiangong_llm::ReasoningEffort::Medium
}
