//! WASM 插件运行时集成测试。
//!
//! 前置条件：需先构建示例 memory wasm 组件：
//! ```sh
//! cargo build -p tiangong-plugin-memory-wasm --target wasm32-wasip2
//! ```
//! 或 `cargo run -p xtask -- build-wasm`。
//! 本测试默认指向 workspace target 目录下的 debug 产物。

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tiangong_core::config::core::CoreConfig;
use tiangong_core::core::Plugin;
use tiangong_core::session::Session;
use tiangong_core::tools::extension::{PromptSectionProvider, ToolSpecProvider};
use tiangong_plugin_runtime::{
    PluginRuntimeConfig, SidecarConnection, ToolCall, WasmPluginAdapter, WasmPluginLoader,
};

#[derive(Default)]
struct MockMemorySidecar {
    calls: Mutex<Vec<(String, serde_json::Value)>>,
    call_recorded: Condvar,
}

impl MockMemorySidecar {
    fn called(&self, operation: &str) -> bool {
        self.calls
            .lock()
            .map(|calls| calls.iter().any(|(item, _)| item == operation))
            .unwrap_or(false)
    }

    fn payload(&self, operation: &str) -> serde_json::Value {
        self.calls
            .lock()
            .ok()
            .and_then(|calls| {
                calls
                    .iter()
                    .rev()
                    .find(|(item, _)| item == operation)
                    .map(|(_, payload)| payload.clone())
            })
            .unwrap_or(serde_json::Value::Null)
    }

    fn wait_for_call_count(&self, operation: &str, expected: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        let Ok(mut calls) = self.calls.lock() else {
            return false;
        };
        loop {
            if calls.iter().filter(|(item, _)| item == operation).count() >= expected {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let Ok((next, result)) = self.call_recorded.wait_timeout(calls, remaining) else {
                return false;
            };
            calls = next;
            if result.timed_out() {
                return calls.iter().filter(|(item, _)| item == operation).count() >= expected;
            }
        }
    }
}

impl SidecarConnection for MockMemorySidecar {
    fn invoke(&self, operation: &str, payload: &str) -> anyhow::Result<String> {
        self.invoke_with_progress(operation, payload, &mut |_| {})
    }

    fn invoke_with_progress(
        &self,
        operation: &str,
        payload: &str,
        on_progress: &mut dyn FnMut(String),
    ) -> anyhow::Result<String> {
        let payload = serde_json::from_str(payload)?;
        self.calls
            .lock()
            .map_err(|_| anyhow::anyhow!("模拟 sidecar 调用记录已损坏"))?
            .push((operation.to_string(), payload));
        self.call_recorded.notify_all();
        let response = match operation {
            "recall_context" => {
                on_progress(serde_json::to_string(
                    &tiangong_types::StreamEvent::MemoryRecallProgress {
                        phase: "检索中".to_string(),
                    },
                )?);
                serde_json::json!({
                    "response": {
                        "content": "模拟记忆结果",
                        "hits": [{
                            "node_id": "node-1",
                            "title": "历史结果",
                            "summary": "模拟摘要",
                            "score": 0.9,
                            "kind": "episode",
                            "importance": 0.8,
                            "depth1_loaded": false
                        }]
                    }
                })
            }
            "load_injection" => serde_json::json!({"items": []}),
            _ => serde_json::json!({}),
        };
        Ok(serde_json::to_string(&response)?)
    }
}

#[derive(Default)]
struct BlockingMemorySidecarState {
    calls: Vec<String>,
    release_rumination: bool,
    auto_released: bool,
}

#[derive(Default)]
struct BlockingMemorySidecar {
    state: Mutex<BlockingMemorySidecarState>,
    changed: Condvar,
}

impl BlockingMemorySidecar {
    fn wait_for_call(&self, operation: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        loop {
            if state.calls.iter().any(|call| call == operation) {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, result) = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poison| poison.into_inner());
            state = next;
            if result.timed_out() {
                return state.calls.iter().any(|call| call == operation);
            }
        }
    }

    fn release_rumination(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.release_rumination = true;
        self.changed.notify_all();
    }

    fn auto_released(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.auto_released)
            .unwrap_or_else(|poison| poison.into_inner().auto_released)
    }
}

