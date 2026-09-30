//! `execute_turn` 单元测试。
//!
//! 模型调用通过 `wiremock` 起本地 HTTP 服务器返回 OpenAI Chat Completions SSE
//! 流,避免依赖真实网络。`execute_turn` 是 `pub(super)`,测试只能放在 `react`
//! 模块内部。turn 结束的信号是 `execute_turn` future 返回(它本身不发
//! `StreamEvent::Done`,终态事件由上层 `run_turn` 发送)。

use super::super::outcome::{TurnExecutionOutcome, TurnExecutionResult};
use super::execute_turn;
use crate::config::agent::AgentConfig;
use crate::core::command::Command;
use crate::core::plugin::Plugin;
use crate::observe::Observer;
use crate::permission::TrustMode;
use crate::prompt::SystemPromptConfig;
use crate::session::{Message, MessageRole, MessageToolCall, Session};
use crate::tools::extension::{PromptSectionProvider, ToolOverrideHandler, ToolSpecProvider};
use crate::tools::result::ToolResult;
use crate::turn_context::TurnContext;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiangong_llm::SingleProviderClient;
use tiangong_llm::tool::{ToolCall, ToolSpec};
use tiangong_llm::{ModelEndpoint, ProviderProtocol};
use tiangong_types::{StreamEvent, TokenUsage, stream::ContextCompressAction};
use tokio::sync::{Barrier, Notify};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "context_cache_tests.rs"]
mod context_cache_tests;

/// 构造一条 OpenAI SSE chunk(`data: {json}\n\n`),末尾追加 `[DONE]`。
fn sse_body(chunks: &[serde_json::Value]) -> Vec<u8> {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {}\n\n", chunk));
    }
    body.push_str("data: [DONE]\n\n");
    body.into_bytes()
}

/// 纯文本 delta chunk。
fn text_delta_chunk(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": content},
            "finish_reason": null,
        }],
    })
}

/// usage chunk(`choices: []` + usage,符合 stream_options.include_usage 约定)。
fn usage_chunk(prompt: u64, completion: u64) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        },
    })
}

/// tool_calls delta chunk(单个工具调用,一次性给出 name + 完整 arguments)。
fn tool_call_chunk(call_id: &str, name: &str, arguments: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }],
            },
            "finish_reason": "tool_calls",
        }],
    })
}

/// 在 mock 服务器上挂载一条 SSE 响应。
///
/// 多次调用时,wiremock 按挂载顺序(FIFO)匹配;`up_to_n_times(1)` 让每条
/// mock 只响应一次,从而实现「第 N 次请求返回第 N 条响应」的顺序语义。
async fn mount_sse(server: &MockServer, chunks: Vec<serde_json::Value>) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_body(&chunks), "text/event-stream"),
        )
        .up_to_n_times(1)
        .mount(server)
        .await;
}

async fn mount_completion(
    server: &MockServer,
    content: &str,
    finish_reason: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    delay: Option<Duration>,
) {
    let body = serde_json::json!({
        "id": "chatcmpl-compression",
        "object": "chat.completion",
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": finish_reason,
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        },
    });
    let mut response = ResponseTemplate::new(200).set_body_json(body);
    if let Some(delay) = delay {
        response = response.set_delay(delay);
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(response)
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// 挂载一条不重试的 OpenAI 兼容请求错误。
async fn mount_request_error(server: &MockServer, message: &str) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "param": null,
                "code": "test_error",
            }
        })))
        .up_to_n_times(1)
        .mount(server)
        .await;
}

/// 一个总是返回固定成功结果的工具覆盖处理器,用于测试工具调用路径。
struct EchoTool {
    invocations: Arc<Mutex<Vec<ToolCall>>>,
}

impl ToolOverrideHandler for EchoTool {
    fn handle(
        &self,
        call: &ToolCall,
        _session: &mut Session,
        _actor_id: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        let name = call.name.clone();
        self.invocations.lock().unwrap().push(call.clone());
        Box::pin(async move {
            Some(ToolResult {
                ok: true,
                summary: format!("{name} 已执行"),
                stdout: "done".to_string(),
                stderr: String::new(),
                exit_code: 0,
                execution: None,
            })
        })
    }
}

/// 同批工具全部启动后一直等待取消，用于确认 Cancel 会终止整个并行批次。
struct BlockingBatchTool {
    barrier: Arc<Barrier>,
    all_started: Arc<Notify>,
}

impl ToolOverrideHandler for BlockingBatchTool {
    fn handle(
        &self,
        _call: &ToolCall,
        _session: &mut Session,
        _actor_id: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        let barrier = self.barrier.clone();
        let all_started = self.all_started.clone();
        Box::pin(async move {
            if barrier.wait().await.is_leader() {
                all_started.notify_one();
            }
            std::future::pending::<Option<ToolResult>>().await
        })
    }
}

struct PausedTool {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl ToolOverrideHandler for PausedTool {
    fn handle(
        &self,
        _call: &ToolCall,
        _session: &mut Session,
        _actor_id: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        let started = self.started.clone();
        let release = self.release.clone();
        Box::pin(async move {
            started.notify_one();
            release.notified().await;
            Some(ToolResult {
                ok: true,
                summary: "测试工具已完成".to_string(),
                stdout: "done".to_string(),
                stderr: String::new(),
                exit_code: 0,
                execution: None,
            })
        })
    }
}

struct FailingTool;

impl ToolOverrideHandler for FailingTool {
    fn handle(
        &self,
        _call: &ToolCall,
        _session: &mut Session,
        _actor_id: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        Box::pin(async {
            Some(ToolResult {
                ok: false,
                summary: "测试工具执行失败".to_string(),
                stdout: String::new(),
                stderr: "test failure".to_string(),
                exit_code: 1,
                execution: None,
            })
        })
    }
}

struct TrustTrackingPlugin {
    modes: Arc<Mutex<Vec<TrustMode>>>,
}

impl ToolOverrideHandler for TrustTrackingPlugin {}
impl ToolSpecProvider for TrustTrackingPlugin {}
impl PromptSectionProvider for TrustTrackingPlugin {}

impl Plugin for TrustTrackingPlugin {
    fn id(&self) -> &str {
        "trust-tracker"
    }

    fn set_trust_mode(&self, trust: TrustMode) {
        self.modes.lock().unwrap().push(trust);
    }
}

fn tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: format!("测试工具 {name}"),
        input_schema: serde_json::json!({"type": "object", "properties": {}}),
    }
}

/// 构造一个指向 mock 服务器的 ModelEndpoint。
fn endpoint_with_protocol(server: &MockServer, protocol: ProviderProtocol) -> ModelEndpoint {
    ModelEndpoint {
        headers: Default::default(),
        base_url: server.uri(),
        api_key: "test-key".to_string(),
        model: "test-model".to_string(),
        protocol,
        timeout_ms: 5_000,
        options: serde_json::Value::Object(serde_json::Map::new()),
        context_window: None,
    }
}

/// 测试用的 TurnContext 构造辅助。
struct TestHarness {
    ctx: TurnContext,
    stream_rx: std::sync::mpsc::Receiver<StreamEvent>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Command>,
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Command>,
    storage_root: std::path::PathBuf,
}

impl TestHarness {
    /// `extra_overrides` / `tools` 用于工具调用路径测试。
    fn new(
        server: &MockServer,
        tools: Vec<ToolSpec>,
        tool_overrides: HashMap<String, Arc<dyn ToolOverrideHandler>>,
    ) -> Self {
        Self::new_with_protocol(
            server,
            ProviderProtocol::OpenAiChatCompletions,
            tools,
            tool_overrides,
            Vec::new(),
        )
    }

    /// 额外注入插件（用于生命周期计数等需要插件观察的测试）。
    fn new_with_plugins(
        server: &MockServer,
        tools: Vec<ToolSpec>,
        tool_overrides: HashMap<String, Arc<dyn ToolOverrideHandler>>,
        plugins: Vec<Arc<dyn Plugin>>,
    ) -> Self {
        Self::new_with_protocol(
            server,
            ProviderProtocol::OpenAiChatCompletions,
            tools,
            tool_overrides,
            plugins,
        )
    }

    /// 使用现成 client 注入测试 provider。
    fn new_with_client(
        client: SingleProviderClient,
        tools: Vec<ToolSpec>,
        tool_overrides: HashMap<String, Arc<dyn ToolOverrideHandler>>,
        plugins: Vec<Arc<dyn Plugin>>,
    ) -> Self {
        let root = tempfile::tempdir().expect("创建临时目录失败");
        let mut session = Session::new("test-session".to_string());
        session.bind_storage_root(root.path());
        session.append_message(MessageRole::User, "你好");
        session.rebuild_system_prompt(&SystemPromptConfig::from_plugin_sections(Vec::new()));
        // 暴露 storage_root 供 turn 级测试磁盘重载验证。
        let storage_root = root.path().to_path_buf();
        // 让 tempdir 存活到 turn 结束(用 `leak` 避免 Rust 借用检查器抱怨;
        // 测试进程结束即回收)。
        std::mem::forget(root);

        let agent_config = AgentConfig {
            reasoning_effort: tiangong_llm::ReasoningEffort::None,
            ..Default::default()
        };
        let (stream_tx, stream_rx) = std::sync::mpsc::channel::<StreamEvent>();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();

        let ctx = TurnContext::builder()
            .client(client)
            .session(session)
            .stream_tx(stream_tx)
            .plugins(plugins)
            .context_limit(200_000)
            .agent_config(agent_config)
            .trust_mode(TrustMode::FullTrust)
            .observer(Observer::new(std::env::temp_dir()))
            .tool_overrides(tool_overrides)
            .tools(tools)
            .build();

        Self {
            ctx,
            stream_rx,
            cmd_tx,
            cmd_rx,
            storage_root,
        }
    }

