use super::*;
use crate::client::rerank_provider_from_config;
use crate::embedding::{EmbeddingEndpointConfig, embedding_provider_from_config};
use crate::rerank::{RerankEndpointConfig, RerankRequest};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn anthropic_usage_keeps_cache_fields_across_partial_and_repeated_frames() {
    let cases = [
        // Anthropic: 输入与缓存先返回，后续帧只带累计输出。
        (
            json!({"input_tokens":55,"output_tokens":0,"cache_read_input_tokens":2496,"cache_creation_input_tokens":128}),
            json!({"output_tokens":3}),
            json!({"input_tokens":55,"output_tokens":3,"cache_read_input_tokens":2496,"cache_creation_input_tokens":128}),
            2679,
            Some(2496),
            Some(183),
        ),
        // GLM: 开始帧都是零，结束帧一次性返回用量。
        (
            json!({"input_tokens":0,"output_tokens":0}),
            json!({"input_tokens":55,"output_tokens":3,"cache_read_input_tokens":2496}),
            json!({"input_tokens":55,"output_tokens":3,"cache_read_input_tokens":2496}),
            2551,
            Some(2496),
            Some(55),
        ),
        // 缓存读取量晚到，不能丢掉之前收到的未缓存输入与写入量。
        (
            json!({"input_tokens":55,"output_tokens":0,"cache_creation_input_tokens":128}),
            json!({"output_tokens":3,"cache_read_input_tokens":2496}),
            json!({"input_tokens":55,"output_tokens":3,"cache_read_input_tokens":2496,"cache_creation_input_tokens":128}),
            2679,
            Some(2496),
            Some(183),
        ),
        (
            json!({"input_tokens":20,"cache_read_input_tokens":0}),
            json!({"output_tokens":3}),
            json!({"input_tokens":20,"output_tokens":3,"cache_read_input_tokens":0}),
            20,
            Some(0),
            Some(20),
        ),
        (
            json!({"input_tokens":20}),
            json!({"output_tokens":3}),
            json!({"input_tokens":20,"output_tokens":3}),
            20,
            None,
            None,
        ),
    ];
    for (start, delta, full, input, hit, miss) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let message = json!({"id":"usage-probe","type":"message","role":"assistant","model":"glm-5.3-flash","content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn","usage":full});
            if body["stream"] != true { return ResponseTemplate::new(200).set_body_json(message); }
            let events = [
                json!({"type":"message_start","message":{"id":"usage-probe","model":"glm-5.3-flash","role":"assistant","content":[],"usage":start}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"OK"}}),
                json!({"type":"message_delta","delta":{},"usage":delta}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":delta}),
                json!({"type":"message_stop"}),
            ];
            ResponseTemplate::new(200).insert_header("content-type","text/event-stream")
                .set_body_string(events.iter().map(|e| format!("event: {}\ndata: {e}\n\n",e["type"].as_str().unwrap())).collect::<String>())
        }).expect(2).mount(&server).await;
        let client = SingleProviderClient::new(ModelEndpoint {
            base_url: server.uri(),
            api_key: "test".into(),
            model: "glm-5.3-flash".into(),
            protocol: ProviderProtocol::Anthropic,
            ..Default::default()
        });
        let req = request("cache-usage");
        let ordinary = client.complete_async(&req).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let streamed = client.stream_async(req, tx).await.unwrap();
        let mut snapshots = TokenUsage::default();
        while let Some(chunk) = rx.recv().await {
            if let Some(usage) = chunk.usage {
                snapshots.merge_snapshot(&usage.into());
            }
        }
        for usage in [ordinary.usage, streamed.usage, snapshots] {
            assert_eq!(usage.prompt_tokens, input);
            assert_eq!(usage.completion_tokens, 3);
            assert_eq!(usage.total_tokens, input + 3);
            assert_eq!(usage.prompt_cache_hit_tokens, hit);
            assert_eq!(usage.prompt_cache_miss_tokens, miss);
        }
    }
}

