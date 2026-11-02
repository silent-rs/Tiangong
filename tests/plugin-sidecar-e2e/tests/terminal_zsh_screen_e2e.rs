//! terminal 插件 zsh 画面快照端到端（issue #697）：验证 headless 屏幕
//! 模拟器（screen.rs）对 zsh + oh-my-zsh 这类「带提示的交互 shell」的
//! 支撑——prompt 渲染、命令回显、ZLE 行编辑（方向键 + 覆写，屏幕经
//! 退格/重绘序列更新）与 CJK 输出都应出现在 `terminal status` 的
//! `screen_snapshot` 中。
//!
//! 依赖已构建的 `tiangong-terminal-sidecar` debug 二进制与本机 `zsh`，
//! 任一缺失时跳过（CI 无 zsh 不算失败）。

use std::path::PathBuf;
use std::time::Duration;

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

fn zsh_available() -> bool {
    std::process::Command::new("zsh")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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

fn parse(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|error| panic!("工具结果不是 JSON：{error}\n{raw}"))
}

fn screen_of(status: &serde_json::Value) -> String {
    status
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .and_then(|inner| serde_json::from_str::<serde_json::Value>(inner).ok())
        .and_then(|inner| {
            inner
                .get("screen_snapshot")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

#[test]
#[serial_test::serial]
fn terminal_zsh_画面快照_prompt_行编辑_中文() {
    let Some(binary) = terminal_sidecar_binary() else {
        eprintln!("跳过：target/debug/tiangong-terminal-sidecar 尚未构建");
        return;
    };
    if !zsh_available() {
        eprintln!("跳过：本机无 zsh");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let connection = headless_connection(root.path(), binary);
    let context = RequestInvocationContext {
        session_id: "session-zsh-screen-e2e".to_string(),
        invocation_id: "call-e2e".to_string(),
        workspace: workspace.display().to_string(),
        actor_id: String::new(),
        deadline_ms: None,
    };
    let call = |operation: &str, payload: serde_json::Value| {
        let mut no_progress = |_message: String| {};
        parse(
            &connection
                .invoke_with_invocation_context_and_progress(
                    operation,
                    &payload.to_string(),
                    &context,
                    &mut no_progress,
                )
                .unwrap_or_else(|error| panic!("{operation} 调用失败：{error:#}")),
        )
    };

    // 1. 拉起交互式 zsh：默认等待窗口耗尽软交还（拉模型，Agent 从输出
    //    自行识别交互 shell，用 terminal send 接管）。
    let zsh_run = call("run_command", serde_json::json!({ "cmd": "zsh" }));
    assert!(
        zsh_run.get("ok").and_then(serde_json::Value::as_bool) == Some(true),
        "zsh 交还结果应为 ok：{zsh_run}"
    );
    let terminal_id = zsh_run
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .and_then(|summary| summary.split("终端 ").nth(1))
        .and_then(|rest| rest.split(['（', ' ']).next())
        .map(str::to_string)
        .unwrap_or_default();
    assert!(
        terminal_id.starts_with("tty-"),
        "摘要应含交还终端编号：{:?}",
        zsh_run.get("summary")
    );

    let send = |input: &str, wait: u64| {
        call(
            "terminal",
            serde_json::json!({
                "action": "send",
                "terminal_id": terminal_id,
                "input": input,
                "enter": false,
                "wait": wait,
            }),
        )
    };
    let status = || {
        call(
            "terminal",
            serde_json::json!({ "action": "status", "terminal_id": terminal_id }),
        )
    };

    // 2. 执行命令并回车（marker 登记期间 enter=false + 末尾 \r 原样写入）：
    //    输出与命令回显都应进入画面快照。
    send("echo zsh-screen-ok\\r", 12);
    let screen = screen_of(&status());
    eprintln!("---- zsh screen after echo ----\n{screen}\n----");
    assert!(
        screen.contains("zsh-screen-ok"),
        "画面快照应含命令回显与输出：\n{screen}"
    );

    // 3. ZLE 行编辑：输入未提交的命令（16 字符），方向键左移 3 格到
    //    `-me` 段首个 `-` 前，插入 X——zsh 经退格/重绘序列更新屏幕，
    //    快照应反映编辑后的 buffer（ZLE 默认插入模式：zle-editX-me）。
    send("echo zle-edit-me", 3);
    send("\u{1b}[D\u{1b}[D\u{1b}[DX", 3);
    let screen_edited = screen_of(&status());
    eprintln!("---- zsh screen after ZLE edit ----\n{screen_edited}\n----");
    assert!(
        screen_edited.contains("zle-editX-me"),
        "ZLE 编辑后的画面应含插入字符：\n{screen_edited}"
    );
    // 清掉未提交行（Ctrl-C），避免干扰后续步骤。
    send("\u{3}", 3);

    // 4. CJK 输出：内容完整进入快照（宽字符单列计的已知取舍，字不丢）。
    send("echo 中文画面\\r", 12);
    let screen_cjk = screen_of(&status());
    eprintln!("---- zsh screen after CJK ----\n{screen_cjk}\n----");
    assert!(
        screen_cjk.contains("中文画面"),
        "画面快照应含中文输出：\n{screen_cjk}"
    );

    // 5. 退出 zsh（软交还的终端回到空闲池）。
    send("exit\\r", 8);
    let _ = call;
    let _ = connection.stop();
}