    fn new_with_protocol(
        server: &MockServer,
        protocol: ProviderProtocol,
        tools: Vec<ToolSpec>,
        tool_overrides: HashMap<String, Arc<dyn ToolOverrideHandler>>,
        plugins: Vec<Arc<dyn Plugin>>,
    ) -> Self {
        let client = SingleProviderClient::new(endpoint_with_protocol(server, protocol));
        Self::new_with_client(client, tools, tool_overrides, plugins)
    }

    /// 排空 stream 通道里的所有积压事件(非阻塞),避免 channel 满导致 send 阻塞。
    fn drain_stream(&self) {
        while self.stream_rx.try_recv().is_ok() {}
    }
}

/// 首轮纯文本响应应直接作为最终回复,返回 `Success`(跳过总结阶段)。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completes_with_direct_text_answer() {
    let server = MockServer::builder().start().await;
    // 单次请求:纯文本 "你好,我是测试助手。",首轮无工具 → can_promote_direct_answer。
    mount_sse(
        &server,
        vec![text_delta_chunk("你好,我是测试助手。"), usage_chunk(10, 5)],
    )
    .await;

    let mut harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    let result: TurnExecutionResult = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;

    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Success),
        "纯文本直接回答应返回 Success,实际: {:?}",
        result.outcome
    );
    assert_eq!(result.usage.total_tokens, 15);
    assert!(
        !harness
            .stream_rx
            .try_iter()
            .any(|event| matches!(event, StreamEvent::DeferredToolInjectionsChanged { .. }))
    );
}

/// `[NEED_MORE_WORK]` 与前端采用相同的宽松前缀规则：忽略前导空白和大小写，
/// 命中后保留该过程消息并继续请求模型，直到得到普通最终回复。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn need_more_work_marker_continues_model_loop() {
    let server = MockServer::builder().start().await;
    mount_sse(
        &server,
        vec![
            text_delta_chunk("\n  [need_more_work]还有检查需要完成。"),
            usage_chunk(10, 4),
        ],
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("所有检查均已完成。"), usage_chunk(12, 3)],
    )
    .await;

    let mut harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;

    assert!(matches!(result.outcome, TurnExecutionOutcome::Success));
    assert_eq!(result.usage.total_tokens, 29, "两次模型请求的用量都应累计");
    let assistant_messages = harness
        .ctx
        .session
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .collect::<Vec<_>>();
    assert!(assistant_messages.iter().any(|message| {
        message.phase == crate::session::MessagePhase::React
            && message.text_content().contains("[need_more_work]")
    }));
    assert!(assistant_messages.iter().any(|message| {
        message.phase == crate::session::MessagePhase::Summary
            && message.text_content() == "所有检查均已完成。"
    }));
}

/// 请求前压缩保留模型可见的续接消息：上一 turn 观测压力超阈值，下一 turn
/// 在发起模型请求前压缩（ALR-303），摘要与 resume 持久化到磁盘。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compression_persists_summary_and_keeps_recent_interaction() {
    let server = MockServer::builder().start().await;
    // 第一段：大用量文本完成，建立跨 turn 的压力信号。
    mount_sse(
        &server,
        vec![text_delta_chunk("最终回答。"), usage_chunk(185_900, 5)],
    )
    .await;
    // 第二段请求前压缩；压缩后的模型请求。
    mount_completion(
        &server,
        "[[CURRENT_TASK]]\n当前任务已完成\n[[SUMMARY]]\n历史摘要",
        "stop",
        100,
        20,
        None,
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("继续回答。"), usage_chunk(30, 5)],
    )
    .await;

    let mut harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    let first = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;
    assert!(matches!(first.outcome, TurnExecutionOutcome::Success));
    assert_eq!(first.usage.total_tokens, 185_905);

    // 新 turn：请求前压力检查触发压缩（新增用户消息成为续接的当前任务）。
    harness
        .ctx
        .session
        .append_message(MessageRole::User, "继续提出新问题");
    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;

    assert!(matches!(result.outcome, TurnExecutionOutcome::Success));
    assert_eq!(result.usage.total_tokens, 155);
    assert_eq!(
        harness.ctx.session.context_summary.as_deref(),
        Some("历史摘要")
    );
    // 压缩只是 session 的一次调整：最近交互保留（锚点用户消息承载当前任务），
    // 不注入合成续接消息。
    let context = harness.ctx.session.context();
    assert_eq!(context[0].role, MessageRole::System);
    assert_eq!(context[1].text_content(), "继续提出新问题");
    assert!(
        harness
            .ctx
            .session
            .messages
            .iter()
            .all(|message| message.phase != crate::session::MessagePhase::CompressedResume),
        "压缩不注入合成续接消息"
    );

    let persisted = Session::load_from_storage(
        harness
            .ctx
            .session
            .bound_storage_root()
            .expect("测试会话应绑定存储目录"),
        &harness.ctx.session.id,
    )
    .expect("压缩结果应持久化");
    assert_eq!(
        persisted.context_summary.as_deref(),
        Some("历史摘要"),
        "摘要边界应持久化"
    );

    let requests = server.received_requests().await.unwrap();
    let compression_body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(compression_body["max_tokens"], serde_json::json!(9_999));
    assert!(
        compression_body["messages"]
            .to_string()
            .contains("不得超过 9999 tokens")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forced_compression_folds_older_history_and_keeps_latest_tool_batch() {
    let server = MockServer::builder().start().await;
    mount_request_error(&server, "context_window_exceeded").await;
    mount_completion(
        &server,
        "[[CURRENT_TASK]]\n继续处理最近工具结果\n[[SUMMARY]]\n较早历史摘要",
        "stop",
        100,
        20,
        None,
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("最终回答。"), usage_chunk(10, 5)],
    )
    .await;

    let mut harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    harness
        .ctx
        .session
        .append_message(MessageRole::Assistant, "较早回答");
    harness
        .ctx
        .session
        .append_message(MessageRole::User, "处理 latest.txt");
    let mut assistant = Message::new(MessageRole::Assistant, "");
    assistant.tool_calls = vec![MessageToolCall {
        id: "latest-call".to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::json!({"path": "latest.txt"}),
    }];
    harness.ctx.session.messages.push(assistant);
    harness.ctx.session.messages.push(Message::tool_result(
        "latest-call",
        "read_file",
        "recent-tool-output",
        false,
    ));

    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;

    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Success),
        "强制压缩后应重试成功，实际结果：{:?}",
        result.outcome
    );
    assert_eq!(result.usage.total_tokens, 135);
    assert_eq!(harness.ctx.session.summary_up_to, 3);
    let context = harness.ctx.session.context();
    assert_eq!(context[0].role, MessageRole::System);
    // 锚点被折叠：注入 LLM 可见、用户不可见的锚点消息（用户请求原文），
    // 随后是完整保留的最近工具批次。
    assert_eq!(context[1].role, MessageRole::User);
    assert_eq!(
        context[1].phase,
        crate::session::MessagePhase::CompressedResume
    );
    assert!(matches!(
        &context[1].content[0],
        crate::session::ContentBlock::ModelInstruction { text }
            if text.contains("处理 latest.txt")
    ));
    assert_eq!(context[2].role, MessageRole::Assistant);
    assert!(context.iter().any(|message| {
        message.tool_call_id.as_deref() == Some("latest-call")
            && message.text_content() == "recent-tool-output"
    }));

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let first_body = String::from_utf8_lossy(&requests[0].body);
    let compression_body = String::from_utf8_lossy(&requests[1].body);
    let retry_body = String::from_utf8_lossy(&requests[2].body);
    assert!(first_body.contains("recent-tool-output"));
    assert!(compression_body.contains("recent-tool-output"));
    assert!(compression_body.contains("较早回答"));
    assert!(retry_body.contains("recent-tool-output"));
}

