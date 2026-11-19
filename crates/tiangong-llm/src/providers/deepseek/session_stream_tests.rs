//! DeepSeek Responses 流式协议的端到端测试（wiremock 走完整
//! SingleProviderClient 链路）。
//!
//! 覆盖：网关一次性 JSON 兜底不丢工具调用、纯协议回复与残缺协议
//! 回复的原文不泄漏（SDK 缓冲 + llm 兜底双层剥离）。

use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::ModelEndpoint;
use crate::ModelRequest;
use crate::ProviderProtocol;
use crate::SingleProviderClient;
use crate::message::{
    ChatMessage, MessageContent as LlmMessageContent, MessageRole as LlmMessageRole,
};
use crate::tool::{
    ToolCall, ToolResult as LlmToolResult, ToolResultContent as LlmToolResultContent, ToolSpec,
};

/// 与 session_tests 同形的最小请求（固定系统提示 + 单条用户消息）。
fn request(session_id: &str) -> ModelRequest {
    ModelRequest {
        session_id: Some(session_id.to_string()),
        system: "固定系统提示".to_string(),
        messages: vec![ChatMessage::text(LlmMessageRole::User, "回答 OK")],
        max_output_tokens: Some(256),
        ..Default::default()
    }
}

#[tokio::test]
async fn deepseek_gateway_json_stream_keeps_tool_calls() {
    // 网关忽略 stream=true 返回一次性 JSON 时，SDK 在内部把完整响应合成事件流。
    // 该兜底路径必须保留工具调用：function_call item 需要合成完整的
    // Start → Delta → End 事件序列，否则上层聚合会因缺少 Start（工具名为空）
    // 而静默丢弃整个工具调用（PR #739 review P1 回归用例）。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_json_gateway",
            "object": "response",
            "created_at": 0,
            "status": "completed",
            "model": "test-model",
            "output": [
                {"type": "function_call", "call_id": "call_1", "name": "read_file",
                 "arguments": "{\"path\":\"/tmp/x\"}"}
            ],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        })))
        .mount(&server)
        .await;
    let client = SingleProviderClient::new(ModelEndpoint {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "test-model".into(),
        protocol: ProviderProtocol::DeepSeek,
        ..Default::default()
    });
    let req = ModelRequest {
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "读取".into(),
            input_schema: json!({"type": "object"}),
        }],
        ..request("json-gateway")
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let response = client.stream_async(req, tx).await.unwrap();
    assert_eq!(
        response.tool_calls,
        vec![ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "/tmp/x"}),
        }],
        "一次性 JSON 兜底流不得丢弃工具调用"
    );
    assert_eq!(
        response.stop_reason,
        Some(crate::response::StopReason::ToolUse)
    );
    assert_eq!(response.usage.total_tokens, 2);
}

#[tokio::test]
async fn deepseek_pure_protocol_stream_does_not_leak_text() {
    // 第三轮 review P1 端到端回归：整条回复只有协议块（无普通正文增量）。
    // SDK 缓冲层把协议文本消费为 textcall 工具调用，completed 兜底不得把
    // 服务端原始 output 里的协议原文回灌为正文。
    let dsml = "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"read_file\">\n<｜｜DSML｜｜parameter name=\"path\" string=\"true\">/tmp/x</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>";
    let delta = format!(
        "data: {{\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":{}}}\n\n",
        serde_json::to_string(dsml).unwrap()
    );
    let completed = json!({
        "type": "response.completed",
        "sequence_number": 2,
        "response": {
            "id": "resp_dsml", "object": "response", "created_at": 0,
            "status": "completed", "model": "test-model",
            "output": [{"type": "message", "id": "msg_1", "role": "assistant",
                "content": [{"type": "output_text", "text": dsml}]}],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        }
    });
    let body = format!("{delta}data: {completed}\n\n");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let client = SingleProviderClient::new(ModelEndpoint {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "test-model".into(),
        protocol: ProviderProtocol::DeepSeek,
        ..Default::default()
    });
    let req = ModelRequest {
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "读取".into(),
            input_schema: json!({"type": "object"}),
        }],
        ..request("dsml-stream")
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let response = client.stream_async(req, tx).await.unwrap();
    assert!(
        !response.text.contains("DSML") && !response.text.contains("｜｜"),
        "协议原文不得回灌正文：{:?}",
        response.text
    );
    assert_eq!(response.tool_calls.len(), 1, "应转出一个工具调用");
    assert_eq!(response.tool_calls[0].name, "read_file");
    assert_eq!(
        response.stop_reason,
        Some(crate::response::StopReason::EndTurn)
    );
}

