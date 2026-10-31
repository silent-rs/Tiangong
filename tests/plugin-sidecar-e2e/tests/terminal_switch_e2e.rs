//! terminal 插件非桌面轮换端到端（issue #697）：经真实宿主 stdio 直连
//! 链路启动 `tiangong-terminal-sidecar`（与 CLI / Server 调用方式一致），
//! 验证 terminal 接替 command 后的命令执行能力——PTY 天然无 UI，无需
//! 任何 headless 适应：
//!
//! - 终端复用：cwd 与 `export` 的环境变量跨调用保留（command 无会话
//!   状态，terminal 复用空闲终端）；
//! - `terminal`（action 模式）开终端、跑 python3 REPL 并读取输出；
//! - `\x03` 中断运行中的命令；
//! - 会话资源释放（listInstances / instanceClosed）回收终端。
//!
//! 依赖已构建的 `tiangong-terminal-sidecar` debug 二进制，未构建时跳过。

use std::path::PathBuf;
use std::time::Duration;

use tiangong_plugin_runtime::manifest::SidecarLifecycle;
use tiangong_plugin_runtime::sidecar::{
    SidecarConfig, SidecarConnection, SidecarInvocationContext, StdioSidecarConnection,
};

fn terminal_sidecar_binary() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let debug_dir = executable.parent()?.parent()?;
    let candidate = debug_dir.join(format!(
        "tiangong-terminal-sidecar{}",
        std::env::consts::EXE_SUFFIX
    ));
    candidate.is_file().then_some(candidate)
}

/// 解析工具调用的 JSON 结果（stdout 字段承载命令输出）。
fn stdout_of(result: &serde_json::Value) -> String {
    result
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[test]
#[serial_test::serial]
fn terminal_非桌面轮换_复用终端_交互与资源释放() {
    let Some(binary) = terminal_sidecar_binary() else {
        eprintln!("跳过：target/debug/tiangong-terminal-sidecar 尚未构建");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let config = SidecarConfig::new(
        "terminal",
        "0.4.1",
        binary,
        root.path().join("endpoint.json"),
        root.path().join("sidecar.log"),
        root.path().join("data"),
        root.path().join("storage"),
    )
    .with_lifecycle(SidecarLifecycle::Resident)
    .with_timeouts(Duration::from_secs(20), Duration::from_secs(60));
    let connection = StdioSidecarConnection::new(config);
    let context = SidecarInvocationContext::new(
        "session-switch-e2e",
        "call-e2e",
        workspace.to_str().unwrap(),
    );
    let no_progress = &mut |_message: String| {};
    let mut call = |operation: &str, payload: serde_json::Value| {
        parse(
            &connection
                .invoke_with_context_and_progress(
                    operation,
                    &payload.to_string(),
                    &context,
                    no_progress,
                )
                .unwrap_or_else(|error| panic!("{operation} 调用失败：{error:#}")),
        )
    };

    // 1. 基本命令执行。
    let echo = call(
        "run_command",
        serde_json::json!({ "cmd": "echo", "args": ["switch-ok"] }),
    );
    assert!(
        stdout_of(&echo).contains("switch-ok"),
        "echo 输出应包含标记：{}",
        stdout_of(&echo)
    );

    // 2. 终端复用：export 的环境变量跨调用保留（同一空闲终端）。
    let export = call(
        "run_shell",
        serde_json::json!({ "script": "export E2E_MARKER=42" }),
    );
    assert!(
        export.get("ok").and_then(serde_json::Value::as_bool) == Some(true),
        "export 应成功：{export}"
    );
    let verify = call(
        "run_shell",
        serde_json::json!({ "script": "echo E2E=$E2E_MARKER" }),
    );
    assert!(
        stdout_of(&verify).contains("E2E=42"),
        "环境变量应跨调用保留：{}",
        stdout_of(&verify)
    );
    assert!(
        verify
            .get("summary")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|summary| summary.contains("复用空闲终端")),
        "命令应在同一空闲终端复用执行：{:?}",
        verify.get("summary")
    );

    // 3. terminal(action=open) + python3 REPL 交互。
    let opened = call("terminal", serde_json::json!({ "action": "open" }));
    let terminal_id = opened
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .and_then(|summary| summary.split_whitespace().last())
        .map(str::to_string)
        .unwrap_or_default();
    assert!(
        !terminal_id.is_empty(),
        "terminal open 应返回编号：{opened}"
    );
    let mut send = |input: &str, enter: bool, wait: u64| {
        call(
            "terminal",
            serde_json::json!({
                "action": "send",
                "terminal_id": terminal_id,
                "input": input,
                "enter": enter,
                "wait": wait,
            }),
        )
    };
    send("python3", true, 15);
    let repl = send("print(6*7)", true, 15);
    let repl_text = stdout_of(&repl);
    assert!(repl_text.contains("42"), "REPL 输出应可读取：{repl_text}");
    send("exit()", true, 10);

    // 4. \x03 中断运行中的命令。
    send("sleep 60", true, 3);
    let interrupted = send("\x03", false, 15);
    let interrupted_text = stdout_of(&interrupted);
    assert!(
        interrupted_text.is_empty() || !interrupted_text.contains("still_running"),
        "中断后不应仍处于执行态：{interrupted_text}"
    );

    // 5. 会话资源释放：listInstances 应列出该终端，instanceClosed 后清空。
    let listed = call(
        "listInstances",
        serde_json::json!({ "session_id": "session-switch-e2e" }),
    );
    let count_before = listed
        .get("instances")
        .and_then(serde_json::Value::as_array)
        .map(|entries| entries.len())
        .unwrap_or_default();
    assert!(count_before >= 1, "关闭前应至少有 1 个终端实例：{listed}");
    if let Some(entries) = listed
        .get("instances")
        .and_then(serde_json::Value::as_array)
    {
        for entry in entries {
            let instance_id = entry
                .get("instance_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let _ = call(
                "instanceClosed",
                serde_json::json!({
                    "session_id": "session-switch-e2e",
                    "instance_id": instance_id,
                }),
            );
        }
    }
    let listed_after = call(
        "listInstances",
        serde_json::json!({ "session_id": "session-switch-e2e" }),
    );
    let count_after = listed_after
        .get("instances")
        .and_then(serde_json::Value::as_array)
        .map(|entries| entries.len())
        .unwrap_or_default();
    assert_eq!(
        count_after, 0,
        "instanceClosed 后实例应清空：{listed_after}"
    );

    let _ = connection.stop();
}

/// 工具调用返回 JSON 文本 → 解析为 Value（解析失败 panic，测试专用）。
fn parse(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|error| panic!("工具结果不是 JSON：{error}\n{raw}"))
}