/// 截断的压缩结果不得推进摘要边界：finish_reason=length 视为失败，
/// session 保持原状（请求前压缩路径，ALR-303）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncated_compression_does_not_advance_summary_boundary() {
    let server = MockServer::builder().start().await;
    mount_sse(
        &server,
        vec![text_delta_chunk("最终回答。"), usage_chunk(185_900, 5)],
    )
    .await;
    mount_completion(
        &server,
        "[[CURRENT_TASK]]\n当前任务已完成\n[[SUMMARY]]\n截断摘要",
        "length",
        100,
        20,
        None,
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("继续回答。"), usage_chunk(30, 5)],
    )
    .await;

    let mut harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    let first = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;
    assert!(matches!(first.outcome, TurnExecutionOutcome::Success));
    harness
        .ctx
        .session
        .append_message(MessageRole::User, "继续提问");
    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;

    assert!(matches!(result.outcome, TurnExecutionOutcome::Success));
    assert_eq!(harness.ctx.session.summary_up_to, 0);
    assert!(harness.ctx.session.context_summary.is_none());
    assert!(harness.stream_rx.try_iter().any(|event| matches!(
        event,
        StreamEvent::ContextCompressed {
            action: ContextCompressAction::Failed,
            ..
        }
    )));
}

/// 压缩结果持久化失败时保持原压缩状态：不发 Auto 成功事件，仅 Failed 事件
///（请求前压缩路径，ALR-303）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistence_failure_keeps_original_compression_state() {
    let server = MockServer::builder().start().await;
    mount_sse(
        &server,
        vec![text_delta_chunk("最终回答。"), usage_chunk(185_900, 5)],
    )
    .await;
    mount_completion(
        &server,
        "[[CURRENT_TASK]]\n当前任务已完成\n[[SUMMARY]]\n历史摘要",
        "stop",
        100,
        20,
        None,
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("继续回答。"), usage_chunk(30, 5)],
    )
    .await;

    let invalid_root = tempfile::tempdir().unwrap();
    let blocking_file = invalid_root.path().join("not-a-directory");
    std::fs::write(&blocking_file, "blocked").unwrap();
    let mut harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    harness.ctx.session.bind_storage_root(blocking_file);

    let _ = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;
    harness
        .ctx
        .session
        .append_message(MessageRole::User, "继续提问");
    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;
    let events = harness.stream_rx.try_iter().collect::<Vec<_>>();

    assert!(matches!(result.outcome, TurnExecutionOutcome::Success));
    assert_eq!(harness.ctx.session.summary_up_to, 0);
    assert!(harness.ctx.session.context_summary.is_none());
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ContextCompressed {
            action: ContextCompressAction::Failed,
            ..
        }
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        StreamEvent::ContextCompressed {
            action: ContextCompressAction::Auto,
            ..
        }
    )));
}

/// 请求前压缩期间取消：压缩被中止（Cancelled 事件），turn 以取消终态结束
/// 且不应用任何压缩结果（ALR-303/306）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_interrupts_active_context_compression() {
    let server = MockServer::builder().start().await;
    mount_sse(
        &server,
        vec![text_delta_chunk("最终回答。"), usage_chunk(185_900, 5)],
    )
    .await;
    mount_completion(
        &server,
        "[[CURRENT_TASK]]\n当前任务已完成\n[[SUMMARY]]\n历史摘要",
        "stop",
        100,
        20,
        Some(Duration::from_secs(5)),
    )
    .await;

    let mut harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    let _ = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;
    harness.drain_stream();
    harness
        .ctx
        .session
        .append_message(MessageRole::User, "继续提问");

    let TestHarness {
        mut ctx,
        stream_rx,
        cmd_tx,
        mut cmd_rx,
        ..
    } = harness;
    let cancel_task = tokio::task::spawn_blocking(move || {
        loop {
            let event = stream_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("等待压缩开始事件超时");
            if matches!(event, StreamEvent::ContextCompressing { .. }) {
                cmd_tx.send(Command::Cancel).unwrap();
                break stream_rx;
            }
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(10), execute_turn(&mut ctx, &mut cmd_rx))
        .await
        .expect("取消压缩后 turn 应及时结束");
    let stream_rx = cancel_task.await.unwrap();

    assert!(matches!(result.outcome, TurnExecutionOutcome::Cancelled));
    assert_eq!(ctx.session.summary_up_to, 0);
    assert!(stream_rx.try_iter().any(|event| matches!(
        event,
        StreamEvent::ContextCompressed {
            action: ContextCompressAction::Cancelled,
            ..
        }
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_interrupts_manual_context_compression() {
    let server = MockServer::builder().start().await;
    mount_completion(
        &server,
        "[[SUMMARY]]\n历史摘要",
        "stop",
        100,
        20,
        Some(Duration::from_secs(5)),
    )
    .await;

    let mut harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    // 必须存在可压缩历史，否则压缩会立即失败，与取消命令发生竞态。
    harness
        .ctx
        .session
        .append_message(MessageRole::Assistant, "较早回答");
    harness
        .ctx
        .session
        .append_message(MessageRole::User, "最近问题");
    let TestHarness {
        ctx,
        stream_rx,
        cmd_tx,
        mut cmd_rx,
        ..
    } = harness;
    let cancel_task = tokio::task::spawn_blocking(move || {
        loop {
            let event = stream_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("等待手动压缩开始事件超时");
            if matches!(event, StreamEvent::ContextCompressing { .. }) {
                cmd_tx.send(Command::Cancel).unwrap();
                break stream_rx;
            }
        }
    });

    tokio::time::timeout(
        Duration::from_secs(10),
        crate::react::compression::run_manual_context_compression(
            ctx,
            &mut cmd_rx,
            crate::core::MANUAL_COMPACT_NOTICE.to_string(),
        ),
    )
    .await
    .expect("取消手动压缩后任务应及时结束");
    let stream_rx = cancel_task.await.unwrap();

    // Cancelled 事件与压缩协程的取消收尾存在竞态：带超时等待而非立即断言。
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if stream_rx.try_iter().any(|event| {
            matches!(
                event,
                StreamEvent::ContextCompressed {
                    action: ContextCompressAction::Cancelled,
                    ..
                }
            )
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待手动压缩的取消事件超时"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 发送 `Command::Cancel` 应中断执行并返回 `Cancelled`(覆盖本次重构的
/// oneshot 取消传播路径)。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn returns_cancelled_on_cancel_command() {
    let server = MockServer::builder().start().await;
    // 挂一条延迟 2s 的响应,确保 cancel 能在模型请求完成前到达。
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(
                    sse_body(&[text_delta_chunk("正在思考")]),
                    "text/event-stream",
                )
                .set_delay(std::time::Duration::from_secs(2)),
        )
        .mount(&server)
        .await;

    let mut harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    let trust_modes = Arc::new(Mutex::new(Vec::new()));
    harness.ctx.plugins.push(Arc::new(TrustTrackingPlugin {
        modes: trust_modes.clone(),
    }));

    // cmd_tx 是 Send 的,可以移到独立任务里延时发送 Cancel;
    // 主任务独占 ctx + cmd_rx 跑 execute_turn。
    let cmd_tx = harness.cmd_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let _ = cmd_tx.send(Command::SetTrustMode(TrustMode::Supervised));
        let _ = cmd_tx.send(Command::SetReasoningEffort(
            tiangong_llm::ReasoningEffort::Max,
        ));
        let _ = cmd_tx.send(Command::InjectTool {
            tool_name: "cancelled_probe".to_string(),
            payload: serde_json::json!({"value": 1}),
        });
        let _ = cmd_tx.send(Command::Cancel);
    });

    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;

    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Cancelled),
        "收到 Cancel 应返回 Cancelled,实际: {:?}",
        result.outcome
    );
    assert_eq!(*trust_modes.lock().unwrap(), vec![TrustMode::Supervised]);
    assert_eq!(harness.ctx.trust_mode, TrustMode::Supervised);
    assert_eq!(harness.ctx.session.trust_mode, TrustMode::Supervised);
    assert_eq!(
        harness.ctx.agent_config.reasoning_effort,
        tiangong_llm::ReasoningEffort::Max
    );
    assert_eq!(
        harness.ctx.session.reasoning_effort,
        Some(tiangong_llm::ReasoningEffort::Max)
    );
    assert!(harness.ctx.session.deferred_tool_injections.is_empty());
    assert!(harness.ctx.session.messages.iter().any(|message| {
        message.role == MessageRole::Tool && message.text_content().contains("cancelled_probe")
    }));
    let injection_snapshots = harness
        .stream_rx
        .try_iter()
        .filter_map(|event| match event {
            StreamEvent::DeferredToolInjectionsChanged { injections } => Some(injections),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(injection_snapshots.len(), 2);
    assert_eq!(injection_snapshots[0].len(), 1);
    assert!(injection_snapshots[1].is_empty());
}

/// 工具调用路径:模型先调用工具 → 工具执行 → 模型给出文本回复 → 候选完成
/// 门控（无工具义务）通过 → `Success`。
///
/// 覆盖最小模型—工具循环的完整链路（任务 15：不再进入总结阶段）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_tool_then_completes() {
    let server = MockServer::builder().start().await;
    let invocations = Arc::new(Mutex::new(Vec::new()));

    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "echo".to_string(),
        Arc::new(EchoTool {
            invocations: invocations.clone(),
        }),
    );
    let tools = vec![ToolSpec {
        name: "echo".to_string(),
        description: "回显输入".to_string(),
        input_schema: serde_json::json!({"type": "object", "properties": {}}),
    }];

    // 按请求顺序挂载响应(wiremock FIFO:先挂载的先匹配;up_to_n_times(1)
    // 让每条 mock 只响应一次,从而实现顺序响应)。
    // 1) 首轮:工具调用 echo。
    mount_sse(
        &server,
        vec![tool_call_chunk("call_1", "echo", "{}"), usage_chunk(15, 3)],
    )
    .await;
    // 2) 工具执行后:文本回复(无工具义务 → 直接完成,问号结尾不影响)。
    mount_sse(
        &server,
        vec![
            text_delta_chunk("结果还需要我做什么吗?"),
            usage_chunk(25, 5),
        ],
    )
    .await;

    let mut harness = TestHarness::new(&server, tools, overrides);
    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;

    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Success),
        "工具+文本回复链路应返回 Success,实际: {:?}",
        result.outcome
    );
    // 工具应被调用一次。
    assert_eq!(
        invocations.lock().unwrap().len(),
        1,
        "echo 工具应被调用一次"
    );
    harness.drain_stream();
}

