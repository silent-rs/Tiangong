//! 前端协作生命周期：实例关闭回收、精确附着等待与存活集合 GC。

use super::*;

/// 宿主实例移除兜底（instanceClosed）：页面未就绪时由宿主补发；
/// 幂等（重复/未知编号成功）、跨会话拒绝，与 terminal_close 一致。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_closed_兜底回收幂等且校验归属() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = TerminalService::new();

    // 模拟手动新建标签：终端编号由调用方指定（tab 编号）。
    let opened = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal",
                serde_json::json!({"action": "open",}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    let terminal = opened["summary"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches("已打开终端 ")
        .to_string();

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
async fn run_command_等待前端精确附着后才执行() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = Arc::new(TerminalService::new());
    let running_service = Arc::clone(&service);
    let running = tokio::spawn(async move {
        running_service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "printf", "args": ["attached"]}),
                Some(("session-attached", workspace.as_str())),
            ))
            .await
    });

    let terminal_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(terminal_id) = service
                .sessions
                .lock()
                .expect("会话表锁损坏")
                .keys()
                .next()
                .cloned()
            {
                break terminal_id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("等待 Agent 终端创建超时");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!running.is_finished(), "前端尚未附着时命令不应开始并完成");

    let found = service.find_by_scope(&FindRequest {
        scope_id: "session-attached".to_string(),
        session_id: Some(terminal_id.clone()),
    });
    assert_eq!(found.session_id.as_deref(), Some(terminal_id.as_str()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !running.is_finished(),
        "仅查到 PTY 不能代替前端完成订阅与附着"
    );
    service
        .attach_frontend(TerminalBindingRequest {
            scope_id: "session-attached".to_string(),
            session_id: terminal_id.clone(),
        })
        .expect("确认前端附着失败");

    let response = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("前端附着后命令未完成")
        .expect("命令任务异常");
    let outcome = outcome_of(response);
    assert_eq!(outcome["stdout"], "attached");
    service
        .kill_session(SessionIdRequest {
            session_id: terminal_id,
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
            interactive: false,
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
