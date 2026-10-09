use super::*;
use serde_json::{Value, json};

#[tokio::test]
async fn compression_request_keeps_every_cache_relevant_param_of_main_loop() {
    use crate::context::compressor::ContextCompressor;
    for protocol in [
        ProviderProtocol::OpenAi,
        ProviderProtocol::OpenAiChatCompletions,
    ] {
        let server = MockServer::builder().start().await;
        let mut harness = TestHarness::new_with_protocol(
            &server,
            protocol,
            vec![tool_spec("read_file")],
            HashMap::new(),
            Vec::new(),
        );
        harness.ctx.agent_config.reasoning_effort = tiangong_llm::ReasoningEffort::High;
        harness.ctx.session.messages.push(Message::with_reasoning(
            MessageRole::Assistant,
            "先前结论",
            "保留这段历史思考",
        ));
        harness.start_turn_with("最新问题仍需原样保留");
        let before_session = serde_json::to_vec(&harness.ctx.session).unwrap();
        if protocol == ProviderProtocol::OpenAi {
            mount_responses(&server, vec![answer("正常回复")], 0, true).await;
        } else {
            mount_completion(&server, "正常回复", "stop", 100, 10, None).await;
        }
        let req = super::super::build_react_request(&harness.ctx);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        harness
            .ctx
            .client
            .clone()
            .stream_async(req.with_tools(harness.ctx.tools.clone(), None), tx)
            .await
            .unwrap();
        if protocol == ProviderProtocol::OpenAi {
            mount_responses(
                &server,
                vec![answer("[[SUMMARY]]\n先前结论摘要")],
                3904,
                true,
            )
            .await;
        } else {
            mount_completion(&server, "[[SUMMARY]]\n先前结论摘要", "stop", 100, 10, None).await;
        }
        let update = ContextCompressor::new(
            harness.ctx.session.clone(),
            harness.ctx.client.clone(),
            harness.ctx.tools.clone(),
            harness.ctx.agent_config.reasoning_effort,
        )
        .compress(2, 4096)
        .await
        .unwrap();
        assert_eq!(update.summary, "先前结论摘要");
        assert_eq!(update.summary_up_to, 2);
        assert_eq!(
            harness.ctx.agent_config.reasoning_effort,
            tiangong_llm::ReasoningEffort::High
        );
        assert_eq!(
            serde_json::to_vec(&harness.ctx.session).unwrap(),
            before_session
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let before: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let after: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(before["model"], after["model"]);
        // 除输出上限与末尾压缩指令外，所有请求参数必须与主循环完全一致：
        // tool_choice / thinking / effort 等参与 provider 缓存键与模板渲染，
        // 任何差异都会让整段历史前缀缓存失效。
        let (input_field, budget_field) = if protocol == ProviderProtocol::OpenAi {
            ("input", "max_output_tokens")
        } else {
            ("messages", "max_tokens")
        };
        assert_eq!(after[budget_field], 4096);
        let strip = |value: &Value| {
            let mut object = value.as_object().unwrap().clone();
            object.remove(input_field);
            object.remove(budget_field);
            // 传输层字段（流式与否）不参与 prompt 前缀缓存。
            object.remove("stream");
            object.remove("stream_options");
            object
        };
        assert_eq!(strip(&before), strip(&after));
        if protocol == ProviderProtocol::OpenAi {
            assert_eq!(after["reasoning"]["effort"], "high");
        } else {
            assert_eq!(after["reasoning_effort"], "high");
        }
        let field = input_field;
        let old = before[field].as_array().unwrap();
        let new = after[field].as_array().unwrap();
        assert_eq!(new.len(), old.len() + 1);
        assert_eq!(&new[..old.len()], old.as_slice());
        assert!(new.last().unwrap().to_string().contains("先前结论"));
        assert!(new.last().unwrap().to_string().contains("不要调用工具"));
        assert!(new.last().unwrap().to_string().contains("尽量缩短思考"));
    }
}

#[tokio::test]
async fn compression_rejects_unexpected_tool_calls() {
    let server = MockServer::builder().start().await;
    let harness = TestHarness::new_with_protocol(
        &server,
        ProviderProtocol::OpenAi,
        vec![tool_spec("cache_probe")],
        HashMap::new(),
        Vec::new(),
    );
    mount_responses(
        &server,
        vec![
            answer("[[SUMMARY]]\n不能提交的摘要"),
            call("unexpected", 0, false, false),
        ],
        0,
        true,
    )
    .await;
    let result = crate::context::compressor::ContextCompressor::new(
        harness.ctx.session.clone(),
        harness.ctx.client.clone(),
        harness.ctx.tools.clone(),
        tiangong_llm::ReasoningEffort::High,
    )
    .compress(1, 4096)
    .await;
    let error = result.unwrap_err();
    assert!(error.message.contains("返回了工具调用"));
    assert!(error.usage.total_tokens > 0);
    assert!(harness.ctx.session.context_summary.is_none());
}

#[tokio::test]
async fn compression_empty_output_keeps_usage_and_original_session() {
    let server = MockServer::builder().start().await;
    let harness = TestHarness::new(&server, vec![tool_spec("read_file")], HashMap::new());
    mount_completion(&server, "", "length", 100, 4096, None).await;
    let before = serde_json::to_vec(&harness.ctx.session).unwrap();
    let result = crate::context::compressor::ContextCompressor::new(
        harness.ctx.session.clone(),
        harness.ctx.client.clone(),
        harness.ctx.tools.clone(),
        tiangong_llm::ReasoningEffort::High,
    )
    .compress(1, 4096)
    .await;
    let error = result.unwrap_err();
    assert!(error.message.contains("最大 token 限制"));
    assert_eq!(error.usage.completion_tokens, 4096);
    assert_eq!(serde_json::to_vec(&harness.ctx.session).unwrap(), before);
}

fn assert_request_prefix(previous: &Value, current: &Value) {
    for field in [
        "model",
        "instructions",
        "tools",
        "tool_choice",
        "reasoning",
        "prompt_cache_key",
    ] {
        assert_eq!(previous[field], current[field], "请求的 {field} 不应漂移");
    }
    let old = previous["input"].as_array().unwrap();
    let new = current["input"].as_array().unwrap();
    assert!(new.len() >= old.len(), "历史不得缩短");
    for (index, item) in old.iter().enumerate() {
        assert_eq!(
            serde_json::to_vec(item).unwrap(),
            serde_json::to_vec(&new[index]).unwrap(),
            "第 {index} 条已发送消息必须逐字节保持一致"
        );
    }
}

async fn responses_request(server: &MockServer, index: usize) -> Value {
    let requests = server.received_requests().await.unwrap();
    serde_json::from_slice(&requests[index].body).unwrap()
}

async fn mount_responses(server: &MockServer, output: Vec<Value>, cached: u64, as_json: bool) {
    let body = json!({
        "id": format!("resp_{}", scru128::new()),
        "model": "test-model", "status": "completed", "output": output,
        "usage": {
            "input_tokens": 4000, "output_tokens": 20, "total_tokens": 4020,
            "input_tokens_details": {"cached_tokens": cached}
        }
    });
    let response = if as_json {
        ResponseTemplate::new(200).set_body_json(body)
    } else {
        let mut chunks = vec![json!({"type":"response.created", "response":{"id":body["id"]}})];
        for item in body["output"].as_array().unwrap() {
            if item["type"] == "function_call" {
                chunks.push(json!({"type":"response.output_item.added", "item":item}));
            }
        }
        chunks.push(json!({"type":"response.completed", "response":body}));
        ResponseTemplate::new(200).set_body_raw(sse_body(&chunks), "text/event-stream")
    };
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(response)
        .up_to_n_times(1)
        .mount(server)
        .await;
}

fn answer(text: &str) -> Value {
    json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":text}]})
}

fn call(id: &str, round: usize, slow: bool, fail: bool) -> Value {
    json!({
        "type":"function_call", "call_id":id, "name":"cache_probe",
        "arguments": json!({"round":round, "slow":slow, "fail":fail,
            "nested":{"unicode":"中文\n第二行", "list":[true, null, 3]}}).to_string()
    })
}

struct CacheProbeTool {
    completed: Arc<Mutex<Vec<String>>>,
}

impl ToolOverrideHandler for CacheProbeTool {
    fn handle(
        &self,
        call: &ToolCall,
        _session: &mut Session,
        _actor_id: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        let call = call.clone();
        let completed = self.completed.clone();
        Box::pin(async move {
            if call.arguments["slow"] == true {
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            completed.lock().unwrap().push(call.id.clone());
            let failed = call.arguments["fail"] == true;
            Some(ToolResult {
                ok: !failed,
                summary: format!("结果 {}", call.id),
                stdout: format!("{}\n{}", call.id, "长结果中文\n".repeat(1500)),
                stderr: if failed {
                    "模拟执行失败".into()
                } else {
                    String::new()
                },
                exit_code: i32::from(failed),
                execution: None,
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complex_turns_preserve_every_sent_item_across_errors_injections_and_reload() {
    let server = MockServer::builder().start().await;
    let completed = Arc::new(Mutex::new(Vec::new()));
    let tool: Arc<dyn ToolOverrideHandler> = Arc::new(CacheProbeTool {
        completed: completed.clone(),
    });
    let spec = ToolSpec {
        name: "cache_probe".into(),
        description: "上下文一致性探针".into(),
        input_schema: json!({"type":"object", "properties":{
            "round":{"type":"integer"}, "slow":{"type":"boolean"},
            "fail":{"type":"boolean"}, "nested":{"type":"object"}
        }, "required":["round","slow","fail","nested"]}),
    };
    let mut harness = TestHarness::new_with_protocol(
        &server,
        ProviderProtocol::OpenAi,
        vec![crate::core::plugin::injection_tool_spec(), spec],
        HashMap::from([("cache_probe".to_string(), tool)]),
        Vec::new(),
    );
    let mut previous = None;
    let mut request_index = 0;
    let mut hits = 0;
    for turn in 0..4 {
        if turn > 0 {
            harness
                .ctx
                .session
                .append_message(MessageRole::User, format!("追问 {turn}"));
            // 同一用户连续追加，不能合并、替换以前发出的消息。
            harness.start_turn_with(format!("补充条件 {turn}"));
        }
        crate::react::message::inject_tool_to_messages(
            &mut harness.ctx.session,
            "browser",
            &json!({"turn":turn, "page":"稳定页面"}),
        );
        let slow = format!("slow_{turn}");
        let fast = format!("fast_{turn}");
        mount_responses(
            &server,
            vec![
                call(&slow, turn, true, false),
                call(&fast, turn, false, turn == 1),
            ],
            1024,
            false,
        )
        .await;
        mount_responses(
            &server,
            vec![call(&format!("next_{turn}"), turn + 100, false, false)],
            2048,
            false,
        )
        .await;
        mount_responses(
            &server,
            vec![answer(&format!("第 {turn} 轮完成"))],
            3072,
            true,
        )
        .await;

        let before = serde_json::to_value(harness.ctx.session.context()).unwrap();
        let turn_id = harness.ctx.turn_id.clone().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            execute_turn(&mut harness.ctx, &mut harness.cmd_rx),
        )
        .await
        .unwrap();
        assert!(
            matches!(result.outcome, TurnExecutionOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.usage.prompt_cache_hit_tokens, Some(6144));
        assert_eq!(result.usage.prompt_cache_miss_tokens, Some(5856));
        hits += result.usage.prompt_cache_hit_tokens.unwrap();
        let calls: Vec<_> = harness
            .ctx
            .session
            .messages
            .iter()
            .filter_map(|message| message.usage())
            .filter(|usage| usage.turn_id.as_deref() == Some(turn_id.as_str()))
            .collect();
        assert_eq!(calls.len(), 3, "一次模型调用只保存一条用量");
        assert_eq!(
            calls
                .iter()
                .map(|usage| usage.tokens.total_tokens)
                .sum::<usize>(),
            result.usage.total_tokens
        );
        assert!(
            calls.iter().all(
                |usage| usage.model == "test-model" && usage.agent_id == harness.ctx.session.id
            )
        );
        assert!(
            harness
                .ctx
                .session
                .messages
                .iter()
                .filter(|message| matches!(message.kind(), MessageRole::User | MessageRole::Tool))
                .all(|message| message.usage().is_none())
        );
        let after = serde_json::to_value(harness.ctx.session.context()).unwrap();
        assert!(
            after
                .as_array()
                .unwrap()
                .starts_with(before.as_array().unwrap())
        );

        for _ in 0..3 {
            let request = responses_request(&server, request_index).await;
            assert_eq!(request["prompt_cache_key"], harness.ctx.session.id);
            assert!(
                !request.to_string().contains("cache_hit_rate"),
                "统计元数据不能发送给模型"
            );
            if let Some(old) = &previous {
                assert_request_prefix(old, &request);
            }
            assert!(
                !request.to_string().contains("已截断"),
                "展示截断不能影响模型历史"
            );
            previous = Some(request);
            request_index += 1;
        }
        {
            let completion_order = completed.lock().unwrap();
            assert!(
                completion_order.iter().position(|id| id == &fast).unwrap()
                    < completion_order.iter().position(|id| id == &slow).unwrap()
            );
        }
        let followup = responses_request(&server, request_index - 2).await;
        let ids: Vec<_> = followup["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .filter_map(|item| item["call_id"].as_str())
            .filter(|id| *id == slow || *id == fast)
            .collect();
        assert_eq!(
            ids,
            vec![slow.as_str(), fast.as_str()],
            "完成顺序不能改变调用顺序"
        );
        harness.ctx.session.try_persist_to_disk().unwrap();
        harness.ctx.session =
            Session::load_from_storage(&harness.storage_root, &harness.ctx.session.id).unwrap();
        assert_eq!(
            serde_json::to_value(harness.ctx.session.context()).unwrap(),
            after,
            "重新加载不能改变历史"
        );
        harness.drain_stream();
    }
    assert_eq!(request_index, 12);
    assert_eq!(hits, 24576);
    assert_eq!(server.received_requests().await.unwrap().len(), 12);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_steering_preserves_the_inflight_request_prefix() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(2))
                .set_body_json(json!({
                    "status":"completed", "output":[answer("旧回复")]
                })),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_responses(&server, vec![answer("已按追加要求完成")], 1024, true).await;
    let mut harness = TestHarness::new_with_protocol(
        &server,
        ProviderProtocol::OpenAi,
        Vec::new(),
        HashMap::new(),
        Vec::new(),
    );
    let cmd_tx = harness.cmd_tx.clone();
    let send = async {
        tokio::time::timeout(Duration::from_secs(3), async {
            while server.received_requests().await.unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        cmd_tx
            .send(Command::InjectUserMessage {
                message: Box::new(tiangong_types::Message::user_prepared(
                    scru128::new().to_string(),
                    vec![tiangong_types::ContentBlock::text("追加要求")],
                )),
            })
            .unwrap();
    };
    let (result, _) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(execute_turn(&mut harness.ctx, &mut harness.cmd_rx), send)
    })
    .await
    .unwrap();
    assert!(matches!(result.outcome, TurnExecutionOutcome::Success));
    assert_request_prefix(
        &responses_request(&server, 0).await,
        &responses_request(&server, 1).await,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successive_continuations_do_not_merge_previously_sent_assistant_messages() {
    let server = MockServer::builder().start().await;
    for text in [
        "[NEED_MORE_WORK] 第一段进展",
        "[NEED_MORE_WORK] 第二段进展",
        "最终完成",
    ] {
        mount_responses(&server, vec![answer(text)], 1024, true).await;
    }
    let mut harness = TestHarness::new_with_protocol(
        &server,
        ProviderProtocol::OpenAi,
        Vec::new(),
        HashMap::new(),
        Vec::new(),
    );
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        execute_turn(&mut harness.ctx, &mut harness.cmd_rx),
    )
    .await
    .unwrap();
    assert!(matches!(result.outcome, TurnExecutionOutcome::Success));
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    for index in 1..3 {
        assert_request_prefix(
            &responses_request(&server, index - 1).await,
            &responses_request(&server, index).await,
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_response_keeps_received_usage_without_polluting_model_history() {
    // 断言请求序号时使用独立服务，避免池中旧请求在取消后迟到。
    let server = MockServer::builder().start().await;
    let chunks = [json!({"type":"response.failed", "response":{
        "error":{"message":"upstream failed after processing"},
        "usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110,
            "input_tokens_details":{"cached_tokens":80}}
    }})];
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_body(&chunks), "text/event-stream"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let mut harness = TestHarness::new_with_protocol(
        &server,
        ProviderProtocol::OpenAi,
        Vec::new(),
        HashMap::new(),
        Vec::new(),
    );
    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;
    assert!(matches!(result.outcome, TurnExecutionOutcome::Failed(_)));
    assert_eq!(result.usage.total_tokens, 110);
    let records: Vec<_> = harness
        .ctx
        .session
        .messages
        .iter()
        .filter(|message| message.usage().is_some())
        .collect();
    assert_eq!(records.len(), 1);
    // 流内失败定稿为空响应（issue #551）：turn 因无效输出失败，用量随空
    // assistant 消息保留，不再走 Notice 通道。
    assert_eq!(records[0].role, MessageRole::Assistant);
    let record_id = records[0].id.clone();
    let usage = records[0].usage().unwrap();
    assert_eq!(usage.tokens.cache_hit_rate(), Some(0.8));
    // 空消息留在 session 消息列表中，但空内容在 provider 映射层被跳过、
    // 不进入模型请求（下方下一轮请求断言覆盖）。
    let loaded =
        Session::load_from_storage(&harness.storage_root, &harness.ctx.session.id).unwrap();
    assert_eq!(
        loaded
            .messages
            .iter()
            .find(|message| message.id == record_id)
            .unwrap()
            .usage()
            .as_ref()
            .unwrap()
            .tokens
            .total_tokens,
        110
    );

    mount_responses(&server, vec![answer("继续完成")], 1024, true).await;
    harness.start_turn_with("继续");
    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;
    assert!(matches!(result.outcome, TurnExecutionOutcome::Success));
    let next = responses_request(&server, 1).await;
    assert!(!next.to_string().contains("cache_hit_rate"));
    assert!(!next.to_string().contains("[调用用量]"));
    assert_request_prefix(&responses_request(&server, 0).await, &next);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_drains_queued_usage_snapshots_and_records_only_once() {
    use crate::react::execute::{AgentLoopState, ToolInjectionBuffer};
    use crate::react::phase::{ActiveLlm, ExecutionPhase, LlmPurpose, StreamTiming};
    use crate::stream_throttle::{StreamTextKind, ThrottledStreamSink};
    use tiangong_llm::{ModelResponse, ModelStreamChunk};

    let server = MockServer::builder().start().await;
    let mut harness = TestHarness::new_with_protocol(
        &server,
        ProviderProtocol::OpenAi,
        Vec::new(),
        HashMap::new(),
        Vec::new(),
    );
    harness.ctx.turn_id = Some(harness.ctx.session.messages[0].id.clone());
    let pending_msg_id = scru128::new().to_string();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    for hit in [70, 80] {
        tx.send(ModelStreamChunk {
            content: String::new(),
            reasoning_content: String::new(),
            usage: Some(tiangong_llm::usage::TokenUsageData {
                prompt_tokens: 100,
                completion_tokens: 10,
                total_tokens: 110,
                prompt_cache_hit_tokens: Some(hit),
                prompt_cache_miss_tokens: Some(100 - hit),
            }),
        })
        .unwrap();
    }
    let active = ActiveLlm {
        purpose: LlmPurpose::React {
            request_injection_generation: 0,
        },
        pending_msg_id: pending_msg_id.clone(),
        sink: ThrottledStreamSink::with_text_kind(
            pending_msg_id.clone(),
            harness.ctx.stream_tx.clone(),
            StreamTextKind::React,
        ),
        chunk_rx: rx,
        task: tokio::spawn(std::future::pending::<anyhow::Result<ModelResponse>>()),
        streamed_text: String::new(),
        streamed_reasoning: String::new(),
        streaming_usage: TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 10,
            total_tokens: 110,
            prompt_cache_hit_tokens: Some(70),
            prompt_cache_miss_tokens: Some(30),
        },
        timing: StreamTiming::default(),
    };
    let mut state = AgentLoopState::new(&harness.ctx);
    state.phase = Some(ExecutionPhase::WaitingModel(active));
    let mut injections = ToolInjectionBuffer::new(&harness.ctx);
    let stream_tx = harness.ctx.stream_tx.clone();
    crate::react::interrupt::interrupt_active_work(
        &mut harness.ctx,
        &mut state,
        &mut injections,
        &stream_tx,
        200_000,
        false,
    )
    .await;
    assert_eq!(state.accumulated_usage.total_tokens, 110);
    assert_eq!(state.accumulated_usage.prompt_cache_hit_tokens, Some(80));
    let recorded = harness
        .ctx
        .session
        .messages
        .iter()
        .find(|message| message.id == pending_msg_id)
        .unwrap();
    assert_eq!(recorded.role, MessageRole::Notice);
    assert_eq!(
        recorded.usage().unwrap().status,
        tiangong_types::TurnStatus::Cancelled
    );
    assert_eq!(recorded.usage().unwrap().tokens.total_tokens, 110);
    assert!(
        !harness
            .ctx
            .session
            .context()
            .iter()
            .any(|message| message.id == pending_msg_id)
    );
    let reported: usize = harness
        .stream_rx
        .try_iter()
        .filter_map(|event| match event {
            StreamEvent::TokenUsage { usage, .. } => Some(usage.total_tokens),
            _ => None,
        })
        .sum();
    assert_eq!(reported, 110);
}