/// ALR-111 用量权威：多轮模型请求的用量必须累计到终态结果，重构后仍须保证
/// 最终终态和 Session 使用封口前最新累计用量。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accumulated_usage_is_aggregated_across_requests() {
    let server = MockServer::builder().start().await;
    let invocations = Arc::new(Mutex::new(Vec::new()));
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "echo".to_string(),
        Arc::new(EchoTool {
            invocations: invocations.clone(),
        }),
    );
    // 1) 工具调用；2) 文本回复（无工具义务 → 直接完成，不再有总结请求）。
    mount_sse(
        &server,
        vec![tool_call_chunk("call_1", "echo", "{}"), usage_chunk(15, 3)],
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("结果还需要补充吗?"), usage_chunk(25, 5)],
    )
    .await;

    let mut harness = TestHarness::new(&server, vec![tool_spec("echo")], overrides);
    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;

    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Success),
        "工具+文本链路应返回 Success，实际: {:?}",
        result.outcome
    );
    // 两轮请求用量累计：(15+3) + (25+5) = 48
    assert_eq!(
        result.usage.total_tokens, 48,
        "终态用量应为各轮模型请求用量之和（ALR-111）"
    );
    harness.drain_stream();
}

/// ALR-302 事件契约：工具执行必须先发 ToolStart，完成后发 ToolResult，
/// 重构后事件顺序需保持。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_execution_emits_start_before_result_event() {
    let server = MockServer::builder().start().await;
    let invocations = Arc::new(Mutex::new(Vec::new()));
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "echo".to_string(),
        Arc::new(EchoTool {
            invocations: invocations.clone(),
        }),
    );
    // 工具调用 → 文本以问号结尾 → Summary 完成。
    mount_sse(
        &server,
        vec![tool_call_chunk("call_1", "echo", "{}"), usage_chunk(15, 3)],
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("结果还需要补充吗?"), usage_chunk(25, 5)],
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("已完成。"), usage_chunk(30, 4)],
    )
    .await;

    let mut harness = TestHarness::new(&server, vec![tool_spec("echo")], overrides);
    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;
    assert!(matches!(result.outcome, TurnExecutionOutcome::Success));

    let events: Vec<StreamEvent> = harness.stream_rx.try_iter().collect();
    let start_idx = events
        .iter()
        .position(|e| matches!(e, StreamEvent::ToolStart { name, .. } if name.as_str() == "echo"));
    let result_idx = events
        .iter()
        .position(|e| matches!(e, StreamEvent::ToolResult { name, .. } if name.as_str() == "echo"));
    assert!(start_idx.is_some(), "应发送 echo 的 ToolStart 事件");
    assert!(result_idx.is_some(), "应发送 echo 的 ToolResult 事件");
    assert!(
        start_idx < result_idx,
        "ToolStart 必须在 ToolResult 之前（ALR-302 事件契约）"
    );
}

/// ALR-107/109：run_turn 只发一次 Done，并把 turn_status 写入最新用户消息；
/// 重载磁盘 session 后锚点一致。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_turn_emits_single_done_and_anchors_status_to_latest_user_message() {
    use super::super::turn::run_turn;

    let server = MockServer::builder().start().await;
    mount_sse(
        &server,
        vec![text_delta_chunk("你好,我是测试助手。"), usage_chunk(10, 5)],
    )
    .await;
    let harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    // 标题为 "test-session"（非默认），spawn_title_generation 跳过。
    let storage_root = harness.storage_root.clone();
    let session_id = harness.ctx.session.id.clone();
    let (_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let stream_rx = harness.stream_rx;
    let terminal = run_turn(harness.ctx, &mut cmd_rx).await;

    // 每轮独立终态：run_turn 收尾完成即在内部发布 Done（返回值同源）。
    assert!(matches!(terminal, StreamEvent::Done { .. }));
    let events: Vec<StreamEvent> = stream_rx.try_iter().collect();
    let done = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::Done { .. }))
        .count();
    assert_eq!(done, 1, "turn 收尾应恰好发布一次 Done");

    // ALR-107 最新消息锚点：重载磁盘 session，最新用户消息应有 turn_status。
    let reloaded = Session::load_from_storage(&storage_root, &session_id).expect("重载 session");
    let latest_has_status = reloaded
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .is_some_and(|m| m.turn_status.is_some());
    assert!(
        latest_has_status,
        "最新用户消息应写入 turn_status（ALR-107）"
    );
}

/// 计数 on_turn_started / on_turn_finished 调用次数的插件，用于验证生命周期唯一性。
struct LifecycleCountingPlugin {
    started: Arc<AtomicU32>,
    finished: Arc<AtomicU32>,
}

impl ToolOverrideHandler for LifecycleCountingPlugin {}
impl ToolSpecProvider for LifecycleCountingPlugin {}
impl PromptSectionProvider for LifecycleCountingPlugin {}

impl Plugin for LifecycleCountingPlugin {
    fn id(&self) -> &str {
        "lifecycle-counter"
    }
    fn on_turn_started(&self, _: &mut Session, _: usize) {
        self.started.fetch_add(1, Ordering::SeqCst);
    }
    fn on_turn_finished(&self, _: &Session, _: usize) {
        self.finished.fetch_add(1, Ordering::SeqCst);
    }
}

/// 等待原子计数器达到期望值（通知型钩子在后台线程执行，断言前需等它落地）。
fn wait_for_counter(counter: &AtomicU32, expected: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) != expected {
        assert!(
            std::time::Instant::now() < deadline,
            "等待钩子通知超时：期望 {expected}，实际 {}",
            counter.load(Ordering::SeqCst)
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// ALR-108：一个物理 turn 只调用一次 on_turn_started 和一次 on_turn_finished。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_turn_invokes_lifecycle_hooks_exactly_once() {
    use super::super::turn::run_turn;

    let server = MockServer::builder().start().await;
    mount_sse(
        &server,
        vec![text_delta_chunk("你好,我是测试助手。"), usage_chunk(10, 5)],
    )
    .await;
    let started = Arc::new(AtomicU32::new(0));
    let finished = Arc::new(AtomicU32::new(0));
    let plugin = Arc::new(LifecycleCountingPlugin {
        started: started.clone(),
        finished: finished.clone(),
    });
    let harness = TestHarness::new_with_plugins(&server, Vec::new(), HashMap::new(), vec![plugin]);
    let (_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    run_turn(harness.ctx, &mut cmd_rx).await;
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "on_turn_started 应只调用一次（ALR-108）"
    );
    wait_for_counter(&finished, 1);
    assert_eq!(
        finished.load(Ordering::SeqCst),
        1,
        "on_turn_finished 应只通知一次（ALR-108，后台投递）"
    );
}

/// ALR-101/110：运行中注入用户消息——中断工具等待（协议闭合）、保存消息、
/// 确认接收，并从新意图重启直至成功。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inject_user_message_interrupts_tools_and_restarts() {
    let server = MockServer::builder().start().await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "paused_probe".to_string(),
        Arc::new(PausedTool {
            started: started.clone(),
            release: release.clone(),
        }),
    );
    // 1) 首轮：工具调用（PausedTool 阻塞，制造"工具等待中"）。
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_1", "paused_probe", "{}"),
            usage_chunk(15, 3),
        ],
    )
    .await;
    // 2) 注入后新意图的请求：直接文本回答 → Success。
    mount_sse(
        &server,
        vec![
            text_delta_chunk("好的，按新要求完成了。"),
            usage_chunk(20, 4),
        ],
    )
    .await;

    let harness = TestHarness::new(&server, vec![tool_spec("paused_probe")], overrides);
    let TestHarness {
        mut ctx,
        stream_rx,
        cmd_tx,
        mut cmd_rx,
        ..
    } = harness;
    // 注入由独立任务发送：等工具进入运行后投递引导消息。
    let inject_tx = cmd_tx.clone();
    let started_wait = started.clone();
    tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(2), started_wait.notified())
            .await
            .expect("工具应已启动");
        inject_tx
            .send(Command::InjectUserMessage {
                message_id: "injected-1".to_string(),
                content: vec![tiangong_types::ContentBlock::text("改成先做另一件事")],
            })
            .unwrap();
    });
    let result = execute_turn(&mut ctx, &mut cmd_rx).await;
    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Success),
        "新意图执行应成功，实际: {:?}",
        result.outcome
    );
    // 消息已进入 session（校验+事务保存）。
    assert!(
        ctx.session
            .messages
            .iter()
            .any(|m| m.id == "injected-1" && m.role == MessageRole::User),
        "注入的消息应保存进 session"
    );
    // 保存成功后才发 UserMessage 确认（ALR-202 的同轮部分）。
    assert!(
        stream_rx.try_iter().any(|e| matches!(e,
            StreamEvent::UserMessage { message_id, .. } if message_id == "injected-1")),
        "保存成功后应发送 UserMessage 确认"
    );
    // 被中断的工具调用协议已闭合：call_1 有对应的失败结果消息。
    let call_closed = ctx
        .session
        .messages
        .iter()
        .any(|m| m.tool_call_id.as_deref() == Some("call_1") && m.tool_result_is_error);
    assert!(call_closed, "被中断的工具调用应有失败结果（ALR-110）");
}

