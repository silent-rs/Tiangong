//! Client 持有的不可变生成/执行配置；不携带消息，不共享可变任务状态。
use crate::{
    request::ReasoningEffort,
    tool::{ToolChoice, ToolSpec},
};

#[derive(Debug, Clone)]
pub struct Options {
    pub reasoning_effort: ReasoningEffort,
    pub max_output_tokens: u32,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: Option<ToolChoice>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Vec<String>,
    pub cache_key: Option<String>,
    pub timeout_ms: Option<u64>,
    pub max_retries: u32,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            reasoning_effort: ReasoningEffort::default(),
            max_output_tokens: 32_768,
            tools: Vec::new(),
            tool_choice: None,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            cache_key: None,
            timeout_ms: None,
            max_retries: crate::retry::DEFAULT_MAX_RETRIES,
        }
    }
}
