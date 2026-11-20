//! Core 本地模型结果与工具参数校验；LLM 不依赖此类型。
#[derive(Debug, Clone)]
pub struct CoreModelOutput {
    pub output: Vec<tiangong_llm::message::MessageContent>,
    pub status: tiangong_llm::CompletionStatus,
    pub text: String,
    pub reasoning_content: String,
    pub reasoning_signature: Option<String>,
    pub stop_reason: Option<tiangong_llm::StopReason>,
    pub usage: tiangong_types::TokenUsage,
    pub tool_calls: Vec<tiangong_llm::tool::ToolCall>,
    pub invalid_tool_calls: Vec<tiangong_llm::InvalidToolCall>,
    pub output_elapsed_ms: Option<u64>,
    pub reasoning_elapsed_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
}
impl From<tiangong_llm::ModelResponse> for CoreModelOutput {
    fn from(value: tiangong_llm::ModelResponse) -> Self {
        Self {
            output: value.output,
            status: value.status,
            text: value.text,
            reasoning_content: value.reasoning_content,
            reasoning_signature: value.reasoning_signature,
            stop_reason: value.stop_reason,
            usage: value.usage,
            tool_calls: value.tool_calls,
            invalid_tool_calls: value.invalid_tool_calls,
            output_elapsed_ms: value.output_elapsed_ms,
            reasoning_elapsed_ms: value.reasoning_elapsed_ms,
            ttft_ms: value.ttft_ms,
        }
    }
}

impl CoreModelOutput {
    pub(crate) fn validate_tools(
        &mut self,
        tools: &[tiangong_llm::tool::ToolSpec],
    ) -> anyhow::Result<()> {
        let mut valid = Vec::new();
        for call in std::mem::take(&mut self.tool_calls) {
            let reason = match tools.iter().find(|spec| spec.name == call.name) {
                None => Some(format!("工具 {} 不在本次 tools 定义中", call.name)),
                Some(spec) if spec.input_schema.is_null() => None,
                Some(spec) => {
                    let validator = jsonschema::validator_for(&spec.input_schema)?;
                    let errors = validator
                        .iter_errors(&call.arguments)
                        .take(3)
                        .map(|error| error.to_string())
                        .collect::<Vec<_>>();
                    (!errors.is_empty()).then(|| errors.join("；"))
                }
            };
            if let Some(reason) = reason {
                self.invalid_tool_calls.push(tiangong_llm::InvalidToolCall {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                    reason,
                });
            } else {
                valid.push(call);
            }
        }
        self.tool_calls = valid;
        Ok(())
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    #[test]
    fn validation_is_protocol_independent_and_retains_valid_parallel_calls() {
        let response = tiangong_llm::ModelResponse {
            provider_usage: None,
            output: Vec::new(),
            status: tiangong_llm::CompletionStatus::Complete,
            text: String::new(),
            reasoning_content: String::new(),
            reasoning_signature: None,
            stop_reason: Some(tiangong_llm::StopReason::ToolUse),
            usage: Default::default(),
            tool_calls: vec![
                tiangong_llm::tool::ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path":"a"}),
                },
                tiangong_llm::tool::ToolCall {
                    id: "b".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                },
                tiangong_llm::tool::ToolCall {
                    id: "c".into(),
                    name: "unknown".into(),
                    arguments: serde_json::json!({}),
                },
            ],
            invalid_tool_calls: Vec::new(),
            output_elapsed_ms: None,
            reasoning_elapsed_ms: None,
            ttft_ms: None,
        };
        let mut output = CoreModelOutput::from(response);
        output.validate_tools(&[tiangong_llm::tool::ToolSpec { name:"read".into(), description:String::new(), input_schema:serde_json::json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) }]).unwrap();
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.tool_calls[0].id, "a");
        assert_eq!(output.invalid_tool_calls.len(), 2);
    }
}