/// 引导消息仍属于当前轮次：新图片路径进入模型上下文，但不重复触发开始钩子。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_image_guidance_keeps_lifecycle_hooks_once() {
    use super::super::turn::run_turn;

    let server = MockServer::builder().start().await;
    let started = Arc::new(Notify::new());
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "paused_probe".to_string(),
        Arc::new(PausedTool {
            started: started.clone(),
            release: Arc::new(Notify::new()),
        }),
    );
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_1", "paused_probe", "{}"),
            usage_chunk(15, 3),
        ],
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("好的。"), usage_chunk(20, 4)],
    )
    .await;
    let hook_started = Arc::new(AtomicU32::new(0));
    let hook_finished = Arc::new(AtomicU32::new(0));
    let plugin = Arc::new(LifecycleCountingPlugin {
        started: hook_started.clone(),
        finished: hook_finished.clone(),
    });
    let TestHarness {
        ctx,
        cmd_tx,
        mut cmd_rx,
        ..
    } = TestHarness::new_with_plugins(
        &server,
        vec![tool_spec("paused_probe")],
        overrides,
        vec![plugin],
    );
    let inject_tx = cmd_tx.clone();
    tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("工具应已启动");
        let path = "/tmp/guidance-image.png";
        inject_tx
            .send(Command::InjectUserMessage {
                message_id: scru128::new().to_string(),
                content: vec![
                    tiangong_types::ContentBlock::text("看这张新截图"),
                    tiangong_types::ContentBlock::AssetReference {
                        asset: tiangong_types::StoredAsset {
                            asset_id: scru128::new().to_string(),
                            local_path: path.into(),
                            original_name: "guidance-image.png".into(),
                            mime_type: "image/png".into(),
                            size: 0,
                            kind: tiangong_types::MediaKind::Image,
                        },
                    },
                    tiangong_types::ContentBlock::ModelInstruction {
                        text: format!("请通过 images={} 分析新截图", serde_json::json!([path])),
                    },
                ],
            })
            .unwrap();
    });
    run_turn(ctx, &mut cmd_rx).await;
    assert_eq!(
        hook_started.load(Ordering::SeqCst),
        1,
        "引导消息不触发 on_turn_started"
    );
    wait_for_counter(&hook_finished, 1);
    assert_eq!(hook_finished.load(Ordering::SeqCst), 1);
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| {
                String::from_utf8_lossy(&request.body).contains("guidance-image.png")
            }),
        "新图片路径必须进入引导后的模型上下文"
    );
}

/// ALR-107（多消息）：注入引导消息后，最终 turn_status 写入最新（注入的）
/// 用户消息，原始消息不被覆盖；磁盘重载后一致。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_status_anchors_to_injected_latest_user_message() {
    use super::super::turn::run_turn;

    let server = MockServer::builder().start().await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "paused_probe".to_string(),
        Arc::new(PausedTool {
            started: started.clone(),
            release: release.clone(),
        }),
    );
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_1", "paused_probe", "{}"),
            usage_chunk(15, 3),
        ],
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("按新要求完成。"), usage_chunk(20, 4)],
    )
    .await;
    // 新意图首轮后 request_round>0，文本回答进入完成度检查 → Summary 完成。
    mount_sse(
        &server,
        vec![text_delta_chunk("已完成。"), usage_chunk(25, 3)],
    )
    .await;

    let harness = TestHarness::new(&server, vec![tool_spec("paused_probe")], overrides);
    let TestHarness {
        ctx,
        stream_rx,
        cmd_tx: _,
        storage_root,
        ..
    } = harness;
    let session_id = ctx.session.id.clone();
    // 注入走独立通道任务：等工具启动后把命令投给 run_turn 持有的接收端。
    // 生产中发送端由 TurnTask 注册表持有；测试克隆一份保持通道开启，否则任务
    // 结束后通道关闭会被 execute_turn 当作取消。
    let started_wait = started.clone();
    let (inject_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let _keep_channel_open = inject_tx.clone();
    let inject_task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(2), started_wait.notified())
            .await
            .expect("工具应已启动");
        inject_tx
            .send(Command::InjectUserMessage {
                message_id: "injected-anchor".to_string(),
                content: vec![tiangong_types::ContentBlock::text("请改用另一方案")],
            })
            .unwrap();
    });
    run_turn(ctx, &mut cmd_rx).await;
    let _ = inject_task.await;

    // 磁盘重载验证：最新用户消息（注入的）有 turn_status，原始消息无。
    let reloaded = Session::load_from_storage(&storage_root, &session_id).expect("重载 session");
    let latest = reloaded
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .expect("应有用户消息");
    assert_eq!(latest.id, "injected-anchor", "最新用户消息应为注入的消息");
    assert!(
        latest.turn_status.is_some(),
        "最终状态应写入最新（注入的）用户消息（ALR-107）"
    );
    let first = reloaded
        .messages
        .iter()
        .find(|m| m.role == MessageRole::User)
        .expect("应有原始用户消息");
    assert_eq!(first.text_content(), "你好", "原始用户消息应保持");
    assert!(
        first.turn_status.is_none(),
        "原始用户消息不应被写入最终状态（ALR-107）"
    );
    // 唯一终态保持（ALR-109）。
    let terminal: Vec<String> = stream_rx
        .try_iter()
        .filter_map(|e| match e {
            StreamEvent::Done { .. } => Some("done".to_string()),
            StreamEvent::Error { message } => Some(format!("error: {message}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        terminal,
        vec!["done".to_string()],
        "一个物理 turn 只发一次 Done（ALR-109），实际终态: {terminal:?}"
    );
}

/// 计数 on_cancel 调用次数的插件（ALR-103 语义验证）。
struct CancelCountingPlugin {
    cancelled: Arc<AtomicU32>,
}

impl ToolOverrideHandler for CancelCountingPlugin {}
impl ToolSpecProvider for CancelCountingPlugin {}
impl PromptSectionProvider for CancelCountingPlugin {}

impl Plugin for CancelCountingPlugin {
    fn id(&self) -> &str {
        "cancel-counter"
    }
    fn on_cancel<'a>(
        &'a self,
        _session: &mut Session,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let cancelled = self.cancelled.clone();
        Box::pin(async move {
            cancelled.fetch_add(1, Ordering::SeqCst);
        })
    }
}

/// ALR-103：普通引导消息不取消插件后台任务（on_cancel 不被调用）；显式取消
/// 整个 turn 时 on_cancel 恰好调用一次。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inject_does_not_cancel_plugins_but_explicit_cancel_does() {
    use super::super::turn::run_turn;
    let server = MockServer::builder().start().await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "paused_probe".to_string(),
        Arc::new(PausedTool {
            started: started.clone(),
            release: release.clone(),
        }),
    );
    // 两轮工具调用：第一轮被注入打断（新意图再次进入工具等待），随后显式取消。
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_1", "paused_probe", "{}"),
            usage_chunk(15, 3),
        ],
    )
    .await;
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_2", "paused_probe", "{}"),
            usage_chunk(20, 4),
        ],
    )
    .await;

    let cancelled = Arc::new(AtomicU32::new(0));
    let plugin = Arc::new(CancelCountingPlugin {
        cancelled: cancelled.clone(),
    });
    let harness = TestHarness::new_with_plugins(
        &server,
        vec![tool_spec("paused_probe")],
        overrides,
        vec![plugin],
    );
    let TestHarness { ctx, stream_rx, .. } = harness;
    // on_cancel 由 run_turn 在终态判定后调用，须走 run_turn 层验证。
    // 顺序驱动：等工具启动 → 注入 → 等第二次工具启动 → 显式取消。
    let started_wait = started.clone();
    let cancelled_probe = cancelled.clone();
    let (tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let _keep_open = tx.clone();
    let cmd_task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(2), started_wait.notified())
            .await
            .expect("第一个工具应已启动");
        tx.send(Command::InjectUserMessage {
            message_id: "injected-cancel-probe".to_string(),
            content: vec![tiangong_types::ContentBlock::text("换个方向")],
        })
        .unwrap();
        // 注入后引导阶段不触发 on_cancel（此刻计数必须为 0）。
        tokio::time::timeout(Duration::from_secs(2), started_wait.notified())
            .await
            .expect("新意图的工具应已启动");
        assert_eq!(
            cancelled_probe.load(Ordering::SeqCst),
            0,
            "引导消息不应触发 on_cancel（ALR-103）"
        );
        tx.send(Command::Cancel).unwrap();
    });
    run_turn(ctx, &mut cmd_rx).await;
    let _ = cmd_task.await;
    assert_eq!(
        cancelled.load(Ordering::SeqCst),
        1,
        "显式取消应恰好触发一次 on_cancel（ALR-103）"
    );
    // 取消终态 + 注入的消息已保存（引导路径生效）。
    let terminal: Vec<String> = stream_rx
        .try_iter()
        .filter_map(|e| match e {
            StreamEvent::Error { message } => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        terminal.iter().any(|m| m.contains("已取消")),
        "应发布取消终态，实际: {terminal:?}"
    );
}

