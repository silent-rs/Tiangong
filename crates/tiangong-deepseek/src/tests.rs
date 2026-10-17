use serde_json::json;

use crate::chat::parse_stream_chunk;
use crate::error::DeepSeekError;
use crate::types::{
    BalanceResponse, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, Currency,
    ListModelsResponse, MessageRole, ReasoningEffort, StreamEvent, StreamOptions, ThinkingConfig,
    ToolCall, ToolSpec, Usage,
};

// ── 配置 ──────────────────────────────────────────────

#[test]
#[allow(deprecated)]
fn config_default_values() {
    let config = crate::DeepSeekConfig::new("test-key");
    assert_eq!(config.api_key, "test-key");
    assert_eq!(config.base_url, "https://api.deepseek.com");
}

#[tokio::test]
#[allow(deprecated)]
async fn 旧版from_config仍可用() {
    let body = br#"{"object":"list","data":[]}"#;
    let server = MockServer::start(vec![http_response("200 OK", "application/json", body)]);
    let mut config = crate::DeepSeekConfig::new("test-key");
    config.base_url = format!("http://{}", server.addr);
    let client = crate::DeepSeekClient::from_config(config).unwrap();
    let models = client.models().list().await.unwrap();
    assert!(models.data.is_empty());
    assert!(server.request_line().starts_with("GET /models"));
}

// ── 错误类型 ──────────────────────────────────────────

#[test]
fn error_retryable() {
    assert!(DeepSeekError::Transport("timeout".into()).is_retryable());
    assert!(DeepSeekError::RateLimited("429".into()).is_retryable());
    assert!(!DeepSeekError::Authentication("401".into()).is_retryable());
    assert!(!DeepSeekError::InvalidRequest("400".into()).is_retryable());
    assert!(!DeepSeekError::Serialization("parse error".into()).is_retryable());
    assert!(!DeepSeekError::Api("unknown".into()).is_retryable());
}

// ── Chat 类型序列化 ──────────────────────────────────

#[test]
fn chat_message_role_serde() {
    let cases = vec![
        (MessageRole::System, "\"system\""),
        (MessageRole::User, "\"user\""),
        (MessageRole::Assistant, "\"assistant\""),
        (MessageRole::Tool, "\"tool\""),
    ];
    for (role, expected) in cases {
        assert_eq!(serde_json::to_string(&role).unwrap(), expected);
        assert_eq!(serde_json::from_str::<MessageRole>(expected).unwrap(), role);
    }
}

#[test]
fn chat_message_skip_none_fields() {
    let msg = ChatMessage {
        role: MessageRole::User,
        content: Some(json!("hello")),
        reasoning_content: None,
        name: None,
        tool_calls: None,
        tool_call_id: None,
        prefix: false,
    };
    let serialized = serde_json::to_string(&msg).unwrap();
    assert!(!serialized.contains("name"));
    assert!(!serialized.contains("reasoning_content"));
    assert!(!serialized.contains("tool_calls"));
    assert!(!serialized.contains("tool_call_id"));
    assert!(!serialized.contains("prefix"));
}

#[test]
fn chat_message_with_reasoning_content() {
    let msg = ChatMessage {
        role: MessageRole::Assistant,
        content: Some(json!("final text")),
        reasoning_content: Some("thinking trace".into()),
        name: None,
        tool_calls: None,
        tool_call_id: None,
        prefix: false,
    };
    let value = serde_json::to_value(&msg).unwrap();
    assert_eq!(value["reasoning_content"], "thinking trace");
}

#[test]
fn chat_message_with_tool_calls() {
    let msg = ChatMessage {
        role: MessageRole::Assistant,
        content: None,
        reasoning_content: None,
        name: None,
        tool_calls: Some(vec![ToolCall {
            id: "call_123".into(),
            kind: "function".into(),
            function: crate::types::FunctionCall {
                name: "get_weather".into(),
                arguments: r#"{"city":"Beijing"}"#.into(),
            },
        }]),
        tool_call_id: None,
        prefix: false,
    };
    let value = serde_json::to_value(&msg).unwrap();
    assert_eq!(value["tool_calls"][0]["type"], "function");
    assert_eq!(value["tool_calls"][0]["function"]["name"], "get_weather");
}

#[test]
fn chat_request_serialization() {
    let request = ChatCompletionRequest {
        model: "deepseek-v4-flash".into(),
        messages: vec![ChatMessage {
            role: MessageRole::User,
            content: Some(json!("hello")),
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: None,
            prefix: false,
        }],
        max_tokens: Some(1024),
        temperature: Some(0.7),
        top_p: None,
        stream: None,
        stream_options: None,
        stop: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        reasoning_effort: None,
        response_format: None,
        user_id: None,
    };
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["model"], "deepseek-v4-flash");
    assert_eq!(value["max_tokens"], 1024);
    assert!((value["temperature"].as_f64().unwrap() - 0.7).abs() < 0.01);
    assert!(!value.as_object().unwrap().contains_key("top_p"));
}

#[test]
fn stream_options_serialization() {
    let opts = StreamOptions {
        include_usage: true,
    };
    let value = serde_json::to_value(&opts).unwrap();
    assert_eq!(value["include_usage"], true);
}

#[test]
fn thinking_config_serialization() {
    let enabled = ThinkingConfig {
        thinking_type: "enabled".into(),
    };
    let value = serde_json::to_value(&enabled).unwrap();
    assert_eq!(value["type"], "enabled");
    assert!(!value.as_object().unwrap().contains_key("budget_tokens"));

    let disabled = ThinkingConfig {
        thinking_type: "disabled".into(),
    };
    let value = serde_json::to_value(&disabled).unwrap();
    assert_eq!(value["type"], "disabled");
}

#[test]
fn reasoning_effort_serde() {
    assert_eq!(
        serde_json::to_string(&ReasoningEffort::Low).unwrap(),
        "\"low\""
    );
    assert_eq!(
        serde_json::to_string(&ReasoningEffort::High).unwrap(),
        "\"high\""
    );
    assert_eq!(
        serde_json::to_string(&ReasoningEffort::Max).unwrap(),
        "\"max\""
    );
}

#[test]
fn choice_message_accepts_thinking_content_alias() {
    // 官方部分版本可能用 thinking_content 命名，应能反序列化为 reasoning_content。
    use crate::types::ChoiceMessage;
    let json = r#"{"role":"assistant","content":"hi","thinking_content":"思绪"}"#;
    let msg: ChoiceMessage = serde_json::from_str(json).unwrap();
    assert_eq!(msg.reasoning_content.as_deref(), Some("思绪"));
}

// ── Chat 响应反序列化 ──────────────────────────────────