fn request(session_id: &str) -> ModelRequest {
    ModelRequest {
        session_id: Some(session_id.to_string()),
        user_input: String::new(),
        context: vec![
            Message::new(MessageRole::System, "固定系统提示"),
            Message::new(MessageRole::User, "回答 OK"),
        ],
        reasoning_effort: ReasoningEffort::None,
        max_output_tokens: Some(256),
        ..Default::default()
    }
}

fn reply(protocol: ProviderProtocol) -> Value {
    if protocol == ProviderProtocol::OpenAi {
        json!({"id":"resp_test","status":"completed","model":"test-model",
            "output":[{"type":"message","content":[{"type":"output_text","text":"OK"}]}],
            "usage":{"input_tokens":100,"output_tokens":2,"total_tokens":102,"input_tokens_details":{"cached_tokens":80}}})
    } else {
        json!({"id":"chat_test","created":0,"object":"chat.completion","model":"test-model",
            "choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"OK"}}],
            "usage":{"prompt_tokens":100,"completion_tokens":2,"total_tokens":102,"prompt_tokens_details":{"cached_tokens":80}}})
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_sync_async_and_stream_requests_preserve_parameters_and_usage() {
    for protocol in [
        ProviderProtocol::OpenAi,
        ProviderProtocol::OpenAiChatCompletions,
        ProviderProtocol::DeepSeek,
        ProviderProtocol::Anthropic,
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(move |request: &wiremock::Request| {
            let payload: Value = serde_json::from_slice(&request.body).unwrap();
            if protocol != ProviderProtocol::Anthropic {
                return ResponseTemplate::new(200).set_body_json(reply(protocol));
            }
            if payload["stream"] != true {
                return ResponseTemplate::new(200).set_body_json(json!({"id":"m","type":"message","role":"assistant","model":"test-model","content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn","usage":{"input_tokens":20,"output_tokens":2,"cache_read_input_tokens":80}}));
            }
            let events = [
                json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"test-model","content":[],"usage":{"input_tokens":20,"output_tokens":0,"cache_read_input_tokens":80}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"OK"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
                json!({"type":"message_stop"}),
            ];
            ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
                .set_body_string(events.iter().map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap())).collect::<String>())
        }).expect(4).mount(&server).await;
        let client = SingleProviderClient::new(ModelEndpoint {
            base_url: server.uri(),
            api_key: "test-key".into(),
            model: "test-model".into(),
            protocol,
            headers: [("x-session".into(), "${session_id}".into())].into(),
            ..Default::default()
        });
        let req = ModelRequest {
            tools: vec![ToolSpec {
                name: "probe".into(),
                description: "固定工具".into(),
                input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            }],
            tool_choice: Some(ToolChoice::None),
            temperature: Some(0.2),
            timeout_ms: Some(5_000),
            ..request("unified-session")
        };
        let first = client.complete_async(&req).await.unwrap();
        let sync_client = client.clone();
        let sync_req = req.clone();
        let second = tokio::task::spawn_blocking(move || sync_client.complete(&sync_req))
            .await
            .unwrap()
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let third = client.clone().stream_async(req.clone(), tx).await.unwrap();
        let chunks: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(chunks.iter().any(|chunk| chunk.usage.is_some()));
        let (fourth, chunks) = tokio::task::spawn_blocking(move || {
            let mut chunks = Vec::new();
            let response = client
                .complete_stream(&req, &mut |chunk| chunks.push(chunk.clone()))
                .unwrap();
            (response, chunks)
        })
        .await
        .unwrap();
        assert!(chunks.iter().any(|chunk| chunk.usage.is_some()));
        for response in [second, third, fourth] {
            assert_eq!(response.text, first.text);
            assert_eq!(response.stop_reason, first.stop_reason, "{protocol:?}");
            assert_eq!(
                serde_json::to_value(response.usage).unwrap(),
                serde_json::to_value(&first.usage).unwrap()
            );
        }
        let requests = server.received_requests().await.unwrap();
        let mut previous = None;
        for request in requests {
            assert_eq!(request.headers.get("x-session").unwrap(), "unified-session");
            let mut payload: Value = serde_json::from_slice(&request.body).unwrap();
            payload.as_object_mut().unwrap().remove("stream");
            payload.as_object_mut().unwrap().remove("stream_options");
            if let Some(previous) = previous {
                assert_eq!(payload, previous);
            }
            previous = Some(payload);
        }
    }
}