#[tokio::test]
async fn deepseek_malformed_protocol_stream_discards_text() {
    // 第四轮 review P2-4 回归：纯残缺协议（SDK Confirmed 后解析失败、整体
    // 抛弃、无文本增量）——终止事件 output 被 SDK 重写为可见文本（空），
    // llm 兜底不得把原始协议文本回灌为正文。
    let malformed = "<｜｜DSML｜｜tool_calls>\n这不是调用，只是一段说明";
    let delta = format!(
        "data: {{\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":{}}}\n\n",
        serde_json::to_string(malformed).unwrap()
    );
    let completed = json!({
        "type": "response.completed",
        "sequence_number": 2,
        "response": {
            "id": "resp_bad", "object": "response", "created_at": 0,
            "status": "completed", "model": "test-model",
            "output": [{"type": "message", "id": "msg_1", "role": "assistant",
                "content": [{"type": "output_text", "text": malformed}]}],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        }
    });
    let body = format!("{delta}data: {completed}\n\n");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let client = SingleProviderClient::new(ModelEndpoint {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "test-model".into(),
        protocol: ProviderProtocol::DeepSeek,
        ..Default::default()
    });
    let req = ModelRequest {
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "读取".into(),
            input_schema: json!({"type": "object"}),
        }],
        ..request("malformed-dsml")
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let response = client.stream_async(req, tx).await.unwrap();
    assert!(
        !response.text.contains("DSML") && !response.text.contains("｜｜"),
        "残缺协议原文不得回灌正文：{:?}",
        response.text
    );
    assert!(response.tool_calls.is_empty(), "残缺协议不得转出工具调用");
}

/// 服务端 function_call 不带 call_id、只带 item id（fc_1，每轮局部编号）
/// 时，流式聚合回退用 item id 作为调用 id。两轮流各自回退到同一个
/// `fc_1` → 跨轮重复。此测试固定该回退行为，供跨轮重复回归测试引用。
#[tokio::test]
async fn deepseek_stream_call_id_missing_falls_back_to_item_id() {
    let added = json!({
        "type": "response.output_item.added",
        "sequence_number": 1,
        "output_index": 0,
        "item": {"type": "function_call", "id": "fc_1", "name": "read_file", "arguments": ""}
    });
    let delta = json!({
        "type": "response.function_call_arguments.delta",
        "sequence_number": 2,
        "item_id": "fc_1",
        "output_index": 0,
        "delta": "{\"path\":\"/tmp/x\"}"
    });
    let completed = json!({
        "type": "response.completed",
        "sequence_number": 3,
        "response": {
            "id": "resp_fb", "object": "response", "created_at": 0,
            "status": "completed", "model": "test-model",
            "output": [{"type": "function_call", "id": "fc_1", "name": "read_file",
                        "arguments": "{\"path\":\"/tmp/x\"}"}],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        }
    });
    let body = format!("data: {added}\n\ndata: {delta}\n\ndata: {completed}\n\n");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let client = SingleProviderClient::new(ModelEndpoint {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "test-model".into(),
        protocol: ProviderProtocol::DeepSeek,
        ..Default::default()
    });
    let req = ModelRequest {
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "读取".into(),
            input_schema: json!({"type": "object"}),
        }],
        ..request("call-id-fallback")
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let response = client.stream_async(req, tx).await.unwrap();
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(
        response.tool_calls[0].id, "fc_1",
        "call_id 缺失时回退用 item id（每轮局部编号，跨轮重复）"
    );
}