#[test]
fn chat_response_deserialization() {
    let raw = json!({
        "id": "chatcmpl-abc123",
        "object": "chat.completion",
        "created": 1700000000u64,
        "model": "deepseek-v4-flash",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Hello! How can I help?"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 8,
            "total_tokens": 18,
            "prompt_cache_hit_tokens": 5,
            "prompt_cache_miss_tokens": 5
        },
        "system_fingerprint": "fp_123"
    });
    let response: ChatCompletionResponse = serde_json::from_value(raw).unwrap();
    assert_eq!(response.id, "chatcmpl-abc123");
    assert_eq!(response.choices.len(), 1);
    assert_eq!(
        response.choices[0].message.content.as_deref(),
        Some("Hello! How can I help?")
    );
    assert_eq!(response.usage.prompt_cache_hit_tokens, Some(5));
    assert_eq!(response.usage.prompt_cache_miss_tokens, Some(5));
}

#[test]
fn chat_response_with_reasoning() {
    let raw = json!({
        "id": "chatcmpl-reasoning",
        "object": "chat.completion",
        "created": 1700000000u64,
        "model": "deepseek-v4-pro",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "The answer is 42.",
                "reasoning_content": "Let me think about this..."
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "total_tokens": 150
        },
        "system_fingerprint": ""
    });
    let response: ChatCompletionResponse = serde_json::from_value(raw).unwrap();
    assert_eq!(
        response.choices[0].message.reasoning_content.as_deref(),
        Some("Let me think about this...")
    );
}

#[test]
fn chat_response_with_tool_calls() {
    let raw = json!({
        "id": "chatcmpl-tools",
        "object": "chat.completion",
        "created": 1700000000u64,
        "model": "deepseek-v4-flash",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_001",
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "arguments": "{\"city\":\"Beijing\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 20,
            "completion_tokens": 15,
            "total_tokens": 35
        },
        "system_fingerprint": ""
    });
    let response: ChatCompletionResponse = serde_json::from_value(raw).unwrap();
    let tool_calls = response.choices[0].message.tool_calls.as_ref().unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].function.name, "get_weather");
    assert_eq!(response.choices[0].finish_reason, "tool_calls");
}

// ── 流式 chunk 解析 ──────────────────────────────────

/// 取 parse_stream_chunk 产出的首个事件，断言有且仅有一个。
fn single_event(data: &str) -> StreamEvent {
    let events = parse_stream_chunk(data);
    assert_eq!(
        events.len(),
        1,
        "预期单个事件，实际得到 {} 个：{events:?}",
        events.len()
    );
    events.into_iter().next().unwrap().expect("事件应为 Ok")
}

#[test]
fn stream_text_delta() {
    let data = r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1700000000,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
    let event = single_event(data);
    assert!(matches!(event, StreamEvent::TextDelta(ref s) if s == "Hello"));
}

#[test]
fn stream_reasoning_delta() {
    let data = r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1700000000,"model":"deepseek-v4-pro","choices":[{"index":0,"delta":{"reasoning_content":"thinking..."},"finish_reason":null}]}"#;
    let event = single_event(data);
    assert!(matches!(
        event,
        StreamEvent::ReasoningDelta(ref s) if s == "thinking..."
    ));
}

#[test]
fn stream_tool_call_start() {
    let data = r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1700000000,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_001","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#;
    let event = single_event(data);
    match event {
        StreamEvent::ToolCallStart { id, name } => {
            assert_eq!(id, "call_001");
            assert_eq!(name, "get_weather");
        }
        _ => panic!("expected ToolCallStart, got {event:?}"),
    }
}

#[test]
fn stream_tool_call_delta() {
    let data = r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1700000000,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]},"finish_reason":null}]}"#;
    let event = single_event(data);
    match event {
        StreamEvent::ToolCallDelta { index, arguments } => {
            assert_eq!(index, 0);
            assert_eq!(arguments, "{\"city\":");
        }
        _ => panic!("expected ToolCallDelta, got {event:?}"),
    }
}

#[test]
fn stream_usage_event() {
    let data = r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1700000000,"model":"deepseek-v4-flash","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"prompt_cache_hit_tokens":80}}"#;
    let event = single_event(data);
    match event {
        StreamEvent::Usage(usage) => {
            assert_eq!(usage.prompt_tokens, 100);
            assert_eq!(usage.prompt_cache_hit_tokens, Some(80));
        }
        _ => panic!("expected Usage, got {event:?}"),
    }
}

#[test]
fn stream_chunk_emits_multiple_events() {
    // 同一 delta 同时含 reasoning_content + content + tool_calls，应全部产出。
    let data = r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1700000000,"model":"deepseek-v4-pro","choices":[{"index":0,"delta":{"reasoning_content":"想一下","content":"回复","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"fn","arguments":"{}"}}]},"finish_reason":null}]}"#;
    let events = parse_stream_chunk(data);
    // reasoning + text + ToolCallStart + ToolCallDelta = 4
    assert!(
        events.len() >= 3,
        "应产出 reasoning + text + toolcall 多个事件，实际 {} 个",
        events.len()
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Ok(StreamEvent::ReasoningDelta(_))))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Ok(StreamEvent::TextDelta(_))))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Ok(StreamEvent::ToolCallStart { .. })))
    );
}

#[test]
fn stream_empty_delta_preserves_finish_reason() {
    // OpenAI 兼容协议：role 首片和 finish_reason 结束片 delta 全空，是正常 chunk。
    let role_chunk = r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1700000000,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#;
    assert!(parse_stream_chunk(role_chunk).is_empty());

    let finish_chunk = r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1700000000,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
    assert!(
        matches!(parse_stream_chunk(finish_chunk).as_slice(), [Ok(StreamEvent::FinishReason(reason))] if reason == "stop")
    );
}

#[test]
fn stream_invalid_json_is_error() {
    let result = parse_stream_chunk("not json");
    assert!(result.iter().all(Result::is_err));
}

// ── 模型列表反序列化 ──────────────────────────────────

#[test]
fn list_models_response() {
    let raw = json!({
        "object": "list",
        "data": [
            {"id": "deepseek-v4-flash", "object": "model", "owned_by": "deepseek"},
            {"id": "deepseek-v4-pro", "object": "model", "owned_by": "deepseek"}
        ]
    });
    let response: ListModelsResponse = serde_json::from_value(raw).unwrap();
    assert_eq!(response.data.len(), 2);
    assert_eq!(response.data[0].id, "deepseek-v4-flash");
    assert_eq!(response.data[1].owned_by, "deepseek");
}

// ── 余额响应反序列化 ──────────────────────────────────