impl SidecarConnection for BlockingMemorySidecar {
    fn invoke(&self, operation: &str, _payload: &str) -> anyhow::Result<String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.calls.push(operation.to_string());
        self.changed.notify_all();

        if operation == "run_enhanced_micro_rumination" {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !state.release_rumination {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    state.auto_released = true;
                    break;
                };
                let (next, result) = self
                    .changed
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(|poison| poison.into_inner());
                state = next;
                if result.timed_out() && !state.release_rumination {
                    state.auto_released = true;
                    break;
                }
            }
        }

        Ok("{}".to_string())
    }
}

/// 定位示例 memory wasm 组件文件。
fn memory_wasm_path() -> PathBuf {
    if let Ok(p) = std::env::var("MEMORY_WASM_PATH") {
        return PathBuf::from(p);
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("target/wasm32-wasip2/debug/tiangong_plugin_memory_wasm.wasm");
    path
}

/// 若 wasm 文件不存在则提示并跳过当前测试。
fn wasm_or_skip() -> Option<PathBuf> {
    let path = memory_wasm_path();
    if !path.exists() {
        eprintln!(
            "跳过测试：未找到 wasm 组件 {}，请先执行 `cargo run -p xtask -- build-wasm`",
            path.display()
        );
        return None;
    }
    Some(path)
}

#[test]
fn load_and_describe() {
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader = WasmPluginLoader::new(&config).expect("创建加载器失败");
    let mut plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");

    let desc = plugin.describe().expect("describe 失败");
    assert_eq!(desc.id, "memory");
    assert_eq!(desc.name, "Memory");
    assert!(!desc.version.is_empty());
}

#[test]
fn tool_specs_contains_recall_memory() {
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader = WasmPluginLoader::new(&config).expect("创建加载器失败");
    let mut plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");

    let specs = plugin.tool_specs().expect("tool-specs 失败");
    let recall = specs
        .iter()
        .find(|s| s.name == "recall_memory")
        .expect("应声明 recall_memory 工具");
    assert!(recall.description.contains("回忆"));
    let schema: serde_json::Value =
        serde_json::from_str(&recall.input_schema).expect("input_schema 应为合法 JSON");
    assert_eq!(schema["type"], "object");
}

#[test]
fn handle_unknown_tool_returns_error() {
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader = WasmPluginLoader::new(&config).expect("创建加载器失败");
    let mut plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");

    let result = plugin.handle_tool(
        ToolCall {
            id: "call_2".into(),
            name: "nonexistent_tool".into(),
            arguments: "{}".into(),
        },
        &config,
    );
    assert!(result.is_err(), "未知工具应返回错误");

    // 插件已接管调用后产生的错误必须穿过适配器，不能退成未注册。
    let adapter = WasmPluginAdapter::new(plugin, config, "test-session");
    let runtime = tokio::runtime::Runtime::new().expect("创建 runtime 失败");
    let call = tiangong_llm::tool::ToolCall {
        id: "call_2".into(),
        name: "nonexistent_tool".into(),
        arguments: serde_json::json!({}),
    };
    let result = runtime
        .block_on(
            tiangong_core::tools::extension::ToolOverrideHandler::handle(
                &adapter,
                &call,
                &mut test_session(),
                "test",
            ),
        )
        .expect("插件执行错误不得变成 None");
    assert!(!result.ok);
    assert!(result.stderr.contains("nonexistent_tool"));
    assert!(!result.summary.contains("未注册"));
}

#[test]
fn fuel_limit_traps_runaway_execution() {
    // 给极少 fuel，迫使一次正常调用因 fuel 不足被中断，证明 fuel 限制生效。
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let strict = PluginRuntimeConfig {
        fuel_limit: 1_000,
        epoch_deadline: Duration::from_secs(10),
        ..PluginRuntimeConfig::default()
    };
    let loader = WasmPluginLoader::new(&PluginRuntimeConfig::default()).expect("创建加载器失败");
    let mut plugin = loader
        .load(&wasm, &PluginRuntimeConfig::default())
        .expect("加载 wasm 组件失败");

    let result = plugin.handle_tool(
        ToolCall {
            id: "call_3".into(),
            name: "recall_memory".into(),
            arguments: r#"{"query":"test"}"#.into(),
        },
        &strict,
    );
    assert!(result.is_err(), "极少 fuel 下调用应被中断");
}

#[test]
fn handle_recall_memory_without_handle_returns_disabled() {
    // 无 handle 时，经 request 转发应返回 disabled 降级提示（而非错误）。
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader = WasmPluginLoader::new(&config).expect("创建加载器失败");
    let mut plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");

    let outcome = plugin
        .handle_tool(
            ToolCall {
                id: "c1".into(),
                name: "recall_memory".into(),
                arguments: r#"{"query":"测试查询"}"#.into(),
            },
            &config,
        )
        .expect("handle-tool 失败");
    assert!(!outcome.ok);
    assert_eq!(outcome.exit_code, 1);
    assert!(outcome.execution.is_some());
    // 无 handle 时应返回降级提示。
    assert!(
        outcome.summary.contains("未启用") || outcome.summary.contains("失败"),
        "无 handle 时应降级提示，实际: {}",
        outcome.summary
    );
}

#[test]
fn handle_recall_memory_with_connection_uses_sidecar() {
    let sidecar = Arc::new(MockMemorySidecar::default());
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader =
        WasmPluginLoader::with_sidecar(&config, Some(sidecar.clone())).expect("创建加载器失败");
    let mut plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");

    let outcome = plugin
        .handle_tool(
            ToolCall {
                id: "c2".into(),
                name: "recall_memory".into(),
                arguments: r#"{"query":"测试查询"}"#.into(),
            },
            &config,
        )
        .expect("handle-tool 失败");
    assert!(outcome.ok);
    assert_eq!(outcome.summary, "命中 1 条相关记忆并完成整理");
    let execution = outcome.execution.expect("应保留工具执行记录");
    assert_eq!(execution.tool_name, "recall_memory");
    assert_eq!(execution.args, vec!["测试查询"]);
    // 有 handle 时应正常返回（不再含"未启用"降级提示）。
    assert!(
        !outcome.summary.contains("未启用"),
        "有 sidecar 时不应降级，实际: {}",
        outcome.summary
    );
    assert!(sidecar.called("recall_context"));
}

#[test]
fn chat_config_does_not_reload_memory_service() {
    let sidecar = Arc::new(MockMemorySidecar::default());
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader =
        WasmPluginLoader::with_sidecar(&config, Some(sidecar.clone())).expect("创建加载器失败");
    let mut plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");

    let core_config = CoreConfig::default();
    let config_json = serde_json::to_string(&core_config).expect("序列化失败");
    assert!(plugin.on_config_updated(config_json).is_ok());
    assert!(!sidecar.called("reconfigure"));
}

#[test]
fn adapter_integrates_with_core_plugin_trait() {
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader = WasmPluginLoader::new(&config).expect("创建加载器失败");
    let plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");
    let adapter = WasmPluginAdapter::new(plugin, config, "test-session");

    assert_eq!(adapter.id(), "memory");
    let specs = <WasmPluginAdapter as ToolSpecProvider>::tool_specs(&adapter);
    assert!(specs.iter().any(|s| s.name == "recall_memory"));

    // config 事件经 trait 调用不 panic。
    <WasmPluginAdapter as Plugin>::on_config_updated(&adapter, &CoreConfig::default());
}

// ── 生命周期钩子 + session 注入测试 ──

/// 构造 `on_turn_finished` 的本轮快照（同 Core：从 `idx` 起为本轮消息）。
fn turn_at(session: &Session, idx: usize) -> tiangong_types::PluginSession {
    let workspace_id = session
        .cwd
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    tiangong_types::PluginSession {
        id: session.id.clone(),
        title: session.title.clone(),
        cwd: session.cwd.clone(),
        workspace_id,
        reasoning_effort: None,
        messages: session.messages[idx..].to_vec(),
        context_summary: session.context_summary.clone(),
        created_at: session.created_at.clone(),
        updated_at: session.updated_at.clone(),
    }
}
/// 构造一个测试 Session。
fn test_session() -> Session {
    let mut session = Session::new("test-session");
    session.cwd = "/tmp/test-workspace".to_string();
    session
}

/// 构造一个有用户消息的测试 Session（供 turn_finished 测试用）。
fn test_session_with_user_message() -> Session {
    use tiangong_types::{Message, MessageRole};
    let mut session = Session::new("test-session");
    session.cwd = "/tmp/test-workspace".to_string();
    let mut message = Message::new(MessageRole::User, "测试用户输入");
    message.id = "msg-1".to_string();
    message.created_at = "2026-08-01T00:00:00".to_string();
    session.messages.push(message);
    session
}

#[test]
fn lifecycle_hooks_forward_session_without_panic() {
    // 经 Plugin trait 调用全部生命周期钩子，验证 session 序列化传入不 panic。
    // 无 sidecar 时各钩子内部 best-effort 忽略 request 错误，仍正常返回。
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader = WasmPluginLoader::new(&config).expect("创建加载器失败");
    let plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");
    let adapter = WasmPluginAdapter::new(plugin, config, "test-session");

    let session = test_session_with_user_message();
    // 全部钩子调用不应 panic。
    <WasmPluginAdapter as Plugin>::on_session_ready(&adapter);
    <WasmPluginAdapter as Plugin>::on_turn_started(&adapter);
    <WasmPluginAdapter as Plugin>::on_turn_finished(&adapter, &turn_at(&session, 0));
    <WasmPluginAdapter as Plugin>::on_session_ended(&adapter);
}

#[test]
fn on_turn_finished_with_connection_forwards_rumination() {
    let sidecar = Arc::new(MockMemorySidecar::default());
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader =
        WasmPluginLoader::with_sidecar(&config, Some(sidecar.clone())).expect("创建加载器失败");
    let plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");
    let adapter = WasmPluginAdapter::new(plugin, config, "test-session");

    let session = test_session_with_user_message();
    // 有用户消息时，on_turn_finished 应有序投递反刍任务并等待 sidecar 入队确认。
    <WasmPluginAdapter as Plugin>::on_turn_finished(&adapter, &turn_at(&session, 0));
    assert!(sidecar.wait_for_call_count("run_enhanced_micro_rumination", 1));
    assert!(sidecar.called("run_enhanced_micro_rumination"));
    let payload = sidecar.payload("run_enhanced_micro_rumination");
    let turn = &payload["turn_result"];
    assert_eq!(turn["session_id"], session.id);
    assert_eq!(turn["workspace_id"], "test-workspace");
    assert_eq!(turn["user_input"], "测试用户输入");
    assert_eq!(turn["turn_messages"][0]["role"], "user");
    assert_eq!(turn["turn_id"].as_str().map(str::len), Some(25));
}

#[test]
fn turn_finish_waits_only_for_sidecar_enqueue_and_releases_wasm_lock() {
    let sidecar = Arc::new(BlockingMemorySidecar::default());
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader =
        WasmPluginLoader::with_sidecar(&config, Some(sidecar.clone())).expect("创建加载器失败");
    let plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");
    let adapter = Arc::new(WasmPluginAdapter::new(plugin, config, "test-session"));
    let session = test_session_with_user_message();
    let (finish_tx, finish_rx) = std::sync::mpsc::sync_channel(1);
    let finish_adapter = Arc::clone(&adapter);
    let finish_thread = std::thread::spawn(move || {
        let session = session;
        <WasmPluginAdapter as Plugin>::on_turn_finished(&finish_adapter, &turn_at(&session, 0));
        let _ = finish_tx.send(());
    });
    assert!(sidecar.wait_for_call("run_enhanced_micro_rumination", Duration::from_secs(1)));
    assert!(
        finish_rx.recv_timeout(Duration::from_millis(250)).is_err(),
        "sidecar 尚未确认入队时，收尾调用不应虚报完成"
    );
    sidecar.release_rumination();
    finish_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("sidecar 确认入队后收尾调用应尽快返回");
    finish_thread.join().expect("收尾线程不应异常退出");
    let started = Instant::now();
    <WasmPluginAdapter as Plugin>::on_config_updated(&adapter, &CoreConfig::default());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "收尾入队完成后不应继续占用 WASM 实例锁"
    );
    assert!(!sidecar.auto_released(), "测试不应依赖自动释放上限");
}
#[test]
fn session_hooks_use_session_identity_from_construction() {
    // 会话级钩子不携带会话数据：适配器以构造时获得的会话身份与已注入的
    // 工作目录组装 WIT 所需的会话 JSON。
    let sidecar = Arc::new(MockMemorySidecar::default());
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader =
        WasmPluginLoader::with_sidecar(&config, Some(sidecar.clone())).expect("创建加载器失败");
    let plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");
    let adapter = WasmPluginAdapter::new(plugin, config, "owner-session");

    <WasmPluginAdapter as Plugin>::set_workspace(
        &adapter,
        Some(std::path::Path::new("/tmp/owner-ws")),
    );
    <WasmPluginAdapter as Plugin>::on_session_ended(&adapter);
    assert!(sidecar.wait_for_call_count("run_meso_rumination", 1));
    let payload = sidecar.payload("run_meso_rumination");
    assert_eq!(payload["session_id"], "owner-session");
    assert_eq!(payload["workspace_id"], "owner-ws");
}

