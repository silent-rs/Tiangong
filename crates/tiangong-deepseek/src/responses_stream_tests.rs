//! Responses 流式文本协议兜底的集成级测试（本地 mock SSE / JSON）。
//!
//! 覆盖：网关一次性 JSON 兜底（结构化调用跳过文本识别 / 纯协议文本
//! 转出）、failed 终止不转出工具、Confirmed 协议与结构化调用混合
//! 场景不泄漏协议原文。

use crate::testing::{MockServer, http_response, mock_client, sse_response};

#[tokio::test]
async fn responses_json_stream_with_structured_call_skips_text_protocol() {
    // 网关忽略 stream 返回一次性 JSON：output 同时含结构化 function_call 与
    // 协议样正文（如模型在讨论 DSML）。合成流不产结构化 item 事件，缓冲层
    // 必须从 created / completed 的完整 output 感知结构化通道并停用文本协议
    // 识别，否则协议讨论样文本会被额外转出 textcall 与真实调用叠加执行。
    use crate::types::ResponsesStreamEvent;
    use futures_util::StreamExt;
    let body = r#"{"id":"resp_1","object":"response","status":"completed","model":"m","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"DSML 协议示例：<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"demo\">\n</｜｜DSML｜｜tool_calls>"}]},{"type":"function_call","call_id":"call_real","name":"read_file","arguments":"{\"path\":\"/tmp/x\"}"}]}"#.as_bytes();
    let server = MockServer::start(vec![http_response("200 OK", "application/json", body)]);
    let client = mock_client(&server.addr);
    let mut stream = client
        .responses()
        .create_stream(crate::types::CreateResponseRequest {
            model: crate::types::MODEL_V4_FLASH.into(),
            instructions: Some("hi".into()),
            tools: Some(vec![crate::types::ResponsesTool::Function(
                crate::types::ResponsesFunctionTool {
                    name: "demo".into(),
                    description: None,
                    parameters: None,
                },
            )]),
            ..Default::default()
        })
        .await
        .unwrap();
    // SDK 合成流不模拟 function_call 事件（真实调用由调用方在终止事件的
    // 完整 output 中统一转出），因此这里断言：任何 function_call 事件都
    // 不出现——包括协议样文本被误转出的 textcall。
    let mut call_events = 0usize;
    let mut saw_completed = false;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            ResponsesStreamEvent::OutputItemAdded {
                item: crate::types::ResponseOutputItem::FunctionCall(_),
                ..
            } => call_events += 1,
            ResponsesStreamEvent::ResponseCompleted { response, .. } => {
                saw_completed = true;
                // 真实结构化调用仍完整保留在终止事件的 output 里，
                // 供调用方转出。
                assert!(response.output.iter().any(|item| matches!(
                    item,
                    crate::types::ResponseOutputItem::FunctionCall(call)
                        if call.call_id == "call_real"
                )));
            }
            _ => {}
        }
    }
    assert!(saw_completed, "流应以 completed 终止");
    assert_eq!(call_events, 0, "协议样文本不应被额外转出为调用事件");
}

#[tokio::test]
async fn responses_json_stream_resolves_pure_text_protocol_call() {
    // 一次性 JSON 只含协议正文（无结构化调用）：缓冲层在 completed 前解析并
    // 转出结构化 function_call 事件组，协议原文不作为正文透传。
    use crate::types::ResponsesStreamEvent;
    use futures_util::StreamExt;
    let body = r#"{"id":"resp_2","object":"response","status":"completed","model":"m","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"我来读取。\n<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"read_file\">\n<｜｜DSML｜｜parameter name=\"path\" string=\"true\">/tmp/x</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>"}]}]}"#.as_bytes();
    let server = MockServer::start(vec![http_response("200 OK", "application/json", body)]);
    let client = mock_client(&server.addr);
    let mut stream = client
        .responses()
        .create_stream(crate::types::CreateResponseRequest {
            model: crate::types::MODEL_V4_FLASH.into(),
            instructions: Some("hi".into()),
            tools: Some(vec![crate::types::ResponsesTool::Function(
                crate::types::ResponsesFunctionTool {
                    name: "read_file".into(),
                    description: None,
                    parameters: None,
                },
            )]),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut text = String::new();
    let mut resolved_calls = 0usize;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            ResponsesStreamEvent::OutputTextDelta { delta, .. } => text.push_str(&delta),
            ResponsesStreamEvent::OutputItemAdded {
                item: crate::types::ResponseOutputItem::FunctionCall(call),
                ..
            } => {
                assert_eq!(call.name, "read_file");
                resolved_calls += 1;
            }
            _ => {}
        }
    }
    assert_eq!(resolved_calls, 1, "纯协议文本应转出一个调用");
    assert!(!text.contains("DSML"), "协议原文不应作为正文透传：{text:?}");
    assert!(text.contains("我来读取。"), "说明文字应保留：{text:?}");
}