#[test]
fn balance_response_deserialization() {
    let raw = json!({
        "is_available": true,
        "balance_infos": [
            {
                "currency": "CNY",
                "total_balance": "10.50",
                "granted_balance": "5.00",
                "topped_up_balance": "5.50"
            },
            {
                "currency": "USD",
                "total_balance": "2.00",
                "granted_balance": "0.00",
                "topped_up_balance": "2.00"
            }
        ]
    });
    let response: BalanceResponse = serde_json::from_value(raw).unwrap();
    assert!(response.is_available);
    assert_eq!(response.balance_infos.len(), 2);
    assert_eq!(response.balance_infos[0].currency, Currency::Cny);
    assert_eq!(response.balance_infos[0].total_balance, "10.50");
    assert_eq!(response.balance_infos[1].currency, Currency::Usd);
}

#[test]
fn balance_response_unavailable() {
    let raw = json!({
        "is_available": false,
        "balance_infos": []
    });
    let response: BalanceResponse = serde_json::from_value(raw).unwrap();
    assert!(!response.is_available);
    assert!(response.balance_infos.is_empty());
}

// ── Usage 类型 ──────────────────────────────────────

#[test]
fn usage_with_cache_tokens() {
    let raw = json!({
        "prompt_tokens": 100,
        "completion_tokens": 50,
        "total_tokens": 150,
        "prompt_cache_hit_tokens": 80,
        "prompt_cache_miss_tokens": 20,
        "completion_tokens_details": {
            "reasoning_tokens": 30
        }
    });
    let usage: Usage = serde_json::from_value(raw).unwrap();
    assert_eq!(usage.prompt_tokens, 100);
    assert_eq!(usage.prompt_cache_hit_tokens, Some(80));
    assert_eq!(usage.prompt_cache_miss_tokens, Some(20));
    assert_eq!(
        usage.completion_tokens_details.unwrap().reasoning_tokens,
        Some(30)
    );
}

#[test]
fn usage_without_cache_tokens() {
    let raw = json!({
        "prompt_tokens": 50,
        "completion_tokens": 25,
        "total_tokens": 75
    });
    let usage: Usage = serde_json::from_value(raw).unwrap();
    assert_eq!(usage.prompt_cache_hit_tokens, None);
    assert_eq!(usage.prompt_cache_miss_tokens, None);
    assert!(usage.completion_tokens_details.is_none());
}

// ── 工具定义序列化 ──────────────────────────────────

#[test]
fn tool_spec_serialization() {
    let spec = ToolSpec {
        kind: "function".into(),
        function: crate::types::FunctionSpec {
            name: "get_weather".into(),
            description: Some("Get weather for a city".into()),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "city": {"type": "string", "description": "City name"}
                },
                "required": ["city"]
            })),
            strict: false,
        },
    };
    let value = serde_json::to_value(&spec).unwrap();
    assert_eq!(value["type"], "function");
    assert_eq!(value["function"]["name"], "get_weather");
    assert!(
        !value["function"]
            .as_object()
            .unwrap()
            .contains_key("strict")
    );
}

// ── Responses API 请求序列化 ────────────────────────

#[test]
fn responses_request_full_serialization() {
    use crate::types::{
        ContentBlock, CreateResponseRequest, FunctionCallInputItem, FunctionCallOutputInputItem,
        FunctionOutputContent, ImageDetail, InputImageBlock, InputMessage, MODEL_V4_FLASH,
        MessageContent, ReasoningConfig, ReasoningEffortLevel, ResponseInput, ResponseInputItem,
        ResponseRole, ResponsesFunctionTool, ResponsesTool, TextBlock, TextFormat,
        TextFormatConfig,
    };

    let request = CreateResponseRequest {
        model: MODEL_V4_FLASH.into(),
        input: Some(ResponseInput::Items(vec![
            ResponseInputItem::Message(InputMessage {
                role: Some(ResponseRole::User),
                content: Some(MessageContent::Blocks(vec![
                    ContentBlock::InputText(TextBlock {
                        text: "描述这张图".into(),
                    }),
                    ContentBlock::InputImage(InputImageBlock {
                        image_url: None,
                        detail: Some(ImageDetail::High),
                        file_id: Some("file-api-abc".into()),
                    }),
                ])),
            }),
            ResponseInputItem::FunctionCall(FunctionCallInputItem {
                call_id: "call_0".into(),
                name: "get_weather".into(),
                arguments: "{}".into(),
            }),
            ResponseInputItem::FunctionCallOutput(FunctionCallOutputInputItem {
                call_id: "call_0".into(),
                output: FunctionOutputContent::Text("晴".into()),
            }),
        ])),
        instructions: Some("你是助手".into()),
        reasoning: Some(ReasoningConfig {
            effort: Some(ReasoningEffortLevel::High),
        }),
        max_output_tokens: Some(4096),
        temperature: Some(1.5),
        top_p: Some(0.9),
        tools: Some(vec![ResponsesTool::Function(ResponsesFunctionTool {
            name: "get_weather".into(),
            description: Some("查天气".into()),
            parameters: Some(json!({"type": "object"})),
        })]),
        text: Some(TextFormatConfig {
            format: Some(TextFormat::Text),
        }),
        ..Default::default()
    };
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["model"], "deepseek-v4-flash");
    assert_eq!(value["instructions"], "你是助手");
    assert_eq!(value["reasoning"]["effort"], "high");
    assert_eq!(value["input"][0]["type"], "message");
    assert_eq!(value["input"][0]["role"], "user");
    assert_eq!(value["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(value["input"][0]["content"][1]["type"], "input_image");
    assert_eq!(value["input"][0]["content"][1]["file_id"], "file-api-abc");
    assert_eq!(value["input"][0]["content"][1]["detail"], "high");
    assert_eq!(value["input"][1]["type"], "function_call");
    assert_eq!(value["input"][2]["type"], "function_call_output");
    assert_eq!(value["input"][2]["output"], "晴");
    assert_eq!(value["tools"][0]["type"], "function");
    assert_eq!(value["tools"][0]["name"], "get_weather");
    assert_eq!(value["text"]["format"]["type"], "text");
    // 未设置的字段不应出现
    assert!(value.get("stream").is_none());
    assert!(value.get("user").is_none());
    assert!(value.get("tool_choice").is_none());
}

#[test]
fn responses_request_input_text_shorthand() {
    use crate::types::{CreateResponseRequest, MODEL_V4_PRO, ResponseInput};

    let request = CreateResponseRequest {
        model: MODEL_V4_PRO.into(),
        input: Some(ResponseInput::Text("你好".into())),
        ..Default::default()
    };
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["input"], "你好");
}