#[test]
fn every_tenth_turn_forwards_meta_rumination() {
    let sidecar = Arc::new(MockMemorySidecar::default());
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader =
        WasmPluginLoader::with_sidecar(&config, Some(sidecar.clone())).expect("创建加载器失败");
    let plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");
    let adapter = WasmPluginAdapter::new(plugin, config, "test-session");
    let session = test_session_with_user_message();

    for _ in 0..9 {
        <WasmPluginAdapter as Plugin>::on_turn_finished(&adapter, &turn_at(&session, 0));
    }
    assert!(sidecar.wait_for_call_count("run_enhanced_micro_rumination", 9));
    assert!(!sidecar.called("run_meta_rumination"));
    <WasmPluginAdapter as Plugin>::on_turn_finished(&adapter, &turn_at(&session, 0));
    assert!(sidecar.wait_for_call_count("run_meta_rumination", 1));
    assert!(sidecar.called("run_meta_rumination"));
}

// ── set_workspace + prompt_sections 测试 ──

#[test]
fn prompt_sections_without_handle_returns_empty() {
    // 无 handle 时，prompt_sections 应返回空（不注入），不报错。
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader = WasmPluginLoader::new(&config).expect("创建加载器失败");
    let plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");
    let adapter = WasmPluginAdapter::new(plugin, config, "test-session");

    let sections = <WasmPluginAdapter as PromptSectionProvider>::prompt_sections(&adapter);
    assert!(sections.is_empty(), "无 handle 时应返回空注入");
}

