use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use tiangong_plugin_mcp_protocol::config::ResolvedMcpTransport;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenStatsView {
    pub current_tokens: usize,
    pub compression_threshold_tokens: usize,
    pub context_limit_tokens: usize,
    pub total_prompt_tokens: usize,
    pub total_completion_tokens: usize,
    pub total_tokens: usize,
    pub active_agent_current_tokens: usize,
    pub active_agent_id: Option<String>,
    pub agent_current_tokens: HashMap<String, usize>,
    pub agent_token_usage: HashMap<String, tiangong_types::TokenUsage>,
}

impl TokenStatsView {
    pub fn from_session(
        core_session: &tiangong_core::session::Session,
        context_limit_tokens: usize,
    ) -> Self {
        let usage = core_session.total_usage();
        Self {
            current_tokens: core_session.current_tokens,
            compression_threshold_tokens: tiangong_core::context::organizer::ContextOrganizer::new(
                context_limit_tokens,
            )
            .token_threshold(),
            context_limit_tokens,
            total_prompt_tokens: usage.prompt_tokens,
            total_completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            active_agent_current_tokens: core_session.active_agent_current_tokens,
            active_agent_id: core_session.active_agent_id.clone(),
            agent_current_tokens: core_session.agent_current_tokens.clone(),
            agent_token_usage: core_session.agent_token_usage.clone(),
        }
    }
}

/// 首次打开会话时从磁盘加载的界面数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedSessionView {
    pub id: String,
    pub messages: Vec<tiangong_types::Message>,
    pub token_stats: TokenStatsView,
    pub last_duration_ms: Option<u64>,
    pub last_usage: Option<tiangong_types::TokenUsage>,
    pub cwd: String,
    pub reasoning_effort: String,
}

impl LoadedSessionView {
    pub fn from_session(
        session: &tiangong_core::session::Session,
        context_limit_tokens: usize,
        default_reasoning_effort: &tiangong_llm::request::ReasoningEffort,
    ) -> Self {
        let usage = session.total_usage();
        Self {
            id: session.id.clone(),
            messages: session.messages.clone(),
            token_stats: TokenStatsView::from_session(session, context_limit_tokens),
            last_duration_ms: session
                .messages
                .iter()
                .rev()
                .find_map(|message| message.elapsed_ms),
            last_usage: (usage.total_tokens > 0).then_some(usage),
            cwd: session.cwd.clone(),
            reasoning_effort: session
                .reasoning_effort
                .unwrap_or(*default_reasoning_effort)
                .as_str()
                .to_string(),
        }
    }
}

/// @提及候选项（统一使用 tiangong_types::MentionCandidate，避免重复定义）。
pub use tiangong_types::MentionCandidate;
/// @提及候选分组（统一使用 tiangong_types::MentionGroup，避免重复定义）。
pub use tiangong_types::MentionGroup;

/// 会话列表项（前端使用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListItem {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: usize,
    /// 会话工作目录，前端用于按 workspace 分组展示
    pub cwd: String,
}

impl SessionListItem {
    pub fn from_core(core_session: &tiangong_core::session::Session) -> Self {
        Self {
            id: core_session.id.clone(),
            title: core_session.title.clone(),
            created_at: core_session.created_at.clone(),
            updated_at: core_session.updated_at.clone(),
            message_count: core_session.messages.len(),
            cwd: core_session.cwd.clone(),
        }
    }

    /// 从 SessionMetadata 构造（issue #245）：UI 列表展示走元数据缓存，
    /// 不再依赖完整 Session。
    pub fn from_metadata(metadata: &tiangong_core_manager::SessionMetadata) -> Self {
        Self {
            id: metadata.id.clone(),
            title: metadata.title.clone(),
            created_at: metadata.created_at.clone(),
            updated_at: metadata.updated_at.clone(),
            message_count: metadata.message_count,
            cwd: metadata.cwd.clone(),
        }
    }
}

/// MCP 服务器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerView {
    pub name: String,
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    pub endpoint: String,
    pub auth_header: String,
    pub headers: Option<BTreeMap<String, String>>,
    pub env: Option<HashMap<String, String>>,
    pub enabled: bool,
}

impl McpServerView {
    pub fn from_core(core_server: &tiangong_plugin_mcp_protocol::config::McpServerConfig) -> Self {
        let transport = match core_server.resolved_transport() {
            ResolvedMcpTransport::Stdio => "stdio",
            ResolvedMcpTransport::Http => "http",
            ResolvedMcpTransport::Metadata => "auto",
        }
        .to_string();

        Self {
            name: core_server.name.clone(),
            transport,
            command: core_server.command.clone(),
            args: core_server.args.clone(),
            endpoint: core_server.endpoint.clone(),
            auth_header: core_server.auth_header.clone(),
            headers: if core_server.headers.is_empty() {
                None
            } else {
                Some(core_server.headers.clone())
            },
            env: if core_server.env.is_empty() {
                None
            } else {
                Some(core_server.env.clone().into_iter().collect())
            },
            enabled: core_server.enabled,
        }
    }
}

