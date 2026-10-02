use std::collections::BTreeMap;

use serde_json::Value;
use tiangong_anthropic::types::{
    CacheControl, ContentBlock, ContentBlockDeltaData, ContentBlockParam, ContentBlockStartData,
    EffortLevel, ImageSourceParam, Message as AnthropicMessage,
    MessageRole as AnthropicMessageRole, MessagesCreateRequest, MessagesCreateResponse,
    OutputConfig, StreamEvent, SystemContent, TextBlock, ThinkingConfig, Tool as AnthropicTool,
    ToolChoice as AnthropicToolChoice, Usage,
};

use crate::error::LlmError;
use crate::message::{ChatMessage, MessageContent, MessageRole, ThinkingContent};
use crate::request::{ProviderRequest, ReasoningEffort};
use crate::response::{ProviderResponse, StopReason};
use crate::stream::ProviderStreamEvent;
use crate::tool::{ToolCall, ToolChoice, ToolResult, ToolResultContent};
use crate::usage::TokenUsageData;

pub(super) fn to_anthropic_request(
    request: &ProviderRequest,
) -> Result<MessagesCreateRequest, LlmError> {
    let mut messages = request
        .messages
        .iter()
        .filter_map(map_message)
        .collect::<Result<Vec<_>, _>>()?;

    let mut tools = build_tools(request);

    let tool_choice = request.tool_choice.as_ref().map(|choice| match choice {
        ToolChoice::Auto => AnthropicToolChoice::Auto,
        ToolChoice::Any => AnthropicToolChoice::Any,
        ToolChoice::Tool(name) => AnthropicToolChoice::Tool {
            name: name.clone(),
            disable_parallel_tool_use: None,
        },
        ToolChoice::None => AnthropicToolChoice::None,
    });

    let thinking = map_thinking_config(request);

    // 提示缓存断点（官方每请求最多 4 个）：断点标记缓存写入位置，命中由
    // 服务端按前缀内容匹配（请求断点处及更早已缓存前缀均可命中，并不依赖
    // 断点位置逐轮对齐——ReAct 循环每轮追加多条消息，上轮断点位置必然被
    // 甩开）。布局：tools 尾、system 尾保住固定大头；消息尾两断点保证
    // 尾部内容可写缓存。实测多轮对话命中率 97-100%，每轮未命中仅当轮
    // 新增量（~33K 固定前缀+全量历史的旧形态为 42-79%）。缓存 5 分钟
    // 不活动会被服务端驱逐，空闲后首轮 0 命中重建属预期行为。
    let breakpoint = || Some(CacheControl::ephemeral());
    if let Some(last_tool) = tools.as_mut().and_then(|tools| tools.last_mut()) {
        last_tool.cache_control = breakpoint();
    }
    let system = request.system.as_ref().and_then(|system| {
        let text = system.trim();
        if text.is_empty() {
            return None;
        }
        Some(SystemContent::Blocks(vec![TextBlock::new(
            system.clone(),
            breakpoint(),
        )]))
    });
    mark_message_tail_breakpoints(&mut messages, breakpoint);

    // 官方约束：开启思考时 temperature 只能为 1、top_p/top_k 不可自定义，
    // 省略字段走协议默认即满足；Claude Code 同款行为，兼容端点用各自
    // 默认采样。丢弃用户配置时留 debug 痕迹，便于排查"温度不生效"类困惑。
    let thinking_enabled = matches!(thinking, Some(ThinkingConfig::Adaptive));
    if thinking_enabled {
        if request.temperature.is_some() {
            tracing::debug!(
                model = %request.model,
                "thinking enabled: temperature dropped per Anthropic constraint"
            );
        }
        if request.top_p.is_some() {
            tracing::debug!(
                model = %request.model,
                "thinking enabled: top_p dropped per Anthropic constraint"
            );
        }
    }
    Ok(MessagesCreateRequest {
        model: request.model.clone(),
        max_tokens: request.max_tokens,
        system,
        messages,
        temperature: request.temperature.filter(|_| !thinking_enabled),
        stop_sequences: (!request.stop_sequences.is_empty())
            .then(|| request.stop_sequences.clone()),
        top_p: request.top_p.filter(|_| !thinking_enabled),
        metadata: request.metadata.clone(),
        tools,
        tool_choice,
        stream: None,
        thinking,
        output_config: map_output_config(request),
    })
}

