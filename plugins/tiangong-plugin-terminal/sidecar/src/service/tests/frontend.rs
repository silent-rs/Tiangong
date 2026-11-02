//! 前端协作生命周期：实例关闭回收、精确附着等待与存活集合 GC，
//! 以及 xterm 画面回传对模拟器快照的覆盖（后写者胜出）。

use super::*;

/// 宿主实例移除兜底（instanceClosed）：页面未就绪时由宿主补发；
/// 幂等（重复/未知编号成功）、跨会话拒绝，与 terminal_close 一致。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_closed_兜底回收幂等且校验归属() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = TerminalService::new();

    // 模拟宿主标签：终端编号由调用方指定（tab 编号，v7 经 spawn 建立）。
    let terminal = service
        .spawn_session(SpawnRequest {
            session_id: Some("terminal-inst-1".to_string()),
            cmd: String::new(),
            args: Vec::new(),
            script: None,
            cwd: Some(workspace.clone()),
            scope_id: Some("session-a".to_string()),
            reserve: false,
            cols: default_cols(),
            rows: default_rows(),
        })
        .expect("建立终端失败")
        .session_id;

    // 实例通知不携带工具上下文（宿主生命周期操作）：
    // session_id=宿主会话，instance_id=标签（终端）编号。
    let host_notice = |scope_id: &str, instance_id: &str| Request {
        protocol_version: PROTOCOL_VERSION.to_string(),
        request_id: format!("test-{}", scru128::new()),
        operation: "instanceClosed".to_string(),
        payload: serde_json::json!({
            "session_id": scope_id,
            "instance_id": instance_id,
        }),
    };
    let list_instances = |scope_id: &str| Request {
        protocol_version: PROTOCOL_VERSION.to_string(),
        request_id: format!("test-{}", scru128::new()),
        operation: "listInstances".to_string(),
        payload: serde_json::json!({ "session_id": scope_id }),
    };
    let listed = service.dispatch(list_instances("session-a")).await;
    assert!(listed.success, "listInstances 应成功: {listed:?}");
    assert_eq!(
        listed
            .payload
            .as_ref()
            .and_then(|p| p["instances"][0]["instance_id"].as_str()),
        Some(terminal.as_str()),
        "listInstances 应返回会话下存活终端"
    );
    // 跨会话兜底通知：拒绝（终端不属于该会话）。
    let denied = service.dispatch(host_notice("session-b", &terminal)).await;
    assert!(!denied.success, "跨会话兜底必须拒绝: {denied:?}");

    // 归属会话：回收成功；重复补发与未知编号幂等成功。
    let closed = service.dispatch(host_notice("session-a", &terminal)).await;
    assert!(closed.success, "归属会话兜底应成功: {closed:?}");
    assert!(
        service.with_session(&terminal, |_| Ok(())).is_err(),
        "终端应已被兜底回收"
    );
    let again = service.dispatch(host_notice("session-a", &terminal)).await;
    assert!(again.success, "重复兜底应幂等成功");
    let unknown = service
        .dispatch(host_notice("session-a", "tty-not-exist"))
        .await;
    assert!(unknown.success, "未知编号兜底应幂等成功");
    let emptied = service.dispatch(list_instances("session-a")).await;
    assert_eq!(
        emptied
            .payload
            .as_ref()
            .and_then(|p| p["instances"].as_array().map(Vec::len)),
        Some(0)
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_command_不等前端附着即执行_历史重放兜底() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = Arc::new(TerminalService::new());
    let running_service = Arc::clone(&service);
    let running = tokio::spawn(async move {
        running_service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "printf attached"}),
                Some(("session-attached", workspace.as_str())),
            ))
            .await
    });

    // PTY 执行不等观看者（issue #697）：无前端附着时命令照常执行完成，
    // 页面随后附着时经 display_history 重放看到全部输出。
    let response = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("命令应不等前端附着即执行完成")
        .expect("命令任务异常");
    let outcome = outcome_of(response);
    assert_eq!(outcome["stdout"], "attached");
    let terminal_id = service
        .sessions
        .lock()
        .expect("会话表锁损坏")
        .keys()
        .next()
        .cloned()
        .expect("应已创建终端");
    let found = service.find_by_scope(&FindRequest {
        scope_id: "session-attached".to_string(),
        session_id: Some(terminal_id.clone()),
    });
    assert_eq!(found.session_id.as_deref(), Some(terminal_id.as_str()));
    // 迟到附着（页面加载完成后）：历史重放包含命令输出。
    assert!(found.history.contains("attached"), "重放历史应含命令输出");
    service
        .attach_frontend(TerminalBindingRequest {
            scope_id: "session-attached".to_string(),
            session_id: terminal_id.clone(),
        })
        .expect("确认前端附着失败");
    service
        .kill_session(SessionIdRequest {
            session_id: terminal_id,
        })
        .expect("清理测试终端失败");
}