/// 并行工具批次：单个响应返回两个工具调用（不同参数，避免同批去重），
/// 两者都执行并产出结果，协议完整后进入完成度检查（不变量 3：任务↔记录对应）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_tool_batch_executes_both_and_closes_protocol() {
    let server = MockServer::builder().start().await;
    let invocations = Arc::new(Mutex::new(Vec::new()));
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "echo".to_string(),
        Arc::new(EchoTool {
            invocations: invocations.clone(),
        }),
    );
    // 1) 单响应两个工具调用（参数不同，避免同批去重跳过）。
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_a", "echo", r#"{"a":1}"#),
            tool_call_chunk("call_b", "echo", r#"{"b":2}"#),
            usage_chunk(15, 3),
        ],
    )
    .await;
    // 2) 工具后文本以问号结尾 → 进入 Summary；3) Summary 完成。
    mount_sse(
        &server,
        vec![text_delta_chunk("两个都完成了吗?"), usage_chunk(25, 5)],
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("已完成。"), usage_chunk(30, 4)],
    )
    .await;

    let mut harness = TestHarness::new(&server, vec![tool_spec("echo")], overrides);
    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;
    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Success),
        "并行批次完整链路应 Success，实际: {:?}",
        result.outcome
    );
    assert_eq!(invocations.lock().unwrap().len(), 2, "两个并行工具都应执行");
    for call_id in ["call_a", "call_b"] {
        let has_result = harness
            .ctx
            .session
            .messages
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some(call_id));
        assert!(has_result, "{call_id} 应有对应工具结果（协议闭合）");
    }
    harness.drain_stream();
}

/// ALR-101（压缩分支）：压缩进行中收到引导消息——取消压缩、保存新消息、
/// 从新意图重启；被取消压缩的迟到结果不得应用（context_summary 保持为空）。
/// 请求前压缩期间注入用户消息：压缩被中止、迟到结果不应用，新意图重启后
/// 正常完成（ALR-102/303）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inject_during_compression_cancels_and_restarts_without_applying_summary() {
    let server = MockServer::builder().start().await;
    // 1) 第一段：大用量文本完成（建立压力信号）。
    mount_sse(
        &server,
        vec![text_delta_chunk("部分回答。"), usage_chunk(185_900, 5)],
    )
    .await;
    // 2) 第二段请求前压缩（响应延迟制造注入窗口）。
    mount_completion(
        &server,
        "[[CURRENT_TASK]]\n旧任务\n[[SUMMARY]]\n旧摘要（不应被应用）",
        "stop",
        100,
        20,
        Some(Duration::from_millis(400)),
    )
    .await;
    // 3) 注入重启后：再次请求前压缩（无可用压缩响应，失败不应用）。
    mount_sse(
        &server,
        vec![text_delta_chunk("按新要求完成。"), usage_chunk(20, 4)],
    )
    .await;
    // 4) 最终模型请求。
    mount_sse(
        &server,
        vec![text_delta_chunk("已完成。"), usage_chunk(25, 3)],
    )
    .await;

    let mut harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    let _ = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;
    harness.drain_stream();
    harness
        .ctx
        .session
        .append_message(MessageRole::User, "继续任务");

    let TestHarness {
        mut ctx,
        stream_rx: _,
        cmd_tx,
        mut cmd_rx,
        ..
    } = harness;
    let turn = tokio::spawn(async move {
        let result = execute_turn(&mut ctx, &mut cmd_rx).await;
        (result, ctx)
    });
    // 第一段已发出 1 个请求；等待第二段的压缩请求（第 2 个）后注入。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while server.received_requests().await.map_or(0, |r| r.len()) < 2 {
        if tokio::time::Instant::now() >= deadline {
            panic!("压缩请求未在期限内发出");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cmd_tx
        .send(Command::InjectUserMessage {
            message_id: "injected-during-compression".to_string(),
            content: vec![tiangong_types::ContentBlock::text("换个方向，不用压缩了")],
        })
        .unwrap();

    let (result, ctx) = turn.await.expect("turn task 不应 panic");
    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Success),
        "新意图执行应成功，实际: {:?}",
        result.outcome
    );
    assert!(
        ctx.session
            .messages
            .iter()
            .any(|m| m.id == "injected-during-compression" && m.role == MessageRole::User),
        "引导消息应保存进 session"
    );
    assert!(
        ctx.session.context_summary.is_none(),
        "被中断压缩的迟到结果不得应用（context_summary 应为空）"
    );
}

/// ALR-203 连续命令顺序：引导消息与取消接连到达时按序处理——注入先保存
/// 消息并重启，随后的取消形成最终取消终态（不被重启意图覆盖）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consecutive_inject_then_cancel_terminates_in_order() {
    let server = MockServer::builder().start().await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "paused_probe".to_string(),
        Arc::new(PausedTool {
            started: started.clone(),
            release: release.clone(),
        }),
    );
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_1", "paused_probe", "{}"),
            usage_chunk(15, 3),
        ],
    )
    .await;

    let harness = TestHarness::new(&server, vec![tool_spec("paused_probe")], overrides);
    let TestHarness {
        mut ctx,
        stream_rx: _,
        cmd_tx,
        mut cmd_rx,
        ..
    } = harness;
    let tx = cmd_tx.clone();
    let started_wait = started.clone();
    let cmd_task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(2), started_wait.notified())
            .await
            .expect("工具应已启动");
        // 两条命令背靠背投递：注入在前、取消在后。
        tx.send(Command::InjectUserMessage {
            message_id: "injected-then-cancel".to_string(),
            content: vec![tiangong_types::ContentBlock::text("先换个方向")],
        })
        .unwrap();
        tx.send(Command::Cancel).unwrap();
    });
    let result = execute_turn(&mut ctx, &mut cmd_rx).await;
    let _ = cmd_task.await;
    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Cancelled),
        "连续注入后取消应形成取消终态，实际: {:?}",
        result.outcome
    );
    assert!(
        ctx.session
            .messages
            .iter()
            .any(|m| m.id == "injected-then-cancel" && m.role == MessageRole::User),
        "注入的消息应已保存（先于取消处理）"
    );
}

/// 压力场景（任务 09）：连续两条引导消息——都保存成功、按序重启、
/// 最终一次成功终态；锚点为最后一条注入消息。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consecutive_injects_are_all_saved_and_restart_in_order() {
    let server = MockServer::builder().start().await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "paused_probe".to_string(),
        Arc::new(PausedTool {
            started: started.clone(),
            release: release.clone(),
        }),
    );
    // 两次工具调用（两次打断窗口）+ 新意图文本 + Summary。
    for i in 1..=2 {
        let _ = i;
        mount_sse(
            &server,
            vec![
                tool_call_chunk("call_x", "paused_probe", "{}"),
                usage_chunk(15, 3),
            ],
        )
        .await;
    }
    mount_sse(
        &server,
        vec![text_delta_chunk("按最新要求完成。"), usage_chunk(20, 4)],
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("已完成。"), usage_chunk(25, 3)],
    )
    .await;

    let harness = TestHarness::new(&server, vec![tool_spec("paused_probe")], overrides);
    let TestHarness {
        mut ctx,
        stream_rx: _,
        cmd_tx,
        mut cmd_rx,
        ..
    } = harness;
    let tx = cmd_tx.clone();
    let started_wait = started.clone();
    let cmd_task = tokio::spawn(async move {
        for n in 1..=2u32 {
            tokio::time::timeout(Duration::from_secs(2), started_wait.notified())
                .await
                .expect("工具应已启动");
            tx.send(Command::InjectUserMessage {
                message_id: format!("injected-chain-{n}"),
                content: vec![tiangong_types::ContentBlock::text(format!("第 {n} 次调整"))],
            })
            .unwrap();
        }
    });
    let result = execute_turn(&mut ctx, &mut cmd_rx).await;
    let _ = cmd_task.await;
    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Success),
        "连续引导后应成功完成，实际: {:?}",
        result.outcome
    );
    for n in 1..=2u32 {
        let id = format!("injected-chain-{n}");
        assert!(
            ctx.session
                .messages
                .iter()
                .any(|m| m.id == id && m.role == MessageRole::User),
            "{id} 应已保存"
        );
    }
    // 锚点：最后一条用户消息是第二次注入。
    let latest = ctx
        .session
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .expect("应有用户消息");
    assert_eq!(latest.id, "injected-chain-2", "最新锚点应为最后一条注入");
}