/// 在消息尾部的两个可标记内容块（Text/ToolResult）上放断点，保证请求
/// 前缀的尾部有断点可写缓存。从尾部向前找：断点必须落在前缀真正结束
/// 的块上才有效。
fn mark_message_tail_breakpoints(
    messages: &mut [AnthropicMessage],
    mut breakpoint: impl FnMut() -> Option<CacheControl>,
) {
    let mut placed = 0;
    'outer: for message in messages.iter_mut().rev() {
        for block in message.content.iter_mut().rev() {
            match block {
                ContentBlockParam::Text { cache_control, .. }
                | ContentBlockParam::ToolResult { cache_control, .. } => {
                    *cache_control = breakpoint();
                    placed += 1;
                    if placed == 2 {
                        break 'outer;
                    }
                }
                _ => {}
            }
        }
    }
}

fn build_tools(request: &ProviderRequest) -> Option<Vec<AnthropicTool>> {
    if request.tools.is_empty() {
        return None;
    }
    Some(
        request
            .tools
            .iter()
            .map(|tool| AnthropicTool {
                name: tool.name.clone(),
                description: Some(tool.description.clone()),
                input_schema: tool.input_schema.clone(),
                cache_control: None,
            })
            .collect(),
    )
}

fn map_thinking_config(request: &ProviderRequest) -> Option<ThinkingConfig> {
    // reasoning_effort 有值即开启思考，统一 adaptive 形态（不区分模型：
    // Claude 新模型仅接受该形态，智谱 GLM 等兼容端点实测已完整支持）；
    // 深度档位由 map_output_config 对齐到 output_config.effort。
    // 关闭思考时显式下发 disabled：智谱 GLM 等兼容端点省略 thinking
    // 字段会回落到"默认开启思考"；Claude 系模型不接受显式 disabled，
    // 由 AnthropicClient::normalize_thinking_for_model 按模型去掉。
    Some(if request.reasoning_effort.is_thinking_enabled() {
        ThinkingConfig::Adaptive
    } else {
        ThinkingConfig::Disabled
    })
}

/// adaptive 思考深度：天工 ReasoningEffort 档位与 Anthropic effort
/// 档位语义一致，直接对齐映射；未开启思考不下发 output_config
/// （effort 省略时走模型默认档）。
fn map_output_config(request: &ProviderRequest) -> Option<OutputConfig> {
    if !request.reasoning_effort.is_thinking_enabled() {
        return None;
    }
    let effort = match request.reasoning_effort {
        ReasoningEffort::None => return None,
        ReasoningEffort::Low => EffortLevel::Low,
        ReasoningEffort::Medium => EffortLevel::Medium,
        ReasoningEffort::High => EffortLevel::High,
        ReasoningEffort::Max => EffortLevel::Max,
    };
    Some(OutputConfig {
        effort: Some(effort),
    })
}

fn map_message(message: &ChatMessage) -> Option<Result<AnthropicMessage, LlmError>> {
    match message.role {
        MessageRole::System => None,
        MessageRole::User | MessageRole::Assistant | MessageRole::Tool => {
            let role = match message.role {
                MessageRole::Assistant => AnthropicMessageRole::Assistant,
                _ => AnthropicMessageRole::User,
            };

            let content = message
                .content
                .iter()
                .map(map_content)
                .collect::<Result<Vec<_>, _>>();
            Some(content.map(|content| AnthropicMessage { role, content }))
        }
    }
}

fn map_content(content: &MessageContent) -> Result<ContentBlockParam, LlmError> {
    match content {
        MessageContent::Text(text) => Ok(ContentBlockParam::Text {
            text: text.clone(),
            cache_control: None,
        }),
        MessageContent::Thinking(thinking) => Ok(ContentBlockParam::Thinking {
            thinking: thinking.thinking.clone(),
            signature: thinking.signature.clone(),
        }),
        MessageContent::RedactedThinking(data) => {
            Ok(ContentBlockParam::RedactedThinking { data: data.clone() })
        }
        MessageContent::ToolCall(tool_call) => Ok(ContentBlockParam::ToolUse {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            input: tool_call.arguments.clone(),
        }),
        MessageContent::ToolResult(tool_result) => Ok(ContentBlockParam::ToolResult {
            tool_use_id: tool_result.tool_call_id.clone(),
            content: Some(match &tool_result.content {
                ToolResultContent::Text(text) => Value::String(text.clone()),
                ToolResultContent::Json(value) => value.clone(),
            }),
            is_error: Some(tool_result.is_error),
            cache_control: None,
        }),
        MessageContent::Image(image) => {
            let data = image
                .data
                .split_once(',')
                .map(|(_, data)| data)
                .unwrap_or(&image.data)
                .to_string();
            Ok(ContentBlockParam::Image {
                source: ImageSourceParam {
                    source_type: "base64".to_string(),
                    media_type: image.mime_type.clone(),
                    data,
                },
            })
        }
        MessageContent::File(file) => {
            let data = file
                .data
                .split_once(',')
                .map(|(_, data)| data)
                .unwrap_or(&file.data)
                .to_string();
            Ok(ContentBlockParam::Document {
                source: ImageSourceParam {
                    source_type: "base64".to_string(),
                    media_type: file.mime_type.clone(),
                    data,
                },
            })
        }
    }
}