#[tokio::test]
async fn empty_truncated_response_preserves_stop_reason_and_usage_in_both_modes() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id":"empty","choices":[{"index":0,"finish_reason":"length","message":{"role":"assistant","content":""}}],
        "usage":{"prompt_tokens":100,"completion_tokens":256,"total_tokens":356,"prompt_tokens_details":{"cached_tokens":80}}
    }))).expect(2).mount(&server).await;
    let client = SingleProviderClient::new(ModelEndpoint {
        base_url: server.uri(),
        api_key: "test".into(),
        model: "test-model".into(),
        protocol: ProviderProtocol::OpenAiChatCompletions,
        ..Default::default()
    });
    let req = request("empty-response");
    let first = client.complete_async(&req).await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let second = client.stream_async(req, tx).await.unwrap();
    for response in [first, second] {
        assert!(response.text.is_empty());
        assert_eq!(
            response.stop_reason,
            Some(crate::response::StopReason::MaxTokens)
        );
        assert_eq!(response.usage.total_tokens, 356);
        assert_eq!(response.usage.prompt_cache_hit_tokens, Some(80));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lite_and_text_helpers_preserve_their_request_options() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(reply(ProviderProtocol::OpenAiChatCompletions)),
        )
        .expect(2)
        .mount(&server)
        .await;
    let client = SingleProviderClient::new(ModelEndpoint {
        base_url: server.uri(),
        api_key: "test".into(),
        model: "test-model".into(),
        protocol: ProviderProtocol::OpenAiChatCompletions,
        ..Default::default()
    });
    assert_eq!(
        tokio::task::spawn_blocking(move || client.complete_lite("标题输入"))
            .await
            .unwrap()
            .unwrap(),
        "OK"
    );
    let config = crate::text::LlmEndpointConfig {
        protocol: ProviderProtocol::OpenAiChatCompletions,
        max_retries: 0,
        ..crate::text::LlmEndpointConfig::new("test", server.uri(), "test-model")
    };
    let (text, usage) = crate::text::complete_text_with_usage(&config, "固定系统", "文本输入", 123)
        .await
        .unwrap();
    assert_eq!(text, "OK");
    assert_eq!(usage.unwrap().prompt_cache_hit_tokens, Some(80));
    let requests = server.received_requests().await.unwrap();
    for (request, limit, temperature) in [(&requests[0], 200, 0.3), (&requests[1], 123, 0.2)] {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["max_tokens"], limit);
        assert!((body["temperature"].as_f64().unwrap() - temperature).abs() < 0.0001);
        assert_eq!(body["thinking"]["type"], "disabled");
    }
    for protocol in [
        ProviderProtocol::OpenAiChatCompletions,
        ProviderProtocol::OpenAi,
    ] {
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(2)
            .mount(&server)
            .await;
        let config = crate::text::LlmEndpointConfig {
            protocol,
            ..config.clone()
        };
        assert!(
            crate::text::complete_text(&config, "固定系统", "失败输入", 123)
                .await
                .is_err()
        );
        let client = SingleProviderClient::new(ModelEndpoint {
            base_url: server.uri(),
            api_key: "test".into(),
            model: "test-model".into(),
            protocol,
            ..Default::default()
        });
        let provider = client.build_provider_dispatch(5_000, None, 0).unwrap();
        assert!(
            provider
                .stream(client.build_provider_request(&request("no-retry")).unwrap())
                .await
                .is_err()
        );
        server.verify().await;
    }
}

