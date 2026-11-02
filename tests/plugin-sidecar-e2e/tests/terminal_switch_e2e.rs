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
use tiangong_plugin_runtime::protocol::RequestInvocationContext;
use tiangong_plugin_runtime::sidecar::{SidecarConfig, SidecarConnection, StdioSidecarConnection};

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
    let context = RequestInvocationContext {
        session_id: "session-switch-e2e".to_string(),
        invocation_id: "call-e2e".to_string(),
        workspace: workspace.display().to_string(),
        actor_id: String::new(),
        deadline_ms: None,
    };
    let no_progress = &mut |_message: String| {};
    let mut call = |operation: &str, payload: serde_json::Value| {
        parse(
            &connection
                .invoke_with_invocation_context_and_progress(
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
        serde_json::json!({ "cmd": "echo switch-ok" }),
    );
    assert!(
        stdout_of(&echo).contains("switch-ok"),
        "echo 输出应包含标记：{}",
        stdout_of(&echo)
    );

    // 2. 终端复用：export 的环境变量跨调用保留（同一空闲终端）。
    let export = call(
        "run_command",
        serde_json::json!({ "cmd": "export E2E_MARKER=42" }),
    );
    assert!(
        export.get("ok").and_then(serde_json::Value::as_bool) == Some(true),
        "export 应成功：{export}"
    );
    let verify = call(
        "run_command",
        serde_json::json!({ "cmd": "echo E2E=$E2E_MARKER" }),
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

    // 3. run_command 拉起 python3 REPL：等待窗口耗尽软交还（issue #697），
    //    Agent 从输出识别后用 terminal(action=send) 接管交互。
    let repl_run = call("run_command", serde_json::json!({ "cmd": "python3 -q" }));
    assert!(
        repl_run.get("ok").and_then(serde_json::Value::as_bool) == Some(true),
        "交还结果应为 ok：{repl_run}"
    );
    let terminal_id = repl_run
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .and_then(|summary| summary.split("终端 ").nth(1))
        .and_then(|rest| rest.split(['（', ' ']).next())
        .map(str::to_string)
        .unwrap_or_default();
    assert!(
        terminal_id.starts_with("tty-"),
        "摘要应含交还终端编号：{:?}",
        repl_run.get("summary")
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
    // REPL 内交互：不设 enter、input 末尾带 \r 原样写入（marker 登记期间
    // enter=true 会被拒绝——那是「向 shell 排队新命令」的语义）。
    let repl = send("print(6*7)\\r", false, 15);
    let repl_text = stdout_of(&repl);
    assert!(repl_text.contains("42"), "REPL 输出应可读取：{repl_text}");
    // 退出 REPL 回到 shell（退出后 wrapper 收尾，marker 闭合、终端回池）。
    send("exit()\\r", false, 15);

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