#[test]
fn responses_web_search_call_item_keeps_unknown_fields() {
    use crate::types::ResponseInputItem;

    let item: ResponseInputItem = serde_json::from_value(json!({
        "type": "web_search_call",
        "id": "ws_1",
        "status": "completed",
        "action": {"type": "search", "query": "rust"},
        "future_field": 42
    }))
    .unwrap();
    // 原样回传：未知字段保留在 extra 中
    let value = serde_json::to_value(&item).unwrap();
    assert_eq!(value["future_field"], 42);
    assert_eq!(value["action"]["query"], "rust");
}

// ── Responses API 响应反序列化 ──────────────────────

#[test]
fn responses_object_all_output_item_kinds() {
    use crate::types::{ResponseObject, ResponseOutputItem, ResponseStatus};

    let response: ResponseObject = serde_json::from_value(json!({
        "id": "resp_1",
        "object": "response",
        "created_at": 1756000000,
        "status": "completed",
        "model": "deepseek-v4-flash",
        "output": [
            {"type": "reasoning", "id": "rs_1", "status": "completed",
             "content": [{"type": "reasoning_text", "text": "思考"}]},
            {"type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
             "content": [{"type": "output_text", "text": "你好"}]},
            {"type": "function_call", "id": "fc_1", "status": "completed",
             "call_id": "call_0", "name": "get_weather", "arguments": "{\"city\":\"北京\"}"},
            {"type": "custom_tool_call", "id": "ct_1", "status": "completed",
             "call_id": "call_1", "name": "apply_patch", "input": "*** Begin Patch"},
            {"type": "web_search_call", "id": "ws_1", "status": "completed",
             "action": {"type": "search", "query": "rust"}}
        ],
        "usage": {
            "input_tokens": 100,
            "input_tokens_details": {"cached_tokens": 80},
            "output_tokens": 50,
            "output_tokens_details": {"reasoning_tokens": 20},
            "total_tokens": 150
        }
    }))
    .unwrap();

    assert_eq!(response.status, ResponseStatus::Completed);
    assert_eq!(response.output.len(), 5);
    assert_eq!(response.output_text(), "你好");
    let usage = response.usage.unwrap();
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.input_tokens_details.unwrap().cached_tokens, 80);
    assert_eq!(usage.output_tokens_details.unwrap().reasoning_tokens, 20);
    assert_eq!(usage.total_tokens, 150);
    match &response.output[2] {
        ResponseOutputItem::FunctionCall(call) => {
            assert_eq!(call.call_id, "call_0");
            assert_eq!(call.arguments, "{\"city\":\"北京\"}");
        }
        other => panic!("第 3 项应为 function_call：{other:?}"),
    }
    match &response.output[3] {
        ResponseOutputItem::CustomToolCall(call) => assert_eq!(call.name, "apply_patch"),
        other => panic!("第 4 项应为 custom_tool_call：{other:?}"),
    }
}

#[test]
fn responses_object_failed_and_incomplete() {
    use crate::types::{ResponseObject, ResponseStatus};

    let failed: ResponseObject = serde_json::from_value(json!({
        "id": "resp_2", "object": "response", "status": "failed", "model": "m",
        "output": [],
        "error": {"code": "server_error", "message": "boom"}
    }))
    .unwrap();
    assert_eq!(failed.status, ResponseStatus::Failed);
    assert_eq!(
        failed.error.as_ref().unwrap().code.as_deref(),
        Some("server_error")
    );

    let incomplete: ResponseObject = serde_json::from_value(json!({
        "id": "resp_3", "object": "response", "status": "incomplete", "model": "m",
        "output": [],
        "incomplete_details": {"reason": "max_output_tokens"}
    }))
    .unwrap();
    assert_eq!(incomplete.status, ResponseStatus::Incomplete);
    assert_eq!(
        incomplete.incomplete_details.unwrap().reason.as_deref(),
        Some("max_output_tokens")
    );
}

#[test]
fn responses_output_text_concatenates_only_message_text() {
    use crate::types::{OutputContentBlock, OutputMessage, ResponseObject, ResponseOutputItem};

    let response = ResponseObject {
        output: vec![
            ResponseOutputItem::Reasoning(Default::default()),
            ResponseOutputItem::Message(OutputMessage {
                content: vec![
                    OutputContentBlock::OutputText(crate::types::TextBlock {
                        text: "第一段，".into(),
                    }),
                    OutputContentBlock::OutputText(crate::types::TextBlock {
                        text: "第二段".into(),
                    }),
                ],
                ..Default::default()
            }),
            ResponseOutputItem::FunctionCall(Default::default()),
        ],
        ..Default::default()
    };
    assert_eq!(response.output_text(), "第一段，第二段");
}

// ── Responses API 流式事件解析 ──────────────────────

#[test]
fn responses_stream_event_delta_and_item_events() {
    use crate::responses::parse_stream_event;
    use crate::types::ResponsesStreamEvent;

    let delta = parse_stream_event(
        r#"{"type":"response.output_text.delta","sequence_number":5,"item_id":"msg_1","output_index":1,"content_index":0,"delta":"你好"}"#,
    ).unwrap();
    match delta {
        ResponsesStreamEvent::OutputTextDelta {
            sequence_number,
            item_id,
            output_index,
            content_index,
            delta,
        } => {
            assert_eq!(sequence_number, 5);
            assert_eq!(item_id, "msg_1");
            assert_eq!(output_index, 1);
            assert_eq!(content_index, 0);
            assert_eq!(delta, "你好");
        }
        other => panic!("{other:?}"),
    }

    let reasoning = parse_stream_event(
        r#"{"type":"response.reasoning_text.delta","sequence_number":2,"delta":"思考中"}"#,
    )
    .unwrap();
    assert!(matches!(reasoning,
        ResponsesStreamEvent::ReasoningTextDelta { delta, .. } if delta == "思考中"));

    let args_done = parse_stream_event(
        r#"{"type":"response.function_call_arguments.done","sequence_number":8,"item_id":"fc_1","output_index":2,"arguments":"{\"x\":1}"}"#,
    ).unwrap();
    assert!(matches!(args_done,
        ResponsesStreamEvent::FunctionCallArgumentsDone { arguments, .. } if arguments == "{\"x\":1}"));

    let custom_input = parse_stream_event(
        r#"{"type":"response.custom_tool_call_input.delta","sequence_number":9,"delta":"*** Begin"}"#,
    ).unwrap();
    assert!(matches!(custom_input,
        ResponsesStreamEvent::CustomToolCallInputDelta { delta, .. } if delta == "*** Begin"));

    let item_added = parse_stream_event(
        r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}"#,
    ).unwrap();
    match item_added {
        ResponsesStreamEvent::OutputItemAdded {
            output_index, item, ..
        } => {
            assert_eq!(output_index, 0);
            assert!(matches!(item, crate::types::ResponseOutputItem::Message(_)));
        }
        other => panic!("{other:?}"),
    }

    let web_search = parse_stream_event(
        r#"{"type":"response.web_search_call.in_progress","sequence_number":3,"output_index":4,"item":{"type":"web_search_call","id":"ws_1"}}"#,
    ).unwrap();
    assert!(matches!(
        web_search,
        ResponsesStreamEvent::WebSearchCallInProgress { .. }
    ));

    let content_part = parse_stream_event(
        r#"{"type":"response.content_part.done","sequence_number":6,"item_id":"msg_1","output_index":1,"content_index":0,"part":{"type":"output_text","text":"你好"}}"#,
    ).unwrap();
    assert!(matches!(
        content_part,
        ResponsesStreamEvent::ContentPartDone { .. }
    ));
}

