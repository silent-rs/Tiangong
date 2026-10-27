use crate::message::{ChatMessage, MessageContent, MessageRole};
use crate::request::{ProviderRequest, ReasoningEffort};
use crate::response::{ProviderResponse, StopReason};
use crate::tool::{ToolChoice, ToolSpec, parse_tool_arguments_or_error};
use crate::usage::TokenUsageData;
use anyhow::Result;
use serde_json::{Value, json};
use tiangong_deepseek::types::{
    ContentBlock, CreateResponseRequest, FunctionCallInputItem, FunctionCallOutputInputItem,
    FunctionOutputContent, InputImageBlock, InputMessage, OutputContentBlock, ReasoningConfig,
    ReasoningEffortLevel, ReasoningInputItem, ResponseInput, ResponseInputItem, ResponseObject,
    ResponseOutputItem, ResponseRole, ResponseStatus, ResponsesFunctionTool, ResponsesTool,
    TextBlock, TextFormat, TextFormatConfig,
};

/// 构建 Responses API 请求（POST /responses）。
///
/// Responses API 为无状态设计：多轮对话需把完整历史回传到 `input`，
/// 系统提示走 `instructions`。注意该 API 不支持 stop 序列，
/// `stop_sequences` 无法传递（官方参数兼容性表未提供等价字段）。
pub fn to_deepseek_request(req: &ProviderRequest) -> Result<CreateResponseRequest> {
    let thinking_enabled = req.reasoning_effort.is_thinking_enabled();
    let mut tool_choice_value = None;
    let mut tools_value = None;
    if !req.tools.is_empty() {
        tools_value = Some(build_tools(&req.tools));
        tool_choice_value = build_tool_choice(req.tool_choice.as_ref());
    }
    let input = build_input(req);
    let instructions = req
        .system
        .as_ref()
        .filter(|system| !system.trim().is_empty())
        .cloned();
    // 官方要求 input 与 instructions 至少提供一个；消息流为空且无系统提示
    // 时提前失败，避免服务端 400 带来的不透明错误。
    if matches!(&input, ResponseInput::Items(items) if items.is_empty()) && instructions.is_none() {
        anyhow::bail!(
            "DeepSeek Responses 请求缺少内容：input 与 instructions 至少需要一个（消息流为空且未提供系统提示）"
        );
    }
    Ok(CreateResponseRequest {
        model: req.model.clone(),
        input: Some(input),
        instructions,
        // 官方文档：reasoning.effort 为 none 关闭思考，low/high/max 开启；
        // 不传则使用模型默认行为（默认开启），因此必须显式传 none 才能关闭。
        reasoning: Some(ReasoningConfig {
            effort: Some(build_reasoning_effort(req.reasoning_effort)),
        }),
        max_output_tokens: Some(req.max_tokens),
        // 官方文档：思考模式下 temperature 不生效；top_p 仅在思考模式下生效
        // （有效范围 0.95–1.0），非思考模式下传入值被忽略。与 Chat Completions
        // 侧一致，思考模式开启时省略这两个参数，保持请求干净。
        temperature: if thinking_enabled {
            None
        } else {
            req.temperature
        },
        top_p: if thinking_enabled { None } else { req.top_p },
        // SDK 在 create / create_stream 内部强制设置 stream，这里保持 None。
        stream: None,
        text: build_text_format(req),
        tools: tools_value,
        tool_choice: tool_choice_value,
        top_logprobs: None,
        user: req.session_id.clone(),
    })
}

/// 消息流 → `input` item 列表。
fn build_input(req: &ProviderRequest) -> ResponseInput {
    let mut items = Vec::new();
    for message in &req.messages {
        match message.role {
            // 系统指令统一走 instructions（Responses API 无消息内 system 语义，
            // 消息流中插入的 System 消息并入会丢失时序，维持 chat 侧的跳过策略）。
            MessageRole::System => {}
            MessageRole::User => {
                if let Some(item) = build_user_message(message) {
                    items.push(ResponseInputItem::Message(item));
                }
            }
            MessageRole::Assistant => {
                // 思考内容以独立 reasoning item 回传，置于 message item 之前，
                // 与服务端的输出顺序保持一致。
                if let Some(reasoning) = build_reasoning_item(message) {
                    items.push(ResponseInputItem::Reasoning(reasoning));
                }
                if let Some(item) = build_assistant_message(message) {
                    items.push(ResponseInputItem::Message(item));
                }
                for call in build_function_calls(message) {
                    items.push(ResponseInputItem::FunctionCall(call));
                }
            }
            MessageRole::Tool => {
                // 每个工具结果映射为 function_call_output item；
                // 官方要求每个 function_call 必须有对应的 function_call_output。
                for output in build_function_call_outputs(message) {
                    items.push(ResponseInputItem::FunctionCallOutput(output));
                }
            }
        }
    }
    ResponseInput::Items(items)
}