/// Skill 安装前检查结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInspection {
    pub env_vars: Vec<String>,
    pub missing_env_vars: Vec<String>,
    pub dependencies: Vec<String>,
}

/// Server 配置（前端使用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfigView {
    pub host: String,
    pub port: u16,
    pub auth_token_masked: String,
    /// 用户保存的持续开启意图。
    pub enabled: bool,
    /// 实时健康检查是否正常。
    pub running: bool,
    /// stopped / running / error。
    pub status: String,
}

/// 模型能力（前端使用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilityInfo {
    pub key: String,
    pub display_name: String,
}

/// 能力可用性状态（基于当前配置快速检测）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityAvailabilityInfo {
    pub key: String,
    pub display_name: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routed_model: Option<String>,
}

/// Provider 连接配置（前端使用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfigView {
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub base_url: String,
    pub api_key: String,
    pub timeout_ms: u64,
    pub protocol: String,
}

/// 单个模型配置（前端使用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntryView {
    pub provider: String,
    pub model: String,
    pub capabilities: Vec<String>,
    pub options: serde_json::Value,
    /// 模型上下文窗口（仅 Chat/Multimodal 适用，None 表示用映射默认）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
}

/// 模型配置（前端使用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsConfigView {
    pub providers: HashMap<String, ProviderConfigView>,
    pub models: HashMap<String, ModelEntryView>,
    pub routing: HashMap<String, ModelEntryView>,
}

impl ModelsConfigView {
    pub fn from_core(config: &tiangong_llm::models_config::ModelsConfig) -> Self {
        let providers = config
            .providers
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    ProviderConfigView {
                        headers: v.headers.clone(),
                        base_url: v.base_url.clone(),
                        api_key: v.api_key.clone(),
                        timeout_ms: v.timeout_ms,
                        protocol: v.protocol.as_str().to_string(),
                    },
                )
            })
            .collect();

        let models = config
            .models
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    ModelEntryView {
                        provider: v.provider.clone(),
                        model: v.model.clone(),
                        capabilities: v
                            .capabilities
                            .iter()
                            .map(|c| serde_json::to_value(c).unwrap_or_default())
                            .map(|v| v.as_str().unwrap_or_default().to_string())
                            .collect(),
                        options: v.options.clone(),
                        context_window: v.context_window,
                    },
                )
            })
            .collect();

        let routing = config
            .routing
            .iter()
            .map(|(k, v)| {
                let key = serde_json::to_value(k).unwrap_or_default();
                (
                    key.as_str().unwrap_or_default().to_string(),
                    ModelEntryView {
                        provider: v.provider.clone(),
                        model: v.model.clone(),
                        capabilities: v
                            .capabilities
                            .iter()
                            .map(|c| serde_json::to_value(c).unwrap_or_default())
                            .map(|v| v.as_str().unwrap_or_default().to_string())
                            .collect(),
                        options: v.options.clone(),
                        context_window: v.context_window,
                    },
                )
            })
            .collect();

        Self {
            providers,
            models,
            routing,
        }
    }

    pub fn to_core(&self) -> tiangong_llm::models_config::ModelsConfig {
        use tiangong_llm::models_config::{
            ModelCapability, ModelEntry, ModelsConfig, ProviderConfig, RoutingSlot,
        };

        let providers = self
            .providers
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    ProviderConfig {
                        headers: v.headers.clone(),
                        base_url: v.base_url.clone(),
                        api_key: v.api_key.clone(),
                        timeout_ms: v.timeout_ms,
                        protocol: v.protocol.parse().unwrap_or_default(),
                    },
                )
            })
            .collect();

        let models = self
            .models
            .iter()
            .map(|(k, v)| {
                let capabilities: Vec<ModelCapability> = v
                    .capabilities
                    .iter()
                    .filter_map(|c| {
                        let json_str = format!("\"{}\"", c);
                        serde_json::from_str(&json_str).ok()
                    })
                    .collect();
                (
                    k.clone(),
                    ModelEntry {
                        provider: v.provider.clone(),
                        model: v.model.clone(),
                        capabilities,
                        options: v.options.clone(),
                        context_window: v.context_window,
                    },
                )
            })
            .collect();

        let routing = self
            .routing
            .iter()
            .filter_map(|(k, v)| {
                let json_str = format!("\"{}\"", k);
                let slot: RoutingSlot = serde_json::from_str(&json_str).ok()?;
                let capabilities: Vec<ModelCapability> = v
                    .capabilities
                    .iter()
                    .filter_map(|c| {
                        let json_str = format!("\"{}\"", c);
                        serde_json::from_str(&json_str).ok()
                    })
                    .collect();
                Some((
                    slot,
                    ModelEntry {
                        provider: v.provider.clone(),
                        model: v.model.clone(),
                        capabilities,
                        options: v.options.clone(),
                        context_window: v.context_window,
                    },
                ))
            })
            .collect();

        ModelsConfig {
            providers,
            models,
            routing,
        }
    }
}