pub(super) fn from_anthropic_response(
    response: MessagesCreateResponse,
) -> Result<ProviderResponse, LlmError> {
    let raw = serde_json::to_value(&response)
        .map(Some)
        .map_err(|err| LlmError::Serialization(err.to_string()))?;

    let mut reasoning_chunks = Vec::new();
    let assistant_content = response
        .content
        .into_iter()
        .filter_map(
            |content| match from_content(content, &mut reasoning_chunks) {
                Ok(Some(item)) => Some(Ok(item)),
                Ok(None) => None,
                Err(err) => Some(Err(err)),
            },
        )
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ProviderResponse {
        id: Some(response.id),
        model: Some(response.model),
        assistant_message: ChatMessage {
            role: MessageRole::Assistant,
            content: assistant_content,
        },
        reasoning_content: (!reasoning_chunks.is_empty()).then(|| reasoning_chunks.join("")),
        stop_reason: response.stop_reason.as_deref().map(map_stop_reason),
        usage: response.usage.map(map_usage),
        raw,
    })
}

fn from_content(
    content: ContentBlock,
    reasoning_chunks: &mut Vec<String>,
) -> Result<Option<MessageContent>, LlmError> {
    match content {
        ContentBlock::Text { text } => Ok(Some(MessageContent::Text(text))),
        ContentBlock::ToolUse { id, name, input } => Ok(Some(MessageContent::ToolCall(ToolCall {
            id,
            name,
            arguments: input,
        }))),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => Ok(Some(MessageContent::ToolResult(ToolResult {
            tool_call_id: tool_use_id,
            content: match content {
                Some(Value::String(text)) => ToolResultContent::Text(text),
                Some(value) => ToolResultContent::Json(value),
                None => ToolResultContent::Text(String::new()),
            },
            is_error: is_error.unwrap_or(false),
        }))),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            reasoning_chunks.push(thinking.clone());
            Ok(Some(MessageContent::Thinking(ThinkingContent {
                thinking,
                signature,
            })))
        }
        ContentBlock::RedactedThinking { data } => Ok(Some(MessageContent::RedactedThinking(data))),
        ContentBlock::Unknown => Ok(None),
    }
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        other => StopReason::Other(other.to_string()),
    }
}

fn map_usage(usage: Usage) -> TokenUsageData {
    let uncached = usage.input_tokens.unwrap_or(0) as usize;
    let read = usage.cache_read_input_tokens.map(|value| value as usize);
    let created = usage
        .cache_creation_input_tokens
        .map(|value| value as usize);
    let miss = uncached + created.unwrap_or(0);
    let mut mapped = TokenUsageData::new(
        miss + read.unwrap_or(0),
        usage.output_tokens.unwrap_or(0) as usize,
    );
    mapped.prompt_cache_hit_tokens = read;
    // GLM 可只返回 cache_read；完全不提供缓存字段的旧响应仍保持未知。
    if usage.input_tokens.is_some() && (read.is_some() || created.is_some()) {
        mapped.prompt_cache_miss_tokens = Some(miss);
    }
    mapped
}

#[derive(Default)]
pub(super) struct AnthropicStreamState {
    tool_calls: BTreeMap<usize, ToolCallAccumulator>,
    usage: Usage,
}