#[test]
fn new_turns_do_not_load_memory_into_system_prompt() {
    let sidecar = Arc::new(MockMemorySidecar::default());
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader =
        WasmPluginLoader::with_sidecar(&config, Some(sidecar.clone())).expect("创建加载器失败");
    let plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");
    let adapter = WasmPluginAdapter::new(plugin, config, "test-session");

    // 注入 workspace 和 session。
    <WasmPluginAdapter as Plugin>::set_workspace(
        &adapter,
        Some(std::path::Path::new("/tmp/test-ws")),
    );
    let mut session = test_session();
    <WasmPluginAdapter as Plugin>::on_session_ready(&adapter);

    session.system_prompt_message = Some(tiangong_types::Message::new(
        tiangong_types::MessageRole::System,
        "旧版自动注入的 Session Memory 和更新时间",
    ));
    let mut previous_prompt = None;
    for turn in 0..3 {
        <WasmPluginAdapter as Plugin>::on_config_updated(&adapter, &CoreConfig::default());
        <WasmPluginAdapter as Plugin>::set_execution_context(
            &adapter,
            Some(std::path::Path::new("/tmp/test-ws")),
            tiangong_core::permission::TrustMode::FullTrust,
        );
        session.append_message(tiangong_types::MessageRole::User, format!("继续 {turn}"));
        <WasmPluginAdapter as Plugin>::on_turn_started(&adapter);
        let sections = <WasmPluginAdapter as PromptSectionProvider>::prompt_sections(&adapter);
        assert!(
            sections.is_empty(),
            "记忆只能经 handler 返回，不能拼入系统提示"
        );
        session.rebuild_system_prompt(
            &tiangong_core::prompt::SystemPromptConfig::from_plugin_sections(sections),
        );
        let prompt = session.system_prompt_message.as_ref().unwrap();
        assert!(!prompt.text_content().contains("Session Memory"));
        let serialized_prompt = serde_json::to_vec(prompt).unwrap();
        if let Some(previous) = &previous_prompt {
            assert_eq!(
                previous, &serialized_prompt,
                "重新加载和追加消息不应重建系统提示"
            );
        }
        previous_prompt = Some(serialized_prompt);
        assert!(!sidecar.called("load_injection"));
        assert!(!sidecar.called("recall_context"));
        assert!(
            !sidecar.called("reconfigure"),
            "发送准备不得唤醒 Memory 重载独立配置"
        );
        session = serde_json::from_slice(&serde_json::to_vec(&session).unwrap()).unwrap();
    }
}

