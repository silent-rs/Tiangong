//! 工具调用无插件 UI 接应时的通用拉起机制端到端验证：
//! 无订阅者 → 经 app.open 原语请求打开插件 App → 订阅建立 →
//! 挂起调用重放 → tool.resolve 闭合；已有订阅者时不请求拉起。

use std::sync::{Arc, Mutex, Once};

use tiangong_core::core::Plugin;
use tiangong_core::session::Session;
use tiangong_llm::tool::ToolCall;
use tiangong_plugin_runtime::bridge_call;
use tiangong_plugin_runtime::registry::{
    RuntimeKind, load_installed_plugins, preload_installed_plugins,
};
use tiangong_plugin_runtime::{
    bridge_subscribe, bridge_unsubscribe, set_app_handler, set_event_emitter,
};

static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// app 处理器与事件推送均为进程级 OnceLock：统一初始化一次，用共享
/// 记录器断言各场景的触发情况。
static INIT: Once = Once::new();
static APP_HITS: Mutex<Vec<(String, String, String)>> = Mutex::new(Vec::new());
static EVENT_HITS: Mutex<Vec<(String, String, String)>> = Mutex::new(Vec::new());

fn init_globals() {
    INIT.call_once(|| {
        // 模拟宿主注入的 app.* 处理器：记录 (plugin_id, method, session_id)，
        // 未知方法返回错误（与桌面宿主一致）。
        set_app_handler(Arc::new(
            |plugin_id: &str, method: &str, payload: &str| -> anyhow::Result<String> {
                if method != "app.open" && method != "app.close" {
                    anyhow::bail!("app 原语不支持方法 {method}");
                }
                let value = serde_json::from_str::<serde_json::Value>(payload).ok();
                let session_id = value
                    .as_ref()
                    .and_then(|item| item["session_id"].as_str().map(str::to_string))
                    .unwrap_or_default();
                let mode = value
                    .as_ref()
                    .and_then(|item| item["mode"].as_str().map(str::to_string))
                    .unwrap_or_default();
                APP_HITS.lock().unwrap().push((
                    plugin_id.to_string(),
                    format!("{method}#{mode}"),
                    session_id,
                ));
                Ok(r#"{"ok":true}"#.to_string())
            },
        ));
        set_event_emitter(Arc::new(|plugin_id: &str, channel: &str, payload: &str| {
            EVENT_HITS.lock().unwrap().push((
                plugin_id.to_string(),
                channel.to_string(),
                payload.to_string(),
            ));
        }));
    });
}

fn app_hits() -> Vec<(String, String, String)> {
    APP_HITS.lock().unwrap().clone()
}

fn event_hits() -> Vec<(String, String, String)> {
    EVENT_HITS.lock().unwrap().clone()
}

/// 写一个声明 TS 工具与 tool.* 事件能力的桌面插件。
fn stage_tool_plugin(root: &std::path::Path, id: &str) {
    stage_tool_plugin_with_permissions(root, id, &["tool.provide", "bridge.call", "app.use"]);
}

fn stage_tool_plugin_with_permissions(root: &std::path::Path, id: &str, permissions: &[&str]) {
    let dir = root.join("plugins").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let permissions = permissions
        .iter()
        .map(|permission| format!("\"{permission}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        dir.join("plugin.json"),
        format!(
            r#"{{
                "schema_version": 2,
                "id": "{id}",
                "version": "0.1.0",
                "entrypoints": ["desktop"],
                "permissions": [{permissions}],
                "capabilities": {{
                    "tools": true,
                    "events": ["tool.*"]
                }},
                "tools": [{{
                    "name": "demo_tool",
                    "description": "演示工具",
                    "input_schema": {{"type": "object"}},
                    "timeout_ms": 3000
                }}],
                "ui": {{
                    "contributions": [{{
                        "slot": "extension.tab",
                        "id": "panel",
                        "title": "面板",
                        "entry": "index.html"
                    }}]
                }}
            }}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("index.html"), "<html></html>").unwrap();
}

fn load_plugin(root: &std::path::Path, id: &str) -> Arc<dyn Plugin> {
    tiangong_config::registry::init_from_dir(root);
    preload_installed_plugins(root);
    load_installed_plugins(root, RuntimeKind::Desktop, "tool-ui-session")
        .into_iter()
        .find(|plugin| plugin.id() == id)
        .expect("工具插件应注册")
}

#[test]
fn 无订阅者时请求拉起且订阅后重放并闭合() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    init_globals();
    let root = tempfile::TempDir::new().unwrap();
    stage_tool_plugin(root.path(), "tool-ui-demo-a");
    let plugin = load_plugin(root.path(), "tool-ui-demo-a");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = runtime.block_on(async {
        let mut session = Session::new("测试会话");
        let session_id = session.id.clone();
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "demo_tool".to_string(),
            arguments: serde_json::json!({}),
        };
        let task = tokio::spawn({
            let plugin = plugin.clone();
            let call = call.clone();
            async move { plugin.handle(&call, &mut session, "tester").await }
        });

        // 等待调用登记 pending 并触发拉起请求（无订阅者）。
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            app_hits().iter().any(
                |(plugin_id, method, session_arg)| plugin_id == "tool-ui-demo-a"
                    && method == "app.open#background"
                    && session_arg == &session_id
            ),
            "无订阅者时应经 app.open 请求打开插件 App"
        );

        // 模拟插件 UI 挂载完成：订阅触发挂起调用重放。
        bridge_subscribe("tool-ui-demo-a", "tool.requested").unwrap();
        let replayed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let hits = event_hits();
                if let Some((_, _, payload)) = hits.iter().rev().find(|(plugin_id, channel, _)| {
                    plugin_id == "tool-ui-demo-a" && channel == "tool.requested"
                }) {
                    return payload.clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("订阅建立后应重放挂起的工具调用");
        let invocation: serde_json::Value = serde_json::from_str(&replayed).unwrap();
        let invocation_id = invocation["invocation_id"].as_str().unwrap().to_string();

        // 模拟插件 shell 提交结果，闭合调用。
        bridge_call(
            "tool-ui-demo-a",
            "tool.resolve",
            &serde_json::json!({
                "invocation_id": invocation_id,
                "status": "answered",
                "result": {
                    "ok": true,
                    "summary": "done",
                    "exit_code": 0
                }
            })
            .to_string(),
        )
        .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .expect("工具调用应被插件接走执行")
    });
    assert!(result.ok, "拉起→订阅→重放→闭合全链路应成功");
    assert_eq!(result.summary, "done");

    bridge_unsubscribe("tool-ui-demo-a", "tool.requested").unwrap();
}