/// 两轮工具调用历史携带重复 call_id（服务端每轮局部编号 / 网关固定 id）
/// 时，回传构造必须重写后续重复：首个 call_id 保留原值，之后重复的
/// function_call 与对应 function_call_output 同步改写为全局唯一 id
/// （官方 Responses API 要求 call_id 唯一，历史每轮全量回传不得重复，
/// 见 dsml.rs 文本调用注释；重复直接透传会触发服务端
/// 「function call 与 output 不匹配」类 400）。
#[tokio::test]
async fn deepseek_history_repeated_call_id_passes_through() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_repeat", "object": "response", "created_at": 0,
            "status": "completed", "model": "test-model",
            "output": [],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        })))
        .mount(&server)
        .await;
    let client = SingleProviderClient::new(ModelEndpoint {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "test-model".into(),
        protocol: ProviderProtocol::DeepSeek,
        ..Default::default()
    });
    let tool_call = |id: &str| {
        LlmMessageContent::ToolCall(ToolCall {
            id: id.to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "/tmp/x"}),
        })
    };
    let tool_result = |id: &str| {
        LlmMessageContent::ToolResult(LlmToolResult {
            tool_call_id: id.to_string(),
            content: LlmToolResultContent::Text(format!("result-{id}")),
            is_error: false,
        })
    };
    // 两轮工具调用：fc_1 各出现一次（各自成对，sanitize 不会剔除）。
    let req = ModelRequest {
        messages: vec![
            ChatMessage::text(LlmMessageRole::User, "读文件"),
            ChatMessage::new(LlmMessageRole::Assistant, vec![tool_call("fc_1")]),
            ChatMessage::new(LlmMessageRole::Tool, vec![tool_result("fc_1")]),
            ChatMessage::new(LlmMessageRole::Assistant, vec![tool_call("fc_1")]),
            ChatMessage::new(LlmMessageRole::Tool, vec![tool_result("fc_1")]),
        ],
        ..request("repeat-call-id")
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    client.stream_async(req, tx).await.unwrap();

    let requests = server.received_requests().await.expect("应捕获到请求");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("请求体应为 JSON");
    // input 序列化形态为字符串简写或 items 数组，两种都兼容取数组。
    let items = body["input"]["0"]
        .as_array()
        .or_else(|| body["input"].as_array())
        .cloned()
        .unwrap_or_default();
    let call_ids: Vec<String> = items
        .iter()
        .filter(|item| item["type"] == "function_call")
        .filter_map(|item| item["call_id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(call_ids.len(), 2, "两轮调用都应保留");
    assert_eq!(
        call_ids[0], "fc_1",
        "首个出现的 call_id 保留原值（稳定前缀）"
    );
    assert!(
        call_ids[1].starts_with("call_") && call_ids[1] != "fc_1",
        "后续重复 call_id 应重写为全局唯一 id，实际: {}",
        call_ids[1]
    );
    // 对应的 function_call_output 必须同步重写，否则仍不成对。
    let output_ids: Vec<String> = items
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .filter_map(|item| item["call_id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        output_ids, call_ids,
        "output 的 call_id 必须与 function_call 一一同步（含重写）"
    );
}

/// 双策略端到端（层 1 接收 + 层 2 回传）：网关每轮返回相同 call_id
/// （fc_1，无 item id）。第一轮无历史冲突 → 保留 server 原值 fc_1；
/// 第二轮请求历史已含 fc_1，响应又解析出 fc_1 → 接收层当场换发全局
/// 唯一 textcall id；回传 input 中 fc_1 仅一份、新调用唯一，无重复。
#[tokio::test]
async fn deepseek_two_rounds_conflicting_id_stays_unique() {
    let added = json!({
        "type": "response.output_item.added",
        "sequence_number": 1, "output_index": 0,
        "item": {"type": "function_call", "call_id": "fc_1", "name": "read_file", "arguments": ""}
    });
    let delta = json!({
        "type": "response.function_call_arguments.delta",
        "sequence_number": 2, "item_id": "fc_1", "output_index": 0,
        "delta": "{\"path\":\"/tmp/x\"}"
    });
    let completed = json!({
        "type": "response.completed", "sequence_number": 3,
        "response": {
            "id": "resp_round", "object": "response", "created_at": 0,
            "status": "completed", "model": "test-model",
            "output": [{"type": "function_call", "call_id": "fc_1", "name": "read_file",
                        "arguments": "{\"path\":\"/tmp/x\"}"}],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        }
    });
    let body = format!("data: {added}\n\ndata: {delta}\n\ndata: {completed}\n\n");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let client = SingleProviderClient::new(ModelEndpoint {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "test-model".into(),
        protocol: ProviderProtocol::DeepSeek,
        ..Default::default()
    });
    let tools = vec![ToolSpec {
        name: "read_file".into(),
        description: "读取".into(),
        input_schema: json!({"type": "object"}),
    }];

    // 第一轮：无历史 → 保留 server 原值 fc_1（以 server 为主）。
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let first = client
        .clone()
        .stream_async(
            ModelRequest {
                tools: tools.clone(),
                ..request("two-rounds")
            },
            tx,
        )
        .await
        .unwrap();
    assert_eq!(first.tool_calls[0].id, "fc_1");

    // 第二轮：历史含第一轮的 fc_1 调用与结果，响应又是 fc_1 → 换发唯一 id。
    let tool_call = |id: &str| {
        LlmMessageContent::ToolCall(ToolCall {
            id: id.to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "/tmp/x"}),
        })
    };
    let tool_result = |id: &str| {
        LlmMessageContent::ToolResult(LlmToolResult {
            tool_call_id: id.to_string(),
            content: LlmToolResultContent::Text("ok".to_string()),
            is_error: false,
        })
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let second = client
        .stream_async(
            ModelRequest {
                tools,
                messages: vec![
                    ChatMessage::text(LlmMessageRole::User, "读文件"),
                    ChatMessage::new(LlmMessageRole::Assistant, vec![tool_call("fc_1")]),
                    ChatMessage::new(LlmMessageRole::Tool, vec![tool_result("fc_1")]),
                ],
                ..request("two-rounds")
            },
            tx,
        )
        .await
        .unwrap();
    assert!(
        second.tool_calls[0].id.starts_with("call_"),
        "第二轮与历史冲突的 fc_1 应换发唯一 id：{}",
        second.tool_calls[0].id
    );

    // 第二次请求的回传 input：fc_1 仅一份（首个保留），新调用唯一，无重复。
    let requests = server.received_requests().await.expect("应捕获请求");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[1].body).expect("请求体应为 JSON");
    let items = body["input"]["0"]
        .as_array()
        .or_else(|| body["input"].as_array())
        .cloned()
        .unwrap_or_default();
    let call_ids: Vec<String> = items
        .iter()
        .filter(|item| item["type"] == "function_call")
        .filter_map(|item| item["call_id"].as_str().map(str::to_string))
        .collect();
    let mut unique = call_ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        call_ids.len(),
        unique.len(),
        "回传 call_id 不得重复：{call_ids:?}"
    );
    assert!(
        call_ids.contains(&"fc_1".to_string()),
        "首个 server id 保留"
    );
}