#[test]
fn responses_stream_terminal_events_carry_response() {
    use crate::responses::parse_stream_event;
    use crate::types::ResponsesStreamEvent;

    let completed = parse_stream_event(
        r#"{"type":"response.completed","sequence_number":10,"response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#,
    ).unwrap();
    match completed {
        ResponsesStreamEvent::ResponseCompleted {
            sequence_number,
            response,
        } => {
            assert_eq!(sequence_number, 10);
            assert_eq!(response.usage.unwrap().total_tokens, 15);
        }
        other => panic!("{other:?}"),
    }

    let failed = parse_stream_event(
        r#"{"type":"response.failed","sequence_number":11,"response":{"id":"resp_1","status":"failed","error":{"message":"boom"}}}"#,
    ).unwrap();
    assert!(matches!(
        failed,
        ResponsesStreamEvent::ResponseFailed { .. }
    ));
}

#[test]
fn responses_stream_unknown_event_falls_back() {
    use crate::responses::parse_stream_event;
    use crate::types::ResponsesStreamEvent;

    let unknown =
        parse_stream_event(r#"{"type":"response.brand_new_event","sequence_number":99}"#).unwrap();
    assert!(matches!(unknown,
        ResponsesStreamEvent::Unknown { event_type } if event_type == "response.brand_new_event"));

    // 非 JSON 数据仍是硬错误
    assert!(parse_stream_event("not-json").is_err());
    assert!(parse_stream_event("").is_err());
}

// ── Files API 类型 ──────────────────────────────────

#[test]
fn files_types_serde() {
    use crate::types::{DeleteFileResponse, FileObject, ListFilesResponse};

    let file: FileObject = serde_json::from_value(json!({
        "id": "file-api-0a1b2c3d4e5f60718293a4b5c6d7e8f9",
        "object": "file",
        "bytes": 102400,
        "created_at": 1700000000,
        "filename": "image.jpg",
        "purpose": "user_data",
        "expires_at": 1700086400
    }))
    .unwrap();
    assert_eq!(file.expires_at, Some(1700086400));
    // expires_at 可选
    let permanent: FileObject = serde_json::from_value(json!({
        "id": "f", "object": "file", "bytes": 1, "created_at": 1,
        "filename": "a.png", "purpose": "user_data"
    }))
    .unwrap();
    assert_eq!(permanent.expires_at, None);

    let listed: ListFilesResponse = serde_json::from_value(json!({
        "object": "list", "data": [file], "first_id": "file-api-0a1b",
        "last_id": "file-api-0a1b", "has_more": true
    }))
    .unwrap();
    assert_eq!(listed.data.len(), 1);
    assert!(listed.has_more);
    assert_eq!(listed.first_id.as_deref(), Some("file-api-0a1b"));

    let deleted: DeleteFileResponse = serde_json::from_value(json!({
        "id": "file-api-0a1b2c3d4e5f60718293a4b5c6d7e8f9",
        "object": "file", "deleted": true
    }))
    .unwrap();
    assert!(deleted.deleted);
}

// ── multipart 表单与图片嗅探 ────────────────────────

#[test]
fn multipart_form_encoding() {
    use crate::client::MultipartForm;

    let png_bytes: Vec<u8> = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 1, 2];
    let form = MultipartForm::new()
        .field("purpose", "user_data")
        .field("expires_after[anchor]", "created_at")
        .field("expires_after[seconds]", "7200")
        .file("file", "图.png", png_bytes.clone());
    let (content_type, body) = form.encode();

    assert!(content_type.starts_with("multipart/form-data; boundary="));
    let boundary = content_type
        .trim_start_matches("multipart/form-data; boundary=")
        .to_string();
    let text = String::from_utf8_lossy(&body);
    assert!(text.starts_with(&format!("--{boundary}\r\n")));
    assert!(text.contains("Content-Disposition: form-data; name=\"purpose\"\r\n\r\nuser_data\r\n"));
    assert!(text.contains(
        "Content-Disposition: form-data; name=\"expires_after[anchor]\"\r\n\r\ncreated_at\r\n"
    ));
    assert!(text.contains(
        "Content-Disposition: form-data; name=\"expires_after[seconds]\"\r\n\r\n7200\r\n"
    ));
    assert!(text.contains(
        "Content-Disposition: form-data; name=\"file\"; filename=\"图.png\"\r\nContent-Type: image/png\r\n\r\n"
    ));
    assert!(text.ends_with(&format!("--{boundary}--\r\n")));

    // 二进制内容原样保留（PNG 头中的 \r\n 不能被破坏）
    let needle = b"image/png\r\n\r\n";
    let pos = body
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("应找到文件部分的头部结束标记");
    assert_eq!(
        &body[pos + needle.len()..pos + needle.len() + png_bytes.len()],
        &png_bytes[..]
    );
}

#[test]
fn multipart_form_filename_escaping() {
    use crate::client::MultipartForm;

    let form = MultipartForm::new().file("file", "a\"b\\c\nd.png", vec![0]);
    let (_, body) = form.encode();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("filename=\"a\\\"b\\\\c d.png\""),
        "引号/反斜杠应转义、换行替换为空格：{text}"
    );
}