#[test]
fn 已有订阅者时仍按会话请求拉起() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    init_globals();
    let root = tempfile::TempDir::new().unwrap();
    stage_tool_plugin(root.path(), "tool-ui-demo-b");
    let plugin = load_plugin(root.path(), "tool-ui-demo-b");

    let before = app_hits().len();
    bridge_subscribe("tool-ui-demo-b", "tool.requested").unwrap();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = runtime.block_on(async {
        let mut session = Session::new("测试会话");
        let session_id = session.id.clone();
        let call = ToolCall {
            id: "call-2".to_string(),
            name: "demo_tool".to_string(),
            arguments: serde_json::json!({}),
        };
        let task = tokio::spawn({
            let plugin = plugin.clone();
            let call = call.clone();
            async move { plugin.handle(&call, &mut session, "tester").await }
        });

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // 工具订阅是插件级全局信号，但处理实例按会话过滤：全局已有
        // 订阅者不代表本会话已有匹配实例，仍须按「插件 + 会话」请求
        // 执行壳（前端对同会话实例去重）。
        assert!(
            app_hits()[before..]
                .iter()
                .any(|(plugin_id, method, session_arg)| {
                    plugin_id == "tool-ui-demo-b"
                        && method == "app.open#background"
                        && session_arg == &session_id
                }),
            "已有订阅者时仍应按会话经 app.open 请求执行壳"
        );

        // 正常投递路径同样可用：直接消费重放/新事件并闭合。
        let replayed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let hits = event_hits();
                if let Some((_, _, payload)) = hits.iter().rev().find(|(plugin_id, channel, _)| {
                    plugin_id == "tool-ui-demo-b" && channel == "tool.requested"
                }) {
                    return payload.clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("已有订阅者时应正常投递工具调用");
        let invocation: serde_json::Value = serde_json::from_str(&replayed).unwrap();
        let invocation_id = invocation["invocation_id"].as_str().unwrap().to_string();
        bridge_call(
            "tool-ui-demo-b",
            "tool.resolve",
            &serde_json::json!({
                "invocation_id": invocation_id,
                "status": "answered",
                "result": {
                    "ok": true,
                    "summary": "ok",
                    "exit_code": 0
                }
            })
            .to_string(),
        )
        .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .expect("工具调用应被插件接走执行")
    });
    assert!(result.ok);

    bridge_unsubscribe("tool-ui-demo-b", "tool.requested").unwrap();
}

