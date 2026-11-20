use serde_json::{Value, json};
use tiangong_llm::tool::ToolSpec;
use tiangong_llm::{ModelEndpoint, ProviderProtocol, SingleProviderClient};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn item(id: &str, call_id: Option<&str>, name: &str, args: &str) -> Value {
    let mut item = json!({"type":"function_call", "id":id, "name":name, "arguments":args});
    if let Some(call_id) = call_id {
        item["call_id"] = json!(call_id);
    }
    item
}
fn response(items: &[Value]) -> Value {
    json!({"id":"resp_probe", "object":"response", "created_at":0,
        "status":"completed", "model":"deepseek-v4-pro", "output":items,
        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}})
}
fn completed(items: &[Value]) -> Value {
    json!({"type":"response.completed", "sequence_number":99, "response":response(items)})
}
fn lifecycle(items: &[Value], deltas: bool) -> Vec<Value> {
    let mut events = Vec::new();
    for (index, call) in items.iter().enumerate() {
        let mut initial = call.clone();
        initial["arguments"] = json!("");
        events.push(
            json!({"type":"response.output_item.added", "sequence_number":index*3+1,
            "output_index":index, "item":initial}),
        );
        if deltas {
            events.push(
                json!({"type":"response.function_call_arguments.delta", "sequence_number":index*3+2,
                "output_index":index, "item_id":call["id"], "delta":call["arguments"]}),
            );
        }
        events.push(
            json!({"type":"response.output_item.done", "sequence_number":index*3+3,
            "output_index":index, "item":call}),
        );
    }
    events.push(completed(items));
    events
}
async fn probe(
    label: &str,
    protocol: ProviderProtocol,
    body: String,
    sse: bool,
    stream: bool,
    expected: &[Value],
) -> bool {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            body,
            if sse {
                "text/event-stream"
            } else {
                "application/json"
            },
        ))
        .expect(1)
        .mount(&server)
        .await;
    let client = SingleProviderClient::new(ModelEndpoint {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "deepseek-v4-pro".into(),
        protocol,
        timeout_ms: 5_000,
        ..Default::default()
    });
    let client=client.with_options(tiangong_llm::Options {
        tools:vec![ToolSpec { name:"read_file".into(), description:"读取".into(), input_schema:json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) }],
        max_output_tokens:256, ..Default::default()
    });
    let messages = vec![tiangong_types::Message::new(
        tiangong_types::MessageRole::User,
        "读取两个文件",
    )];
    let generation = client.stream(messages).unwrap();
    let result: tiangong_llm::ModelResponse = if stream {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            generation.collect_with(tx),
        )
        .await
        .unwrap()
        .unwrap()
    } else {
        tokio::time::timeout(std::time::Duration::from_secs(10), generation.collect())
            .await
            .unwrap()
            .unwrap()
    };
    let requests = server.received_requests().await.unwrap();
    let request_body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        request_body["stream"], true,
        "所有消费方式都必须只发流式请求"
    );
    let actual: Vec<Value> = result
        .tool_calls
        .iter()
        .map(|call| call.arguments.clone())
        .collect();
    let matches =
        actual.len() == expected.len() && expected.iter().all(|args| actual.contains(args));
    println!(
        "{}",
        json!({"case":label,"protocol":protocol.as_str(),"stream":stream,"sse":sse,
        "expected_arguments":expected,"actual_calls":result.tool_calls,"invalid_calls":format!("{:?}",result.invalid_tool_calls),
        "stop_reason":format!("{:?}",result.stop_reason),"preserved":matches})
    );
    server.verify().await;
    assert!(
        matches,
        "{protocol:?}/{label}: calls={actual:?} expected={expected:?}"
    );
    matches
}
fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}
#[tokio::test]
async fn stream_contract_matrix() {
    let expected = [json!({"path":"/tmp/a"}), json!({"path":"/tmp/b"})];
    for protocol in [ProviderProtocol::DeepSeek, ProviderProtocol::OpenAi] {
        let normal = [
            item("fc_a", Some("call_a"), "read_file", "{\"path\":\"/tmp/a\"}"),
            item("fc_b", Some("call_b"), "read_file", "{\"path\":\"/tmp/b\"}"),
        ];
        assert!(
            probe(
                "control_unique_ids",
                protocol,
                sse(&lifecycle(&normal, true)),
                true,
                true,
                &expected
            )
            .await
        );
        let repeated = [
            item("fc_a", Some("call_0"), "read_file", "{\"path\":\"/tmp/a\"}"),
            item("fc_b", Some("call_0"), "read_file", "{\"path\":\"/tmp/b\"}"),
        ];
        probe(
            "same_call_id_different_items",
            protocol,
            sse(&lifecycle(&repeated, true)),
            true,
            true,
            &expected,
        )
        .await;
        probe(
            "same_call_id_done_arguments",
            protocol,
            sse(&lifecycle(&repeated, false)),
            true,
            true,
            &expected,
        )
        .await;
        probe(
            "completed_only_unique_ids",
            protocol,
            sse(&[completed(&normal)]),
            true,
            true,
            &expected,
        )
        .await;
        probe(
            "completed_only_repeated_ids",
            protocol,
            sse(&[completed(&repeated)]),
            true,
            true,
            &expected,
        )
        .await;
        probe(
            "json_stream_repeated_ids",
            protocol,
            response(&repeated).to_string(),
            false,
            true,
            &expected,
        )
        .await;
        assert!(
            probe(
                "collected_json_repeated_ids",
                protocol,
                response(&repeated).to_string(),
                false,
                false,
                &expected
            )
            .await
        );
        let missing = [
            item("fc_a", None, "read_file", "{\"path\":\"/tmp/a\"}"),
            item("fc_b", None, "read_file", "{\"path\":\"/tmp/b\"}"),
        ];
        probe(
            "missing_call_id_with_item_id",
            protocol,
            sse(&lifecycle(&missing, true)),
            true,
            true,
            &expected,
        )
        .await;
        let mut late = lifecycle(&normal, false);
        late[0]["item"].as_object_mut().unwrap().remove("call_id");
        late[2]["item"].as_object_mut().unwrap().remove("call_id");
        probe(
            "call_id_arrives_in_done",
            protocol,
            sse(&late),
            true,
            true,
            &expected,
        )
        .await;
        let done_only = vec![
            json!({"type":"response.output_item.done","sequence_number":1,"output_index":0,"item":normal[0]}),
            json!({"type":"response.output_item.done","sequence_number":2,"output_index":1,"item":normal[1]}),
            completed(&normal),
        ];
        probe(
            "done_only_before_completed",
            protocol,
            sse(&done_only),
            true,
            true,
            &expected,
        )
        .await;
    }
    let protocol = ProviderProtocol::OpenAiChatCompletions;
    for same_id in [false, true] {
        let calls = json!([
            {"index":0,"id":if same_id {"call_0"} else {"call_a"},"type":"function","function":{"name":"read_file","arguments":"{\"path\":\"/tmp/a\"}"}},
            {"index":1,"id":if same_id {"call_0"} else {"call_b"},"type":"function","function":{"name":"read_file","arguments":"{\"path\":\"/tmp/b\"}"}}
        ]);
        let events = [
            json!({"choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}]}),
        ];
        let ok = probe(
            if same_id {
                "same_call_id_different_indexes"
            } else {
                "control_unique_ids"
            },
            protocol,
            format!("{}data: [DONE]\n\n", sse(&events)),
            true,
            true,
            &expected,
        )
        .await;
        if !same_id {
            assert!(ok);
        }
        let json_response = json!({"id":"chat_probe","object":"chat.completion","created":0,"model":"deepseek-v4-pro",
            "choices":[{"index":0,"message":{"role":"assistant","tool_calls":calls},"finish_reason":"tool_calls"}]});
        probe(
            if same_id {
                "json_stream_repeated_ids"
            } else {
                "json_stream_unique_ids"
            },
            protocol,
            json_response.to_string(),
            false,
            true,
            &expected,
        )
        .await;
        assert!(
            probe(
                "collected_json",
                protocol,
                json_response.to_string(),
                false,
                false,
                &expected
            )
            .await
        );
    }
}