#[tokio::test]
async fn responses_stream_failed_terminal_skips_text_protocol_resolution() {
    // P2-2 回归：SSE 正文命中协议（Confirmed 缓冲）后收到 response.failed，
    // 失败响应不得把缓冲的协议文本转出为工具调用。
    use crate::types::ResponsesStreamEvent;
    use futures_util::StreamExt;
    let dsml = "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"read_file\">\n<｜｜DSML｜｜parameter name=\"path\" string=\"true\">/tmp/x</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>";
    let sse = format!(
        "data: {{\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":{dsml_json}}}\n\n\
         data: {{\"type\":\"response.failed\",\"sequence_number\":2,\"response\":{{\"id\":\"resp_f\",\"object\":\"response\",\"created_at\":0,\"status\":\"failed\",\"error\":{{\"code\":\"s\",\"message\":\"m\"}},\"model\":\"m\",\"output\":[]}}}}\n\n",
        dsml_json = serde_json::to_string(dsml).unwrap()
    );
    let server = MockServer::start(vec![sse_response(&sse)]);
    let client = mock_client(&server.addr);
    let mut stream = client
        .responses()
        .create_stream(crate::types::CreateResponseRequest {
            model: crate::types::MODEL_V4_FLASH.into(),
            instructions: Some("hi".into()),
            tools: Some(vec![crate::types::ResponsesTool::Function(
                crate::types::ResponsesFunctionTool {
                    name: "read_file".into(),
                    description: None,
                    parameters: None,
                },
            )]),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut call_events = 0usize;
    let mut saw_failed = false;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            ResponsesStreamEvent::OutputItemAdded {
                item: crate::types::ResponseOutputItem::FunctionCall(_),
                ..
            } => call_events += 1,
            ResponsesStreamEvent::ResponseFailed { .. } => saw_failed = true,
            _ => {}
        }
    }
    assert!(saw_failed, "流应以 failed 终止");
    assert_eq!(call_events, 0, "failed 不得转出文本协议工具调用");
}

#[tokio::test]
async fn responses_stream_confirmed_protocol_plus_structured_call_no_leak() {
    // 第五轮 review 混合场景 a：SSE 正文先命中协议（Confirmed 缓冲完整
    // 调用块），随后出现结构化 function_call——缓冲的协议文本必须丢弃
    // （不得冲刷为正文），也不得再转出 textcall（结构化调用权威，
    // 恰好一个调用）。
    use crate::types::ResponsesStreamEvent;
    use futures_util::StreamExt;
    let dsml =
        "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"demo\">\n</｜｜DSML｜｜tool_calls>";
    let added = r#"{"type":"response.output_item.added","sequence_number":2,"output_index":1,"item":{"type":"function_call","call_id":"call_real","name":"read_file","arguments":""}}"#;
    let completed = r#"{"type":"response.completed","sequence_number":3,"response":{"id":"resp_m","object":"response","created_at":0,"status":"completed","model":"m","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":REPLACE_ME}]},{"type":"function_call","call_id":"call_real","name":"read_file","arguments":"{\"path\":\"/tmp/x\"}"}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#
        .replace("REPLACE_ME", &serde_json::to_string(&format!("说明文字\n{dsml}")).unwrap());
    let mixed_json = serde_json::to_string(&format!("说明文字\n{dsml}")).unwrap();
    let sse = format!(
        "data: {{\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":{mixed_json}}}\n\n\
         data: {added}\n\n\
         data: {completed}\n\n"
    );
    let server = MockServer::start(vec![sse_response(&sse)]);
    let client = mock_client(&server.addr);
    let mut stream = client
        .responses()
        .create_stream(crate::types::CreateResponseRequest {
            model: crate::types::MODEL_V4_FLASH.into(),
            instructions: Some("hi".into()),
            tools: Some(vec![crate::types::ResponsesTool::Function(
                crate::types::ResponsesFunctionTool {
                    name: "demo".into(),
                    description: None,
                    parameters: None,
                },
            )]),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut text = String::new();
    let mut calls = Vec::new();
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            ResponsesStreamEvent::OutputTextDelta { delta, .. } => text.push_str(&delta),
            ResponsesStreamEvent::OutputItemAdded {
                item: crate::types::ResponseOutputItem::FunctionCall(call),
                ..
            } => calls.push(call.call_id),
            ResponsesStreamEvent::ResponseCompleted { response, .. } => {
                // 终止事件 output 已被重写：可见文本不含协议原文。
                assert!(
                    !response.output_text().contains("DSML"),
                    "重写后 output_text 不得含协议原文：{}",
                    response.output_text()
                );
            }
            _ => {}
        }
    }
    assert!(
        !text.contains("DSML") && !text.contains("｜｜"),
        "协议原文不得作为正文透出：{text:?}"
    );
    assert!(text.contains("说明文字"), "说明文字应保留：{text:?}");
    assert_eq!(calls, vec!["call_real"], "只应转出结构化调用");
}