#[tokio::test]
async fn session_headers_reach_streaming_and_non_streaming_openai_requests() {
    for protocol in [
        ProviderProtocol::OpenAi,
        ProviderProtocol::OpenAiChatCompletions,
    ] {
        let server = MockServer::start().await;
        let session_id = scru128::new().to_string();
        Mock::given(method("POST"))
            .and(header("x-opencode-session", session_id.as_str()))
            .and(header("x-custom", "unchanged"))
            .and(header(
                "user-agent",
                concat!("tiangong/", env!("CARGO_PKG_VERSION")),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply(protocol)))
            .expect(2)
            .mount(&server)
            .await;
        let client = SingleProviderClient::new(ModelEndpoint {
            base_url: server.uri(),
            api_key: "test-key".into(),
            model: "test-model".into(),
            protocol,
            headers: [
                ("x-opencode-session".into(), "${session_id}".into()),
                ("x-custom".into(), "unchanged".into()),
            ]
            .into(),
            ..Default::default()
        });
        let req = request(&session_id);
        let response = client.complete_async(&req).await.unwrap();
        assert_eq!(response.text, "OK");
        assert_eq!(response.usage.prompt_cache_hit_tokens, Some(80));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert_eq!(client.stream_async(req, tx).await.unwrap().text, "OK");
        let requests = server.received_requests().await.unwrap();
        for request in requests {
            let payload: Value = serde_json::from_slice(&request.body).unwrap();
            if protocol == ProviderProtocol::OpenAi {
                assert_eq!(payload["prompt_cache_key"], session_id);
                assert_eq!(payload["max_output_tokens"], 256);
            } else {
                assert!(payload.get("prompt_cache_key").is_none());
                assert_eq!(payload["max_tokens"], 256);
            }
            assert!(payload.get("session_id").is_none());
        }
        server.verify().await;
    }
}

#[tokio::test]
async fn rerank_request_carries_tiangong_user_agent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rerank"))
        .and(header(
            "user-agent",
            concat!("tiangong/", env!("CARGO_PKG_VERSION")),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"results":[{"index":1,"relevance_score":0.87}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider = rerank_provider_from_config(&RerankEndpointConfig {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "rerank-probe".into(),
        protocol: ProviderProtocol::OpenAiChatCompletions,
        timeout: Duration::from_secs(10),
    })
    .unwrap();
    let response = provider
        .rerank(RerankRequest {
            query: "天工".into(),
            documents: vec!["甲".into(), "乙".into()],
            top_n: 1,
        })
        .await
        .unwrap();
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].index, 1);
    server.verify().await;
}

#[tokio::test]
async fn embedding_request_carries_tiangong_user_agent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header(
            "user-agent",
            concat!("tiangong/", env!("CARGO_PKG_VERSION")),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"embedding":[0.25,0.5],"index":0}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider = embedding_provider_from_config(&EmbeddingEndpointConfig {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "embedding-probe".into(),
        protocol: ProviderProtocol::OpenAiChatCompletions,
        timeout: Duration::from_secs(10),
        dimension: 2,
    })
    .unwrap();
    let vectors = provider.embed(vec!["天工".into()]).await.unwrap();
    assert_eq!(vectors, vec![vec![0.25, 0.5]]);
    server.verify().await;
}

