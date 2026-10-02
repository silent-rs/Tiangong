use std::sync::Arc;

use crate::request::ReasoningEffort;

use async_trait::async_trait;
use futures_util::stream;
use serde_json::json;
use tiangong_anthropic::types::{
    ContentBlock, ContentBlockDeltaData, ContentBlockStartData, EventStream, MessageStartData,
    MessagesCreateRequest, MessagesCreateResponse, StreamEvent, Usage,
};

use crate::message::{ChatMessage, MessageContent, MessageRole};
use crate::provider::LlmProvider;
use crate::providers::anthropic::client::{AnthropicClient, AnthropicTransport};
use crate::providers::anthropic::config::AnthropicConfig;
use crate::providers::anthropic::provider::AnthropicProvider;
use crate::request::ProviderRequest;
use crate::stream::ProviderStreamEvent;
use crate::tool::{ToolChoice, ToolResult, ToolResultContent, ToolSpec};

#[derive(Clone, Default)]
struct MockAnthropicTransport {
    response: Option<MessagesCreateResponse>,
    stream_events: Vec<Result<StreamEvent, crate::error::LlmError>>,
}

#[async_trait]
impl AnthropicTransport for MockAnthropicTransport {
    async fn create(
        &self,
        _request: MessagesCreateRequest,
    ) -> Result<MessagesCreateResponse, crate::error::LlmError> {
        Ok(self.response.clone().expect("mock response"))
    }

    async fn create_stream(
        &self,
        _request: MessagesCreateRequest,
    ) -> Result<EventStream, crate::error::LlmError> {
        let items = self
            .stream_events
            .clone()
            .into_iter()
            .map(|item| match item {
                Ok(event) => Ok(event),
                Err(err) => Err(tiangong_anthropic::AnthropicError::Stream(err.to_string())),
            });
        Ok(Box::pin(stream::iter(items)))
    }

    async fn list_models(
        &self,
    ) -> Result<Vec<crate::model::ProviderModelInfo>, crate::error::LlmError> {
        Ok(vec![crate::model::ProviderModelInfo {
            id: "claude-3-7-sonnet".to_string(),
            display_name: Some("Claude 3.7 Sonnet".to_string()),
            ..Default::default()
        }])
    }
}

fn sample_request() -> ProviderRequest {
    ProviderRequest {
        session_id: None,
        model: "claude-3-7-sonnet".to_string(),
        system: Some("你是测试助手".to_string()),
        messages: vec![
            ChatMessage::text(MessageRole::User, "你好"),
            ChatMessage {
                role: MessageRole::Assistant,
                content: vec![MessageContent::ToolCall(crate::tool::ToolCall {
                    id: "call_1".to_string(),
                    name: "search_web".to_string(),
                    arguments: json!({"q": "rust"}),
                })],
            },
            ChatMessage {
                role: MessageRole::Tool,
                content: vec![MessageContent::ToolResult(ToolResult {
                    tool_call_id: "call_1".to_string(),
                    content: ToolResultContent::Json(json!({"answer": "ok"})),
                    is_error: false,
                })],
            },
        ],
        tools: vec![ToolSpec {
            name: "search_web".to_string(),
            description: "搜索网页".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "q": { "type": "string" }
                },
                "required": ["q"]
            }),
        }],
        tool_choice: Some(ToolChoice::Auto),
        max_tokens: 1024,
        temperature: Some(0.2),
        top_p: None,
        stop_sequences: vec!["STOP".to_string()],
        metadata: None,
        reasoning_effort: ReasoningEffort::High,
    }
}