impl AnthropicStreamState {
    fn merge_usage(&mut self, next: Usage) -> TokenUsageData {
        // message_delta 通常只包含累计输出；缺失字段沿用 message_start。
        // 必须先合并原始字段，再计算完整输入，不能把各帧的输入总量相加。
        if next.input_tokens.is_some() {
            self.usage.input_tokens = next.input_tokens;
        }
        if next.output_tokens.is_some() {
            self.usage.output_tokens = next.output_tokens;
        }
        if next.cache_read_input_tokens.is_some() {
            self.usage.cache_read_input_tokens = next.cache_read_input_tokens;
        }
        if next.cache_creation_input_tokens.is_some() {
            self.usage.cache_creation_input_tokens = next.cache_creation_input_tokens;
        }
        map_usage(self.usage.clone())
    }
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
}

pub(super) fn map_stream_event(
    state: &mut AnthropicStreamState,
    event: StreamEvent,
) -> Result<Vec<ProviderStreamEvent>, LlmError> {
    match event {
        StreamEvent::MessageStart { message } => {
            let mut events = vec![ProviderStreamEvent::MessageStart];
            if let Some(usage) = message.usage {
                events.push(ProviderStreamEvent::Usage(state.merge_usage(usage)));
            }
            Ok(events)
        }
        StreamEvent::ContentBlockStart {
            index,
            content_block,
        } => {
            match content_block {
                ContentBlockStartData::ToolUse { id, name, input } => {
                    state
                        .tool_calls
                        .insert(index, ToolCallAccumulator { id: id.clone() });
                    Ok(vec![ProviderStreamEvent::ToolCallStart(ToolCall {
                        id,
                        name,
                        arguments: input,
                    })])
                }
                ContentBlockStartData::Thinking {
                    thinking,
                    signature,
                } if !thinking.is_empty() => {
                    let mut events = vec![ProviderStreamEvent::ReasoningDelta(thinking)];
                    if !signature.is_empty() {
                        events.push(ProviderStreamEvent::ReasoningSignatureDelta(signature));
                    }
                    Ok(events)
                }
                ContentBlockStartData::Thinking { signature, .. } if !signature.is_empty() => Ok(
                    vec![ProviderStreamEvent::ReasoningSignatureDelta(signature)],
                ),
                ContentBlockStartData::RedactedThinking { .. } => Ok(Vec::new()),
                ContentBlockStartData::Text { text } if !text.is_empty() => {
                    Ok(vec![ProviderStreamEvent::TextDelta(text)])
                }
                _ => Ok(Vec::new()),
            }
        }
        StreamEvent::ContentBlockDelta { index, delta } => match delta {
            ContentBlockDeltaData::TextDelta { text } => {
                Ok(vec![ProviderStreamEvent::TextDelta(text)])
            }
            ContentBlockDeltaData::InputJsonDelta { partial_json } => {
                let call_id = state
                    .tool_calls
                    .get(&index)
                    .map(|call| call.id.clone())
                    .unwrap_or_else(|| format!("tool_call_{index}"));
                Ok(vec![ProviderStreamEvent::ToolCallDelta {
                    call_id,
                    partial_json,
                }])
            }
            ContentBlockDeltaData::ThinkingDelta { thinking } => {
                Ok(vec![ProviderStreamEvent::ReasoningDelta(thinking)])
            }
            ContentBlockDeltaData::SignatureDelta { signature } => {
                Ok(vec![ProviderStreamEvent::ReasoningSignatureDelta(
                    signature,
                )])
            }
            ContentBlockDeltaData::Unknown => Ok(Vec::new()),
        },
        StreamEvent::ContentBlockStop { index } => {
            if let Some(call) = state.tool_calls.remove(&index) {
                Ok(vec![ProviderStreamEvent::ToolCallEnd { call_id: call.id }])
            } else {
                Ok(Vec::new())
            }
        }
        StreamEvent::MessageDelta { delta, usage } => {
            let mut events = Vec::new();
            if let Some(usage) = usage {
                events.push(ProviderStreamEvent::Usage(state.merge_usage(usage)));
            }
            if let Some(reason) = delta.stop_reason {
                events.push(ProviderStreamEvent::MessageEnd {
                    stop_reason: Some(map_stop_reason(&reason)),
                });
            }
            Ok(events)
        }
        StreamEvent::MessageStop => Ok(vec![ProviderStreamEvent::MessageEnd { stop_reason: None }]),
        StreamEvent::Error { message } => Ok(vec![ProviderStreamEvent::Error(message)]),
        StreamEvent::Ping | StreamEvent::Unknown => Ok(Vec::new()),
    }
}

pub(super) fn map_stream_error(
    error: crate::error::LlmError,
) -> Vec<Result<ProviderStreamEvent, LlmError>> {
    vec![Err(error)]
}