/// 压力场景（任务 09）：命令风暴——工具等待中背靠背投递混合命令（标题/用量/
/// 工具注入/思考强度/流事件/引导/取消），按序处理不 panic，取消形成终态，
/// 副作用（标题、用量）生效。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_storm_is_processed_in_order_without_panicking() {
    let server = MockServer::builder().start().await;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "paused_probe".to_string(),
        Arc::new(PausedTool {
            started: started.clone(),
            release: release.clone(),
        }),
    );
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_1", "paused_probe", "{}"),
            usage_chunk(15, 3),
        ],
    )
    .await;

    let harness = TestHarness::new(&server, vec![tool_spec("paused_probe")], overrides);
    let TestHarness {
        mut ctx,
        stream_rx: _,
        cmd_tx,
        mut cmd_rx,
        ..
    } = harness;
    let tx = cmd_tx.clone();
    let started_wait = started.clone();
    let cmd_task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(2), started_wait.notified())
            .await
            .expect("工具应已启动");
        // 命令风暴：混合非决定性命令 + 引导 + 取消（最后）。
        tx.send(Command::SetTitle {
            title: "风暴标题".to_string(),
            only_if_default: false,
        })
        .unwrap();
        tx.send(Command::ReportUsage {
            usage: TokenUsage {
                prompt_tokens: 7,
                completion_tokens: 3,
                total_tokens: 10,
                prompt_cache_hit_tokens: None,
                prompt_cache_miss_tokens: None,
            },
            source: "storm-probe".to_string(),
            emit_event: false,
        })
        .unwrap();
        tx.send(Command::InjectTool {
            tool_name: "storm_data".to_string(),
            payload: serde_json::json!({"k": 1}),
        })
        .unwrap();
        tx.send(Command::SetReasoningEffort(
            tiangong_llm::ReasoningEffort::High,
        ))
        .unwrap();
        tx.send(Command::EmitStreamEvent(Box::new(
            StreamEvent::TitleChanged {
                title: "不应直接出现的标题".to_string(),
            },
        )))
        .unwrap();
        tx.send(Command::InjectUserMessage {
            message_id: "storm-injected".to_string(),
            content: vec![tiangong_types::ContentBlock::text("风暴中的引导")],
        })
        .unwrap();
        tx.send(Command::Cancel).unwrap();
    });
    let result = execute_turn(&mut ctx, &mut cmd_rx).await;
    let _ = cmd_task.await;
    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Cancelled),
        "风暴以取消收尾应形成取消终态，实际: {:?}",
        result.outcome
    );
    assert_eq!(ctx.session.title, "风暴标题", "标题命令应已生效");
    assert_eq!(
        ctx.session.reasoning_effort,
        Some(tiangong_llm::ReasoningEffort::High),
        "思考强度命令应已生效"
    );
    assert!(
        result.usage.total_tokens >= 10,
        "插件用量应累计进终态（ALR-111），实际: {}",
        result.usage.total_tokens
    );
    assert!(
        ctx.session
            .messages
            .iter()
            .any(|m| m.id == "storm-injected" && m.role == MessageRole::User),
        "风暴中的引导消息应已保存"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_effort_update_applies_to_next_model_request() {
    let server = MockServer::builder().start().await;
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_1", "paused_probe", "{}"),
            usage_chunk(15, 3),
        ],
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("好的，已完成。"), usage_chunk(20, 4)],
    )
    .await;

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "paused_probe".to_string(),
        Arc::new(PausedTool {
            started: started.clone(),
            release: release.clone(),
        }),
    );
    let mut harness = TestHarness::new(&server, vec![tool_spec("paused_probe")], overrides);
    let cmd_tx = harness.cmd_tx.clone();
    let update_task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("工具应在期限内开始执行");
        cmd_tx
            .send(Command::SetReasoningEffort(
                tiangong_llm::ReasoningEffort::Max,
            ))
            .expect("运行中的 turn 应接收思考强度更新");
        release.notify_one();
    });

    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;
    update_task.await.unwrap();

    assert!(matches!(result.outcome, TurnExecutionOutcome::Success));
    assert_eq!(
        harness.ctx.agent_config.reasoning_effort,
        tiangong_llm::ReasoningEffort::Max
    );
    assert_eq!(
        harness.ctx.session.reasoning_effort,
        Some(tiangong_llm::ReasoningEffort::Max)
    );

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let first_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let second_body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert!(first_body.get("reasoning_effort").is_none());
    assert_eq!(first_body["thinking"]["type"], "disabled");
    assert_eq!(second_body["reasoning_effort"], "max");
    assert_eq!(second_body["thinking"]["type"], "enabled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn returns_failed_when_react_request_fails() {
    let server = MockServer::builder().start().await;
    mount_request_error(&server, "execute turn request rejected").await;

    let mut harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;

    let TurnExecutionOutcome::Failed(message) = &result.outcome else {
        panic!("模型请求失败应返回 Failed，实际: {:?}", result.outcome);
    };
    assert!(!message.is_empty(), "模型请求失败必须返回错误原因");
    assert!(
        harness
            .ctx
            .session
            .messages
            .iter()
            .any(|session_message| session_message.text_content().contains(message))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handles_runtime_feedback_while_request_is_running() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(
                    sse_body(&[text_delta_chunk("运行时命令已处理。"), usage_chunk(10, 5)]),
                    "text/event-stream",
                )
                .set_delay(Duration::from_millis(500)),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("插件结果已处理。"), usage_chunk(6, 2)],
    )
    .await;

    let harness = TestHarness::new(&server, Vec::new(), HashMap::new());
    let TestHarness {
        mut ctx,
        stream_rx,
        cmd_tx,
        mut cmd_rx,
        ..
    } = harness;
    let runtime_cmd_tx = cmd_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        runtime_cmd_tx
            .send(Command::InjectTool {
                tool_name: "runtime_probe".to_string(),
                payload: serde_json::json!({"value": 1}),
            })
            .unwrap();
        runtime_cmd_tx
            .send(Command::EmitStreamEvent(Box::new(
                StreamEvent::AgentNotification {
                    agent_id: "runtime-probe".to_string(),
                    agent_label: "测试".to_string(),
                    content: "命令已转发".to_string(),
                    level: "info".to_string(),
                },
            )))
            .unwrap();
        runtime_cmd_tx
            .send(Command::ReportUsage {
                usage: TokenUsage {
                    prompt_tokens: 4,
                    completion_tokens: 3,
                    total_tokens: 7,
                    prompt_cache_hit_tokens: None,
                    prompt_cache_miss_tokens: None,
                },
                source: "runtime-probe".to_string(),
                emit_event: true,
            })
            .unwrap();
    });
    let pending_event_task = tokio::task::spawn_blocking(move || {
        let event = stream_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("模型请求完成前应收到插件待处理快照");
        (event, stream_rx)
    });
    let mut execution = Box::pin(execute_turn(&mut ctx, &mut cmd_rx));
    let (first_event, stream_rx) = tokio::select! {
        event = pending_event_task => event.unwrap(),
        result = &mut execution => panic!("插件待处理快照晚于 turn 结束：{:?}", result.outcome),
    };
    assert!(matches!(
        &first_event,
        StreamEvent::DeferredToolInjectionsChanged { injections }
            if injections.len() == 1 && injections[0].tool_name == "runtime_probe"
    ));

    let result = execution.await;
    drop(cmd_tx);
    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Success),
        "运行时命令处理结果: {:?}",
        result.outcome
    );
    assert_eq!(result.usage.total_tokens, 30);
    assert!(ctx.session.messages.iter().any(|message| {
        message.role == MessageRole::Tool && message.text_content().contains("runtime_probe")
    }));
    assert!(ctx.session.deferred_tool_injections.is_empty());

    let mut events = vec![first_event];
    events.extend(stream_rx.try_iter());
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::AgentNotification { agent_id, .. } if agent_id == "runtime-probe"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::TokenUsage { source, .. } if source == "runtime-probe"
    )));
    let injection_snapshots = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::DeferredToolInjectionsChanged { injections } => Some(injections),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(injection_snapshots.len(), 2);
    assert_eq!(injection_snapshots[0].len(), 1);
    assert!(injection_snapshots[1].is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn returns_cancelled_when_tool_execution_is_cancelled() {
    let server = MockServer::builder().start().await;
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_block_1", "blocking_1", "{}"),
            tool_call_chunk("call_block_2", "blocking_2", "{}"),
            usage_chunk(9, 2),
        ],
    )
    .await;

    let all_started = Arc::new(Notify::new());
    let handler: Arc<dyn ToolOverrideHandler> = Arc::new(BlockingBatchTool {
        barrier: Arc::new(Barrier::new(2)),
        all_started: all_started.clone(),
    });
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert("blocking_1".to_string(), handler.clone());
    overrides.insert("blocking_2".to_string(), handler);
    let mut harness = TestHarness::new(
        &server,
        vec![tool_spec("blocking_1"), tool_spec("blocking_2")],
        overrides,
    );
    let cmd_tx = harness.cmd_tx.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), all_started.notified())
            .await
            .expect("并行阻塞工具未全部开始执行");
        cmd_tx.send(Command::Cancel).unwrap();
    });

    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;
    cancel_task.await.unwrap();

    assert!(matches!(result.outcome, TurnExecutionOutcome::Cancelled));
    let interrupted_ids = harness
        .ctx
        .session
        .messages
        .iter()
        .filter(|message| {
            message.role == MessageRole::Tool && message.text_content().contains("中断")
        })
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(interrupted_ids, vec!["call_block_1", "call_block_2"]);
    let app_results = harness
        .stream_rx
        .try_iter()
        .filter_map(|event| match event {
            StreamEvent::ToolResult { tool_call_id, .. } => tool_call_id,
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(app_results, vec!["call_block_1", "call_block_2"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continues_after_tool_failure_with_recovery_context() {
    let server = MockServer::builder().start().await;
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_fail", "failing", "{}"),
            usage_chunk(9, 2),
        ],
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("已改用其他方案完成。"), usage_chunk(11, 3)],
    )
    .await;

    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert("failing".to_string(), Arc::new(FailingTool));
    let mut harness = TestHarness::new(&server, vec![tool_spec("failing")], overrides);

    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;

    assert!(
        matches!(result.outcome, TurnExecutionOutcome::Success),
        "工具失败恢复后的结果: {:?}",
        result.outcome
    );
    assert_eq!(result.usage.total_tokens, 25);
    assert!(
        harness
            .ctx
            .session
            .messages
            .iter()
            .any(|message| { message.tool_name.as_deref() == Some("react_failed_tool_recovery") })
    );
    assert!(harness.ctx.session.messages.iter().any(|message| {
        message.role == MessageRole::Tool
            && message.tool_name.as_deref() == Some("failing")
            && message.text_content().contains("test failure")
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_invalid_tool_calls_are_filtered_then_regenerated() {
    let server = MockServer::builder().start().await;
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_invalid", "echo", "{}"),
            usage_chunk(9, 2),
        ],
    )
    .await;
    mount_sse(
        &server,
        vec![
            tool_call_chunk("call_valid", "echo", r#"{"message":"schema 已修正"}"#),
            usage_chunk(11, 3),
        ],
    )
    .await;
    mount_sse(
        &server,
        vec![text_delta_chunk("已完成。"), usage_chunk(13, 4)],
    )
    .await;

    let invocations = Arc::new(Mutex::new(Vec::new()));
    let mut overrides: HashMap<String, Arc<dyn ToolOverrideHandler>> = HashMap::new();
    overrides.insert(
        "echo".to_string(),
        Arc::new(EchoTool {
            invocations: invocations.clone(),
        }),
    );
    let tool = ToolSpec {
        name: "echo".to_string(),
        description: "回显消息".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"message": {"type": "string"}},
            "required": ["message"],
            "additionalProperties": false
        }),
    };
    let mut harness = TestHarness::new(&server, vec![tool], overrides);

    let result = execute_turn(&mut harness.ctx, &mut harness.cmd_rx).await;

    assert!(matches!(result.outcome, TurnExecutionOutcome::Success));
    assert_eq!(result.usage.total_tokens, 42);
    {
        let calls = invocations.lock().unwrap();
        assert_eq!(calls.len(), 1, "被剔除的调用不应执行");
        assert_eq!(calls[0].id, "call_valid");
    }

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let second_body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let second_body_text = second_body.to_string();
    assert!(second_body_text.contains("invalid_tool_calls"));
    assert!(second_body_text.contains("required"));
    assert!(harness.ctx.session.messages.iter().all(|message| {
        !message
            .tool_calls
            .iter()
            .any(|call| call.id == "call_invalid")
    }));
}

/// 插件收尾（on_turn_finished）同步阻塞：通知型钩子后台投递、turn 不等待——
/// 终态正常发布、turn 任务立即结束（issue #404）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalling_plugin_finish_does_not_swallow_terminal() {
    use super::super::turn::run_turn;
    let server = MockServer::builder().start().await;
    mount_sse(
        &server,
        vec![text_delta_chunk("普通答复。"), usage_chunk(10, 2)],
    )
    .await;

    struct StallingFinishPlugin;
    impl crate::tools::extension::ToolSpecProvider for StallingFinishPlugin {}
    impl crate::tools::extension::ToolOverrideHandler for StallingFinishPlugin {}
    impl crate::tools::extension::PromptSectionProvider for StallingFinishPlugin {}
    impl Plugin for StallingFinishPlugin {
        fn id(&self) -> &str {
            "stalling-finish"
        }
        fn on_turn_finished(&self, _session: &Session, _turn_start_idx: usize) {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }

    let harness = TestHarness::new_with_plugins(
        &server,
        Vec::new(),
        HashMap::new(),
        vec![Arc::new(StallingFinishPlugin)],
    );
    let TestHarness {
        ctx,
        stream_rx,
        mut cmd_rx,
        ..
    } = harness;
    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        run_turn(ctx, &mut cmd_rx),
    )
    .await
    .expect("turn 任务不得等待慢速插件收尾（通知即返回）");
    assert!(
        matches!(terminal, StreamEvent::Done { .. }),
        "终态必须正常发布，实际: {terminal:?}"
    );
    let done = stream_rx
        .try_iter()
        .filter(|e| matches!(e, StreamEvent::Done { .. }))
        .count();
    assert_eq!(done, 1, "Done 事件必须已到达事件流");
}