/// user 消息：纯文本走字符串简写；含图片时映射为 input_text / input_image 块，
/// 保持原始交错顺序。
fn build_user_message(message: &ChatMessage) -> Option<InputMessage> {
    let text = extract_text(message);
    let has_images = message
        .content
        .iter()
        .any(|content| matches!(content, MessageContent::Image(_)));
    if !has_images {
        if text.trim().is_empty() {
            return None;
        }
        return Some(InputMessage {
            role: Some(ResponseRole::User),
            content: Some(tiangong_deepseek::types::MessageContent::Text(text)),
        });
    }
    let mut blocks = Vec::new();
    for block in &message.content {
        match block {
            MessageContent::Image(image) => {
                blocks.push(ContentBlock::InputImage(InputImageBlock {
                    image_url: Some(image.data.clone()),
                    detail: None,
                    file_id: None,
                }));
            }
            MessageContent::Text(text) if !text.trim().is_empty() => {
                blocks.push(ContentBlock::InputText(TextBlock { text: text.clone() }));
            }
            // File 附件（PDF/Office 等）：Responses API 的 message 输入不支持
            // 文件块（官方文档明确「文件输入不支持」），目前与 chat 侧一致
            // 不发送，显式告警避免静默丢失。
            MessageContent::File(file) => {
                tracing::warn!(
                    title = file.title.as_deref().unwrap_or("attachment"),
                    mime_type = %file.mime_type,
                    "user 消息含文件附件，DeepSeek Responses 输入不支持文件块，附件未随请求发送"
                );
            }
            _ => {}
        }
    }
    Some(InputMessage {
        role: Some(ResponseRole::User),
        content: Some(tiangong_deepseek::types::MessageContent::Blocks(blocks)),
    })
}

/// assistant 历史文本：以 output_text 块回传（贴近服务端输出形态）。
fn build_assistant_message(message: &ChatMessage) -> Option<InputMessage> {
    let text = extract_text(message);
    if text.trim().is_empty() {
        return None;
    }
    Some(InputMessage {
        role: Some(ResponseRole::Assistant),
        content: Some(tiangong_deepseek::types::MessageContent::Blocks(vec![
            ContentBlock::OutputText(TextBlock { text }),
        ])),
    })
}

/// assistant 思考内容 → reasoning item（reasoning_text 块承载）。
fn build_reasoning_item(message: &ChatMessage) -> Option<ReasoningInputItem> {
    let thinking = extract_thinking(message)?;
    Some(ReasoningInputItem {
        content: Some(tiangong_deepseek::types::MessageContent::Blocks(vec![
            ContentBlock::ReasoningText(TextBlock { text: thinking }),
        ])),
    })
}

/// assistant 历史工具调用 → function_call items。
fn build_function_calls(message: &ChatMessage) -> Vec<FunctionCallInputItem> {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            MessageContent::ToolCall(call) => Some(FunctionCallInputItem {
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.to_string(),
            }),
            _ => None,
        })
        .collect()
}

/// 工具执行结果 → function_call_output items。
fn build_function_call_outputs(message: &ChatMessage) -> Vec<FunctionCallOutputInputItem> {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            MessageContent::ToolResult(result) => {
                let output = match &result.content {
                    crate::tool::ToolResultContent::Text(text) => text.clone(),
                    crate::tool::ToolResultContent::Json(value) => value.to_string(),
                };
                Some(FunctionCallOutputInputItem {
                    call_id: result.tool_call_id.clone(),
                    output: FunctionOutputContent::Text(output),
                })
            }
            _ => None,
        })
        .collect()
}

fn build_tools(specs: &[ToolSpec]) -> Vec<ResponsesTool> {
    specs
        .iter()
        .map(|spec| {
            ResponsesTool::Function(ResponsesFunctionTool {
                name: spec.name.clone(),
                description: Some(spec.description.clone()),
                parameters: Some(spec.input_schema.clone()),
            })
        })
        .collect()
}

/// tool_choice 映射。注意 Responses API 的指定工具形态为
/// `{"type":"function","name":...}`（Chat Completions 为
/// `{"type":"function","function":{"name":...}}`，两者不同）。
fn build_tool_choice(choice: Option<&ToolChoice>) -> Option<Value> {
    match choice {
        Some(ToolChoice::Any) => Some(json!("required")),
        Some(ToolChoice::Auto) => Some(json!("auto")),
        Some(ToolChoice::Tool(name)) => Some(json!({ "type": "function", "name": name })),
        // 明确禁止工具调用（Responses 兼容 tool_choice: "none"）。
        Some(ToolChoice::None) => Some(json!("none")),
        None => None,
    }
}