#[test]
fn ui_handler超过清单时限仍可正常返回() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    init_globals();
    let root = tempfile::TempDir::new().unwrap();
    stage_tool_plugin(root.path(), "tool-ui-no-timeout");
    let plugin = load_plugin(root.path(), "tool-ui-no-timeout");
    bridge_subscribe("tool-ui-no-timeout", "tool.requested").unwrap();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = runtime.block_on(async {
        let mut session = Session::new("无时限测试");
        let call = ToolCall {
            id: "call-no-timeout".to_string(),
            name: "demo_tool".to_string(),
            arguments: serde_json::json!({}),
        };
        let task = tokio::spawn({
            let plugin = plugin.clone();
            async move { plugin.handle(&call, &mut session, "tester").await }
        });
        let invocation_id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some((_, _, payload)) = event_hits().iter().rev().find(|(id, channel, _)| {
                    id == "tool-ui-no-timeout" && channel == "tool.requested"
                }) {
                    let value: serde_json::Value = serde_json::from_str(payload).unwrap();
                    break value["invocation_id"].as_str().unwrap().to_string();
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("工具请求应送达");

        // 清单 timeout_ms=3000；等待超过该值后再返回，调用仍应保持有效。
        tokio::time::sleep(std::time::Duration::from_millis(3200)).await;
        bridge_call(
            "tool-ui-no-timeout",
            "tool.resolve",
            &serde_json::json!({
                "invocation_id": invocation_id,
                "status": "answered",
                "result": {"ok": true, "summary": "late but valid", "exit_code": 0}
            })
            .to_string(),
        )
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("测试保护时限内应结束")
            .unwrap()
            .expect("UI Handler 应返回结果")
    });
    assert!(result.ok);
    assert_eq!(result.summary, "late but valid");
    bridge_unsubscribe("tool-ui-no-timeout", "tool.requested").unwrap();
}