/// 复评小问题 4 回归：失败的工具（result.ok == false）即使 stdout 带
/// injected_assets 声明也不得注入——错误输出（含失败前的半成品产物）
/// 不进模型上下文。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_tool_call_does_not_inject_images() {
    use crate::react::execute::{CompletedToolCall, ToolCallHistory, record_completed_tool_call};
    use crate::tools::result::ToolResult;
    use tiangong_llm::tool::ToolCall;

    let storage = tempfile::tempdir().unwrap();
    let media = storage.path().join("media");
    std::fs::create_dir_all(&media).unwrap();
    let image_path = media.join("shot.png");
    std::fs::write(&image_path, [137_u8, 80, 78, 71, 1, 2, 3]).unwrap();
    let stdout = serde_json::json!({
        "path": image_path,
        "injected_assets": [{
            "local_path": image_path,
            "mime_type": "image/png",
            "original_name": "shot.png",
            "size_bytes": 7,
            "kind": "image"
        }]
    })
    .to_string();

    let (stream_tx, _stream_rx) = std::sync::mpsc::channel();
    let client = SingleProviderClient::new(ModelEndpoint {
        headers: Default::default(),
        base_url: "http://127.0.0.1:1".to_string(),
        api_key: "test-key".to_string(),
        model: "test-model".to_string(),
        protocol: ProviderProtocol::OpenAiChatCompletions,
        timeout_ms: 1_000,
        options: serde_json::Value::Object(serde_json::Map::new()),
        context_window: None,
    });
    let mut ctx = TurnContext::builder()
        .client(client)
        .session(crate::session::Session::new("inject-gate").with_storage_root(storage.path()))
        .stream_tx(stream_tx)
        .plugins(Vec::new())
        .context_limit(200_000)
        .agent_config(crate::config::agent::AgentConfig::default())
        .trust_mode(crate::permission::TrustMode::FullTrust)
        .observer(crate::observe::Observer::new(std::env::temp_dir()))
        .tool_overrides(Default::default())
        .tools(Vec::new())
        .build();

    let call = ToolCall {
        id: "call-fail".to_string(),
        name: "desktop_screenshot".to_string(),
        arguments: serde_json::json!({}),
    };
    let failed = ToolResult {
        ok: false,
        summary: "截图失败".to_string(),
        stdout: stdout.clone(),
        stderr: String::new(),
        exit_code: 1,
        execution: None,
    };
    let mut history = ToolCallHistory::default();
    let mut pending = Vec::new();
    let needs_recovery = record_completed_tool_call(
        &mut ctx,
        CompletedToolCall {
            call: &call,
            args_summary: "",
            result: &failed,
            duration_ms: 5,
        },
        &mut history,
        &mut pending,
    );
    assert!(needs_recovery, "失败结果应要求恢复提示");
    assert!(pending.is_empty(), "失败的工具不得注入图片");

    // 同一份 stdout、成功结果：正常注入。
    let ok_result = ToolResult {
        ok: true,
        summary: "截图成功".to_string(),
        stdout,
        stderr: String::new(),
        exit_code: 0,
        execution: None,
    };
    record_completed_tool_call(
        &mut ctx,
        CompletedToolCall {
            call: &call,
            args_summary: "",
            result: &ok_result,
            duration_ms: 5,
        },
        &mut history,
        &mut pending,
    );
    assert_eq!(pending.len(), 1, "成功结果应注入图片");
}
