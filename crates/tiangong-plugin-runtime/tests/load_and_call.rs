//! WASM 插件运行时集成测试。
//!
//! 使用仓库 `tests/fixtures/plugins/runtime-probe/` 中随仓库提交的专用测试插件制品，
//! 只验证运行时职责（加载、调用、错误语义、fuel、生命周期转发、锁释放），
//! 不涉及任何业务插件；制品缺失或摘要不符时测试直接失败，不会静默跳过。

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tiangong_core::config::core::CoreConfig;
use tiangong_core::core::Plugin;
use tiangong_core::session::Session;
use tiangong_core::tools::extension::{PromptSectionProvider, ToolSpecProvider};
use tiangong_plugin_runtime::loader::instantiate_component;
use tiangong_plugin_runtime::test_support::{runtime_probe_component, runtime_probe_plugin};
use tiangong_plugin_runtime::{
    PluginRuntimeConfig, SidecarConnection, ToolCall, WasmPluginAdapter,
};

/// 记录全部 sidecar 调用；`block_operation` 命中时阻塞到测试显式放行（最长 10 秒）。
#[derive(Default)]
struct RecordingSidecarState {
    calls: Vec<(String, String)>,
    released: bool,
    auto_released: bool,
}

#[derive(Default)]
struct RecordingSidecar {
    state: Mutex<RecordingSidecarState>,
    changed: Condvar,
    block_operation: Option<&'static str>,
}

