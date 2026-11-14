use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::DeepSeekError;
use crate::types::SafetyCheck;

// ── 请求类型 ──────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: MessageRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub prefix: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionSpec,
}

/// Chat Completions 的 tools 元素：函数工具或 DeepSeek 服务端工具。
///
/// 服务端工具只需声明 `{"type": "deepseek_web_search"}` /
/// `{"type": "deepseek_read_url"}`，是否调用由模型判断，
/// `tool_choice` 不能指定它们（服务端返回 400）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ChatTool {
    #[serde(rename = "function")]
    Function { function: FunctionSpec },
    /// 联网搜索 / 深度思考搜索（关闭思考时为联网搜索，
    /// 开启思考时模型在多步推理中自主搜索、打开网页、页内查找）。
    #[serde(rename = "deepseek_web_search")]
    DeepSeekWebSearch,
    /// 阅读链接：自动识别最后一条用户消息中的 URL 并抓取内容作答。
    #[serde(rename = "deepseek_read_url")]
    DeepSeekReadUrl,
}

impl From<FunctionSpec> for ChatTool {
    fn from(function: FunctionSpec) -> Self {
        Self::Function { function }
    }
}

impl ChatTool {
    /// 便捷构造函数工具。
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Option<serde_json::Value>,
    ) -> Self {
        Self::Function {
            function: FunctionSpec {
                name: name.into(),
                description: Some(description.into()),
                parameters,
                strict: false,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionSpec {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub strict: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    High,
    Max,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThinkingConfig {
    #[serde(rename = "type")]
    pub thinking_type: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ChatTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// 模型内容安全校验；`level: "high"` 开启基础风控。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_check: Option<SafetyCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamOptions {
    pub include_usage: bool,
}

// ── 响应类型 ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
    #[serde(default)]
    pub system_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Choice {
    pub index: u32,
    pub message: ChoiceMessage,
    /// DeepSeek 原始结束原因；保留未知值，含义由上层解释。
    pub finish_reason: String,
    #[serde(default)]
    pub logprobs: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChoiceMessage {
    pub role: MessageRole,
    #[serde(default)]
    pub content: Option<String>,
    // 兼容官方不同版本字段命名（reasoning_content 为主，thinking_content 为历史别名）。
    #[serde(default, alias = "thinking_content")]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// 正文引用（DeepSeek 扩展）：`url_citation`，按 content 的 Unicode 码点计数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Vec<UrlCitationAnnotation>>,
    /// 服务端工具调用过程（DeepSeek 扩展）：多轮对话需原样回传以还原上下文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_calls: Option<Vec<ServerToolCallStep>>,
    /// 内容审核拒答说明（`finish_reason == "content_filter"` 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

/// DeepSeek Chat Completions 的引用注解。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UrlCitationAnnotation {
    #[serde(rename = "type")]
    pub kind: String,
    pub url_citation: UrlCitation,
}

/// 引用位置：`start_index`/`end_index` 按 content 的 Unicode 码点计数。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UrlCitation {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub start_index: u64,
    #[serde(default)]
    pub end_index: u64,
}

/// 服务端工具调用的一步：这一步的思考与这一步发起的调用。
///
/// 多轮对话时把 assistant 消息连同 `server_tool_calls` 原样回传；
/// 阅读链接得到的网页由对应调用 `signature` 还原进上下文。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerToolCallStep {
    /// 步序号，从 0 开始。
    #[serde(default)]
    pub index: u32,
    /// 这一步的思考；未开启思考时不返回该字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub calls: Vec<ServerToolCall>,
}

/// 单个服务端工具调用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerToolCall {
    /// 调用在这一步内的序号。
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub id: String,
    /// `deepseek_web_search` 或 `deepseek_read_url`。
    #[serde(rename = "type", default)]
    pub kind: String,
    /// `search` / `deep_search` / `read_link` / `image_search` / `deep_image_search`。
    #[serde(rename = "kind", default, skip_serializing_if = "Option::is_none")]
    pub tool_kind: Option<String>,
    /// 调用参数：`{type: "search", queries}` / `{type: "open_page", url}` /
    /// `{type: "find_in_page", url, pattern}` / `{type: "read_link", url}`。
    #[serde(default)]
    pub action: Option<Value>,
    /// `completed` / `failed`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// 搜索结果（展示字段与 Responses API 相同）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<Value>>,
    /// 打开的网页或阅读的链接的 `{url, title, published_at}`，仅供展示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<Value>,
    /// 加密的网页正文；只有成功阅读的链接才有，回传时原样保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// 调用失败时的 `{code}`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

/// 服务端联网工具的用量（`usage.server_tool_use`）：
/// `tool_search` / `tool_fetch` / `tool_image_search` 为计费次数，
/// `web_search_requests` / `web_fetch_requests` 为实际请求数。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerToolUse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search_requests: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_fetch_requests: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_search: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_fetch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_image_search: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    /// 服务端联网工具用量（DeepSeek 扩展）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    /// 安全审核产生的 Token（已包含在 completion_tokens 中）。
    #[serde(default)]
    pub safety_tokens: Option<u64>,
}

// ── 流式类型 ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamChoice {
    pub index: u32,
    pub delta: StreamDelta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// 服务端工具过程事件（DeepSeek 扩展，与 delta 并列）：
    /// `step_reasoning_done` / `call_started` / `call_completed`，原样保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_event: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct StreamDelta {
    #[serde(default)]
    pub role: Option<MessageRole>,
    #[serde(default)]
    pub content: Option<String>,
    // 兼容官方不同版本字段命名。
    #[serde(default, alias = "thinking_content")]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<StreamToolCall>>,
    /// 开启 title 时的会话标题（DeepSeek 扩展，最后一个 chunk 下发）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 内容审核拒答说明（DeepSeek 扩展，最后一个 chunk 下发）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamToolCall {
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub function: Option<StreamFunctionCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamFunctionCall {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

// ── 流式事件（解析后的高层事件） ──────────────────────────

#[derive(Debug, Clone)]
pub enum StreamEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallDelta {
        index: u32,
        arguments: String,
    },
    /// 文本协议兜底解析出的工具调用（整块，含完整参数）。
    /// 由 `create_stream` 在流末统一产出，区别于流式增量到达的结构化 tool_calls。
    TextProtocolToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Usage(Usage),
    /// 原样透传 choices[].finish_reason；中间块的 null 不产生此事件。
    /// Done 只表示流结束，不能代替服务端提供的结束原因。
    FinishReason(String),
    Done,
    Error(String),
}

pub type EventStream = BoxStream<'static, Result<StreamEvent, DeepSeekError>>;