#[test]
fn image_content_type_sniffing() {
    use crate::client::sniff_image_content_type;

    assert_eq!(
        sniff_image_content_type(&[0xFF, 0xD8, 0xFF, 0xE0]),
        "image/jpeg"
    );
    assert_eq!(
        sniff_image_content_type(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']),
        "image/png"
    );
    assert_eq!(sniff_image_content_type(b"GIF87a"), "image/gif");
    assert_eq!(sniff_image_content_type(b"GIF89a"), "image/gif");
    assert_eq!(
        sniff_image_content_type(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
        "image/webp"
    );
    // RIFF 但不是 WEBP
    assert_eq!(
        sniff_image_content_type(b"RIFF\x00\x00\x00\x00WAVE"),
        "application/octet-stream"
    );
    assert_eq!(
        sniff_image_content_type(b"plain text"),
        "application/octet-stream"
    );
    assert_eq!(sniff_image_content_type(&[]), "application/octet-stream");
}

// ── 百分号编码 ──────────────────────────────────────

#[test]
fn percent_encoding_keeps_unreserved_and_escapes_rest() {
    use crate::client::percent_encode;

    // 非保留字符（RFC 3986）原样保留
    assert_eq!(percent_encode("file-api-0a1b.c~d"), "file-api-0a1b.c~d");
    // 查询分隔符与特殊字符
    assert_eq!(percent_encode("a&b=c#d e"), "a%26b%3Dc%23d%20e");
    assert_eq!(percent_encode("a/b?g"), "a%2Fb%3Fg");
    // 非 ASCII 按 UTF-8 字节转义
    assert_eq!(percent_encode("文件"), "%E6%96%87%E4%BB%B6");
    assert_eq!(percent_encode(""), "");
}

// ── HTTP 层（本地 mock 服务器）─────────────────────

/// 极简同步 HTTP mock：按连接顺序弹出一个预置响应，记录收到的请求。
struct MockServer {
    addr: String,
    requests: std::sync::mpsc::Receiver<(String, Vec<u8>)>,
}

impl MockServer {
    fn start(responses: Vec<Vec<u8>>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut queue = responses.into_iter();
            for mut stream in listener.incoming().flatten() {
                let Some(response) = queue.next() else { break };
                if let Some((request_line, body)) = read_request(&stream) {
                    let _ = tx.send((request_line, body));
                    use std::io::Write;
                    let _ = stream.write_all(&response);
                }
            }
        });
        Self { addr, requests: rx }
    }

    fn request_line(&self) -> String {
        self.requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("mock 服务器应收到请求")
            .0
    }

    fn body(&self) -> Vec<u8> {
        self.requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("mock 服务器应收到请求")
            .1
    }
}

fn read_request(mut stream: &std::net::TcpStream) -> Option<(String, Vec<u8>)> {
    use std::io::Read;

    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).ok()?;
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header_text = String::from_utf8_lossy(&header).to_string();
    let content_length = header_text
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body).ok()?;
    }
    let request_line = header_text.lines().next().unwrap_or_default().to_string();
    Some((request_line, body))
}

fn http_response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn sse_response(events: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{events}"
    )
    .into_bytes()
}

fn mock_client(addr: &str) -> crate::client::DeepSeekClient {
    crate::DeepSeekClient::builder("test-key")
        .base_url(format!("http://{addr}"))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap()
}

