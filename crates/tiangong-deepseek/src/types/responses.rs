use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::DeepSeekError;

// ── 支持模型 ──────────────────────────────────────────────

pub const MODEL_V4_FLASH: &str = "deepseek-v4-flash";
pub const MODEL_V4_PRO: &str = "deepseek-v4-pro";
pub const MODEL_V4_FLASH_VISION_EXP: &str = "deepseek-v4-flash-vision-exp";

// ── 请求类型 ──────────────────────────────────────────────

/// Responses API 为无状态设计：服务端不存储会话，多轮对话需在 `input`
/// 中回传完整历史。`input` 与 `instructions` 至少传一个。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CreateResponseRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<ResponseInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextFormatConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ResponseInput {
    /// 纯字符串，视作一条 user 消息。
    Text(String),
    Items(Vec<ResponseInputItem>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ResponseInputItem {
    #[serde(rename = "message")]
    Message(InputMessage),
    #[serde(rename = "function_call")]
    FunctionCall(FunctionCallInputItem),
    #[serde(rename = "function_call_output")]
    FunctionCallOutput(FunctionCallOutputInputItem),
    #[serde(rename = "custom_tool_call")]
    CustomToolCall(CustomToolCallInputItem),
    #[serde(rename = "custom_tool_call_output")]
    CustomToolCallOutput(CustomToolCallOutputInputItem),
    #[serde(rename = "reasoning")]
    Reasoning(ReasoningInputItem),
    #[serde(rename = "web_search_call")]
    WebSearchCall(WebSearchCallItem),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InputMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<ResponseRole>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ResponseRole {
    User,
    Assistant,
    System,
    /// 服务端视同 user。
    #[default]
    Developer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "input_text")]
    InputText(TextBlock),
    #[serde(rename = "output_text")]
    OutputText(TextBlock),
    #[serde(rename = "input_image")]
    InputImage(InputImageBlock),
    #[serde(rename = "reasoning_text")]
    ReasoningText(TextBlock),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TextBlock {
    pub text: String,
}

/// `image_url` 与 `file_id` 互斥（都不传或都传返回 400）；
/// 仅 `deepseek-v4-flash-vision-exp` 真正处理图片输入。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InputImageBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ImageDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    #[default]
    Auto,
    Low,
    High,
    Original,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCallInputItem {
    pub call_id: String,
    pub name: String,
    /// JSON 格式字符串。
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCallOutputInputItem {
    pub call_id: String,
    pub output: FunctionOutputContent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomToolCallInputItem {
    pub call_id: String,
    pub name: String,
    pub input: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomToolCallOutputInputItem {
    pub call_id: String,
    pub output: FunctionOutputContent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FunctionOutputContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReasoningInputItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
}

/// web_search_call 项原样回传即可，服务端自动恢复搜索结果；
/// `extra` 保留服务端新增的未知字段，确保回传不丢信息。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WebSearchCallItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Value>,
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ResponsesTool {
    #[serde(rename = "function")]
    Function(ResponsesFunctionTool),
    #[serde(rename = "web_search")]
    WebSearch,
    #[serde(rename = "web_search_2025_08_26")]
    WebSearch20250826,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponsesFunctionTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReasoningConfig {
    /// none 关闭思考；minimal/low 映射为 low，medium/high/xhigh 映射为 high。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffortLevel>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffortLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TextFormatConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<TextFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum TextFormat {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "json_object")]
    JsonObject,
    #[serde(rename = "json_schema")]
    JsonSchema { name: String, schema: Value },
}

// ── 响应类型 ──────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResponseObject {
    #[serde(default)]
    pub id: String,
    /// 恒为 `response`。
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub status: ResponseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub output: Vec<ResponseOutputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponseUsage>,
}

impl ResponseObject {
    /// 拼接全部输出文本，等价于官方文档的 `response.output_text` 便捷访问。
    pub fn output_text(&self) -> String {
        self.output
            .iter()
            .filter_map(|item| match item {
                ResponseOutputItem::Message(message) => Some(message),
                _ => None,
            })
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                OutputContentBlock::OutputText(text) => Some(text.text.clone()),
                OutputContentBlock::ReasoningText(_) => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    #[default]
    InProgress,
    Completed,
    Incomplete,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResponseError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IncompleteDetails {
    /// `max_output_tokens` / `content_filter`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ResponseOutputItem {
    #[serde(rename = "message")]
    Message(OutputMessage),
    #[serde(rename = "reasoning")]
    Reasoning(OutputReasoning),
    #[serde(rename = "function_call")]
    FunctionCall(OutputFunctionCall),
    #[serde(rename = "custom_tool_call")]
    CustomToolCall(OutputCustomToolCall),
    #[serde(rename = "web_search_call")]
    WebSearchCall(WebSearchCallItem),
    /// 服务端新增而 SDK 尚未支持的输出 item（如官方文档列出的 `tool_search_call`），
    /// 按未知类型兜底反序列化，避免整个 ResponseObject 解析失败；回传时
    /// 服务端会忽略不认识的 item 类型。
    #[serde(other)]
    Unknown,
}

impl Default for ResponseOutputItem {
    fn default() -> Self {
        Self::Message(OutputMessage::default())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OutputMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// 恒为 `assistant`。
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: Vec<OutputContentBlock>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OutputReasoning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default)]
    pub content: Vec<OutputContentBlock>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OutputFunctionCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub name: String,
    /// 模型可能生成非法 JSON 或未定义参数，使用前需自行校验。
    #[serde(default)]
    pub arguments: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OutputCustomToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum OutputContentBlock {
    #[serde(rename = "output_text")]
    OutputText(TextBlock),
    #[serde(rename = "reasoning_text")]
    ReasoningText(TextBlock),
}

impl Default for OutputContentBlock {
    fn default() -> Self {
        Self::OutputText(TextBlock::default())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResponseUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<InputTokensDetails>,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<OutputTokensDetails>,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InputTokensDetails {
    /// 缓存命中的输入 token 数。
    #[serde(default)]
    pub cached_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OutputTokensDetails {
    /// 思维链 token 数。
    #[serde(default)]
    pub reasoning_tokens: u64,
}

// ── 流式类型 ──────────────────────────────────────────────

/// Responses 流式 SSE 事件。事件格式与 OpenAI Responses API 兼容，
/// 每个事件携带 `type` 与递增 `sequence_number`；流以
/// `response.completed` / `response.incomplete` / `response.failed`
/// 结束，没有 `[DONE]` 标记。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ResponsesStreamEvent {
    #[serde(rename = "response.created")]
    ResponseCreated {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        response: ResponseObject,
    },
    #[serde(rename = "response.in_progress")]
    ResponseInProgress {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        response: ResponseObject,
    },
    #[serde(rename = "response.completed")]
    ResponseCompleted {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        response: ResponseObject,
    },
    #[serde(rename = "response.incomplete")]
    ResponseIncomplete {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        response: ResponseObject,
    },
    #[serde(rename = "response.failed")]
    ResponseFailed {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        response: ResponseObject,
    },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        item: ResponseOutputItem,
    },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        item: ResponseOutputItem,
    },
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        content_index: u32,
        #[serde(default)]
        part: OutputContentBlock,
    },
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        content_index: u32,
        #[serde(default)]
        part: OutputContentBlock,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        content_index: u32,
        #[serde(default)]
        delta: String,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        content_index: u32,
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        content_index: u32,
        #[serde(default)]
        delta: String,
    },
    #[serde(rename = "response.reasoning_text.done")]
    ReasoningTextDone {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        content_index: u32,
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        delta: String,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        arguments: String,
    },
    #[serde(rename = "response.custom_tool_call_input.delta")]
    CustomToolCallInputDelta {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        delta: String,
    },
    #[serde(rename = "response.custom_tool_call_input.done")]
    CustomToolCallInputDone {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        input: String,
    },
    #[serde(rename = "response.web_search_call.in_progress")]
    WebSearchCallInProgress {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        #[serde(default)]
        output_index: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<WebSearchCallItem>,
    },
    #[serde(rename = "response.web_search_call.searching")]
    WebSearchCallSearching {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        #[serde(default)]
        output_index: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<WebSearchCallItem>,
    },
    #[serde(rename = "response.web_search_call.completed")]
    WebSearchCallCompleted {
        #[serde(default)]
        sequence_number: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        #[serde(default)]
        output_index: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<WebSearchCallItem>,
    },
    /// 服务端新增而 SDK 尚未支持的事件，保留事件名透传，不中断流。
    #[serde(skip)]
    Unknown { event_type: String },
}

pub type ResponsesEventStream = BoxStream<'static, Result<ResponsesStreamEvent, DeepSeekError>>;
