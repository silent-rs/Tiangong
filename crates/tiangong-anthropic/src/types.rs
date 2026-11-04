use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AnthropicError;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// 从缓存读取的输入，不包含在 input_tokens 中。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    /// 写入缓存的输入，不包含在 input_tokens 中。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
}

/// 提示缓存断点标记。官方限制每请求最多 4 个；标记所在的块（及之前的
/// 完整前缀）写入缓存，后续请求前缀一致时按缓存读计费（约 1 折）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub cache_type: String,
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral".to_string(),
        }
    }
}

/// system 参数的两种官方形态：纯字符串或文本块数组（块上可携带
/// cache_control 断点）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SystemContent {
    Text(String),
    Blocks(Vec<TextBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextBlock {
    /// 块类型标签，官方协议要求必填，固定为 "text"。
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl TextBlock {
    pub fn new(text: String, cache_control: Option<CacheControl>) -> Self {
        Self {
            block_type: "text".to_string(),
            text,
            cache_control,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlockParam>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockParam {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Image {
        source: ImageSourceParam,
    },
    Document {
        source: ImageSourceParam,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageSourceParam {
    #[serde(rename = "type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<Value>,
        #[serde(default)]
        is_error: Option<bool>,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    Tool {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// 禁止调用任何工具（Anthropic API：`tool_choice: {"type": "none"}`）。
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// 自适应思考（现行唯一思考形态）：模型自行决定是否思考及思考多深，
    /// 深度由 `output_config.effort` 档位控制。Claude 4.7+ 新模型仅接受
    /// 该形态，智谱 GLM 等第三方兼容端点实测也已完整支持。
    Adaptive,
    Disabled,
}

/// `output_config.effort` 档位：adaptive thinking 模型的深度控制。
/// 档位语义：low 省token / medium 平衡（Opus 5.5 默认）/ high 默认强度
/// （其余模型默认）/ xhigh 长程 agentic / max 无上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// 请求级输出配置，当前承载 adaptive thinking 模型的 effort 深度档位。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessagesCreateRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemContent>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// 输出配置：adaptive thinking 模型的深度档位（output_config.effort）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessagesCreateResponse {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelsListResponse {
    #[serde(default)]
    pub data: Vec<ModelInfo>,
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub first_id: Option<String>,
    #[serde(default)]
    pub last_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageStartData {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    pub role: MessageRole,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockStartData {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockDeltaData {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageDeltaData {
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    MessageStart {
        message: MessageStartData,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlockStartData,
    },
    ContentBlockDelta {
        index: usize,
        delta: ContentBlockDeltaData,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDeltaData,
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    Error {
        /// 错误类别（error.type，如 overloaded_error / api_error）。
        error_type: String,
        message: String,
    },
    Unknown,
}

pub type EventStream = BoxStream<'static, Result<StreamEvent, AnthropicError>>;
