use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

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
///
/// 长会话分段加载：`messages` 只含最近一段（从 `start` 开始），更早的消息由
/// `load_session_messages` 按需向前分页；`user_outline` 始终包含完整的用户提问
/// 目录，供刻度条、回合跳转使用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedSessionView {
    pub id: String,
    pub messages: Vec<tiangong_types::Message>,
    /// `messages` 第一条在完整消息序列中的下标；0 表示已是全部历史。
    #[serde(default)]
    pub start: usize,
    /// 完整消息条数。
    #[serde(default)]
    pub total: usize,
    /// 完整的用户提问目录（按时间顺序）。
    #[serde(default)]
    pub user_outline: Vec<UserOutlineItem>,
    pub token_stats: TokenStatsView,
    pub last_duration_ms: Option<u64>,
    pub last_usage: Option<tiangong_types::TokenUsage>,
    pub cwd: String,
    pub reasoning_effort: String,
}

/// 用户提问目录项：刻度条与回合跳转所需的最小信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserOutlineItem {
    /// 用户消息 id（与 `messages` 中的消息一一对应）。
    pub id: String,
    /// 提问预览（截断）。
    pub question: String,
    /// 该轮回复预览（截断）。
    pub answer: String,
}

/// 向前分页加载的一段历史消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessagesPage {
    pub messages: Vec<tiangong_types::Message>,
    /// 本段第一条在完整消息序列中的下标；0 表示已到最早。
    pub start: usize,
    pub total: usize,
}

/// 每段至少包含的消息条数。
pub const SESSION_PAGE_MIN_MESSAGES: usize = 200;
/// 每段序列化后的大致字节上限（超过后在下一个轮次边界截断，至少保留一轮）。
pub const SESSION_PAGE_MAX_BYTES: usize = 2 * 1024 * 1024;
/// 目录项预览的最大字符数。
const OUTLINE_QUESTION_CHARS: usize = 160;
const OUTLINE_ANSWER_CHARS: usize = 360;

/// 是否为轮次锚点（真正的用户提问）：与前端分组规则一致——来源为真人输入的
/// 用户消息；宿主注入、压缩续写、Agent 协作等非真人来源不作为锚点。
pub fn is_turn_anchor(message: &tiangong_types::Message) -> bool {
    message.is_user_input()
}

/// 粗略估计单条消息序列化后的字节数（用于分段上限，不要求精确）。
fn approx_message_bytes(message: &tiangong_types::Message) -> usize {
    let content: usize = message
        .content
        .iter()
        .map(|block| match block.as_text() {
            Some(text) => text.len(),
            None => 256,
        })
        .sum();
    let tool_calls: usize = message
        .tool_calls()
        .iter()
        .map(|call| call.name.len() + call.arguments.to_string().len())
        .sum();
    content + message.reasoning_content().len() + tool_calls + 256
}

/// 计算 `[.., end)` 这一段的起点：从 `end` 向前收集，至少 `min_messages` 条，
/// 累计超过 `max_bytes` 后停止；起点总落在轮次锚点上（不拆开一轮），
/// 找不到锚点时一直退到 0。
pub fn page_start(
    messages: &[tiangong_types::Message],
    end: usize,
    min_messages: usize,
    max_bytes: usize,
) -> usize {
    let end = end.min(messages.len());
    let mut bytes = 0usize;
    let mut index = end;
    while index > 0 {
        index -= 1;
        bytes += approx_message_bytes(&messages[index]);
        let enough = end - index >= min_messages || bytes >= max_bytes;
        if enough && is_turn_anchor(&messages[index]) {
            return index;
        }
    }
    0
}

fn truncate_chars(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    match trimmed.char_indices().nth(max) {
        Some((cut, _)) => trimmed[..cut].to_string(),
        None => trimmed.to_string(),
    }
}

