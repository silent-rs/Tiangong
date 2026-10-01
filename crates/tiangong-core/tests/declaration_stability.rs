//! Core 声明收集测试：直接模拟 Plugin 接口，不加载 WASM 或启动插件服务。
//!
//! 分层契约：声明内容的运行期稳定由 runtime 适配器的冻结快照保证
//!（本文件的 MockPlugin 以冻结语义模拟适配器行为）；core 不做任何稳定
//! 化处理，只做固定顺序加载与透传——读取失败的插件本轮缺席、恢复下一
//! 轮回归，插件缺席/移除即时反映。
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tiangong_core::{
    agent_input::{AgentInput, AgentInputKind},
    config::core::{CoreConfig, CoreConfigProvider},
    core::{Plugin, TiangongCore},
    permission::TrustMode,
    session::Session,
    tools::extension::{PromptSectionProvider, ToolOverrideHandler, ToolSpecProvider},
    tools::result::ToolResult,
};
use tiangong_llm::tool::{ToolCall, ToolSpec};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
struct PluginState {
    available: AtomicBool,
    revision: AtomicUsize,
    calls: Mutex<Vec<String>>,
    /// 模拟 runtime 适配器的冻结快照：首次成功后固定返回，失败不穿透。
    /// 挂在共享状态上，模拟适配器实例跨 Core 重建存活。
    cached_declaration: Mutex<Option<String>>,
}
impl PluginState {
    fn new() -> Self {
        Self {
            available: AtomicBool::new(true),
            revision: AtomicUsize::new(0),
            calls: Mutex::new(Vec::new()),
            cached_declaration: Mutex::new(None),
        }
    }
    /// 声明读取：冻结语义——已有冻结值直接返回，否则真实读取。
    fn declaration(&self, operation: &str) -> Result<String, String> {
        self.calls.lock().unwrap().push(operation.into());
        if !self.available.load(Ordering::SeqCst) {
            let cached = self.cached_declaration.lock().unwrap().clone();
            return match cached {
                Some(value) => Ok(value),
                None => Err("插件声明暂时不可用".into()),
            };
        }
        let value = format!("revision-{}", self.revision.load(Ordering::SeqCst));
        *self.cached_declaration.lock().unwrap() = Some(value.clone());
        Ok(value)
    }
}
struct MockPlugin {
    id: String,
    state: Arc<PluginState>,
}
impl Plugin for MockPlugin {
    fn id(&self) -> &str {
        &self.id
    }
}
impl ToolSpecProvider for MockPlugin {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        let description = if self.id.starts_with("dynamic-tools") {
            // 冻结模拟：失败且无冻结值时返回空（core 不感知失败）。
            match self.state.declaration("tools") {
                Ok(value) => value,
                Err(_) => return Vec::new(),
            }
        } else {
            "固定工具".into()
        };
        vec![ToolSpec {
            name: "probe_read".into(),
            description,
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        }]
    }
}
impl PromptSectionProvider for MockPlugin {
    fn prompt_sections(&self) -> Vec<String> {
        if self.id.starts_with("dynamic-prompt") {
            // 冻结模拟：失败且无冻结值时返回空。
            match self.state.declaration("prompt") {
                Ok(value) => vec![value],
                Err(_) => Vec::new(),
            }
        } else {
            vec!["固定插件提示".into()]
        }
    }
}
impl ToolOverrideHandler for MockPlugin {
    fn handle(
        &self,
        call: &ToolCall,
        _session: &mut Session,
        _actor_id: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<ToolResult>> + Send>> {
        let handled = call.name == "probe_read";
        let ok = self.state.available.load(Ordering::SeqCst);
        Box::pin(async move {
            handled.then(|| ToolResult {
                ok,
                summary: if ok {
                    "完成"
                } else {
                    "工具暂时不可用"
                }
                .into(),
                stdout: if ok { "OK" } else { "" }.into(),
                stderr: if ok { "" } else { "工具暂时不可用" }.into(),
                exit_code: if ok { 0 } else { 1 },
                execution: None,
            })
        })
    }
}
fn plugin(id: &str, state: Arc<PluginState>) -> Arc<dyn Plugin> {
    Arc::new(MockPlugin {
        id: id.into(),
        state,
    })
}

fn core(
    root: &std::path::Path,
    server: &MockServer,
    plugin: Arc<dyn Plugin>,
) -> (
    TiangongCore,
    CoreConfig,
    std::sync::mpsc::Receiver<tiangong_types::StreamEvent>,
    String,
) {
    let mut session = Session::new("稳定声明验证");
    session.cwd = root.to_string_lossy().into_owned();
    session.bind_storage_root(root);
    session.try_persist_to_disk().unwrap();
    let config = CoreConfig::builder()
        .with_trust_mode(TrustMode::FullTrust)
        .build();
    let (tx, rx) = std::sync::mpsc::channel();
    let core = TiangongCore::builder()
        .session_id(session.id.clone())
        .storage_root(root.to_path_buf())
        .workspace_dir(root.to_string_lossy())
        .trust_mode(TrustMode::FullTrust)
        .config(CoreConfigProvider::new(config.clone()))
        .stream_tx(tx)
        .plugins(vec![plugin])
        .model_endpoint(turn_model(&server.uri()))
        .build();
    (core, config, rx, session.id)
}

/// 测试用运行时模型：Core 构造时必须持有实际模型端点。
fn turn_model(base_url: &str) -> tiangong_llm::ModelEndpoint {
    tiangong_llm::ModelEndpoint {
        base_url: base_url.to_string(),
        api_key: "test".to_string(),
        model: "test-model".to_string(),
        ..Default::default()
    }
}

async fn send(
    core: &TiangongCore,
    rx: &std::sync::mpsc::Receiver<tiangong_types::StreamEvent>,
    text: &str,
) {
    core.deliver(AgentInputKind::prepared(vec![
        tiangong_types::ContentBlock::text(text),
    ]))
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        let mut done = false;
        loop {
            done |= rx
                .try_iter()
                .any(|event| matches!(event, tiangong_types::StreamEvent::Done { .. }));
            if done && !core.is_busy() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

/// `call` 为 (序号, 插件 id)：模型按对外名 `{插件id}__probe_read` 调用工具。
fn reply(call: Option<(usize, &str)>) -> ResponseTemplate {
    let (message, finish) = match call {
        Some((index, plugin_id)) => (
            json!({"role":"assistant","tool_calls":[{"id":format!("call-{index}"),"type":"function","function":{"name":format!("{plugin_id}__probe_read"),"arguments":"{\"path\":\"probe\"}"}}]}),
            "tool_calls",
        ),
        None => (json!({"role":"assistant","content":"完成"}), "stop"),
    };
    ResponseTemplate::new(200)
        .set_body_json(json!({"id":"reply","choices":[{"message":message,"finish_reason":finish}]}))
}

/// 同一进程内声明抖动被进程缓存兜住：插件声明时好时坏，请求里的
/// tools/prompt 集合保持稳定；Core 重建（App 未重启）同样由缓存兜住；
/// 插件恢复新版本后的下一轮即生效。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declaration_jitter_is_absorbed_by_process_cache_across_core_recreation() {
    for id in ["dynamic-tools-jitter", "dynamic-prompt-jitter"] {
        let state = Arc::new(PluginState::new());
        let root = tempfile::tempdir().unwrap();
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(reply(None))
            .mount(&server)
            .await;
        let (core, config, rx, sid) = core(root.path(), &server, plugin(id, state.clone()));
        for (round, available) in [true, false, true].into_iter().enumerate() {
            state.available.store(available, Ordering::SeqCst);
            core.replace_config(config.clone()).unwrap();
            send(&core, &rx, &format!("继续 {round}")).await;
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
        for request in &requests[1..] {
            let next: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(first["tools"], next["tools"]);
            let old = first["messages"].as_array().unwrap();
            assert_eq!(
                &next["messages"].as_array().unwrap()[..old.len()],
                old.as_slice()
            );
        }
        let operation = if id.starts_with("dynamic-tools") {
            "tools"
        } else {
            "prompt"
        };
        // 每轮实时收集一次（第二、三轮失败但尝试过）。
        assert_eq!(
            state
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.as_str() == operation)
                .count(),
            3
        );
        core.shutdown_join().unwrap();

        // Core 重建 + 插件不可用 + 版本已变：进程缓存兜住旧声明，
        // 请求前缀保持不变。
        state.available.store(false, Ordering::SeqCst);
        state.revision.store(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel();
        let restored = TiangongCore::builder()
            .session_id(sid.clone())
            .storage_root(root.path().to_path_buf())
            .workspace_dir(root.path().to_string_lossy())
            .trust_mode(TrustMode::FullTrust)
            .config(CoreConfigProvider::new(config))
            .stream_tx(tx)
            .plugins(vec![plugin(id, state.clone()) as Arc<dyn Plugin>])
            .model_endpoint(turn_model(&server.uri()))
            .build();
        send(&restored, &rx, "重建后继续").await;
        let requests = server.received_requests().await.unwrap();
        let replay: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        assert_eq!(first["tools"], replay["tools"]);
        assert_eq!(first["messages"][0], replay["messages"][0]);

        // 插件恢复（新版本）后的下一轮：tools 与 prompt 类都即时生效
        //（每轮 turn 启动时就地重拼 system prompt，内容由插件冻结快照
        // 保证稳定，变化即真实变化）。
        state.available.store(true, Ordering::SeqCst);
        send(&restored, &rx, "恢复后继续").await;
        let requests = server.received_requests().await.unwrap();
        let next: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        if id.starts_with("dynamic-tools") {
            assert_ne!(first["tools"], next["tools"]);
        } else {
            assert_ne!(first["messages"][0], next["messages"][0]);
        }
        restored.shutdown_join().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_execution_and_retry_do_not_change_declarations_or_invent_feedback() {
    let state = Arc::new(PluginState::new());
    let root = tempfile::tempdir().unwrap();
    let server = MockServer::builder().start().await;
    let step = AtomicUsize::new(0);
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let step = step.fetch_add(1, Ordering::SeqCst);
            reply(matches!(step, 1 | 3).then_some((step, "dynamic-tools-retry")))
        })
        .mount(&server)
        .await;
    let (core, _, rx, id) = core(
        root.path(),
        &server,
        plugin("dynamic-tools-retry", state.clone()),
    );
    send(&core, &rx, "初始化").await;
    state.available.store(false, Ordering::SeqCst);
    send(&core, &rx, "调用工具").await;
    state.available.store(true, Ordering::SeqCst);
    send(&core, &rx, "重试工具").await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 5);
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let last: Value = serde_json::from_slice(&requests[4].body).unwrap();
    for request in &requests[1..] {
        let next: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(first["tools"], next["tools"]);
        assert_eq!(first["messages"][0], next["messages"][0]);
    }
    assert!(
        String::from_utf8_lossy(&requests[2].body).contains("暂时不可用"),
        "{}",
        String::from_utf8_lossy(&requests[2].body)
    );
    assert!(!last["messages"].to_string().contains("plugin_availability"));
    let session = Session::load_from_storage(root.path(), &id).unwrap();
    assert_eq!(
        session
            .messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .filter(|call| call.arguments["source"] == "plugin_availability")
            .count(),
        0
    );
    core.shutdown_join().unwrap();
}

/// 插件在场与否是合法变化：Core 重建后插件缺席，其工具即时从请求消失
///（缓存只兜“在场但读取失败”，不代持缺席插件）；模型仍被引导调用时
/// 以“未注册的工具”报告执行失败。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_plugin_after_recreation_drops_tools_and_reports_execution_failure() {
    let root = tempfile::tempdir().unwrap();
    let server = MockServer::builder().start().await;
    let step = AtomicUsize::new(0);
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let step = step.fetch_add(1, Ordering::SeqCst);
            reply((step == 1).then_some((step, "dynamic-tools-vanish")))
        })
        .mount(&server)
        .await;
    let (core, config, rx, sid) = core(
        root.path(),
        &server,
        plugin("dynamic-tools-vanish", Arc::new(PluginState::new())),
    );
    send(&core, &rx, "初始化").await;
    core.shutdown_join().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let restored = TiangongCore::builder()
        .session_id(sid)
        .storage_root(root.path().to_path_buf())
        .workspace_dir(root.path().to_string_lossy())
        .trust_mode(TrustMode::FullTrust)
        .config(CoreConfigProvider::new(config))
        .stream_tx(tx)
        .plugins(Vec::new())
        .model_endpoint(turn_model(&server.uri()))
        .build();
    send(&restored, &rx, "调用原工具").await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let next: Value = serde_json::from_slice(&requests[2].body).unwrap();
    let names_of = |payload: &Value| -> Vec<String> {
        payload["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        names_of(&first),
        vec!["plugin_injection", "dynamic-tools-vanish__probe_read"]
    );
    assert_eq!(
        names_of(&next),
        vec!["plugin_injection"],
        "插件缺席后其工具应即时消失"
    );
    assert!(
        String::from_utf8_lossy(&requests[2].body)
            .contains("工具 dynamic-tools-vanish__probe_read 不在本次 tools 定义中"),
        "插件缺席后模型对其调用的反馈应保留在请求中"
    );
    restored.shutdown_join().unwrap();
}