#[tokio::test]
async fn responses_create_forces_non_stream() {
    let body =
        br#"{"id":"resp_1","object":"response","status":"completed","model":"m","output":[]}"#;
    let server = MockServer::start(vec![http_response("200 OK", "application/json", body)]);
    let client = mock_client(&server.addr);

    let response = client
        .responses()
        .create(crate::types::CreateResponseRequest {
            model: crate::types::MODEL_V4_FLASH.into(),
            instructions: Some("hi".into()),
            stream: Some(true), // 故意传入 true，方法应强制关闭
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(response.status, crate::types::ResponseStatus::Completed);

    let sent: serde_json::Value = serde_json::from_slice(&server.body()).unwrap();
    assert_eq!(sent["stream"], false, "create() 应强制 stream=false");
}

#[tokio::test]
async fn responses_stream_stops_after_terminal_event() {
    use crate::types::ResponsesStreamEvent;
    use futures_util::StreamExt;

    let sse = concat!(
        "data: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"msg_1\",\"delta\":\"你好\"}\n\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{\"id\":\"resp_1\",\"status\":\"completed\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"msg_1\",\"delta\":\"不应出现\"}\n\n",
    );
    let server = MockServer::start(vec![sse_response(sse)]);
    let client = mock_client(&server.addr);

    let mut stream = client
        .responses()
        .create_stream(crate::types::CreateResponseRequest {
            model: crate::types::MODEL_V4_FLASH.into(),
            input: Some(crate::types::ResponseInput::Text("hi".into())),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.expect("流事件应无错误"));
    }
    assert_eq!(events.len(), 3, "终止事件后流应结束：{events:?}");
    assert!(matches!(
        events[0],
        ResponsesStreamEvent::ResponseCreated { .. }
    ));
    assert!(matches!(&events[1],
        ResponsesStreamEvent::OutputTextDelta { delta, .. } if delta == "你好"));
    assert!(matches!(
        events[2],
        ResponsesStreamEvent::ResponseCompleted { .. }
    ));

    // 请求体应自动注入 stream=true
    let sent: serde_json::Value = serde_json::from_slice(&server.body()).unwrap();
    assert_eq!(sent["stream"], true);
}

#[tokio::test]
async fn files_list_query_parameters_are_encoded() {
    let body = br#"{"object":"list","data":[],"has_more":false}"#;
    let server = MockServer::start(vec![http_response("200 OK", "application/json", body)]);
    let client = mock_client(&server.addr);

    client
        .files()
        .list(crate::types::ListFilesParams {
            after: Some("file api#1&x=2".into()),
            limit: Some(20),
            order: Some(crate::types::ListOrder::Desc),
            ..Default::default()
        })
        .await
        .unwrap();

    let path = server.request_line().split(' ').nth(1).unwrap().to_string();
    assert!(
        path.contains("after=file%20api%231%26x%3D2"),
        "after 值应完整编码：{path}"
    );
    assert!(path.contains("limit=20"));
    assert!(path.contains("order=desc"));
}

#[tokio::test]
async fn files_retrieve_and_delete_encode_file_id_in_path() {
    let file_body = br#"{"id":"x","object":"file","bytes":1,"created_at":1,"filename":"a","purpose":"user_data"}"#;
    let deleted_body = br#"{"id":"x","object":"file","deleted":true}"#;
    let server = MockServer::start(vec![
        http_response("200 OK", "application/json", file_body),
        http_response("200 OK", "application/json", deleted_body),
    ]);
    let client = mock_client(&server.addr);

    client.files().retrieve("file api/1?a=b").await.unwrap();
    let retrieve_path = server.request_line().split(' ').nth(1).unwrap().to_string();
    assert!(
        retrieve_path.starts_with("/files/file%20api%2F1%3Fa%3Db"),
        "file_id 应作为路径段编码：{retrieve_path}"
    );

    client.files().delete("file api/1?a=b").await.unwrap();
    let delete_line = server.request_line();
    assert!(
        delete_line.starts_with("DELETE /files/file%20api%2F1%3Fa%3Db"),
        "删除路径应编码：{delete_line}"
    );
}

#[tokio::test]
async fn chat_stream_accepts_server_ignoring_stream_flag() {
    // 网关忽略 stream 参数返回一次性 JSON 时，SDK 应在内部按完整响应接住并
    // 合成流事件，而不是让 SSE 解析器静默丢弃后报空流。
    use futures_util::StreamExt;

    let body = r#"{"id":"cmpl_1","object":"chat.completion","created":0,"model":"deepseek-v4-flash","choices":[{"index":0,"message":{"role":"assistant","content":"一次性完整回复"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":5,"total_tokens":8,"prompt_cache_hit_tokens":0,"prompt_cache_miss_tokens":3}}"#
        .as_bytes();
    let server = MockServer::start(vec![http_response("200 OK", "application/json", body)]);
    let client = mock_client(&server.addr);

    let mut stream = client
        .chat()
        .create_stream(crate::types::ChatCompletionRequest {
            model: crate::types::MODEL_V4_FLASH.into(),
            messages: vec![crate::types::ChatMessage {
                role: crate::types::MessageRole::User,
                content: Some(json!("你好")),
                reasoning_content: None,
                name: None,
                tool_calls: None,
                tool_call_id: None,
                prefix: false,
            }],
            stream: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut text = String::new();
    let mut done = false;
    while let Some(event) = stream.next().await {
        match event.expect("流事件应无错误") {
            crate::types::StreamEvent::TextDelta(delta) => text.push_str(&delta),
            crate::types::StreamEvent::Done => done = true,
            _ => {}
        }
    }
    assert_eq!(text, "一次性完整回复");
    assert!(done, "应收到终止事件");
}

#[tokio::test]
async fn finish_reason_survives_all_chat_response_paths() {
    use futures_util::StreamExt;

    for reason in [
        "stop",
        "length",
        "content_filter",
        "tool_calls",
        "insufficient_system_resource",
        "future_vendor_reason",
    ] {
        for (content_type, streaming) in [
            ("application/json", false),
            ("application/json", true),
            ("text/event-stream", true),
            ("application/octet-stream", true),
        ] {
            // 覆盖启用工具文本缓冲与普通对话两条事件转发路径。
            for with_tools in [false, true] {
                let usage = json!({"prompt_tokens":3,"completion_tokens":5,"total_tokens":8});
                let body = if content_type == "application/json" {
                    json!({"id":"finish","object":"chat.completion","created":0,"model":"deepseek-v4-flash",
                        "choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":reason}],
                        "usage":usage}).to_string()
                } else {
                    // 中间片明确为 null；结束片没有文本，但必须透传原因和用量。
                    let first = json!({"id":"finish","object":"chat.completion.chunk","created":0,"model":"deepseek-v4-flash",
                        "choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]});
                    let last = json!({"id":"finish","object":"chat.completion.chunk","created":0,"model":"deepseek-v4-flash",
                        "choices":[{"index":0,"delta":{},"finish_reason":reason}],"usage":usage});
                    format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")
                };
                let server =
                    MockServer::start(vec![http_response("200 OK", content_type, body.as_bytes())]);
                let client = mock_client(&server.addr);
                let request: ChatCompletionRequest = serde_json::from_value(json!({
                    "model":"deepseek-v4-flash", "messages":[{"role":"user","content":"测试"}],
                    "stream":streaming,
                    "tools": if with_tools { json!([{"type":"function","function":{"name":"probe","parameters":{"type":"object"}}}]) } else { json!(null) }
                })).unwrap();
                if !streaming {
                    let response = client.chat().create(request).await.unwrap();
                    assert_eq!(response.choices[0].finish_reason, reason);
                    assert_eq!(response.usage.total_tokens, 8);
                    continue;
                }
                let events = client
                    .chat()
                    .create_stream(request)
                    .await
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                let reasons: Vec<_> = events
                    .iter()
                    .filter_map(|event| match event {
                        StreamEvent::FinishReason(reason) => Some(reason.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(reasons, vec![reason], "{content_type}, tools={with_tools}");
                assert!(matches!(events.last(), Some(StreamEvent::Done)));
                assert!(events.iter().any(
                    |event| matches!(event, StreamEvent::Usage(usage) if usage.total_tokens == 8)
                ));
            }
        }
    }
}

#[tokio::test]
async fn chat_stream_sniffs_sse_body_when_content_type_mislabeled() {
    // 部分网关返回标准 SSE 数据但漏标/错标响应类型：应按首块内容探测后
    // 仍走流式解析，而不是当成一次性 JSON 读取失败。
    use futures_util::StreamExt;

    let sse = concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"漏标类型的流式回复\"}}]}\n\n",
        "data: {\"id\":\"c2\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let server = MockServer::start(vec![http_response(
        "200 OK",
        "application/octet-stream",
        sse.as_bytes(),
    )]);
    let client = mock_client(&server.addr);

    let mut stream = client
        .chat()
        .create_stream(crate::types::ChatCompletionRequest {
            model: crate::types::MODEL_V4_FLASH.into(),
            messages: vec![crate::types::ChatMessage {
                role: crate::types::MessageRole::User,
                content: Some(json!("你好")),
                reasoning_content: None,
                name: None,
                tool_calls: None,
                tool_call_id: None,
                prefix: false,
            }],
            stream: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut text = String::new();
    let mut done = false;
    while let Some(event) = stream.next().await {
        match event.expect("流事件应无错误") {
            crate::types::StreamEvent::TextDelta(delta) => text.push_str(&delta),
            crate::types::StreamEvent::Done => done = true,
            _ => {}
        }
    }
    assert_eq!(text, "漏标类型的流式回复");
    assert!(done, "应收到终止事件");
}

// ── 连接失败识别与 no_proxy（#662）────────────────────────

#[test]
fn error_is_connect_仅识别连接阶段失败() {
    let prefix = crate::error::CONNECT_ERROR_PREFIX;
    assert!(DeepSeekError::Transport(format!("{prefix}refused")).is_connect());
    assert!(!DeepSeekError::Transport("timeout: read".into()).is_connect());
    assert!(!DeepSeekError::Api("connect: x".into()).is_connect());
}

#[tokio::test]
async fn 连接被拒映射为is_connect() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let client = crate::DeepSeekClient::builder("k")
        .base_url(format!("http://{address}"))
        .no_proxy()
        .build()
        .unwrap();
    let error = client.models().list().await.unwrap_err();
    assert!(error.is_connect(), "{error}");
}

// ── Search / Fetch API ───────────────────────────────

#[tokio::test]
async fn tools_search_posts_request_and_parses_results() {
    let body = br#"{"results":[{"rerank_score":2.4,"title":"DeepSeek V4","url":"https://example.com/v4","snippet":"s","summary":"long","content":"","site_name":null,"site_icon":null,"date":"2026-06-01T00:00:00+00:00","authority":3}]}"#;
    let server = MockServer::start(vec![http_response("200 OK", "application/json", body)]);
    let client = mock_client(&server.addr);

    let response = client
        .tools()
        .search(
            crate::types::SearchRequest::new("DeepSeek site:github.com")
                .count(5)
                .need_content(true),
        )
        .await
        .unwrap();

    let (request_line, sent) = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(
        request_line.starts_with("POST /v1/tools/search "),
        "{request_line}"
    );
    let sent: serde_json::Value = serde_json::from_slice(&sent).unwrap();
    assert_eq!(
        sent,
        serde_json::json!({"query":"DeepSeek site:github.com","count":5,"need_content":true}),
        "未设置的可选字段不应序列化"
    );

    assert_eq!(response.results.len(), 1);
    let first = &response.results[0];
    assert_eq!(first.title, "DeepSeek V4");
    assert_eq!(first.site_name, None);
    assert_eq!(first.date.as_deref(), Some("2026-06-01T00:00:00+00:00"));
    assert!(first.is_high_quality());
    assert!(first.is_highly_authoritative());
}

#[tokio::test]
async fn tools_search_empty_results() {
    let server = MockServer::start(vec![http_response(
        "200 OK",
        "application/json",
        br#"{"results":[]}"#,
    )]);
    let client = mock_client(&server.addr);
    let response = client
        .tools()
        .search(crate::types::SearchRequest::new("nothing"))
        .await
        .unwrap();
    assert!(response.results.is_empty());
}

#[tokio::test]
async fn tools_fetch_posts_url_and_parses_markdown() {
    let body =
        br##"{"title":"Example article","content":"# Example article\n\nArticle content."}"##;
    let server = MockServer::start(vec![http_response("200 OK", "application/json", body)]);
    let client = mock_client(&server.addr);

    let page = client
        .tools()
        .fetch(crate::types::FetchRequest::new(
            "https://example.com/article",
        ))
        .await
        .unwrap();

    let (request_line, sent) = server
        .requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(
        request_line.starts_with("POST /v1/tools/fetch "),
        "{request_line}"
    );
    let sent: serde_json::Value = serde_json::from_slice(&sent).unwrap();
    assert_eq!(
        sent,
        serde_json::json!({"url":"https://example.com/article"})
    );
    assert_eq!(page.title, "Example article");
    assert!(page.content.starts_with("# Example article"));
}

#[tokio::test]
async fn tools_url_tolerates_v1_suffix_in_base_url() {
    let client = crate::DeepSeekClient::builder("k")
        .base_url("https://api.deepseek.com/v1/")
        .build()
        .unwrap();
    assert_eq!(
        client.build_tools_url("/v1/tools/search"),
        "https://api.deepseek.com/v1/tools/search"
    );
    let client = crate::DeepSeekClient::builder("k")
        .base_url("https://api.deepseek.com")
        .build()
        .unwrap();
    assert_eq!(
        client.build_tools_url("/v1/tools/fetch"),
        "https://api.deepseek.com/v1/tools/fetch"
    );
}

#[tokio::test]
async fn tools_error_status_mapping() {
    use crate::DeepSeekError;
    let err_body = br#"{"error":{"message":"x","type":"invalid_request_error","param":null,"code":"invalid_request_error"}}"#;
    fn kind(err: &DeepSeekError) -> &'static str {
        match err {
            DeepSeekError::InvalidRequest(_) => "invalid",
            DeepSeekError::Authentication(_) => "auth",
            DeepSeekError::Api(_) => "api",
            DeepSeekError::Transport(_) => "transport",
            DeepSeekError::RateLimited(_) => "rate",
            _ => "other",
        }
    }
    let cases = [
        ("400 Bad Request", "invalid", false),
        ("401 Unauthorized", "auth", false),
        ("402 Payment Required", "api", false),
        ("403 Forbidden", "api", false),
        ("404 Not Found", "api", false),
        ("408 Request Timeout", "transport", true),
        ("429 Too Many Requests", "rate", true),
        ("500 Internal Server Error", "transport", true),
        ("504 Gateway Timeout", "transport", true),
    ];
    for (status, expected, retryable) in cases {
        let server = MockServer::start(vec![http_response(status, "application/json", err_body)]);
        let client = mock_client(&server.addr);
        let err = client
            .tools()
            .fetch(crate::types::FetchRequest::new("https://example.com"))
            .await
            .unwrap_err();
        assert_eq!(kind(&err), expected, "{status}: {err:?}");
        assert_eq!(err.is_retryable(), retryable, "{status}");
    }
}

#[test]
fn tools_search_validation() {
    use crate::tools::validate_search;
    use crate::types::SearchRequest;

    assert!(validate_search(&SearchRequest::new("ok")).is_ok());
    assert!(validate_search(&SearchRequest::new("   ")).is_err());
    assert!(validate_search(&SearchRequest::new("")).is_err());
    // 128 个中文字符合法，129 个非法（按 Unicode 字符计）
    assert!(validate_search(&SearchRequest::new("搜".repeat(128))).is_ok());
    assert!(validate_search(&SearchRequest::new("搜".repeat(129))).is_err());
    assert!(validate_search(&SearchRequest::new("q").count(0)).is_err());
    assert!(validate_search(&SearchRequest::new("q").count(1)).is_ok());
    assert!(validate_search(&SearchRequest::new("q").count(50)).is_ok());
    assert!(validate_search(&SearchRequest::new("q").count(51)).is_err());
    assert!(validate_search(&SearchRequest::new("q").ori_question(" ")).is_err());
}

#[test]
fn tools_fetch_url_validation() {
    use crate::tools::validate_fetch_url;

    assert!(validate_fetch_url("https://example.com/a?b=c").is_ok());
    assert!(validate_fetch_url("HTTP://example.com").is_ok());
    assert!(validate_fetch_url("http://[::1]:8080/x").is_ok());
    assert!(validate_fetch_url("http://example.com/path@not-userinfo").is_ok());
    assert!(validate_fetch_url("").is_err());
    assert!(validate_fetch_url("ftp://example.com").is_err());
    assert!(validate_fetch_url("example.com").is_err());
    assert!(validate_fetch_url("https:///path").is_err());
    assert!(validate_fetch_url("https://user:pass@example.com").is_err());
    assert!(validate_fetch_url("https://user@example.com").is_err());
    let long = format!("https://example.com/{}", "a".repeat(8192));
    assert!(validate_fetch_url(&long).is_err());
}

#[tokio::test]
async fn tools_invalid_request_is_rejected_locally() {
    // 未启动服务：若本地校验失效会得到 Transport 而非 InvalidRequest
    let client = mock_client("127.0.0.1:1");
    let err = client
        .tools()
        .search(crate::types::SearchRequest::new("q").count(100))
        .await
        .unwrap_err();
    assert!(matches!(err, crate::DeepSeekError::InvalidRequest(_)));
}
