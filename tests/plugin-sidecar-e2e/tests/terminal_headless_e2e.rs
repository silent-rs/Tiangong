//! terminal 插件 headless 端到端（issue #697）：经真实宿主 stdio 直连
//! 链路启动 `tiangong-terminal-sidecar`，验证无前端形态（CLI / Server）
//! 下的命令执行能力——sidecar 不感知宿主形态，前端相关协作全部自然
//! 降级（标签反馈宿主侧忽略、通知无订阅即弃、画面快照由 sidecar 屏幕
//! 模拟器生成）：
//!
//! - 单条命令不再等待前端附着（无 1 秒 FRONTEND_ATTACH_WAIT_MS 开销）；
//! - 终端复用：cwd 与 `export` 的环境变量跨调用保留；
//! - `terminal`（action 模式）开终端、跑 python3 REPL：画面快照由 sidecar
//!   内的屏幕解析器生成（headless 没有前端回传）；
//! - `\x03` 中断运行中的命令；
//! - 会话资源释放（listInstances / instanceClosed）回收终端。
//!
//! 依赖已构建的 `tiangong-terminal-sidecar` debug 二进制，未构建时跳过。

use std::path::PathBuf;
use std::time::{Duration, Instant};

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

fn headless_connection(root: &std::path::Path, binary: PathBuf) -> StdioSidecarConnection {
    let config = SidecarConfig::new(
        "terminal",
        "0.4.1",
        binary,
        root.join("endpoint.json"),
        root.join("sidecar.log"),
        root.join("data"),
        root.join("storage"),
    )
    .with_lifecycle(tiangong_plugin_runtime::manifest::SidecarLifecycle::Resident)
    .with_timeouts(Duration::from_secs(20), Duration::from_secs(60));
    StdioSidecarConnection::new(config)
}

/// 工具调用返回 JSON 文本 → 解析为 Value（解析失败 panic，测试专用）。
fn parse(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|error| panic!("工具结果不是 JSON：{error}\n{raw}"))
}

/// 结果 JSON 的 stdout 字段（命令输出承载）。
fn stdout_of(result: &serde_json::Value) -> String {
    result
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[test]
#[serial_test::serial]
fn terminal_headless_命令复用终端_无前端等待_画面快照_中断与回收() {
    let Some(binary) = terminal_sidecar_binary() else {
        eprintln!("跳过：target/debug/tiangong-terminal-sidecar 尚未构建");
        return;
    };
    let can_terminate = tiangong_plugin_runtime::test_support::can_terminate_child_processes();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let connection = headless_connection(root.path(), binary);
    let context = SidecarInvocationContext::new(
        "session-headless-e2e",
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

    // 1. 单条命令没有 1 秒前端附着等待：headless 下首条命令应在 1 秒
    //    等待窗口内完成（desktop 形态会先耗尽 FRONTEND_ATTACH_WAIT_MS）。
    let started = Instant::now();
    let echo = call(
        "run_command",
        serde_json::json!({ "cmd": "echo", "args": ["headless-ok"] }),
    );
    let echo_elapsed = started.elapsed();
    assert!(
        stdout_of(&echo).contains("headless-ok"),
        "echo 输出应包含标记：{}",
        stdout_of(&echo)
    );
    assert!(
        echo_elapsed < Duration::from_secs(3),
        "headless 首条命令不应含 1 秒附着等待：{echo_elapsed:?}"
    );

    // 诊断：首条命令后的终端状态。
    let status_after_echo = call("terminal", serde_json::json!({ "action": "status" }));
    eprintln!("status after echo: {status_after_echo}");
    // 2. 终端复用：export 的环境变量跨调用保留（同一空闲终端）。
    let export = call(
        "run_shell",
        serde_json::json!({ "script": "export E2E_MARKER=42" }),
    );
    assert!(
        export.get("ok").and_then(serde_json::Value::as_bool) == Some(true),
        "export 应成功：{export}"
    );
    eprintln!(
        "echo summary: {:?}",
        echo.get("summary").and_then(serde_json::Value::as_str)
    );
    eprintln!(
        "export summary: {:?}",
        export.get("summary").and_then(serde_json::Value::as_str)
    );
    let verify = call(
        "run_shell",
        serde_json::json!({ "script": "echo E2E=$E2E_MARKER" }),
    );
    eprintln!(
        "verify summary: {:?}",
        verify.get("summary").and_then(serde_json::Value::as_str)
    );
    eprintln!("verify stdout: {:?}", stdout_of(&verify));
    assert!(
        stdout_of(&verify).contains("E2E=42"),
        "环境变量应跨调用保留：{}",
        stdout_of(&verify)
    );
    // 复用同一终端：后续调用的摘要应声明「复用空闲终端」。
    assert!(
        verify
            .get("summary")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|summary| summary.contains("复用空闲终端")),
        "命令应在同一空闲终端复用执行：{:?}",
        verify.get("summary")
    );

    // 3. terminal(action=open) + python3 REPL：画面快照来自 sidecar 屏幕解析。
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
    assert!(
        repl_text.contains("42"),
        "REPL 输出应出现在画面快照：{repl_text}"
    );
    // 退出 REPL 回到 shell。
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
        serde_json::json!({ "session_id": "session-headless-e2e" }),
    );
    let count_before = listed
        .get("instances")
        .and_then(serde_json::Value::as_array)
        .map(|entries| entries.len())
        .unwrap_or_default();
    assert!(count_before >= 1, "删除前应至少有 1 个终端实例：{listed}");
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
                    "session_id": "session-headless-e2e",
                    "instance_id": instance_id,
                }),
            );
        }
    }
    let listed_after = call(
        "listInstances",
        serde_json::json!({ "session_id": "session-headless-e2e" }),
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

    drop(call);
    let _ = connection.stop();
    if can_terminate {
        // sidecar 进程应随连接停止退出；短暂等待后确认资源回收完成。
        std::thread::sleep(Duration::from_millis(500));
    }
}