#[tokio::test]
async fn streamed_reasoning_is_replayed_verbatim_after_message_reload() {
    let server = MockServer::start().await;
    let reasoning = "  分析工具结果\n保留原文\n";
    let events = [
        json!({"choices":[{"delta":{"reasoning_content":reasoning}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"probe","type":"function","function":{"name":"read_probe","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
    ];
    let body = format!(
        "{}data: [DONE]\n\n",
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>()
    );
    Mock::given(method("POST"))
        .and(header("x-opencode-session", "reasoning_session"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .expect(3)
        .mount(&server)
        .await;
    let client = SingleProviderClient::new(ModelEndpoint {
        base_url: server.uri(),
        api_key: "test".into(),
        model: "deepseek-v4-flash".into(),
        protocol: ProviderProtocol::OpenAiChatCompletions,
        headers: [("x-opencode-session".into(), "${session_id}".into())].into(),
        ..Default::default()
    });
    let mut req = request("reasoning_session");
    req.reasoning_effort = ReasoningEffort::High;
    let mut prefix = Vec::new();
    for _ in 0..3 {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let response = client
            .clone()
            .stream_async(
                req.clone().with_tools(
                    vec![ToolSpec {
                        name: "read_probe".into(),
                        description: "读取".into(),
                        input_schema: json!({"type":"object"}),
                    }],
                    None,
                ),
                tx,
            )
            .await
            .unwrap();
        assert_eq!(response.reasoning_content, reasoning);
        let requests = server.received_requests().await.unwrap();
        let payload: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        let messages = payload["messages"].as_array().unwrap();
        assert_eq!(&messages[..prefix.len()], prefix.as_slice());
        for message in messages
            .iter()
            .filter(|message| message["role"] == "assistant")
        {
            assert_eq!(message["reasoning_content"], reasoning);
        }
        prefix = messages.clone();
        let assistant = Message::with_reasoning(
            MessageRole::Assistant,
            response.text,
            response.reasoning_content,
        )
        .with_tool_calls(
            response
                .tool_calls
                .into_iter()
                .map(|call| tiangong_types::MessageToolCall {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                })
                .collect(),
        );
        req.context.push(assistant);
        req.context
            .push(Message::tool_result("probe", "read_probe", "OK", false));
        req.context = serde_json::from_slice(&serde_json::to_vec(&req.context).unwrap()).unwrap();
    }
}

#[tokio::test]
async fn retry_retains_the_same_session_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(header("x-opencode-session", "session_retry"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(reply(ProviderProtocol::OpenAiChatCompletions)),
        )
        .mount(&server)
        .await;
    let client = SingleProviderClient::new(ModelEndpoint {
        base_url: server.uri(),
        api_key: "test-key".into(),
        model: "test-model".into(),
        headers: [("x-opencode-session".into(), "${session_id}".into())].into(),
        ..Default::default()
    });
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    client
        .stream_async(request("session_retry"), tx)
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].headers.get("x-opencode-session"),
        requests[1].headers.get("x-opencode-session")
    );
    assert_eq!(requests[0].body, requests[1].body);
}

#[test]
fn provider_headers_survive_resolution_and_legacy_configs_remain_valid() {
    let config: crate::models_config::ModelsConfig = serde_json::from_value(json!({
        "providers":{"test":{"base_url":"http://localhost","api_key":"test-key","headers":{"x-opencode-session":"${session_id}"}}},
        "models":{"model":{"provider":"test","model":"test-model"}},"routing":{"chat":"model"}
    })).unwrap();
    let resolved = config
        .resolve_slot(crate::models_config::RoutingSlot::Chat)
        .unwrap();
    let endpoint = ModelEndpoint::from_resolved(resolved);
    assert_eq!(endpoint.headers["x-opencode-session"], "${session_id}");
    assert_eq!(endpoint.to_resolved().headers, endpoint.headers);
    let legacy: crate::models_config::ProviderConfig =
        serde_json::from_value(json!({"base_url":"http://localhost","api_key":"test-key"}))
            .unwrap();
    assert!(legacy.headers.is_empty());
}

#[tokio::test]
async fn deepseek_uses_user_id_and_openai_uses_prompt_cache_key() {
    for protocol in [ProviderProtocol::DeepSeek, ProviderProtocol::OpenAi] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply(protocol)))
            .mount(&server)
            .await;
        let client = SingleProviderClient::new(ModelEndpoint {
            base_url: server.uri(),
            api_key: "test-key".into(),
            model: "test-model".into(),
            protocol,
            ..Default::default()
        });
        for id in ["session_one", "session_two"] {
            client.complete_async(&request(id)).await.unwrap();
        }
        for (request, id) in server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .zip(["session_one", "session_two"])
        {
            let payload: Value = serde_json::from_slice(&request.body).unwrap();
            let field = if protocol == ProviderProtocol::DeepSeek {
                "user_id"
            } else {
                "prompt_cache_key"
            };
            assert_eq!(payload[field], id);
            assert!(payload.get("session_id").is_none());
        }
    }
}

#[test]
fn invalid_header_values_fail_without_echoing_values() {
    let headers = [("x-custom".into(), "private-value\ninvalid".into())].into();
    let error = crate::headers::resolve_headers(&headers, "session_test")
        .unwrap_err()
        .to_string();
    assert!(!error.contains("private-value"));
    assert!(
        crate::headers::resolve_headers(
            &[("bad name".into(), "value".into())].into(),
            "session_test"
        )
        .is_err()
    );
}

#[tokio::test]
#[ignore = "需要显式指定模型并使用本机已配置的真实 API 凭据"]
async fn live_session_routing_smoke() {
    let model_key = std::env::var("TIANGONG_LIVE_MODEL_KEY").expect("设置 TIANGONG_LIVE_MODEL_KEY");
    let path =
        std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".tiangong/models.json");
    let mut config: crate::models_config::ModelsConfig =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let entry = config
        .models
        .get(&model_key)
        .expect("指定模型不存在")
        .clone();
    config
        .routing
        .insert(crate::models_config::RoutingSlot::Chat, entry);
    let mut endpoint = ModelEndpoint::from_resolved(
        config
            .resolve_slot(crate::models_config::RoutingSlot::Chat)
            .unwrap(),
    );
    endpoint.timeout_ms = 90_000;
    let client = SingleProviderClient::new(endpoint);
    let session_id = scru128::new().to_string();
    let mut req = request(&session_id);
    let thinking_tools = std::env::var_os("TIANGONG_LIVE_THINKING_TOOLS").is_some();
    if thinking_tools {
        req.reasoning_effort = ReasoningEffort::High;
        req.max_output_tokens = Some(2048);
        req.context[1] = Message::new(
            MessageRole::User,
            "调用 read_probe 工具一次，然后只回复 OK。",
        );
        let tools = vec![ToolSpec {
            name: "read_probe".into(),
            description: "返回连接检查结果".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        }];
        for round in 0..3 {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let response = tokio::time::timeout(
                Duration::from_secs(100),
                client
                    .clone()
                    .stream_async(req.clone().with_tools(tools.clone(), None), tx),
            )
            .await
            .unwrap()
            .unwrap();
            if round == 0 {
                assert!(!response.reasoning_content.is_empty(), "应收到思考内容");
                assert!(!response.tool_calls.is_empty(), "应触发工具调用");
            }
            println!(
                "thinking tools round={}, input={}, cached={:?}, reasoning_bytes={}",
                round + 1,
                response.usage.prompt_tokens,
                response.usage.prompt_cache_hit_tokens,
                response.reasoning_content.len()
            );
            let assistant = Message::with_reasoning(
                MessageRole::Assistant,
                response.text,
                response.reasoning_content,
            )
            .with_tool_calls(
                response
                    .tool_calls
                    .iter()
                    .map(|call| tiangong_types::MessageToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    })
                    .collect(),
            );
            req.context.push(assistant);
            for call in response.tool_calls {
                req.context
                    .push(Message::tool_result(call.id, call.name, "OK", false));
            }
            if round == 1 {
                let injection = Message::new(MessageRole::Assistant, "").with_tool_calls(vec![
                    tiangong_types::MessageToolCall {
                        id: "internal_probe".into(),
                        name: "plugin_injection".into(),
                        arguments: json!({}),
                    },
                ]);
                req.context.push(injection);
                req.context.push(Message::tool_result(
                    "internal_probe",
                    "plugin_injection",
                    "检查已完成，只回复 OK。",
                    false,
                ));
            }
            req.context =
                serde_json::from_slice(&serde_json::to_vec(&req.context).unwrap()).unwrap();
        }
        return;
    }
    req.context[1] = Message::new(
        MessageRole::User,
        format!(
            "天工代码助手连接与缓存检查。以下是固定代码参考，忽略正文，只回复 OK。\n{}",
            (0..128)
                .map(|index| format!("fn sample_{index}(value: i32) -> i32 {{ value + 1 }}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    );
    for round in 1..=2 {
        let response = tokio::time::timeout(Duration::from_secs(35), client.complete_async(&req))
            .await
            .unwrap()
            .unwrap();
        assert!(response.usage.prompt_tokens > 0);
        println!(
            "model={model_key}, round={round}, input={}, cached={:?}, output={}",
            response.usage.prompt_tokens,
            response.usage.prompt_cache_hit_tokens,
            response.usage.completion_tokens
        );
        req.context.push(Message::with_reasoning(
            MessageRole::Assistant,
            response.text,
            response.reasoning_content,
        ));
        req.context
            .push(Message::new(MessageRole::User, "继续，只回复 OK。"));
    }
}