/// 完整的用户提问目录：每条用户提问及其后第一段非空文本作为回复预览
/// （与前端刻度条预览卡的取值规则一致）。
pub fn user_outline(messages: &[tiangong_types::Message]) -> Vec<UserOutlineItem> {
    let mut outline: Vec<UserOutlineItem> = Vec::new();
    for message in messages {
        if message.user_source() == Some(tiangong_types::UserSource::CompressedResume) {
            continue;
        }
        if is_turn_anchor(message) {
            outline.push(UserOutlineItem {
                id: message.id.clone(),
                question: truncate_chars(&message.text_content(), OUTLINE_QUESTION_CHARS),
                answer: String::new(),
            });
            continue;
        }
        if let Some(last) = outline.last_mut() {
            if last.answer.is_empty() {
                let text = message.text_content();
                if !text.trim().is_empty() {
                    last.answer = truncate_chars(&text, OUTLINE_ANSWER_CHARS);
                }
            }
        }
    }
    outline
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
            start: 0,
            total: session.messages.len(),
            user_outline: user_outline(&session.messages),
            token_stats: TokenStatsView::from_session(session, context_limit_tokens),
            last_duration_ms: session
                .messages
                .iter()
                .rev()
                .find_map(|message| message.elapsed_ms()),
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

#[cfg(test)]
mod paging_tests {
    use super::*;
    use tiangong_types::{Message, MessageRole, UserSource};

    /// 构造 `turns` 轮对话：每轮 1 条用户提问 + `replies` 条助手/工具消息。
    fn conversation(turns: usize, replies: usize) -> Vec<Message> {
        let mut messages = Vec::new();
        for turn in 0..turns {
            messages.push(Message::new(MessageRole::User, format!("问题 {turn}")));
            for reply in 0..replies {
                if reply % 2 == 0 {
                    messages.push(Message::new(
                        MessageRole::Assistant,
                        format!("回答 {turn}-{reply}"),
                    ));
                } else {
                    messages.push(Message::tool_result("call", "tool", "结果", false));
                }
            }
        }
        messages
    }

    #[test]
    fn page_starts_on_turn_boundary() {
        // 30 轮 × 10 条 = 300 条：至少 25 条时应落在轮次起点。
        let messages = conversation(30, 9);
        let start = page_start(&messages, messages.len(), 25, usize::MAX);
        assert!(is_turn_anchor(&messages[start]));
        assert_eq!(messages.len() - start, 30, "25 条向上取整到整轮（3 轮）");

        // 继续向前翻页直到开头，各段首尾相接、不重不漏。
        let mut end = start;
        let mut pages = 1;
        while end > 0 {
            let next = page_start(&messages, end, 25, usize::MAX);
            assert!(next < end);
            assert!(is_turn_anchor(&messages[next]));
            end = next;
            pages += 1;
        }
        assert_eq!(pages, 10);
    }

    #[test]
    fn page_respects_byte_limit_but_keeps_a_whole_turn() {
        let messages = conversation(10, 9);
        // 字节上限极小：仍至少保留最后一整轮。
        let start = page_start(&messages, messages.len(), usize::MAX, 1);
        assert_eq!(messages.len() - start, 10);
        // 会话短于一段时直接从头开始。
        assert_eq!(page_start(&messages, messages.len(), 1000, usize::MAX), 0);
    }

    #[test]
    fn page_never_splits_inside_a_turn_without_anchor() {
        // 没有任何用户提问（例如只有系统通知）时退到开头。
        let messages: Vec<Message> = (0..50)
            .map(|index| Message::new(MessageRole::Assistant, format!("{index}")))
            .collect();
        assert_eq!(page_start(&messages, 50, 10, usize::MAX), 0);
    }

    #[test]
    fn synthetic_user_messages_are_not_anchors() {
        let injected =
            Message::new(MessageRole::User, "宿主注入").with_source(UserSource::HostInjected);
        let resume =
            Message::new(MessageRole::User, "压缩续写").with_source(UserSource::CompressedResume);
        let agent = Message::new(MessageRole::User, "agent").with_source(UserSource::Agent);
        assert!(!is_turn_anchor(&injected));
        assert!(!is_turn_anchor(&resume));
        assert!(!is_turn_anchor(&agent));
        assert!(is_turn_anchor(&Message::new(MessageRole::User, "提问")));
    }

    #[test]
    fn outline_lists_every_question_with_first_reply() {
        let mut messages = conversation(3, 2);
        let resume =
            Message::new(MessageRole::User, "压缩续写").with_source(UserSource::CompressedResume);
        messages.insert(1, resume);
        messages.push(Message::new(MessageRole::User, "x".repeat(500)));

        let outline = user_outline(&messages);
        assert_eq!(outline.len(), 4);
        assert_eq!(outline[0].question, "问题 0");
        assert_eq!(outline[0].answer, "回答 0-0");
        assert_eq!(outline[2].id, messages[messages.len() - 4].id);
        assert_eq!(outline[3].question.chars().count(), OUTLINE_QUESTION_CHARS);
        assert!(outline[3].answer.is_empty());
    }
}