#[test]
fn test_request_mapping_with_system_and_tools() {
    let mapped = super::mapping::to_anthropic_request(&sample_request()).expect("mapped request");
    assert_eq!(mapped.model, "claude-3-7-sonnet");
    // system 块化携带断点：文本内容不变，尾块带 ephemeral 标记。
    assert!(matches!(
        mapped.system.as_ref(),
        Some(tiangong_anthropic::types::SystemContent::Blocks(blocks))
            if blocks.len() == 1
                && blocks[0].text == "你是测试助手"
                && blocks[0].cache_control.is_some()
    ));
    assert_eq!(mapped.messages.len(), 3);
    assert_eq!(mapped.tools.as_ref().map(Vec::len), Some(1));
    // 全模型统一 adaptive 形态（智谱等兼容端点实测已支持），档位在 output_config。
    assert_eq!(
        mapped.thinking,
        Some(tiangong_anthropic::types::ThinkingConfig::Adaptive)
    );
    assert_eq!(
        mapped.output_config,
        Some(tiangong_anthropic::types::OutputConfig {
            effort: Some(tiangong_anthropic::types::EffortLevel::High)
        })
    );
    // 开启思考时官方要求 temperature 为 1：省略字段而非透传 0.2。
    assert_eq!(mapped.temperature, None);
    assert!(matches!(
        mapped.tool_choice,
        Some(tiangong_anthropic::types::ToolChoice::Auto)
    ));
}

#[test]
fn test_temperature_kept_when_thinking_disabled() {
    let mut request = sample_request();
    request.reasoning_effort = ReasoningEffort::None;
    let mapped = super::mapping::to_anthropic_request(&request).expect("mapped request");
    assert_eq!(mapped.temperature, Some(0.2));
    assert_eq!(
        mapped.thinking,
        Some(tiangong_anthropic::types::ThinkingConfig::Disabled)
    );
}

#[test]
fn test_adaptive_model_maps_effort_to_output_config() {
    // opus-5-5 一代模型：thinking 用 adaptive 形态，思考档位对齐到
    // output_config.effort；旧版预算格式已随全切移除。
    let mut request = sample_request();
    request.model = "claude-opus-5-5".to_string();
    let mapped = super::mapping::to_anthropic_request(&request).expect("mapped request");
    assert_eq!(
        mapped.thinking,
        Some(tiangong_anthropic::types::ThinkingConfig::Adaptive)
    );
    assert_eq!(
        mapped.output_config,
        Some(tiangong_anthropic::types::OutputConfig {
            effort: Some(tiangong_anthropic::types::EffortLevel::High)
        })
    );

    // 档位随 reasoning_effort 变化：Max 直达 max。
    request.reasoning_effort = ReasoningEffort::Max;
    let mapped = super::mapping::to_anthropic_request(&request).expect("mapped request");
    assert_eq!(
        mapped.output_config,
        Some(tiangong_anthropic::types::OutputConfig {
            effort: Some(tiangong_anthropic::types::EffortLevel::Max)
        })
    );

    // 关闭思考：显式 disabled（Claude 系由 client 层去掉），不发 output_config。
    request.reasoning_effort = ReasoningEffort::None;
    let mapped = super::mapping::to_anthropic_request(&request).expect("mapped request");
    assert_eq!(
        mapped.thinking,
        Some(tiangong_anthropic::types::ThinkingConfig::Disabled)
    );
    assert_eq!(mapped.output_config, None);
}

#[test]
fn test_unknown_claude_model_defaults_to_adaptive() {
    // 未收录的全新命名族：默认新模式，未来新模型无需改代码即自动生效。
    let mut request = sample_request();
    request.model = "claude-eclipse-9".to_string();
    let mapped = super::mapping::to_anthropic_request(&request).expect("mapped request");
    assert_eq!(
        mapped.thinking,
        Some(tiangong_anthropic::types::ThinkingConfig::Adaptive)
    );
    assert_eq!(
        mapped.output_config,
        Some(tiangong_anthropic::types::OutputConfig {
            effort: Some(tiangong_anthropic::types::EffortLevel::High)
        })
    );
}

#[test]
fn test_compat_endpoint_models_also_use_adaptive() {
    // 第三方 Anthropic 兼容端点（智谱 GLM，实测已完整支持 adaptive +
    // effort：low 档不思考、high 档思考）与旧官方模型名同样统一下发
    // adaptive + output_config。
    let mut request = sample_request();
    request.model = "glm-5.3".to_string();
    let mapped = super::mapping::to_anthropic_request(&request).expect("mapped request");
    assert_eq!(
        mapped.thinking,
        Some(tiangong_anthropic::types::ThinkingConfig::Adaptive)
    );
    assert_eq!(
        mapped.output_config,
        Some(tiangong_anthropic::types::OutputConfig {
            effort: Some(tiangong_anthropic::types::EffortLevel::High)
        })
    );
}