/// 冷启动离线：进程缓存无记录时插件声明缺失（不阻塞对话）；插件恢复
/// 后的下一轮即进入请求（无需清空上下文）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_plugin_starts_missing_and_recovers_next_turn() {
    let root = tempfile::tempdir().unwrap();
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(reply(None))
        .mount(&server)
        .await;
    let state = Arc::new(PluginState::new());
    state.available.store(false, Ordering::SeqCst);
    let (core, _, rx, _sid) = core(
        root.path(),
        &server,
        plugin("dynamic-tools-cold", state.clone()),
    );
    send(&core, &rx, "离线开始").await;
    state.available.store(true, Ordering::SeqCst);
    send(&core, &rx, "恢复后普通续聊").await;
    core.deliver(AgentInputKind::reset_context()).unwrap();
    send(&core, &rx, "整理后继续").await;
    let requests = server.received_requests().await.unwrap();
    let bodies: Vec<Value> = requests
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(bodies.len(), 3);
    let tool_count = |payload: &Value| payload["tools"].as_array().unwrap().len();
    assert_eq!(tool_count(&bodies[0]), 1, "离线开局应只有内置工具");
    assert_eq!(
        tool_count(&bodies[1]),
        2,
        "插件恢复后的下一轮即应带上其工具"
    );
    assert_eq!(tool_count(&bodies[2]), 2);
    core.shutdown_join().unwrap();
}