#[test]
fn file_plugin_preparation_never_starts_a_sidecar() {
    let wasm = memory_wasm_path().with_file_name("tiangong_plugin_fs_wasm.wasm");
    if !wasm.is_file() {
        eprintln!("跳过测试：请先构建 tiangong-plugin-fs-wasm 的 wasm32-wasip2 制品");
        return;
    }
    let config = PluginRuntimeConfig::default();
    let sidecar = Arc::new(MockMemorySidecar::default());
    let loader = WasmPluginLoader::with_sidecar(&config, Some(sidecar.clone())).unwrap();
    // 每个新对话仍有独立 WASM 状态；首次及后续准备均不得启动进程。
    for conversation in 0..3 {
        let plugin = loader.load(&wasm, &config).unwrap();
        let adapter = WasmPluginAdapter::new(plugin, config.clone(), "test-session");
        for turn in 0..3 {
            let started = Instant::now();
            adapter.on_config_updated(&CoreConfig::default());
            adapter.set_execution_context(
                Some(std::path::Path::new("/tmp/send-workspace")),
                tiangong_core::permission::TrustMode::FullTrust,
            );
            assert!(!adapter.tool_specs().is_empty());
            let _ = adapter.prompt_sections();
            eprintln!(
                "fs preparation conversation={conversation} turn={turn} elapsed_us={}",
                started.elapsed().as_micros()
            );
        }
        adapter.set_execution_context(None, tiangong_core::permission::TrustMode::Supervised);
    }
    assert!(
        sidecar.calls.lock().unwrap().is_empty(),
        "仅设置目录、信任模式或读取声明不应调用 sidecar"
    );
}

