//! 工具层集成测试：run_command / run_shell / terminal_open / terminal_send /
//! terminal_close / terminal_status 经分发入口端到端驱动真实 PTY。

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 工具调用缺少宿主上下文被拒绝() {
    let service = TerminalService::new();
    let response = service
        .dispatch_test(tool_request(
            "run_command",
            serde_json::json!({"cmd": "ls"}),
            None,
        ))
        .await;
    assert!(!response.success, "缺上下文必须拒绝: {response:?}");
    let message = response.error_message.unwrap_or_default();
    assert!(message.contains("宿主会话上下文"), "拒绝原因: {message}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_command_建立会话终端_复用空闲终端() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    std::fs::write(cwd.path().join("marker.txt"), "ok").expect("写测试文件失败");
    let service = TerminalService::new();

    let first = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "ls", "args": []}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    let diagnostic = service
        .sessions
        .lock()
        .unwrap()
        .values()
        .map(|session| String::from_utf8_lossy(&session.raw_history).to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        first["ok"], true,
        "首次执行结果: {first}; raw={diagnostic:?}"
    );
    assert_eq!(first["exit_code"], 0);
    assert_eq!(first["stdout"], "marker.txt");
    // 权威 cwd 生效：命令在宿主注入的会话工作目录执行（shell 回显的
    // 目录即传入的工作目录——逻辑路径原样，不做物理化）。
    assert!(
        first["summary"]
            .as_str()
            .unwrap_or_default()
            .contains(&workspace),
        "summary 应体现权威工作目录: {}",
        first["summary"]
    );
    let terminal = first["summary"]
        .as_str()
        .and_then(|summary| {
            summary
                .split("终端 ")
                .nth(1)
                .and_then(|rest| rest.split('（').next())
        })
        .expect("summary 应含终端编号")
        .to_string();
    assert!(!terminal.is_empty());
    assert!(
        terminal.starts_with("tty-"),
        "自动创建的终端编号应带 tty- 前缀: {terminal}"
    );

    let second = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo", "args": ["again"], "timeout": 10}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(second["ok"], true);
    assert!(
        second["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("复用空闲终端"),
        "第二次应复用空闲终端: {}",
        second["summary"]
    );

    service
        .kill_session(SessionIdRequest {
            session_id: terminal,
        })
        .expect("清理测试终端失败");
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_command_windows_首次执行并复用同一终端() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    for explicit_shell in [None, Some("cmd.exe")] {
        let cwd = tempfile::tempdir().expect("创建 Windows 测试目录失败");
        let workspace = cwd
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        std::fs::write(cwd.path().join("terminal-ready-windows.txt"), "ok")
            .expect("写 Windows 测试文件失败");
        let service = TerminalService::new();
        if let Some(shell) = explicit_shell {
            service
                .spawn_session(SpawnRequest {
                    session_id: None,
                    cmd: shell.into(),
                    args: vec!["/d".into()],
                    script: None,
                    cwd: Some(workspace.clone()),
                    scope_id: Some("windows-session".into()),
                    reserve: false,
                    cols: 80,
                    rows: 24,
                })
                .unwrap();
        }

        let first = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "dir", "args": [], "timeout": 10}),
                    Some(("windows-session", workspace.as_str())),
                ))
                .await,
        );
        let diagnostic = service
            .sessions
            .lock()
            .unwrap()
            .values()
            .map(|session| String::from_utf8_lossy(&session.raw_history).to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            first["ok"], true,
            "首次执行结果: {first}; raw={diagnostic:?}"
        );
        assert_eq!(first["exit_code"], 0, "首次执行结果: {first}");
        assert!(
            first["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("terminal-ready-windows.txt"),
            "首次执行未返回目录内容: {first}"
        );
        let terminal = first["summary"]
            .as_str()
            .and_then(|summary| {
                summary
                    .split("终端 ")
                    .nth(1)
                    .and_then(|rest| rest.split('（').next())
            })
            .expect("首次执行摘要应含终端编号")
            .to_string();

        let second = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "echo", "args": ["again"], "timeout": 10}),
                    Some(("windows-session", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(second["ok"], true, "第二次执行结果: {second}");
        assert_eq!(second["exit_code"], 0, "第二次执行结果: {second}");
        let raw = service
            .with_session(&terminal, |session| {
                Ok(String::from_utf8_lossy(&session.raw_history).to_string())
            })
            .unwrap_or_default();
        assert_eq!(
            second["stdout"], "again",
            "第二次执行结果: {second}\nraw={raw:?}"
        );
        assert!(
            second["summary"]
                .as_str()
                .unwrap_or_default()
                .contains("复用空闲终端"),
            "第二次执行未复用终端: {second}"
        );

        let failed = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd":"cmd.exe","args":["/c","exit","7"],"timeout":5}),
                    Some(("windows-session", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(failed["exit_code"], 7, "失败返回: {failed}");
        assert_eq!(failed["ok"], false, "非零退出状态不能报告工具成功");

        if explicit_shell.is_none() {
            let native_error = outcome_of(
                service
                    .dispatch_test(tool_request(
                        "run_shell",
                        serde_json::json!({"script": "Write-Error 'bad'", "timeout": 5}),
                        Some(("windows-session", workspace.as_str())),
                    ))
                    .await,
            );
            assert_eq!(
                native_error["exit_code"], 1,
                "PowerShell 错误不能沿用上一条退出码: {native_error}"
            );
            assert_eq!(native_error["ok"], false);
            for script in [
                "if ($LASTEXITCODE -ne 7) { throw 'last exit lost' }; $value = 123; function Get-ReviewValue { $value }; Set-Variable -Name ('__' + 'TIANGONG_RC_VALUE') -Value 'user-owned'; $__TG_COMMAND_BEFORE = 'before-owned'; $__TG_COMMAND_ERRORS = 'errors-owned'",
                "if ($value -ne 123 -or (Get-ReviewValue) -ne 123) { throw 'scope lost' }; $value += 1",
                "if ($value -ne 124 -or (Get-ReviewValue) -ne 124) { throw 'state update lost' }; if ((Get-Variable ('__' + 'TIANGONG_RC_VALUE')).Value -ne 'user-owned') { throw 'user variable overwritten' }; if ($__TG_COMMAND_BEFORE -ne 'before-owned' -or $__TG_COMMAND_ERRORS -ne 'errors-owned') { throw 'fixed helper variable overwritten' }; $helpers = @(Get-Variable ('__' + 'TIANGONG_*') | Where-Object { $_.Name -ne ('__' + 'TIANGONG_RC_VALUE') }); if ($helpers.Count -ne 1) { throw ('helper leaked: ' + ($helpers.Name -join ',')) }",
            ] {
                let result = outcome_of(
                    service
                        .dispatch_test(tool_request(
                            "run_shell",
                            serde_json::json!({"script": script, "timeout": 10}),
                            Some(("windows-session", workspace.as_str())),
                        ))
                        .await,
                );
                assert_eq!(
                    result["ok"], true,
                    "PowerShell 状态应在连续命令中保留: {result}"
                );
            }
            let invalid = outcome_of(
                service
                    .dispatch_test(tool_request(
                        "run_shell",
                        serde_json::json!({"script": "if (", "timeout": 5}),
                        Some(("windows-session", workspace.as_str())),
                    ))
                    .await,
            );
            assert_eq!(invalid["exit_code"], 1, "语法错误必须及时返回: {invalid}");
            assert_eq!(invalid["ok"], false);
        }
        let script = if explicit_shell.is_some() {
            "echo content>roundtrip.txt & type roundtrip.txt & del roundtrip.txt"
        } else {
            "Set-Content -LiteralPath 'roundtrip.txt' -Value 'content'; Get-Content -LiteralPath 'roundtrip.txt'; Remove-Item -LiteralPath 'roundtrip.txt'"
        };
        let roundtrip = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_shell",
                    serde_json::json!({"script": script, "timeout": 10}),
                    Some(("windows-session", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(roundtrip["ok"], true, "文件读写: {roundtrip}");
        assert!(
            roundtrip["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("content")
        );
        assert!(!cwd.path().join("roundtrip.txt").exists());

        tokio::time::sleep(Duration::from_millis(100)).await;
        let display = service
            .with_session(&terminal, |session| {
                Ok(String::from_utf8_lossy(&session.display_history).to_string())
            })
            .expect("读取 Windows 终端可见历史失败");
        assert!(display.contains("terminal-ready-windows.txt"));
        assert!(
            !contains_marker(&display),
            "可见输出泄露内部标记: {display:?}"
        );
        service
            .kill_session(SessionIdRequest {
                session_id: terminal,
            })
            .expect("清理 Windows 测试终端失败");
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_send_同会话输入送达并返回画面() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = TerminalService::new();

    let first = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo", "args": ["ready"]}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(first["ok"], true);
    let terminal = first["summary"]
        .as_str()
        .and_then(|summary| {
            summary
                .split("终端 ")
                .nth(1)
                .and_then(|rest| rest.split('（').next())
        })
        .expect("summary 应含终端编号")
        .to_string();

    // 命令结束后 shell 回到提示符：向该终端发送新命令，输入应送达
    // 且返回内容包含命令回显与输出（视图在线走画面快照，离线走
    // 原始输出回显——测试无视图，验证回显路径）。
    let sent = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_send",
                serde_json::json!({
                    "terminal_id": terminal,
                    "input": "echo sent-ok\r",
                    "wait": 3,
                }),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(sent["ok"], true, "terminal_send 结果: {sent}");
    assert!(
        sent["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("sent-ok"),
        "回显应包含发送的命令输出: {}",
        sent["stdout"]
    );

    service
        .kill_session(SessionIdRequest {
            session_id: terminal,
        })
        .expect("清理测试终端失败");
}

/// enter=true 单独补发回车让命令执行；缺省原样写入，不额外追加回车，
/// 保证 vi 插入模式等纯文本场景不被注入换行。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_send_自动回车执行且缺省保持纯文本() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = TerminalService::new();

    let first = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo", "args": ["ready"]}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(first["ok"], true);
    let terminal = first["summary"]
        .as_str()
        .and_then(|summary| {
            summary
                .split("终端 ")
                .nth(1)
                .and_then(|rest| rest.split('（').next())
        })
        .expect("summary 应含终端编号")
        .to_string();

    // 缺省：原样写入，不补回车——命令停留在输入行，不会执行。
    let typed = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_send",
                serde_json::json!({
                    "terminal_id": terminal,
                    "input": "echo plain-$((40+2))",
                    "wait": 1,
                }),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(typed["ok"], true, "terminal_send 结果: {typed}");
    assert!(
        !typed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("plain-42"),
        "缺省不应自动执行: {}",
        typed["stdout"]
    );

    // enter=true 且只发空输入：补发回车，执行上一步停留在输入行的命令。
    let entered = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_send",
                serde_json::json!({
                    "terminal_id": terminal,
                    "input": "",
                    "enter": true,
                    "wait": 3,
                }),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(entered["ok"], true, "terminal_send 结果: {entered}");
    assert!(
        entered["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("plain-42"),
        "补发回车后应执行命令: {}",
        entered["stdout"]
    );

    // enter=true 且输入自带换行：只执行一次，不叠加空回车。
    let once = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_send",
                serde_json::json!({
                    "terminal_id": terminal,
                    "input": "echo auto-$((1+1))\\n",
                    "enter": true,
                    "wait": 3,
                }),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(once["ok"], true, "terminal_send 结果: {once}");
    let stdout = once["stdout"].as_str().unwrap_or_default();
    assert!(stdout.contains("auto-2"), "应执行命令: {stdout}");

    service
        .kill_session(SessionIdRequest {
            session_id: terminal,
        })
        .expect("清理测试终端失败");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_open后的唯一编号可继续交互() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = TerminalService::new();

    let opened = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_open",
                serde_json::json!({}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(opened["ok"], true, "terminal_open 结果: {opened}");
    let terminal = opened["summary"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches("已打开终端 ")
        .to_string();
    assert!(
        terminal.starts_with("terminal-"),
        "terminal_open 应返回独立编号: {terminal}"
    );

    // 空闲终端建立后即可按编号发送输入（用户面板的等价路径）。
    let sent = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_send",
                serde_json::json!({
                    "terminal_id": terminal,
                    "input": "echo opened\r",
                    "wait": 3,
                }),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(sent["ok"], true, "send 结果: {sent}");
    assert!(
        sent["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("opened"),
        "回显应包含输出: {}",
        sent["stdout"]
    );

    service
        .kill_session(SessionIdRequest {
            session_id: terminal,
        })
        .expect("清理测试终端失败");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 跨会话操作被明确拒绝() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = TerminalService::new();

    let first = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "sleep", "args": ["30"], "timeout": 60}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    // 长命令超时返回 ok（命令状态），终端保留运行中。
    assert_eq!(first["ok"], true);
    let terminal = first["summary"]
        .as_str()
        .and_then(|summary| {
            summary
                .split("终端 ")
                .nth(1)
                .and_then(|rest| rest.split('（').next())
        })
        .expect("summary 应含终端编号")
        .to_string();

    // 会话 B 不得发送输入或关闭 A 的终端。
    let sent = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_send",
                serde_json::json!({"terminal_id": terminal, "input": "ls"}),
                Some(("session-b", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(sent["ok"], false, "跨会话 terminal_send 必须拒绝");
    let closed = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_close",
                serde_json::json!({"terminal_id": terminal}),
                Some(("session-b", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(closed["ok"], false, "跨会话 terminal_close 必须拒绝");

    // 归属会话关闭正常，且关闭未知终端幂等成功。
    let closed_by_owner = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_close",
                serde_json::json!({"terminal_id": terminal}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(closed_by_owner["ok"], true);
    let closed_missing = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_close",
                serde_json::json!({"terminal_id": "tty-not-exist"}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(closed_missing["ok"], true);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 多会话各自持有独立终端() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    std::fs::write(cwd.path().join("shared.txt"), "ok").expect("写测试文件失败");
    let service = TerminalService::new();

    let (a, b) = tokio::join!(
        service.dispatch_test(tool_request(
            "run_command",
            serde_json::json!({"cmd": "ls"}),
            Some(("session-a", workspace.as_str())),
        )),
        service.dispatch_test(tool_request(
            "run_command",
            serde_json::json!({"cmd": "ls"}),
            Some(("session-b", workspace.as_str())),
        )),
    );
    let (a, b) = (outcome_of(a), outcome_of(b));
    assert_eq!(a["ok"], true);
    assert_eq!(b["ok"], true);
    let terminal_of = |value: &serde_json::Value| {
        value["summary"]
            .as_str()
            .and_then(|summary| {
                summary
                    .split("终端 ")
                    .nth(1)
                    .and_then(|rest| rest.split('（').next())
            })
            .expect("summary 应含终端编号")
            .to_string()
    };
    let (terminal_a, terminal_b) = (terminal_of(&a), terminal_of(&b));
    assert_ne!(terminal_a, terminal_b, "两个会话必须各建独立终端");

    // 关闭 A 的终端不影响 B 的终端继续执行。
    let closed = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_close",
                serde_json::json!({"terminal_id": terminal_a}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(closed["ok"], true);
    let b_again = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo", "args": ["still"]}),
                Some(("session-b", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(b_again["ok"], true);
    assert!(
        b_again["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("复用空闲终端"),
        "B 的终端不受 A 关闭影响: {}",
        b_again["summary"]
    );

    service
        .kill_session(SessionIdRequest {
            session_id: terminal_b,
        })
        .expect("清理测试终端失败");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_open_返回独立编号终端() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = TerminalService::new();
    let opened = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_open",
                serde_json::json!({}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(opened["ok"], true, "terminal_open 结果: {opened}");
    let terminal = opened["summary"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches("已打开终端 ")
        .to_string();
    assert!(
        terminal.starts_with("terminal-"),
        "terminal_open 应返回独立编号: {terminal}"
    );
    service
        .kill_session(SessionIdRequest {
            session_id: terminal,
        })
        .expect("清理测试终端失败");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_send_close_缺少terminal_id明确失败() {
    let service = TerminalService::new();
    let scope = Some(("session-a", "/tmp"));
    let sent = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_send",
                serde_json::json!({"input": "ls"}),
                scope,
            ))
            .await,
    );
    assert_eq!(sent["ok"], false);
    assert!(
        sent["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("terminal_id")
    );
    let closed = outcome_of(
        service
            .dispatch_test(tool_request("terminal_close", serde_json::json!({}), scope))
            .await,
    );
    assert_eq!(closed["ok"], false);
}

/// terminal_status：概要（仅本会话、含已退出终端与状态映射、按创建
/// 序）与详情（含最近输出尾部）；跨会话与不存在终端明确失败。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_status_概要详情与跨会话拒绝() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = TerminalService::new();
    let spawn = |terminal_id: &str, scope_id: &str| SpawnRequest {
        session_id: Some(terminal_id.to_string()),
        cmd: String::new(),
        args: Vec::new(),
        script: None,
        cwd: Some(workspace.clone()),
        scope_id: Some(scope_id.to_string()),
        reserve: false,
        cols: default_cols(),
        rows: default_rows(),
    };
    service
        .spawn_session(spawn("status-idle", "session-a"))
        .unwrap();
    service
        .spawn_session(spawn("status-busy", "session-a"))
        .unwrap();
    service
        .spawn_session(spawn("status-other", "session-b"))
        .unwrap();
    service
        .spawn_session(spawn("status-dead", "session-a"))
        .unwrap();
    // 覆盖状态映射：交互中（带命令）与已退出（待回收）。
    service
        .with_session("status-busy", |session| {
            session.phase = SessionPhase::Interactive;
            session.running_command = Some("vi notes.txt".to_string());
            Ok(())
        })
        .unwrap();
    service
        .with_session("status-dead", |session| {
            session.exited_code = Some(130);
            Ok(())
        })
        .unwrap();

    let listed = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_status",
                serde_json::json!({}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(listed["ok"], true, "概要查询失败: {listed}");
    let overview: serde_json::Value =
        serde_json::from_str(listed["stdout"].as_str().expect("概要应有 stdout"))
            .expect("概要 stdout 应为 JSON");
    let terminals = overview["terminals"].as_array().expect("概要应为数组");
    let ids: Vec<&str> = terminals
        .iter()
        .map(|terminal| terminal["terminal_id"].as_str().expect("应有编号"))
        .collect();
    assert_eq!(
        ids,
        vec!["status-idle", "status-busy", "status-dead"],
        "概要应仅含本会话终端（含已退出）并按创建序"
    );
    let status_of = |id: &str| {
        terminals
            .iter()
            .find(|terminal| terminal["terminal_id"] == id)
            .and_then(|terminal| terminal["status"].as_str())
            .unwrap_or_default()
    };
    assert_eq!(status_of("status-idle"), "idle");
    assert_eq!(status_of("status-busy"), "interactive");
    assert_eq!(status_of("status-dead"), "exited");
    let command_of = |id: &str| {
        terminals
            .iter()
            .find(|terminal| terminal["terminal_id"] == id)
            .and_then(|terminal| terminal["command"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(
        command_of("status-busy"),
        "vi notes.txt",
        "执行中/交互中的终端应带正在执行的命令"
    );
    assert_eq!(
        command_of("status-idle"),
        "",
        "空闲终端不应残留命令（null 序列化为空断言兜底）"
    );
    assert_eq!(
        terminals
            .iter()
            .find(|terminal| terminal["terminal_id"] == "status-dead")
            .and_then(|terminal| terminal["exited_code"].as_i64()),
        Some(130)
    );

    let detail = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_status",
                serde_json::json!({ "terminal_id": "status-idle" }),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(detail["ok"], true, "详情查询失败: {detail}");
    let detail_payload: serde_json::Value =
        serde_json::from_str(detail["stdout"].as_str().expect("详情应有 stdout"))
            .expect("详情 stdout 应为 JSON");
    assert_eq!(detail_payload["status"], "idle");
    assert!(
        detail_payload.get("recent_output").is_some(),
        "详情应含最近输出字段: {detail_payload}"
    );

    // 跨会话与不存在终端：明确失败，与其他工具的归属语义一致。
    let denied = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_status",
                serde_json::json!({ "terminal_id": "status-idle" }),
                Some(("session-b", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(denied["ok"], false, "跨会话查询应被拒绝: {denied}");
    assert!(
        denied["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("不属于当前会话"),
        "拒绝原因应明示归属: {}",
        denied["summary"]
    );
    let missing = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_status",
                serde_json::json!({ "terminal_id": "status-none" }),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(missing["ok"], false, "查询不存在的终端应失败: {missing}");

    // 先收集再循环：for 头部表达式里的锁守卫会活到整个循环结束。
    let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
    for session_id in session_ids {
        let _ = service.kill_session(SessionIdRequest { session_id });
    }
}