impl RecordingSidecar {
    fn blocking(operation: &'static str) -> Self {
        Self {
            block_operation: Some(operation),
            ..Self::default()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RecordingSidecarState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn operations(&self) -> Vec<String> {
        self.lock().calls.iter().map(|(op, _)| op.clone()).collect()
    }

    fn payload(&self, operation: &str) -> serde_json::Value {
        self.lock()
            .calls
            .iter()
            .rev()
            .find(|(op, _)| op == operation)
            .and_then(|(_, payload)| serde_json::from_str(payload).ok())
            .unwrap_or(serde_json::Value::Null)
    }

    fn wait_for_call(&self, operation: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            if state.calls.iter().any(|(op, _)| op == operation) {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            state = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    fn release(&self) {
        self.lock().released = true;
        self.changed.notify_all();
    }
}

impl SidecarConnection for RecordingSidecar {
    fn invoke(&self, operation: &str, payload: &str) -> anyhow::Result<String> {
        let mut state = self.lock();
        state
            .calls
            .push((operation.to_string(), payload.to_string()));
        self.changed.notify_all();
        if self.block_operation == Some(operation) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !state.released {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    state.auto_released = true;
                    break;
                };
                state = self
                    .changed
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(|p| p.into_inner())
                    .0;
            }
        }
        Ok(format!("ack:{operation}"))
    }
}

fn adapter(sidecar: Option<Arc<dyn SidecarConnection>>, session_id: &str) -> WasmPluginAdapter {
    WasmPluginAdapter::new(
        runtime_probe_plugin(sidecar),
        PluginRuntimeConfig::default(),
        session_id,
    )
}

fn llm_call(name: &str, arguments: serde_json::Value) -> tiangong_llm::tool::ToolCall {
    tiangong_llm::tool::ToolCall {
        id: format!("call-{name}"),
        name: name.into(),
        arguments,
    }
}

fn snapshot(session: &Session) -> tiangong_types::PluginSession {
    tiangong_types::PluginSession {
        id: session.id.clone(),
        title: session.title.clone(),
        cwd: session.cwd.clone(),
        workspace_id: "test-workspace".into(),
        reasoning_effort: None,
        messages: session.messages.clone(),
        context_summary: session.context_summary.clone(),
        created_at: session.created_at.clone(),
        updated_at: session.updated_at.clone(),
    }
}

fn session_with_user_message() -> Session {
    let mut session = Session::new("test-session");
    session.cwd = "/tmp/test-workspace".to_string();
    session.messages.push(tiangong_types::Message::new(
        tiangong_types::MessageRole::User,
        "本轮问题",
    ));
    session
}

#[test]
fn load_and_describe() {
    let mut plugin = runtime_probe_plugin(None);
    let desc = plugin.describe().expect("describe 失败");
    assert_eq!(desc.id, "runtime-probe");
    assert_eq!(desc.name, "Runtime Probe");
    assert_eq!(desc.version, "1.0.0");
}

#[test]
fn adapter_exposes_tool_specs_and_prompt_sections() {
    let adapter = adapter(None, "test-session");
    assert_eq!(adapter.id(), "runtime-probe");
    let specs = <WasmPluginAdapter as ToolSpecProvider>::tool_specs(&adapter);
    let names: Vec<_> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert_eq!(names, ["echo", "fail", "spin", "sidecar"]);
    let sections = <WasmPluginAdapter as PromptSectionProvider>::prompt_sections(&adapter);
    assert_eq!(sections, vec!["runtime-probe prompt".to_string()]);
    // 配置事件经 trait 调用不 panic。
    <WasmPluginAdapter as Plugin>::on_config_updated(&adapter, &CoreConfig::default());
}

#[test]
fn plugin_error_is_not_reported_as_unregistered_tool() {
    let adapter = adapter(None, "test-session");
    let runtime = tokio::runtime::Runtime::new().expect("创建 runtime 失败");
    let mut session = Session::new("test-session");
    let result = runtime
        .block_on(
            tiangong_core::tools::extension::ToolOverrideHandler::handle(
                &adapter,
                &llm_call("fail", serde_json::json!({})),
                &mut session,
                "test",
            ),
        )
        .expect("插件执行错误不得变成 None");
    assert!(!result.ok);
    assert!(result.stderr.contains("probe failure"), "{}", result.stderr);
    assert!(!result.summary.contains("未注册"), "{}", result.summary);
}

#[test]
fn fuel_limit_traps_runaway_execution() {
    let strict = PluginRuntimeConfig {
        fuel_limit: 1_000_000,
        epoch_deadline: Duration::from_secs(10),
        ..PluginRuntimeConfig::default()
    };
    let mut plugin = runtime_probe_plugin(None);
    let started = Instant::now();
    let result = plugin.handle_tool(
        ToolCall {
            id: "spin".into(),
            name: "spin".into(),
            arguments: "{}".into(),
        },
        &strict,
    );
    assert!(result.is_err(), "死循环应被 fuel 上限中断");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn tool_forwards_to_sidecar_and_degrades_without_it() {
    let config = PluginRuntimeConfig::default();
    let call = || ToolCall {
        id: "sidecar".into(),
        name: "sidecar".into(),
        arguments: r#"{"operation":"ping","payload":"hi"}"#.into(),
    };

    let sidecar = Arc::new(RecordingSidecar::default());
    let mut plugin = runtime_probe_plugin(Some(sidecar.clone()));
    let outcome = plugin.handle_tool(call(), &config).expect("调用失败");
    assert!(outcome.ok);
    assert_eq!(outcome.stdout, "ack:ping");
    assert_eq!(sidecar.lock().calls, vec![("ping".into(), "hi".into())]);

    let mut plugin = runtime_probe_plugin(None);
    let outcome = plugin.handle_tool(call(), &config).expect("调用失败");
    assert!(!outcome.ok, "未配置 sidecar 时应显式降级");
}

#[test]
fn lifecycle_hooks_forward_session_snapshot() {
    let sidecar = Arc::new(RecordingSidecar::default());
    let adapter = adapter(Some(sidecar.clone()), "test-session");
    let session = session_with_user_message();
    let expected_id = session.id.clone();

    <WasmPluginAdapter as Plugin>::on_session_ready(&adapter);
    <WasmPluginAdapter as Plugin>::on_turn_started(&adapter);
    <WasmPluginAdapter as Plugin>::on_turn_finished(&adapter, &snapshot(&session));
    <WasmPluginAdapter as Plugin>::on_session_ended(&adapter);

    assert_eq!(
        sidecar.operations(),
        [
            "hook.session_ready",
            "hook.turn_started",
            "hook.turn_finished",
            "hook.session_ended"
        ]
    );
    let turn = sidecar.payload("hook.turn_finished");
    assert_eq!(turn["id"], expected_id.as_str());
    assert_eq!(turn["messages"][0]["content"][0]["text"], "本轮问题");
}

#[test]
fn session_hooks_use_session_identity_from_construction() {
    // 会话级钩子不携带会话数据：适配器以构造时获得的会话身份与已注入的
    // 工作目录组装 WIT 所需的会话 JSON。
    let sidecar = Arc::new(RecordingSidecar::default());
    let adapter = adapter(Some(sidecar.clone()), "owner-session");
    <WasmPluginAdapter as Plugin>::set_workspace(
        &adapter,
        Some(std::path::Path::new("/tmp/owner-ws")),
    );
    <WasmPluginAdapter as Plugin>::on_session_ended(&adapter);
    let payload = sidecar.payload("hook.session_ended");
    assert_eq!(payload["id"], "owner-session");
    assert_eq!(payload["workspace_id"], "owner-ws");
    assert!(payload["messages"].as_array().is_some_and(Vec::is_empty));
}

#[test]
fn turn_finish_waits_only_for_sidecar_enqueue_and_releases_wasm_lock() {
    let sidecar = Arc::new(RecordingSidecar::blocking("hook.turn_finished"));
    let adapter = Arc::new(adapter(Some(sidecar.clone()), "test-session"));
    let session = session_with_user_message();
    let (finish_tx, finish_rx) = std::sync::mpsc::sync_channel(1);
    let finish_adapter = Arc::clone(&adapter);
    let finish_thread = std::thread::spawn(move || {
        <WasmPluginAdapter as Plugin>::on_turn_finished(&finish_adapter, &snapshot(&session));
        let _ = finish_tx.send(());
    });
    assert!(sidecar.wait_for_call("hook.turn_finished", Duration::from_secs(1)));
    assert!(
        finish_rx.recv_timeout(Duration::from_millis(250)).is_err(),
        "sidecar 尚未确认入队时，收尾调用不应虚报完成"
    );
    sidecar.release();
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
    assert!(!sidecar.lock().auto_released, "测试不应依赖自动释放上限");
}

#[test]
fn send_preparation_never_calls_sidecar() {
    // 每个新对话有独立 WASM 实例；发送前的准备（配置、目录、信任模式、声明读取）
    // 均不得唤起 sidecar。
    let sidecar = Arc::new(RecordingSidecar::default());
    let config = PluginRuntimeConfig::default();
    for _ in 0..3 {
        let plugin = instantiate_component(
            runtime_probe_component(),
            &config,
            Some(sidecar.clone()),
            "runtime-probe",
            false,
        )
        .unwrap();
        let adapter = WasmPluginAdapter::new(plugin, config.clone(), "test-session");
        for _ in 0..3 {
            adapter.on_config_updated(&CoreConfig::default());
            adapter.set_execution_context(
                Some(std::path::Path::new("/tmp/send-workspace")),
                tiangong_core::permission::TrustMode::FullTrust,
            );
            assert!(!adapter.tool_specs().is_empty());
            let _ = adapter.prompt_sections();
        }
        adapter.set_execution_context(None, tiangong_core::permission::TrustMode::Supervised);
    }
    assert!(
        sidecar.operations().is_empty(),
        "仅设置目录、信任模式或读取声明不应调用 sidecar"
    );
}