#[test]
fn turn_hooks_receive_only_current_turn_messages() {
    // 长会话：轮次钩子只把本轮消息交给插件，跨轮历史不得进入快照。
    use tiangong_types::{Message, MessageRole};
    let sidecar = Arc::new(MockMemorySidecar::default());
    let Some(wasm) = wasm_or_skip() else {
        return;
    };
    let config = PluginRuntimeConfig::default();
    let loader =
        WasmPluginLoader::with_sidecar(&config, Some(sidecar.clone())).expect("创建加载器失败");
    let plugin = loader.load(&wasm, &config).expect("加载 wasm 组件失败");
    let adapter = WasmPluginAdapter::new(plugin, config, "test-session");
    let mut session = test_session();
    for turn in 0..200 {
        session
            .messages
            .push(Message::new(MessageRole::User, format!("历史问题 {turn}")));
        session.messages.push(Message::new(
            MessageRole::Assistant,
            format!("历史回答 {turn}"),
        ));
    }
    let start = session.messages.len();
    session
        .messages
        .push(Message::new(MessageRole::User, "本轮问题"));
    session
        .messages
        .push(Message::new(MessageRole::Notice, "本轮系统通知"));
    session
        .messages
        .push(Message::new(MessageRole::User, "运行中引导"));
    session
        .messages
        .push(Message::new(MessageRole::Assistant, "本轮回答"));

    <WasmPluginAdapter as Plugin>::on_turn_finished(&adapter, &turn_at(&session, start));
    assert!(sidecar.wait_for_call_count("run_enhanced_micro_rumination", 1));
    let turn = sidecar.payload("run_enhanced_micro_rumination")["turn_result"].clone();
    assert_eq!(turn["user_input"], "本轮问题");
    let contents: Vec<String> = turn["turn_messages"]
        .as_array()
        .expect("turn_messages 应为数组")
        .iter()
        .map(|message| message["content"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(contents, vec!["本轮问题", "运行中引导", "本轮回答"]);
}