#[test]
fn test_top_p_dropped_when_thinking_enabled() {
    // 与 temperature 同款官方约束：开思考时 top_p 不可自定义。
    let mut request = sample_request();
    request.top_p = Some(0.9);
    let mapped = super::mapping::to_anthropic_request(&request).expect("mapped request");
    assert_eq!(mapped.top_p, None);
    request.reasoning_effort = ReasoningEffort::None;
    let mapped = super::mapping::to_anthropic_request(&request).expect("mapped request");
    assert_eq!(mapped.top_p, Some(0.9));
}

#[test]
fn test_cache_breakpoints_layout() {
    // 断点布局：仅最后一个工具、system 尾块、消息尾部两个块带标记。
    let mapped = super::mapping::to_anthropic_request(&sample_request()).expect("mapped request");
    let tools = mapped.tools.expect("tools");
    assert!(
        tools[..tools.len() - 1]
            .iter()
            .all(|tool| tool.cache_control.is_none())
    );
    assert!(tools.last().unwrap().cache_control.is_some());

    // sample_request 的可标记块（倒序）：tool 消息的 ToolResult、user 的 Text。
    let marked: Vec<bool> = mapped
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .map(|block| match block {
            tiangong_anthropic::types::ContentBlockParam::Text { cache_control, .. }
            | tiangong_anthropic::types::ContentBlockParam::ToolResult { cache_control, .. } => {
                cache_control.is_some()
            }
            _ => false,
        })
        .collect();
    assert_eq!(
        marked,
        vec![true, false, true],
        "user Text 与 tool ToolResult 带断点，assistant ToolUse 不带"
    );

    // 官方硬性契约：每请求断点总数不得超过 4，超出会被端点拒收。
    // 布局为 tools 尾(1) + system 尾(1) + 消息尾(2)，零冗余——新增断点
    // 前必须先削减现有布局，此断言用于钉住这条隐形契约。
    let total = tools.iter().filter(|t| t.cache_control.is_some()).count()
        + mapped
            .system
            .as_ref()
            .map(|s| match s {
                tiangong_anthropic::types::SystemContent::Blocks(blocks) => {
                    blocks.iter().filter(|b| b.cache_control.is_some()).count()
                }
                _ => 0,
            })
            .unwrap_or(0)
        + marked.iter().filter(|m| **m).count();
    assert!(
        total <= 4,
        "cache breakpoints exceed official cap of 4: {total}"
    );
}

#[test]
fn test_cache_breakpoint_serialization_shape() {
    use tiangong_anthropic::types::{CacheControl, ContentBlockParam};
    let block = ContentBlockParam::Text {
        text: "x".to_string(),
        cache_control: Some(CacheControl::ephemeral()),
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "text");
    assert_eq!(json["cache_control"]["type"], "ephemeral");

    let plain = serde_json::to_value(ContentBlockParam::Text {
        text: "x".to_string(),
        cache_control: None,
    })
    .unwrap();
    assert!(plain.get("cache_control").is_none());

    // system 块数组形态：每块必须带 "type":"text"，缺字段会被严格校验的
    // 端点（官方及 FastAPI 类兼容实现）拒收。
    let mapped = super::mapping::to_anthropic_request(&sample_request()).expect("mapped request");
    let system_json = serde_json::to_value(&mapped.system).unwrap();
    assert_eq!(system_json[0]["type"], "text");
    assert_eq!(system_json[0]["text"], "你是测试助手");
    assert_eq!(system_json[0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn test_tool_choice_none_maps_to_anthropic_none() {
    // ToolChoice::None 必须映射为 Anthropic 的 {"type": "none"}，
    // 在提供 tools schema 的同时禁止调用工具（如总结阶段）。
    let mut req = sample_request();
    req.tool_choice = Some(ToolChoice::None);
    let mapped = super::mapping::to_anthropic_request(&req).expect("mapped request");
    assert!(matches!(
        mapped.tool_choice,
        Some(tiangong_anthropic::types::ToolChoice::None)
    ));
    // tools schema 仍保留，保持 KV cache 前缀一致
    assert!(mapped.tools.is_some());
}

#[test]
fn test_tool_and_thinking_mapping_back_to_message_content() {
    let response = MessagesCreateResponse {
        id: "msg_1".to_string(),
        kind: "message".to_string(),
        role: tiangong_anthropic::types::MessageRole::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "先搜索一下".to_string(),
                signature: Some("sig".to_string()),
            },
            ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "search_web".to_string(),
                input: json!({"q": "rust"}),
            },
        ],
        model: "claude-3-7-sonnet".to_string(),
        stop_reason: Some("tool_use".to_string()),
        stop_sequence: None,
        usage: Some(Usage {
            input_tokens: Some(12),
            output_tokens: Some(7),
            ..Default::default()
        }),
    };

    let mapped = super::mapping::from_anthropic_response(response).expect("mapped response");
    assert_eq!(mapped.reasoning_content.as_deref(), Some("先搜索一下"));
    assert_eq!(
        mapped.usage.as_ref().map(|usage| usage.total_tokens),
        Some(19)
    );
    assert!(matches!(
        mapped.assistant_message.content.first(),
        Some(MessageContent::Thinking(thinking)) if thinking.signature.as_deref() == Some("sig")
    ));
    assert!(matches!(
        mapped.assistant_message.content.get(1),
        Some(MessageContent::ToolCall(_))
    ));
}