/// 桌面端画面权威链路（issue #697）：模拟器与前端回传按「后写者胜出」
/// 滚动覆盖——同一 PTY 输出一份数据三用（模拟器快照、display_history、
/// 通知），前端 xterm 渲染后的画面回传必然晚于模拟器写入，覆盖后以
/// xterm 为权威；回传之后的新输出再次经模拟器写入，成为最新画面，
/// 直到下一次回传。zsh/omz 的重绘流同理，无需按形态分支。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 前端画面回传与模拟器快照后写胜出() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = TerminalService::new();
    let terminal = service
        .spawn_session(SpawnRequest {
            session_id: Some("terminal-screen".to_string()),
            cmd: String::new(),
            args: Vec::new(),
            script: None,
            cwd: Some(workspace.clone()),
            scope_id: Some("session-screen".to_string()),
            reserve: false,
            cols: default_cols(),
            rows: default_rows(),
        })
        .expect("建立终端失败")
        .session_id;

    // 1. 输出到达（headless 与桌面同路径）：模拟器无条件先写入快照。
    //    样本为 zsh 风格重绘流：\r + prompt + buffer + EL 清尾。
    ingest_screen_update(
        &service.sessions,
        &terminal,
        "\x1b[32m❯\x1b[0m echo ping\r❯ echo ping\x1b[K",
    );
    service
        .with_session(&terminal, |session| {
            assert!(
                session.screen_snapshot.contains("echo ping"),
                "模拟器快照应先写入：{}",
                session.screen_snapshot
            );
            Ok(())
        })
        .expect("读取会话失败");

    // 2. 前端 xterm 回传（terminalScreenUpdate，节流 50ms 后）：晚于
    //    模拟器写入，后写者胜出——xterm 渲染画面成为权威。
    service
        .update_screen(ScreenUpdateRequest {
            session_id: terminal.clone(),
            screen: "❯ echo ping\nping\n❯ xterm-authoritative".to_string(),
        })
        .expect("画面回传失败");
    service
        .with_session(&terminal, |session| {
            assert_eq!(
                session.screen_snapshot, "❯ echo ping\nping\n❯ xterm-authoritative",
                "前端回传应覆盖模拟器快照（后写者胜出）"
            );
            assert!(session.screen_updates >= 2, "回传应推进画面版本");
            Ok(())
        })
        .expect("读取会话失败");

    // 3. 回传之后的新输出：模拟器再次写入成为最新画面（滚动权威，
    //    直到下一次回传）——快照服务（terminal status / send）始终
    //    读到最新可用画面。
    ingest_screen_update(
        &service.sessions,
        &terminal,
        "\r❯ echo pong\r❯ echo pong\x1b[K",
    );
    service
        .with_session(&terminal, |session| {
            assert!(
                session.screen_snapshot.ends_with("echo pong"),
                "新输出后模拟器再次胜出：{}",
                session.screen_snapshot
            );
            Ok(())
        })
        .expect("读取会话失败");

    service
        .kill_session(SessionIdRequest {
            session_id: terminal,
        })
        .expect("清理测试终端失败");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_gc_按前端存活集合回收且不跨会话() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let service = TerminalService::new();
    let spawn = |session_id: &str, scope_id: &str| SpawnRequest {
        session_id: Some(session_id.to_string()),
        cmd: String::new(),
        args: Vec::new(),
        script: None,
        cwd: Some(cwd.path().to_string_lossy().to_string()),
        scope_id: Some(scope_id.to_string()),
        reserve: false,
        cols: default_cols(),
        rows: default_rows(),
    };
    service
        .spawn_session(spawn("terminal-live", "session-a"))
        .unwrap();
    service
        .spawn_session(spawn("terminal-closed", "session-a"))
        .unwrap();
    service
        .spawn_session(spawn("terminal-other-scope", "session-b"))
        .unwrap();
    service
        .spawn_session(spawn("terminal-closed-before-gc", "session-a"))
        .unwrap();
    service
        .attach_frontend(TerminalBindingRequest {
            scope_id: "session-a".to_string(),
            session_id: "terminal-closed-before-gc".to_string(),
        })
        .unwrap();

    let initial = service
        .dispatch(Request::new(
            "terminalGc",
            serde_json::json!({
                "session_id": "session-a",
                "live_terminal_ids": ["terminal-live", "terminal-closed"],
            }),
        ))
        .await;
    assert!(initial.success, "首次 Terminal GC 对账失败: {initial:?}");
    assert!(
        service
            .with_session("terminal-closed-before-gc", |_| Ok(()))
            .is_err(),
        "已附着终端即使在首次定时对账前关闭也必须被回收"
    );
    service
        .spawn_session(spawn("terminal-mounting", "session-a"))
        .unwrap();
    service
        .with_session("terminal-closed", |session| {
            session.phase = SessionPhase::Running;
            Ok(())
        })
        .unwrap();
    service.active_tools.lock().unwrap().insert(
        "request-a".to_string(),
        ActiveTool {
            terminal_id: "terminal-closed".to_string(),
        },
    );

    let response = service
        .dispatch(Request::new(
            "terminalGc",
            serde_json::json!({
                "session_id": "session-a",
                "live_terminal_ids": ["terminal-live"],
            }),
        ))
        .await;
    assert!(response.success, "Terminal GC 对账失败: {response:?}");

    assert!(service.with_session("terminal-live", |_| Ok(())).is_ok());
    assert!(service.with_session("terminal-closed", |_| Ok(())).is_err());
    assert!(
        service
            .with_session("terminal-mounting", |_| Ok(()))
            .is_ok(),
        "尚未完成前端挂载的新终端不得被局部存活集合误回收"
    );
    assert!(
        service
            .with_session("terminal-other-scope", |_| Ok(()))
            .is_ok(),
        "GC 不得影响其他会话"
    );
    assert!(service.active_tools.lock().unwrap().is_empty());
    service
        .kill_session(SessionIdRequest {
            session_id: "terminal-live".to_string(),
        })
        .unwrap();
    service
        .kill_session(SessionIdRequest {
            session_id: "terminal-other-scope".to_string(),
        })
        .unwrap();
    service
        .kill_session(SessionIdRequest {
            session_id: "terminal-mounting".to_string(),
        })
        .unwrap();
}
