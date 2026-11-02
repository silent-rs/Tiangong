//! 请求级取消与超时收尾：中断确认、终端保活与复用池恢复。

use super::*;
use crate::service::readiness::{Foreground, foreground_of};

/// 取消链路：宿主超时/会话取消后 Cancel 帧到达，dispatch future 已被
/// runner 丢弃——cancel 钩子必须把 Ctrl+C 落到执行中的终端并复位
/// phase，命令不脱离调用方无限运行、终端可被后续调用复用。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 取消中断执行中的命令并复位终端() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = TerminalService::new();

    // 长命令在独立 task 中执行（对齐 runner：dispatch 由任务持有）。
    let service_for_task = std::sync::Arc::new(service);
    let request = tool_request(
        "run_command",
        serde_json::json!({"cmd": "sleep 60"}),
        Some(("session-a", workspace.as_str())),
    );
    let task_service = std::sync::Arc::clone(&service_for_task);
    let task_request = request.clone();
    let running = tokio::spawn(async move {
        task_service.dispatch_test(task_request).await;
    });
    // 先等待取消目标登记并取得实际终端编号。
    let terminal = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(active) = service_for_task.active_tools.lock()
                && let Some((_, active)) = active.iter().next()
            {
                break active.terminal_id.clone();
            }
            assert!(Instant::now() < deadline, "长命令应登记为活动工具调用");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };

    // 登记发生在前端附着等待之前；继续等到 PTY 已输出命令起始标记，
    // 确保 sleep 长命令确实开始执行后再模拟宿主终止 future。
    let command_start_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let started = service_for_task
            .with_session(&terminal, |session| {
                Ok(session.phase == SessionPhase::Running
                    && String::from_utf8_lossy(&session.raw_history).contains("__TIANGONG_START_"))
            })
            .unwrap_or(false);
        if started {
            break;
        }
        assert!(
            Instant::now() < command_start_deadline,
            "长命令应在取消前进入 PTY 执行"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 模拟生产顺序：stdio runner 先触发取消信号并丢弃 dispatch
    // future，随后才调用服务的 cancel 钩子。
    let request_id = request.request.request_id.clone();
    running.abort();
    let aborted = running.await.expect_err("执行任务应被取消");
    assert!(aborted.is_cancelled(), "执行任务应先于 cancel 钩子终止");
    assert_eq!(
        service_for_task
            .active_tools
            .lock()
            .expect("活动工具表锁损坏")
            .get(&request_id)
            .map(|active| active.terminal_id.clone()),
        Some(terminal.clone()),
        "执行 future 销毁后必须保留取消目标"
    );

    service_for_task
        .cancel(&request.request)
        .await
        .expect("取消钩子不应失败");
    assert!(
        !service_for_task
            .active_tools
            .lock()
            .expect("活动工具表锁损坏")
            .contains_key(&request_id),
        "cancel 完成后应清除取消目标"
    );

    // 终端 phase 已复位：后续调用可立即复用该终端并成功执行。
    let after = outcome_of(
        service_for_task
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo recovered", "timeout": 5}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(after["ok"], true, "取消后终端应可复用: {after}");
    assert_eq!(after["stdout"], "recovered");
    assert!(
        after["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("复用空闲终端"),
        "取消后应复用被中断的终端: {}",
        after["summary"]
    );
    assert!(
        service_for_task.with_session(&terminal, |_| Ok(())).is_ok(),
        "终端应存活（取消中断命令而非杀死终端）"
    );

    service_for_task
        .kill_session(SessionIdRequest {
            session_id: terminal,
        })
        .expect("清理测试终端失败");
}

/// 测试辅助：起一条长命令并在其真正进入 PTY 后模拟 runner 丢弃
/// dispatch future，返回（服务, 执行终端编号, 原请求）。
#[cfg(unix)]
async fn 取消执行中的调用(
    payload: serde_json::Value,
    operation: &str,
    workspace: &str,
) -> (std::sync::Arc<TerminalService>, String, ScopedRequest) {
    let service = std::sync::Arc::new(TerminalService::new());
    let request = tool_request(operation, payload, Some(("session-a", workspace)));
    let task_service = std::sync::Arc::clone(&service);
    let task_request = request.clone();
    let running = tokio::spawn(async move {
        task_service.dispatch_test(task_request).await;
    });
    let terminal = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(active) = service.active_tools.lock()
                && let Some((_, active)) = active.iter().next()
            {
                break active.terminal_id.clone();
            }
            assert!(Instant::now() < deadline, "长命令应登记为活动工具调用");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    // 等到脚本真正执行到 `echo READY_TO_CANCEL`：各用例都在这行之前完成
    // 布置（起后台作业、设 trap）。不能以 start marker 为准——它在脚本
    // 正文执行前输出，负载高时取消会抢在布置之前到达，Ctrl+C 直接打断
    // 脚本，用例验证的场景根本没有发生。
    let started_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let started = service
            .with_session(&terminal, |session| {
                let raw = String::from_utf8_lossy(&session.raw_history).to_string();
                Ok(session.phase == SessionPhase::Running && raw.contains("READY_TO_CANCEL"))
            })
            .unwrap_or(false);
        if started {
            break;
        }
        assert!(
            Instant::now() < started_deadline,
            "长命令应在取消前进入 PTY"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    running.abort();
    let aborted = running.await.expect_err("执行任务应被取消");
    assert!(aborted.is_cancelled(), "执行任务应先于 cancel 钩子终止");
    (service, terminal, request)
}

/// issue #552 主场景：被取消的命令带子进程树，SIGINT 送达后收尾需要
/// 时间。cancel 必须确认 shell 真正回到提示符才放回复用池，否则下一条
/// 命令会在未收尾的终端上握手、探针进入残留进程 stdin 而永不回显。
///
/// 这里用 trap 让 SIGINT 后仍占用前台若干秒，精确复现「账面空闲、实际
/// 仍忙」的窗口；验收点是下一条命令必须正常执行，而不是超时失败。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 取消收尾缓慢的命令后下一条命令仍能正常执行() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    // SIGINT 后仍占用前台约 8 秒（远大于就绪握手的约 3.6 秒预算），模拟
    // cargo nextest 之类带子进程树的长任务收尾。占位必须用**忽略 INT 的
    // 子进程**：cancel 的就绪握手会发第二次 \x03 清残留输入，任何
    // trap 'sleep N' INT 式占位都会被第二次 INT 打穿（bash 5.3 起 source
    // 内 trap 更是直接不接管，断言退化为时序碰运气）；trap '' INT 是
    // 进程级稳定语义，也正是原始场景的形态——收尾慢的子进程不响应
    // Ctrl+C。8 秒占位期间探针超时（Unresponsive），下一条命令换新终端。
    let (service, terminal, request) = 取消执行中的调用(
        serde_json::json!({
            "cmd": "bash -c \"trap '' INT; echo READY_TO_CANCEL; sleep 8\"",
        }),
        "run_command",
        &workspace,
    )
    .await;

    service
        .cancel(&request.request)
        .await
        .expect("取消钩子不应失败");

    // 未就绪终端绝不能以 Idle 放回池——那正是 issue 的根因。
    let phase = service
        .with_session(&terminal, |session| Ok(session.phase))
        .expect("终端应保留");
    assert_ne!(
        phase,
        SessionPhase::Idle,
        "shell 未确认回到提示符时不得放回复用池"
    );

    // 下一条命令必须成功：要么复用已确认就绪的终端，要么自动换新终端。
    let after = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo recovered", "timeout": 10}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(after["ok"], true, "取消后下一条命令必须可执行: {after}");
    assert_eq!(after["stdout"], "recovered", "结果: {after}");

    // 先收集再循环：for 头部表达式里的锁守卫会活到整个循环结束，
    // 循环体内的 kill_session 将重入同一把非重入锁，自死锁。
    let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
    for session_id in session_ids {
        let _ = service.kill_session(SessionIdRequest { session_id });
    }
}

/// 取消不得杀终端：`&` / `nohup` 起的后台作业必须活下来。
///
/// 终端被 kill 会给整个进程组发 SIGHUP，用户特意放到后台长期运行的
/// 进程会被一起带走。就绪确认失败只允许把终端移出复用池，PTY 本身
/// 必须保留（用户仍可查看输出、继续输入）。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 取消不得杀死终端或其后台作业() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let flag = cwd.path().join("nohup-alive.txt");
    let flag_path = flag.to_string_lossy().to_string();
    // 后台作业：持续写标记文件；前台故意忽略 SIGINT 撑过就绪预算，
    // 逼出「确认失败」这条最危险的分支。
    let script = format!(
        "nohup sh -c 'while true; do echo alive >> {flag_path}; sleep 0.2; done' \
         >/dev/null 2>&1 &\n\
         trap '' INT\n\
         echo READY_TO_CANCEL\n\
         sleep 60\n"
    );
    let (service, terminal, request) = 取消执行中的调用(
        serde_json::json!({ "cmd": script }),
        "run_command",
        &workspace,
    )
    .await;

    service
        .cancel(&request.request)
        .await
        .expect("取消钩子不应失败");

    // 终端必须存活：这是后台作业与用户可见输出的载体。
    assert!(
        service.with_session(&terminal, |_| Ok(())).is_ok(),
        "就绪确认失败不得关闭终端（会连带 SIGHUP 杀掉 nohup/& 后台作业）"
    );
    assert!(
        service
            .with_session(&terminal, |session| Ok(session.exited_code))
            .expect("终端应保留")
            .is_none(),
        "终端 shell 不应被终止"
    );

    // 后台作业仍在持续写入：取消只放弃本次工具等待，不影响长期进程。
    // 并发负载下后台作业的首次写入可能稍晚，先等它真正开始。
    let started = Instant::now();
    let mut before = 0;
    while started.elapsed() < Duration::from_secs(5) {
        before = std::fs::metadata(&flag).map(|meta| meta.len()).unwrap_or(0);
        if before > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(before > 0, "后台作业应已开始写入标记文件");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let after = std::fs::metadata(&flag).map(|meta| meta.len()).unwrap_or(0);
    assert!(
        after > before,
        "nohup 后台作业必须在取消后继续运行: before={before} after={after}"
    );

    // 但该终端不再被自动复用，下一条命令换用新终端并正常返回。
    let next = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo fresh", "timeout": 10}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(next["ok"], true, "应换用新终端执行: {next}");
    assert_eq!(next["stdout"], "fresh");
    assert!(
        !next["summary"]
            .as_str()
            .unwrap_or_default()
            .contains(&terminal),
        "未回到提示符的终端不得被复用: {}",
        next["summary"]
    );

    // 先收集再循环：for 头部表达式里的锁守卫会活到整个循环结束，
    // 循环体内的 kill_session 将重入同一把非重入锁，自死锁。
    let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
    for session_id in session_ids {
        let _ = service.kill_session(SessionIdRequest { session_id });
    }
}

/// 前台长任务收尾后，终端应自行回到复用池，而不是永久闲置。
///
/// 就绪确认超时时遗留在输入缓冲里的探针会在前台进程退出后被 shell
/// 执行并回显，选终端据此识别恢复。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 收尾完成的终端重新回到复用池() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    // 前台任务收尾后（约 5 秒，超过就绪预算），终端应自行回到复用池。
    // 占位用**忽略 INT 的子进程**（见上例注释：cancel 的第二次 \x03 会打穿
    // trap 'sleep N' 式占位，bash 5.3 下 source 内 trap 更不接管）；
    // 5 秒后子进程自行退出，登录 shell 回提示符执行遗留探针，恢复检测
    // 命中、终端回池。
    let (service, terminal, request) = 取消执行中的调用(
        serde_json::json!({
            "cmd": "bash -c \"trap '' INT; echo READY_TO_CANCEL; sleep 5\"",
        }),
        "run_command",
        &workspace,
    )
    .await;
    service
        .cancel(&request.request)
        .await
        .expect("取消钩子不应失败");
    assert_eq!(
        service
            .with_session(&terminal, |session| Ok(session.phase))
            .expect("终端应保留"),
        SessionPhase::Unresponsive,
        "确认失败的终端应标记为不可自动复用"
    );

    // 等待前台命令自行收尾，遗留探针被 shell 执行。
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let acquired = service.acquire_session(AcquireRequest {
            scope_id: "session-a".to_string(),
        });
        if acquired.session_id.as_deref() == Some(terminal.as_str()) {
            break;
        }
        // 未选中则把预留状态还原，避免影响下一轮判断。
        if let Some(session_id) = acquired.session_id {
            let _ = service.release_session(SessionIdRequest { session_id });
        }
        assert!(
            Instant::now() < deadline,
            "前台命令收尾后终端应重新进入复用池"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let _ = service.release_session(SessionIdRequest {
        session_id: terminal.clone(),
    });
    let after = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo reusable", "timeout": 10}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(after["ok"], true, "恢复后的终端应能正常执行: {after}");
    assert_eq!(after["stdout"], "reusable");

    // 先收集再循环：for 头部表达式里的锁守卫会活到整个循环结束，
    // 循环体内的 kill_session 将重入同一把非重入锁，自死锁。
    let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
    for session_id in session_ids {
        let _ = service.kill_session(SessionIdRequest { session_id });
    }
}

/// 显式交互调用（run_shell interactive=true）已随 v7 删除（issue #697）：
/// 交互判定不再由插件执行，「取消交互调用保留终端」的保护随之消失——
/// 等待窗口内取消一律 `\x03` + 就绪确认（用户主动取消属于合理打断）；
/// 窗口交还后 active_tools 已清除，取消为 no-op，命令不受影响。
///
/// 取消发生在命令启动前（就绪握手阶段）：终端尚未收到任何输入，
/// 取消必须直接释放预留回到空闲——否则预留永远挂着，终端不再被复用。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 取消发生在命令启动前时终端回到空闲() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = std::sync::Arc::new(TerminalService::new());
    let request = tool_request(
        "run_command",
        serde_json::json!({ "cmd": "echo never-runs" }),
        Some(("session-a", workspace.as_str())),
    );
    let task_service = std::sync::Arc::clone(&service);
    let task_request = request.clone();
    let running = tokio::spawn(async move {
        task_service.dispatch_test(task_request).await;
    });
    // 登记发生在前端附着等待之前：拿到终端编号时它必处于 Reserved
    // （命令尚未写入 PTY），立即取消即命中「启动前取消」窗口。
    let terminal = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(active) = service.active_tools.lock()
                && let Some((_, active)) = active.iter().next()
            {
                break active.terminal_id.clone();
            }
            assert!(Instant::now() < deadline, "调用应登记为活动工具");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    running.abort();
    let aborted = running.await.expect_err("执行任务应被取消");
    assert!(aborted.is_cancelled());
    service
        .cancel(&request.request)
        .await
        .expect("取消钩子不应失败");

    let phase = service
        .with_session(&terminal, |session| Ok(session.phase))
        .expect("终端应存活");
    assert_eq!(phase, SessionPhase::Idle, "启动前取消必须释放预留回到空闲");

    // 取消发生在登录 shell 初始化期间：rc 可能仍在前台运行子程序，此时
    // 选终端按 #673 的规则跳过它并新开终端，属正常保护行为。这里要验证
    // 的是「预留已释放、终端回到复用池」，因此先等它回到提示符。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let at_prompt = service
            .with_session(&terminal, |session| {
                Ok(matches!(foreground_of(session), Some(Foreground::Shell)))
            })
            .expect("终端应存活");
        if at_prompt {
            break;
        }
        assert!(Instant::now() < deadline, "登录 shell 应在限时内回到提示符");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 后续命令应能复用该终端（它干净且空闲）。
    let after = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo reused", "timeout": 10}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(after["ok"], true, "{after}");
    assert_eq!(after["stdout"], "reused");
    assert!(
        after["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("复用空闲终端"),
        "应复用被释放的终端: {}",
        after["summary"]
    );

    // 先收集再循环：避免 for 头部锁守卫跨循环体重入自死锁。
    let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
    for session_id in session_ids {
        let _ = service.kill_session(SessionIdRequest { session_id });
    }
}

/// 命令超时但忽略 SIGINT 仍在前台运行：超时返回后终端必须标记为
/// 不可自动复用并保留命令（状态查询不得错报空闲、丢命令），下一条
/// 命令换新终端执行；前台收尾后遗留探针回显，终端自动恢复回池。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 超时未收尾的终端不得错报空闲() {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let service = TerminalService::new();

    let timed = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({
                    "cmd": "bash -c 'trap \"\" INT; echo RUNNING; sleep 6'",
                    "timeout": 2,
                }),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    // v7（issue #697）：窗口耗尽是软交还（ok=true、未中断），不是失败。
    assert_eq!(timed["ok"], true, "交还不是失败: {timed}");

    // 交还返回时命令仍在跑：状态必须是执行中且命令可见，不得错报空闲。
    let status = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal",
                serde_json::json!({"action": "status",}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    let overview: serde_json::Value =
        serde_json::from_str(status["stdout"].as_str().expect("概要应有 stdout"))
            .expect("概要应为 JSON");
    let stale = overview["terminals"]
        .as_array()
        .expect("概要应为数组")
        .iter()
        .find(|terminal| terminal["status"] == "running")
        .expect("交还终端应保持执行态（未错报空闲）")
        .clone();
    let stale_id = stale["terminal_id"].as_str().expect("应有编号").to_string();
    assert!(
        stale["command"]
            .as_str()
            .is_some_and(|command| command.contains("sleep 6")),
        "交还终端应保留命令: {stale}"
    );

    // 下一条命令换新终端正常执行，不撞仍在跑的前台命令。
    let next = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo fresh", "timeout": 10}),
                Some(("session-a", workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(next["ok"], true, "应换新终端执行: {next}");
    assert_eq!(next["stdout"], "fresh");
    assert!(
        !next["summary"]
            .as_str()
            .unwrap_or_default()
            .contains(&stale_id),
        "未收尾终端不得被复用: {}",
        next["summary"]
    );

    // 前台命令自行收尾（sleep 6 结束）后，遗留探针被 shell 执行回显；
    // 恢复检测在选终端时进行，经 acquire 轮询驱动。
    let recovered_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let acquired = service.acquire_session(AcquireRequest {
            scope_id: "session-a".to_string(),
        });
        if let Some(selected) = acquired.session_id {
            let recovered = selected == stale_id;
            let _ = service.release_session(SessionIdRequest {
                session_id: selected,
            });
            if recovered {
                break;
            }
        }
        assert!(
            Instant::now() < recovered_deadline,
            "前台收尾后终端应自动恢复回池"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // 先收集再循环：避免 for 头部锁守卫跨循环体重入自死锁。
    let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
    for session_id in session_ids {
        let _ = service.kill_session(SessionIdRequest { session_id });
    }
}