#[test]
fn 调用闭合后重放不得补发requested() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    init_globals();
    let root = tempfile::TempDir::new().unwrap();
    stage_tool_plugin(root.path(), "tool-ui-replay-order");
    let plugin = load_plugin(root.path(), "tool-ui-replay-order");
    bridge_subscribe("tool-ui-replay-order", "tool.requested").unwrap();
    bridge_subscribe("tool-ui-replay-order", "tool.closed").unwrap();

    let count_requested = || {
        event_hits()
            .iter()
            .filter(|(id, channel, _)| id == "tool-ui-replay-order" && channel == "tool.requested")
            .count()
    };

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let mut session = Session::new("重放顺序测试");
        let call = ToolCall {
            id: "call-replay-order".to_string(),
            name: "demo_tool".to_string(),
            arguments: serde_json::json!({}),
        };
        let task = tokio::spawn({
            let plugin = plugin.clone();
            async move { plugin.handle(&call, &mut session, "tester").await }
        });
        let invocation_id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some((_, _, payload)) = event_hits().iter().rev().find(|(id, channel, _)| {
                    id == "tool-ui-replay-order" && channel == "tool.requested"
                }) {
                    let value: serde_json::Value = serde_json::from_str(payload).unwrap();
                    return value["invocation_id"].as_str().unwrap().to_string();
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("工具请求应送达");
        assert_eq!(count_requested(), 1, "首次投递只应有一次 requested");

        // 闭合调用并等待 closed 事件与调用任务收尾。
        bridge_call(
            "tool-ui-replay-order",
            "tool.resolve",
            &serde_json::json!({
                "invocation_id": invocation_id,
                "status": "answered",
                "result": {"ok": true, "summary": "done", "exit_code": 0}
            })
            .to_string(),
        )
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .expect("工具调用应正常闭合");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let closed = event_hits().iter().any(|(id, channel, payload)| {
                    id == "tool-ui-replay-order"
                        && channel == "tool.closed"
                        && serde_json::from_str::<serde_json::Value>(payload)
                            .ok()
                            .and_then(|value| value["invocation_id"].as_str().map(str::to_string))
                            == Some(invocation_id.clone())
                });
                if closed {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("闭合事件应送达");

        // 模拟新标签页挂载：退订后重新订阅触发 pending 重放。调用已闭合，
        // 不得补发该调用的 tool.requested（closed 之后永不重放）。
        bridge_unsubscribe("tool-ui-replay-order", "tool.requested").unwrap();
        bridge_subscribe("tool-ui-replay-order", "tool.requested").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            count_requested(),
            1,
            "调用闭合后的重放不得补发 tool.requested"
        );
    });

    bridge_unsubscribe("tool-ui-replay-order", "tool.requested").unwrap();
    bridge_unsubscribe("tool-ui-replay-order", "tool.closed").unwrap();
}

#[test]
fn 插件经桥接调用app原语需声明权限且只能操作自己() {
    let _guard = REGISTRY_LOCK.lock().unwrap();
    init_globals();
    let root = tempfile::TempDir::new().unwrap();
    stage_tool_plugin(root.path(), "tool-ui-demo-c");
    stage_tool_plugin_with_permissions(
        root.path(),
        "tool-ui-demo-no-perm",
        &["tool.provide", "bridge.call"],
    );
    load_plugin(root.path(), "tool-ui-demo-c");
    load_plugin(root.path(), "tool-ui-demo-no-perm");

    // 声明 app.use 权限的插件可以打开/关闭自己的 App（关闭需显式指定
    // instance_id 或 all，宿主侧校验）。
    let before = app_hits().len();
    bridge_call("tool-ui-demo-c", "app.open", r#"{"session_id":"sess-1"}"#).unwrap();
    bridge_call(
        "tool-ui-demo-c",
        "app.close",
        r#"{"session_id":"sess-1","all":true}"#,
    )
    .unwrap();
    let hits = app_hits();
    assert_eq!(
        hits.len(),
        before + 2,
        "app.open / app.close 均应到达宿主处理器"
    );
    assert_eq!(
        hits[before],
        (
            "tool-ui-demo-c".to_string(),
            "app.open#".to_string(),
            "sess-1".to_string()
        )
    );
    assert_eq!(
        hits[before + 1],
        (
            "tool-ui-demo-c".to_string(),
            "app.close#".to_string(),
            "sess-1".to_string()
        )
    );

    // 未声明权限的插件被拒绝，处理器不收到调用。
    let before = app_hits().len();
    let rejected = bridge_call(
        "tool-ui-demo-no-perm",
        "app.open",
        r#"{"session_id":"sess-1"}"#,
    );
    assert!(rejected.is_err(), "未声明 app.use 权限应被拒绝");
    assert_eq!(app_hits().len(), before, "被拒调用不应到达处理器");

    // 未知方法被拒绝。
    assert!(bridge_call("tool-ui-demo-c", "app.destroy", "{}").is_err());
}