#[tokio::test]
async fn test_provider_complete_and_stream_behavior() {
    let response = MessagesCreateResponse {
        id: "msg_1".to_string(),
        kind: "message".to_string(),
        role: tiangong_anthropic::types::MessageRole::Assistant,
        content: vec![ContentBlock::Text {
            text: "你好，世界".to_string(),
        }],
        model: "claude-3-7-sonnet".to_string(),
        stop_reason: Some("end_turn".to_string()),
        stop_sequence: None,
        usage: Some(Usage {
            input_tokens: Some(10),
            output_tokens: Some(4),
            ..Default::default()
        }),
    };

    let stream_events = vec![
        Ok(StreamEvent::MessageStart {
            message: MessageStartData {
                id: "msg_1".to_string(),
                model: "claude-3-7-sonnet".to_string(),
                role: tiangong_anthropic::types::MessageRole::Assistant,
                content: vec![],
                stop_reason: None,
                stop_sequence: None,
                usage: Some(Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(0),
                    ..Default::default()
                }),
            },
        }),
        Ok(StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStartData::Thinking {
                thinking: "先想一下".to_string(),
                signature: String::new(),
            },
        }),
        Ok(StreamEvent::ContentBlockDelta {
            index: 1,
            delta: ContentBlockDeltaData::TextDelta {
                text: "你好".to_string(),
            },
        }),
        Ok(StreamEvent::MessageStop),
    ];

    let transport = MockAnthropicTransport {
        response: Some(response),
        stream_events,
    };
    let provider = AnthropicProvider::new(AnthropicClient::new(
        transport,
        AnthropicConfig {
            headers: Default::default(),
            api_key: "test".to_string(),
            base_url: None,
            timeout: std::time::Duration::from_secs(30),
            max_retries: 0,
            api_version: None,
            beta: None,
            retry_notifier: Some(Arc::new(|_, _, _, _| {})),
        },
    ));

    let complete = provider.complete(sample_request()).await.expect("complete");
    assert_eq!(complete.assistant_message.content.len(), 1);

    let mut stream = provider.stream(sample_request()).await.expect("stream");
    let mut seen = Vec::new();
    use futures_util::StreamExt;
    while let Some(event) = stream.next().await {
        seen.push(event.expect("stream event"));
    }

    assert!(seen.contains(&ProviderStreamEvent::MessageStart));
    assert!(seen.contains(&ProviderStreamEvent::ReasoningDelta("先想一下".to_string())));
    assert!(seen.contains(&ProviderStreamEvent::TextDelta("你好".to_string())));
    assert!(seen.contains(&ProviderStreamEvent::MessageEnd { stop_reason: None }));
}

#[tokio::test]
async fn test_provider_list_models() {
    let provider = AnthropicProvider::new(AnthropicClient::new(
        MockAnthropicTransport::default(),
        AnthropicConfig::new("test"),
    ));

    let models = provider.list_models().await.expect("list models");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "claude-3-7-sonnet");
}