/// 思考强度映射。官方 reasoning.effort 取值 none / low / high / max
/// （minimal/medium/xhigh 由服务端兼容映射），内部 Medium 无对应档位，
/// 统一映射到 high（官方默认档）。
fn build_reasoning_effort(effort: ReasoningEffort) -> ReasoningEffortLevel {
    match effort {
        ReasoningEffort::None => ReasoningEffortLevel::None,
        ReasoningEffort::Low => ReasoningEffortLevel::Low,
        ReasoningEffort::Medium | ReasoningEffort::High => ReasoningEffortLevel::High,
        ReasoningEffort::Max => ReasoningEffortLevel::Max,
    }
}

/// 从 metadata 提取输出格式（text / json_object / json_schema）。
///
/// 上层暂无专用字段，DeepSeek 需要结构化输出时由调用方在 metadata 写入：
/// `{"response_format": "json_object"}`、
/// `{"response_format": {"type": "json_object"}}` 或
/// `{"response_format": {"type": "json_schema", "name": ..., "schema": ...}}`。
fn build_text_format(req: &ProviderRequest) -> Option<TextFormatConfig> {
    let fmt = req.metadata.as_ref()?.get("response_format")?;
    let format = match fmt {
        Value::String(value) => match value.as_str() {
            "json_object" => TextFormat::JsonObject,
            _ => TextFormat::Text,
        },
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("json_object") => TextFormat::JsonObject,
            Some("json_schema") => {
                let (name, schema) = match (
                    object.get("name").and_then(Value::as_str),
                    object.get("schema"),
                ) {
                    (Some(name), Some(schema)) => (name.to_string(), schema.clone()),
                    _ => {
                        // json_schema 缺 name/schema 无法构造：跳过整个 text 配置
                        // 并告警（按默认文本输出），不静默吞掉配置错误。
                        tracing::warn!(
                            ?fmt,
                            "response_format=json_schema 缺少 name/schema，已忽略结构化输出配置"
                        );
                        return None;
                    }
                };
                TextFormat::JsonSchema { name, schema }
            }
            _ => TextFormat::Text,
        },
        _ => return None,
    };
    Some(TextFormatConfig {
        format: Some(format),
    })
}

fn extract_thinking(message: &ChatMessage) -> Option<String> {
    let thinking = message
        .content
        .iter()
        .filter_map(|content| match content {
            MessageContent::Thinking(thinking) => Some(thinking.thinking.trim()),
            _ => None,
        })
        .filter(|thinking| !thinking.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!thinking.is_empty()).then_some(thinking)
}

