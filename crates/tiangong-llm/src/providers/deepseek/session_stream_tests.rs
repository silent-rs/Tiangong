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
use crate::message::{ChatMessage, MessageRole as LlmMessageRole};
use crate::tool::{ToolCall, ToolSpec};

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
