pub mod audio;
pub mod balance;
pub mod chat;
pub mod files;
pub mod model;
pub mod responses;
pub mod tools;

use serde::{Deserialize, Serialize};

pub use audio::{
    AsrResponse, MAX_ASR_AUDIO_SECONDS, MAX_ASR_FILE_BYTES, MAX_TTS_BILLING_CHARS,
    MAX_TTS_BODY_BYTES, TTS_PCM_SAMPLE_RATE, TtsAudioFormat, TtsFinishEvent, TtsRequest,
    TtsStreamEvent, TtsVoice,
};
pub use balance::{BalanceInfo, BalanceResponse, Currency};
pub use chat::{
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, ChatTool, Choice, ChoiceMessage,
    CompletionTokensDetails, EventStream, FunctionCall, FunctionSpec, MessageRole, ReasoningEffort,
    ServerToolCall, ServerToolCallStep, ServerToolUse, StreamChoice, StreamChunk, StreamDelta,
    StreamEvent, StreamFunctionCall, StreamOptions, StreamToolCall, ThinkingConfig, ToolCall,
    ToolSpec, UrlCitation, UrlCitationAnnotation, Usage,
};
pub use files::{
    DeleteFileResponse, FileObject, FileParsePurpose, ListFilesParams, ListFilesResponse,
    ListOrder, MAX_DOCUMENT_FILE_BYTES,
};
pub use model::{ListModelsResponse, Model};
pub use responses::{
    ContentBlock, CreateResponseRequest, CustomToolCallInputItem, CustomToolCallOutputInputItem,
    FunctionCallInputItem, FunctionCallOutputInputItem, FunctionOutputContent, ImageDetail,
    IncompleteDetails, InputFileBlock, InputImageBlock, InputMessage, InputTokensDetails,
    MODEL_V4_FLASH, MODEL_V4_FLASH_VISION_EXP, MODEL_V4_PRO, MessageContent, OutputContentBlock,
    OutputCustomToolCall, OutputFunctionCall, OutputMessage, OutputReasoning, OutputTokensDetails,
    ReasoningConfig, ReasoningEffortLevel, ReasoningInputItem, ResponseError, ResponseInput,
    ResponseInputItem, ResponseObject, ResponseOutputItem, ResponseRole, ResponseStatus,
    ResponseUsage, ResponsesEventStream, ResponsesFunctionTool, ResponsesStreamEvent,
    ResponsesTool, ServerToolUseUsage, TextBlock, TextFormat, TextFormatConfig, WebSearchCallItem,
};
pub use tools::{
    FetchRequest, FetchResponse, HIGH_AUTHORITY, HIGH_QUALITY_RERANK_SCORE, MAX_OCR_FILE_BYTES,
    MAX_OCR_PAGES, OcrPage, OcrPurpose, OcrResponse, OcrUsage, SearchRequest, SearchResponse,
    SearchResult,
};

/// 模型内容安全校验参数（Messages / Responses / Chat Completions 通用）。
///
/// `level: "high"` 开启基础风控，审核模型输入输出，命中风险返回拒答标识；
/// `low` 与不传等价。详见官方「模型内容安全」指南。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SafetyCheck {
    pub level: SafetyLevel,
}

impl SafetyCheck {
    pub fn high() -> Self {
        Self {
            level: SafetyLevel::High,
        }
    }

    pub fn low() -> Self {
        Self {
            level: SafetyLevel::Low,
        }
    }
}

/// 安全校验档位。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SafetyLevel {
    /// API 默认的内容检查，与不传 `safety_check` 相同。
    #[default]
    Low,
    /// 开启基础风控。
    High,
}