fn extract_text(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|c| match c {
            MessageContent::Text(text) => Some(text.clone()),
            MessageContent::ToolResult(result) => Some(match &result.content {
                crate::tool::ToolResultContent::Text(text) => text.clone(),
                crate::tool::ToolResultContent::Json(value) => value.to_string(),
            }),
            MessageContent::File(file) => Some(format!(
                "<attachment title=\"{}\" mime_type=\"{}\">\n{}\n</attachment>",
                file.title.as_deref().unwrap_or("attachment"),
                file.mime_type,
                file.data
            )),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 解析 Responses 完整响应为统一的 ProviderResponse。
///
/// 输出 item 按类型分流：message 的 output_text 拼接为正文，reasoning 的
/// reasoning_text 拼接为思考内容，function_call 映射为工具调用；
/// web_search_call / custom_tool_call / 未知 item 忽略（上层未声明这类工具）。
pub fn from_deepseek_response(
    response: ResponseObject,
    tools_enabled: bool,
) -> Result<ProviderResponse> {
    // failed 属于请求级错误（服务端在 error 字段给出原因），恢复 chat 版
    // 行为直接报错，避免退化为「空回复 + Other("failed")」被上层当作正常
    // 截断处理。
    if response.status == ResponseStatus::Failed {
        let detail = response
            .error
            .as_ref()
            .map(|error| {
                format!(
                    "code={:?} message={:?}",
                    error.code.clone().unwrap_or_default(),
                    error.message.clone().unwrap_or_default()
                )
            })
            .unwrap_or_else(|| "无错误详情".to_string());
        anyhow::bail!("DeepSeek Responses 返回 failed 状态：{detail}");
    }
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls = Vec::new();
    for item in &response.output {
        match item {
            ResponseOutputItem::Message(message) => {
                for block in &message.content {
                    match block {
                        OutputContentBlock::OutputText(block) => {
                            text_parts.push(block.text.clone());
                        }
                        OutputContentBlock::ReasoningText(block) => {
                            reasoning_parts.push(block.text.clone());
                        }
                    }
                }
            }
            ResponseOutputItem::Reasoning(reasoning) => {
                for block in &reasoning.content {
                    if let OutputContentBlock::ReasoningText(block) = block {
                        reasoning_parts.push(block.text.clone());
                    }
                }
            }
            ResponseOutputItem::FunctionCall(call) => {
                tool_calls.push(crate::tool::ToolCall {
                    id: call.call_id.clone(),
                    name: call.name.clone(),
                    arguments: parse_tool_arguments_or_error(
                        &call.name,
                        &call.call_id,
                        &call.arguments,
                    ),
                });
            }
            ResponseOutputItem::CustomToolCall(_)
            | ResponseOutputItem::WebSearchCall(_)
            | ResponseOutputItem::Unknown => {}
        }
    }
    let text = text_parts.concat();
    let reasoning_content = reasoning_parts.concat();
    let mut content = Vec::new();
    if tool_calls.is_empty() {
        // 无结构化工具调用：仅当请求携带 tools 时才做文本协议兜底——无 tools 时
        // （如总结阶段 ToolChoice::None）模型不可能走工具调用，兜底只会误伤讨论
        // 协议的正常文本。
        if tools_enabled {
            match tiangong_deepseek::dsml::parse_dsml_tool_calls(text.trim()) {
                Some(text_calls) if !text_calls.is_empty() => {
                    tracing::warn!(
                        count = text_calls.len(),
                        "DeepSeek 返回了文本形式的工具调用，已兜底解析"
                    );
                    let leftover = tiangong_deepseek::dsml::strip_tool_call_block(text.trim());
                    if !leftover.trim().is_empty() {
                        content.push(MessageContent::Text(leftover.trim().to_string()));
                    }
                    for call in text_calls {
                        // 官方要求 call_id 唯一：历史每轮全量回传，
                        // 局部下标会跨轮重复，取全局唯一 id。
                        let call_id = tiangong_deepseek::dsml::next_text_call_id();
                        content.push(MessageContent::ToolCall(crate::tool::ToolCall {
                            id: call_id.clone(),
                            name: call.name.clone(),
                            arguments: parse_tool_arguments_or_error(
                                &call.name,
                                &call_id,
                                &call.arguments,
                            ),
                        }));
                    }
                }
                _ => {
                    if !text.trim().is_empty() {
                        content.push(MessageContent::Text(text.trim().to_string()));
                    }
                }
            }
        } else if !text.trim().is_empty() {
            // 无 tools 时文本原样保留，不做协议兜底。
            content.push(MessageContent::Text(text.trim().to_string()));
        }
    } else {
        if !text.trim().is_empty() {
            content.push(MessageContent::Text(text.trim().to_string()));
        }
        content.extend(tool_calls.into_iter().map(MessageContent::ToolCall));
    }
    let stop_reason = map_stop_reason(&response);
    let usage = response.usage.as_ref().map(parse_usage);
    let raw = serde_json::to_value(&response).unwrap_or_default();
    let id = response.id;
    let model = response.model;
    Ok(ProviderResponse {
        id: Some(id),
        model: Some(model),
        assistant_message: ChatMessage::new(MessageRole::Assistant, content),
        reasoning_content: (!reasoning_content.trim().is_empty()).then_some(reasoning_content),
        stop_reason: Some(stop_reason),
        usage,
        raw: Some(raw),
    })
}

/// 响应状态 → 停止原因。
///
/// Responses API 没有 finish_reason 字段：completed 状态下有 function_call
/// item 视为工具调用轮，否则视为自然结束；incomplete 按
/// incomplete_details.reason 细分；failed 归入 Other。
fn map_stop_reason(response: &ResponseObject) -> StopReason {
    match response.status {
        ResponseStatus::Completed => {
            if response
                .output
                .iter()
                .any(|item| matches!(item, ResponseOutputItem::FunctionCall(_)))
            {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        }
        ResponseStatus::Incomplete => {
            match response
                .incomplete_details
                .as_ref()
                .and_then(|details| details.reason.as_deref())
            {
                Some("max_output_tokens") => StopReason::MaxTokens,
                Some("content_filter") => StopReason::Other("content_filter".to_string()),
                _ => StopReason::Other("incomplete".to_string()),
            }
        }
        ResponseStatus::Failed => StopReason::Other("failed".to_string()),
        ResponseStatus::InProgress => StopReason::Other("in_progress".to_string()),
    }
}

/// Responses 用量 → 统一用量。cached_tokens 命中缓存输入数，
/// 未命中数按 input - cached 推导（官方无独立 miss 字段）。
fn parse_usage(usage: &tiangong_deepseek::types::ResponseUsage) -> TokenUsageData {
    let prompt = usage.input_tokens as usize;
    let cached = usage
        .input_tokens_details
        .as_ref()
        .map(|details| details.cached_tokens as usize);
    TokenUsageData {
        prompt_tokens: prompt,
        completion_tokens: usage.output_tokens as usize,
        total_tokens: usage.total_tokens as usize,
        prompt_cache_hit_tokens: cached,
        prompt_cache_miss_tokens: cached.map(|hit| prompt.saturating_sub(hit)),
    }
}

pub fn parse_stream_usage(usage: &tiangong_deepseek::types::ResponseUsage) -> TokenUsageData {
    parse_usage(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ImageContent, ThinkingContent};
    use crate::tool::ToolCall;
    use serde_json::json;

    fn base_request(messages: Vec<ChatMessage>) -> ProviderRequest {
        ProviderRequest {
            session_id: None,
            model: "deepseek-v4-pro".to_string(),
            system: None,
            messages,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            stop_sequences: vec!["STOP".to_string()],
            metadata: None,
            reasoning_effort: ReasoningEffort::None,
        }
    }

    #[test]
    fn multimodal_user_content_preserves_interleaved_order() {
        let message = ChatMessage::new(
            MessageRole::User,
            vec![
                MessageContent::Text("图一".to_string()),
                MessageContent::Image(ImageContent {
                    mime_type: "image/png".to_string(),
                    data: "data:image/png;base64,A".to_string(),
                }),
                MessageContent::Text("图二".to_string()),
                MessageContent::Image(ImageContent {
                    mime_type: "image/png".to_string(),
                    data: "data:image/png;base64,B".to_string(),
                }),
            ],
        );
        let request = to_deepseek_request(&base_request(vec![message])).expect("request");
        let items = match request.input.expect("input") {
            ResponseInput::Items(items) => items,
            other => panic!("应为 item 列表：{other:?}"),
        };
        let ResponseInputItem::Message(message) = &items[0] else {
            panic!("应为 message item");
        };
        let blocks = match &message.content.as_ref().expect("content") {
            tiangong_deepseek::types::MessageContent::Blocks(blocks) => blocks,
            other => panic!("应为块列表：{other:?}"),
        };
        assert_eq!(
            blocks[0],
            ContentBlock::InputText(TextBlock {
                text: "图一".to_string()
            })
        );
        match &blocks[1] {
            ContentBlock::InputImage(image) => {
                assert_eq!(image.image_url.as_deref(), Some("data:image/png;base64,A"));
            }
            other => panic!("应为 input_image 块：{other:?}"),
        }
        assert_eq!(
            blocks[2],
            ContentBlock::InputText(TextBlock {
                text: "图二".to_string()
            })
        );
        match &blocks[3] {
            ContentBlock::InputImage(image) => {
                assert_eq!(image.image_url.as_deref(), Some("data:image/png;base64,B"));
            }
            other => panic!("应为 input_image 块：{other:?}"),
        }
    }

    #[test]
    fn system_prompt_maps_to_instructions() {
        let mut req = base_request(vec![ChatMessage::text(MessageRole::User, "你好")]);
        req.system = Some("系统提示".to_string());
        let request = to_deepseek_request(&req).expect("request");
        assert_eq!(request.instructions.as_deref(), Some("系统提示"));
        // 系统提示不应重复出现在 input 消息里。
        match request.input.expect("input") {
            ResponseInput::Items(items) => {
                assert_eq!(items.len(), 1, "应只有一条 user message：{items:?}");
            }
            other => panic!("应为 item 列表：{other:?}"),
        }
    }

    #[test]
    fn assistant_thinking_maps_to_reasoning_item_before_message() {
        let req = base_request(vec![ChatMessage::new(
            MessageRole::Assistant,
            vec![
                MessageContent::Thinking(ThinkingContent {
                    thinking: "需要先查询当前数据".to_string(),
                    signature: None,
                }),
                MessageContent::Text("查询完成".to_string()),
                MessageContent::ToolCall(ToolCall {
                    id: "call_1".to_string(),
                    name: "current_time".to_string(),
                    arguments: json!({}),
                }),
            ],
        )]);
        let request = to_deepseek_request(&req).expect("request");
        let items = match request.input.expect("input") {
            ResponseInput::Items(items) => items,
            other => panic!("应为 item 列表：{other:?}"),
        };
        // reasoning item 在 message / function_call 之前。
        assert!(matches!(items[0], ResponseInputItem::Reasoning(_)));
        assert!(matches!(items[1], ResponseInputItem::Message(_)));
        assert!(matches!(items[2], ResponseInputItem::FunctionCall(_)));
        let ResponseInputItem::Reasoning(reasoning) = &items[0] else {
            unreachable!();
        };
        let content = reasoning.content.as_ref().expect("reasoning content");
        let blocks = match content {
            tiangong_deepseek::types::MessageContent::Blocks(blocks) => blocks,
            other => panic!("应为块列表：{other:?}"),
        };
        assert_eq!(
            blocks[0],
            ContentBlock::ReasoningText(TextBlock {
                text: "需要先查询当前数据".to_string()
            })
        );
    }

    #[test]
    fn tool_result_maps_to_function_call_output() {
        let req = base_request(vec![
            ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageContent::ToolCall(ToolCall {
                    id: "call_9".to_string(),
                    name: "read_file".to_string(),
                    arguments: json!({"path": "/tmp/x"}),
                })],
            ),
            ChatMessage::new(
                MessageRole::Tool,
                vec![MessageContent::ToolResult(crate::tool::ToolResult {
                    tool_call_id: "call_9".to_string(),
                    content: crate::tool::ToolResultContent::Text("内容".to_string()),
                    is_error: false,
                })],
            ),
        ]);
        let request = to_deepseek_request(&req).expect("request");
        let items = match request.input.expect("input") {
            ResponseInput::Items(items) => items,
            other => panic!("应为 item 列表：{other:?}"),
        };
        assert!(matches!(items[0], ResponseInputItem::FunctionCall(_)));
        let ResponseInputItem::FunctionCallOutput(output) = &items[1] else {
            panic!("应为 function_call_output：{items:?}");
        };
        assert_eq!(output.call_id, "call_9");
        assert_eq!(
            output.output,
            FunctionOutputContent::Text("内容".to_string())
        );
    }

    #[test]
    fn reasoning_effort_maps_to_none_low_high_max() {
        for (effort, expected) in [
            (ReasoningEffort::None, ReasoningEffortLevel::None),
            (ReasoningEffort::Low, ReasoningEffortLevel::Low),
            (ReasoningEffort::Medium, ReasoningEffortLevel::High),
            (ReasoningEffort::High, ReasoningEffortLevel::High),
            (ReasoningEffort::Max, ReasoningEffortLevel::Max),
        ] {
            let mut req = base_request(vec![ChatMessage::text(MessageRole::User, "参数")]);
            req.reasoning_effort = effort;
            let request = to_deepseek_request(&req).expect("request");
            assert_eq!(
                request.reasoning.and_then(|config| config.effort),
                Some(expected),
                "{effort:?} 映射错误"
            );
        }
    }

    #[test]
    fn thinking_disabled_sends_none_effort_explicitly() {
        // 不传 reasoning 时服务端默认开启思考，关闭必须显式 effort=none。
        let request = to_deepseek_request(&base_request(vec![ChatMessage::text(
            MessageRole::User,
            "参数",
        )]))
        .expect("request");
        assert_eq!(
            request.reasoning.and_then(|config| config.effort),
            Some(ReasoningEffortLevel::None)
        );
    }

    #[test]
    fn temperature_top_p_omitted_when_thinking_enabled() {
        let mut req = base_request(vec![ChatMessage::text(MessageRole::User, "参数")]);
        req.reasoning_effort = ReasoningEffort::High;
        req.temperature = Some(0.7);
        req.top_p = Some(0.9);
        let request = to_deepseek_request(&req).expect("request");
        assert_eq!(request.temperature, None, "思考模式不应发送 temperature");
        assert_eq!(request.top_p, None, "思考模式不应发送 top_p");
    }

    #[test]
    fn temperature_top_p_sent_when_thinking_disabled() {
        let mut req = base_request(vec![ChatMessage::text(MessageRole::User, "参数")]);
        req.temperature = Some(0.7);
        req.top_p = Some(0.9);
        let request = to_deepseek_request(&req).expect("request");
        assert_eq!(request.temperature, Some(0.7));
        assert_eq!(request.top_p, Some(0.9));
    }

    #[test]
    fn stop_sequences_are_dropped_silently() {
        // Responses API 无 stop 参数；请求序列化后不应出现 stop 字段。
        let request = to_deepseek_request(&base_request(vec![ChatMessage::text(
            MessageRole::User,
            "参数",
        )]))
        .expect("request");
        let body = serde_json::to_value(&request).unwrap();
        assert!(body.get("stop").is_none());
    }

    #[test]
    fn tool_choice_named_tool_uses_responses_shape() {
        // Responses 指定工具形态为 {"type":"function","name":...}（无嵌套 function）。
        assert_eq!(
            build_tool_choice(Some(&ToolChoice::Tool("my_fn".to_string()))),
            Some(json!({"type": "function", "name": "my_fn"}))
        );
        assert_eq!(
            build_tool_choice(Some(&ToolChoice::Any)),
            Some(json!("required"))
        );
        assert_eq!(
            build_tool_choice(Some(&ToolChoice::None)),
            Some(json!("none"))
        );
    }

    #[test]
    fn text_format_from_metadata_variants() {
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "response_format".to_string(),
            Value::String("json_object".to_string()),
        );
        let mut req = base_request(vec![ChatMessage::text(MessageRole::User, "参数")]);
        req.metadata = Some(metadata);
        let request = to_deepseek_request(&req).expect("request");
        assert_eq!(
            request.text,
            Some(TextFormatConfig {
                format: Some(TextFormat::JsonObject)
            })
        );

        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "response_format".to_string(),
            json!({"type": "json_schema", "name": "calendar", "schema": {"type": "object"}}),
        );
        let mut req = base_request(vec![ChatMessage::text(MessageRole::User, "参数")]);
        req.metadata = Some(metadata);
        let request = to_deepseek_request(&req).expect("request");
        assert_eq!(
            request.text,
            Some(TextFormatConfig {
                format: Some(TextFormat::JsonSchema {
                    name: "calendar".to_string(),
                    schema: json!({"type": "object"})
                })
            })
        );
    }

    #[test]
    fn text_format_absent_without_metadata() {
        let request = to_deepseek_request(&base_request(vec![ChatMessage::text(
            MessageRole::User,
            "参数",
        )]))
        .expect("request");
        assert_eq!(request.text, None);
    }

    fn response_object(output: Vec<ResponseOutputItem>) -> ResponseObject {
        ResponseObject {
            id: "resp_test".to_string(),
            object: "response".to_string(),
            created_at: 0,
            status: ResponseStatus::Completed,
            error: None,
            incomplete_details: None,
            model: "deepseek-v4-pro".to_string(),
            output,
            usage: Some(tiangong_deepseek::types::ResponseUsage {
                input_tokens: 100,
                input_tokens_details: Some(tiangong_deepseek::types::InputTokensDetails {
                    cached_tokens: 80,
                }),
                output_tokens: 2,
                output_tokens_details: None,
                total_tokens: 102,
            }),
        }
    }

    fn function_call_item(call_id: &str, name: &str, arguments: &str) -> ResponseOutputItem {
        ResponseOutputItem::FunctionCall(tiangong_deepseek::types::OutputFunctionCall {
            id: Some("fc_1".to_string()),
            status: None,
            call_id: call_id.to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
        })
    }

    #[test]
    fn invalid_tool_arguments_become_parse_error() {
        let response = response_object(vec![function_call_item(
            "call_bad",
            "read_file",
            "{\"path\":",
        )]);
        let result = from_deepseek_response(response, true).expect("response");
        let MessageContent::ToolCall(call) = &result.assistant_message.content[0] else {
            panic!("expected tool call");
        };
        assert!(
            call.arguments
                .get("__parse_error")
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains("工具参数 JSON 无效"))
        );
    }

    #[test]
    fn empty_tool_arguments_become_empty_object() {
        let response = response_object(vec![function_call_item("call_empty", "read_file", "")]);
        let result = from_deepseek_response(response, true).expect("response");
        let MessageContent::ToolCall(call) = &result.assistant_message.content[0] else {
            panic!("expected tool call");
        };
        assert_eq!(
            call.arguments,
            serde_json::json!({}),
            "空参数应按无参调用处理（空对象），不再构成解析错误"
        );
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn usage_maps_cached_tokens() {
        let response = response_object(vec![ResponseOutputItem::Message(
            tiangong_deepseek::types::OutputMessage {
                id: Some("msg_1".to_string()),
                status: None,
                role: "assistant".to_string(),
                content: vec![OutputContentBlock::OutputText(TextBlock {
                    text: "OK".to_string(),
                })],
            },
        )]);
        let result = from_deepseek_response(response, false).expect("response");
        let usage = result.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 2);
        assert_eq!(usage.total_tokens, 102);
        assert_eq!(usage.prompt_cache_hit_tokens, Some(80));
        assert_eq!(usage.prompt_cache_miss_tokens, Some(20));
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn reasoning_and_message_items_split_into_fields() {
        let response = response_object(vec![
            ResponseOutputItem::Reasoning(tiangong_deepseek::types::OutputReasoning {
                id: Some("rs_1".to_string()),
                status: None,
                content: vec![OutputContentBlock::ReasoningText(TextBlock {
                    text: "思考过程".to_string(),
                })],
            }),
            ResponseOutputItem::Message(tiangong_deepseek::types::OutputMessage {
                id: Some("msg_1".to_string()),
                status: None,
                role: "assistant".to_string(),
                content: vec![OutputContentBlock::OutputText(TextBlock {
                    text: "答案".to_string(),
                })],
            }),
        ]);
        let result = from_deepseek_response(response, false).expect("response");
        assert_eq!(result.reasoning_content.as_deref(), Some("思考过程"));
        let MessageContent::Text(text) = &result.assistant_message.content[0] else {
            panic!("应为纯文本");
        };
        assert_eq!(text, "答案");
    }

    #[test]
    fn dsml_text_without_tools_is_kept_verbatim() {
        // 无 tools 时（总结阶段），即使 content 含 DSML 字面量也不应误判为工具调用。
        let response = response_object(vec![ResponseOutputItem::Message(
            tiangong_deepseek::types::OutputMessage {
                id: None,
                status: None,
                role: "assistant".to_string(),
                content: vec![OutputContentBlock::OutputText(TextBlock {
                    text: "讨论 DSML 协议：\n</｜｜DSML｜｜tool_calls>".to_string(),
                })],
            },
        )]);
        let result = from_deepseek_response(response, false).expect("response");
        assert_eq!(result.assistant_message.content.len(), 1);
        let MessageContent::Text(text) = &result.assistant_message.content[0] else {
            panic!("应为纯文本");
        };
        assert!(text.contains("讨论 DSML 协议"), "文本应原样保留：{text}");
    }

    #[test]
    fn dsml_text_with_tools_falls_back_to_tool_calls() {
        let dsml = "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"read_file\">\n<｜｜DSML｜｜parameter name=\"path\" string=\"true\">/tmp/x</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>";
        let response = response_object(vec![ResponseOutputItem::Message(
            tiangong_deepseek::types::OutputMessage {
                id: None,
                status: None,
                role: "assistant".to_string(),
                content: vec![OutputContentBlock::OutputText(TextBlock {
                    text: format!("我来读取文件。\n{dsml}"),
                })],
            },
        )]);
        let result = from_deepseek_response(response, true).expect("response");
        let MessageContent::ToolCall(call) = &result.assistant_message.content[1] else {
            panic!("应兜底解析出工具调用：{:?}", result.assistant_message);
        };
        assert_eq!(call.name, "read_file");
        assert!(
            call.id.starts_with("textcall_"),
            "应使用全局唯一 textcall id（不依赖执行顺序）：{}",
            call.id
        );
        assert_eq!(call.arguments, json!({"path": "/tmp/x"}));
        let MessageContent::Text(text) = &result.assistant_message.content[0] else {
            panic!("说明文字应保留");
        };
        assert_eq!(text, "我来读取文件。");
        // stop_reason 保持服务端语义（无结构化 function_call item → 自然结束），
        // 工具循环由内容中的 ToolCall 驱动，与 chat 版 DSML 兜底行为一致。
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn incomplete_response_maps_max_tokens_stop_reason() {
        let mut response = response_object(Vec::new());
        response.status = ResponseStatus::Incomplete;
        response.incomplete_details = Some(tiangong_deepseek::types::IncompleteDetails {
            reason: Some("max_output_tokens".to_string()),
        });
        let result = from_deepseek_response(response, false).expect("response");
        assert_eq!(result.stop_reason, Some(StopReason::MaxTokens));
    }

    #[test]
    fn failed_response_returns_error_instead_of_empty_reply() {
        // failed 是请求级错误：必须返回 Err（对齐 chat 版 choices 缺失行为），
        // 不得退化为空回复 + Other("failed") 被上层当作正常截断。
        let mut response = response_object(Vec::new());
        response.status = ResponseStatus::Failed;
        response.error = Some(tiangong_deepseek::types::ResponseError {
            code: Some("server_error".to_string()),
            message: Some("内部错误".to_string()),
        });
        let err = from_deepseek_response(response, false).expect_err("应返回错误");
        assert!(
            err.to_string().contains("failed") && err.to_string().contains("内部错误"),
            "错误信息应包含状态与详情：{err}"
        );
    }

    #[test]
    fn empty_input_without_instructions_is_rejected() {
        // 官方要求 input 与 instructions 至少一个；空消息流且无系统提示时
        // 提前给出可读错误，而不是等服务端 400。
        let req = base_request(Vec::new());
        let err = to_deepseek_request(&req).expect_err("应拒绝空请求");
        assert!(
            err.to_string().contains("input 与 instructions"),
            "错误应说明原因：{err}"
        );
    }

    #[test]
    fn unknown_output_item_does_not_break_parsing() {
        // 官方文档列出 tool_search_call 等新增输出 item；SDK 未知类型兜底为
        // Unknown，解析不应失败且不应影响其余 item。
        let raw = json!({
            "id": "resp_x",
            "object": "response",
            "created_at": 0,
            "status": "completed",
            "model": "deepseek-v4-pro",
            "output": [
                {"type": "tool_search_call", "id": "ts_1", "status": "completed"},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "OK"}
                ]}
            ]
        });
        let response: ResponseObject =
            serde_json::from_value(raw).expect("未知 item 不应导致解析失败");
        let result = from_deepseek_response(response, false).expect("response");
        let MessageContent::Text(text) = &result.assistant_message.content[0] else {
            panic!("应为纯文本");
        };
        assert_eq!(text, "OK");
    }
}
