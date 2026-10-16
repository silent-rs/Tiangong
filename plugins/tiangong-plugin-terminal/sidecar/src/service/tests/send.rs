//! terminal_send 的命令完成判定（issue #673）与选终端避让（issue #673
//! 的 run_command 复用场景），全部用真实 PTY 端到端验证。

use super::*;

const SCOPE: &str = "session-send";

/// 经 run_command 建立一个已就绪的终端，返回 (终端编号, 工作目录守卫)。
async fn ready_terminal(service: &TerminalService) -> (String, tempfile::TempDir) {
    let cwd = tempfile::tempdir().expect("创建测试目录失败");
    let workspace = cwd.path().to_string_lossy().to_string();
    let first = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo", "args": ["ready"]}),
                Some((SCOPE, workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(first["ok"], true, "建立终端失败: {first}");
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
    (terminal, cwd)
}

async fn send(
    service: &TerminalService,
    terminal: &str,
    input: &str,
    enter: bool,
    wait: u64,
) -> serde_json::Value {
    outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_send",
                serde_json::json!({
                    "terminal_id": terminal,
                    "input": input,
                    "enter": enter,
                    "wait": wait,
                }),
                Some((SCOPE, "")),
            ))
            .await,
    )
}

fn summary(outcome: &serde_json::Value) -> &str {
    outcome["summary"].as_str().unwrap_or_default()
}

fn stdout(outcome: &serde_json::Value) -> &str {
    outcome["stdout"].as_str().unwrap_or_default()
}

/// issue #673 的核心复现：`sleep` 回显后长时间静默，旧实现 300ms 即返回
/// 「成功」。现在必须如实返回 still_running，拒绝紧随其后的新命令（不写入），
/// 并在命令结束后以 completed 收尾，后续命令输出不再错位。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 静默长命令返回仍在执行且不接受新命令() {
    let service = TerminalService::new();
    let (terminal, _cwd) = ready_terminal(&service).await;

    let started = Instant::now();
    let first = send(&service, &terminal, "sleep 4; echo sleep-done", true, 1).await;
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "wait 期间命令未完成，不得提前返回: {:?}",
        started.elapsed()
    );
    assert!(
        summary(&first).contains("仍在执行"),
        "应返回仍在执行: {first}"
    );

    // 命令未结束时发送新命令：必须拒绝且不写入 PTY。
    let rejected = service
        .dispatch_test(tool_request(
            "terminal_send",
            serde_json::json!({
                "terminal_id": terminal,
                "input": "echo should-not-run",
                "enter": true,
                "wait": 1,
            }),
            Some((SCOPE, "")),
        ))
        .await;
    let rejected = rejected.payload.expect("应有结果");
    assert_eq!(rejected["ok"], false, "未完成时新命令应被拒绝: {rejected}");
    assert!(summary(&rejected).contains("未发送"), "{rejected}");

    // 空输入继续等待：命令结束后返回 completed 与新增输出。
    let waited = send(&service, &terminal, "", false, 10).await;
    assert!(
        summary(&waited).contains("执行完成"),
        "命令结束后应返回完成: {waited}"
    );
    assert!(stdout(&waited).contains("sleep-done"), "{waited}");

    // 后续命令正常执行，被拒绝的命令从未写入。
    let next = send(&service, &terminal, "echo next-$((40+2))", true, 5).await;
    assert!(summary(&next).contains("执行完成"), "{next}");
    assert!(stdout(&next).contains("next-42"), "{next}");
    let history = service
        .with_session(&terminal, |session| {
            Ok(String::from_utf8_lossy(&session.raw_history).to_string())
        })
        .expect("读取输出失败");
    assert!(
        !history.contains("should-not-run"),
        "被拒绝的命令不得写入终端"
    );

    service
        .kill_session(SessionIdRequest {
            session_id: terminal,
        })
        .expect("清理测试终端失败");
}

/// 秒级命令在 wait 内完成：一次调用直接拿到完整输出，不必等满 wait。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 短命令完成即返回不等满上限() {
    let service = TerminalService::new();
    let (terminal, _cwd) = ready_terminal(&service).await;
    let started = Instant::now();
    let done = send(&service, &terminal, "sleep 1; echo quick-done", true, 30).await;
    assert!(summary(&done).contains("执行完成"), "{done}");
    assert!(stdout(&done).contains("quick-done"), "{done}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "完成即返回，实际耗时 {:?}",
        started.elapsed()
    );
    service
        .kill_session(SessionIdRequest {
            session_id: terminal,
        })
        .expect("清理测试终端失败");
}

/// SSH 场景的等价复现：在终端里进入一个嵌套的交互 shell（前台是子
/// 程序，读不到其内部进程状态），`sleep` 仍须判为执行中，提示符重新
/// 出现后判为完成。
///
/// 两种嵌套 shell 都要覆盖：带行编辑器的 shell 会把 tty 切成非规范模式；
/// dash（Linux 的 /bin/sh、Debian 系远端的默认 sh）不切，tty 保持规范
/// 模式，只能按「静默 + 光标停在提示符行」识别。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 交互程序内的命令以提示符重新出现判定完成() {
    let shells: Vec<&str> = ["/bin/sh", "/bin/dash"]
        .into_iter()
        .filter(|shell| std::path::Path::new(shell).exists())
        .collect();
    for shell in shells {
        let service = TerminalService::new();
        let (terminal, _cwd) = ready_terminal(&service).await;

        // 进入一个固定提示符的嵌套 shell，模拟 SSH 远端 shell。
        let entered = send(
            &service,
            &terminal,
            &format!("env -i PS1='remote$ ' TERM=dumb {shell}"),
            true,
            8,
        )
        .await;
        assert!(
            summary(&entered).contains("交互程序"),
            "{shell}：进入嵌套 shell 应判为交互程序: {entered}"
        );

        let running = send(&service, &terminal, "sleep 4; echo remote-done", true, 1).await;
        assert!(
            summary(&running).contains("仍在执行"),
            "{shell}：远端 sleep 不得被判为完成: {running}"
        );
        let waited = send(&service, &terminal, "", false, 10).await;
        assert!(
            summary(&waited).contains("执行完成"),
            "{shell}：提示符重新出现后应完成: {waited}"
        );
        assert!(stdout(&waited).contains("remote-done"), "{shell}: {waited}");

        let echoed = send(&service, &terminal, "echo hello-remote", true, 5).await;
        assert!(summary(&echoed).contains("执行完成"), "{shell}: {echoed}");
        assert!(
            stdout(&echoed).contains("hello-remote"),
            "{shell}：后续命令结果必须当次返回，不得滞后: {echoed}"
        );

        // 退出嵌套 shell 回到登录 shell：出现的是登录 shell 的提示符，
        // 也必须判为完成，不能让登记残留、拒绝后续所有输入。
        let exited = send(&service, &terminal, "exit", true, 5).await;
        assert!(summary(&exited).contains("执行完成"), "{shell}: {exited}");
        let back = send(&service, &terminal, "echo back-home", true, 5).await;
        assert!(summary(&back).contains("执行完成"), "{shell}: {back}");
        assert!(stdout(&back).contains("back-home"), "{shell}: {back}");

        service
            .kill_session(SessionIdRequest {
                session_id: terminal,
            })
            .expect("清理测试终端失败");
    }
}

/// 交互程序内提示符变了（远端 `cd` 改了带路径的提示符等）：等不到发送前
/// 的提示符，也不能让登记残留拒绝后续所有命令。静默且光标停在新提示符
/// 上即交还控制权，下一条命令以新提示符为基线正常判定完成。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 交互程序内提示符改变后不锁死终端() {
    let shells: Vec<&str> = ["/bin/sh", "/bin/dash"]
        .into_iter()
        .filter(|shell| std::path::Path::new(shell).exists())
        .collect();
    for shell in shells {
        let service = TerminalService::new();
        let (terminal, _cwd) = ready_terminal(&service).await;
        send(
            &service,
            &terminal,
            &format!("env -i PS1='remote$ ' TERM=dumb {shell}"),
            true,
            8,
        )
        .await;

        let changed = send(&service, &terminal, "PS1='changed> '", true, 8).await;
        assert!(
            !summary(&changed).contains("仍在执行"),
            "{shell}：提示符改变后应交还控制权: {changed}"
        );
        let next = send(&service, &terminal, "echo after-$((40+2))", true, 5).await;
        assert!(summary(&next).contains("执行完成"), "{shell}: {next}");
        assert!(stdout(&next).contains("after-42"), "{shell}: {next}");

        service
            .kill_session(SessionIdRequest {
                session_id: terminal,
            })
            .expect("清理测试终端失败");
    }
}

/// REPL 多行输入：续行提示符 `...` 不是发送前的 `>>>`，必须交还控制权
/// 让下一行能写入；空行结束代码块后 `>>>` 重新出现，判为完成。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repl_多行输入续行提示符不锁死终端() {
    let Some(python) = [
        "/usr/bin/python3",
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
    ]
    .into_iter()
    .find(|path| std::path::Path::new(path).exists()) else {
        return;
    };
    let service = TerminalService::new();
    let (terminal, _cwd) = ready_terminal(&service).await;
    // 基础 REPL（3.13+ 默认的新 REPL 会重绘整行），关闭用户启动脚本。
    let entered = send(
        &service,
        &terminal,
        &format!("env -i PYTHON_BASIC_REPL=1 TERM=dumb {python} -q -I"),
        true,
        10,
    )
    .await;
    assert!(summary(&entered).contains("交互程序"), "{entered}");

    let header = send(&service, &terminal, "for i in range(3):", true, 8).await;
    assert!(
        !summary(&header).contains("仍在执行"),
        "续行提示符出现后应交还控制权: {header}"
    );
    let body = send(&service, &terminal, "    print('loop', i * 7)", true, 8).await;
    assert_eq!(body["ok"], true, "续行内容不得被拒绝: {body}");
    let done = send(&service, &terminal, "", true, 8).await;
    assert!(summary(&done).contains("执行完成"), "{done}");
    assert!(stdout(&done).contains("loop 14"), "{done}");

    let exited = send(&service, &terminal, "exit()", true, 8).await;
    assert!(summary(&exited).contains("执行完成"), "{exited}");
    let back = send(&service, &terminal, "echo back-home", true, 5).await;
    assert!(stdout(&back).contains("back-home"), "{back}");

    service
        .kill_session(SessionIdRequest {
            session_id: terminal,
        })
        .expect("清理测试终端失败");
}

/// 交互程序内的中断键：不带回车的 `\x03` 只写给程序、不清除登记。
/// 程序真被中断时提示符重绘、按规则完成，下一条命令立即执行；忽略中断
///（`trap '' INT`）仍在跑时继续拒绝新命令，不能让它排进 tty 输入缓冲、
/// 等程序结束才执行。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 交互程序内中断键只在程序真正结束后放行新命令() {
    let service = TerminalService::new();
    let (terminal, _cwd) = ready_terminal(&service).await;
    send(
        &service,
        &terminal,
        "env -i PS1='remote$ ' TERM=dumb /bin/sh",
        true,
        8,
    )
    .await;

    // 可中断的命令：中断后立即完成，新命令当场执行。
    let running = send(&service, &terminal, "sleep 30", true, 1).await;
    assert!(summary(&running).contains("仍在执行"), "{running}");
    let started = Instant::now();
    send(&service, &terminal, "\\x03", false, 3).await;
    let next = send(&service, &terminal, "echo interrupted-ok", true, 5).await;
    assert_eq!(next["ok"], true, "中断生效后新命令应被接受: {next}");
    assert!(stdout(&next).contains("interrupted-ok"), "{next}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "sleep 30 应被真正中断，而不是等它跑完: {:?}",
        started.elapsed()
    );

    // 忽略中断的命令：中断键无效，新命令仍被拒绝且不写入。
    let stuck = send(&service, &terminal, "trap '' INT; sleep 4", true, 1).await;
    assert!(summary(&stuck).contains("仍在执行"), "{stuck}");
    send(&service, &terminal, "\\x03", false, 1).await;
    let rejected = send(&service, &terminal, "echo queued-too-early", true, 1).await;
    assert_eq!(rejected["ok"], false, "程序未结束时仍应拒绝: {rejected}");
    let waited = send(&service, &terminal, "", false, 10).await;
    assert!(summary(&waited).contains("执行完成"), "{waited}");
    let history = service
        .with_session(&terminal, |session| {
            Ok(String::from_utf8_lossy(&session.raw_history).to_string())
        })
        .expect("读取输出失败");
    assert!(
        !history.contains("queued-too-early"),
        "被拒绝的命令不得写入终端"
    );

    service
        .kill_session(SessionIdRequest {
            session_id: terminal,
        })
        .expect("清理测试终端失败");
}

/// 并行发出 terminal_send 与 run_command：terminal_send 判定完成、清除
/// 登记后还要等提示符绘制完整才返回，这段收尾窗口里选终端不得把该终端
/// 交给 run_command，否则包装脚本会混进 terminal_send 的输出。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 进行中的terminal_send终端不被run_command复用() {
    let service = Arc::new(TerminalService::new());
    let (terminal, _cwd) = ready_terminal(&service).await;

    for round in 0..3 {
        let sender = {
            let service = Arc::clone(&service);
            let terminal = terminal.clone();
            tokio::spawn(async move {
                send(
                    &service,
                    &terminal,
                    &format!("sleep 1; echo send-{round}"),
                    true,
                    10,
                )
                .await
            })
        };
        // 等到「登记已清除（判定完成）、调用尚未返回」的收尾窗口。
        let mut seen_pending = false;
        let mut probed = false;
        while !sender.is_finished() {
            let (pending, in_flight) = service
                .with_session(&terminal, |session| {
                    Ok((session.pending_send.is_some(), session.send_in_flight))
                })
                .expect("读取终端状态失败");
            seen_pending |= pending;
            if seen_pending && !pending && in_flight > 0 {
                let acquired = service.acquire_session(AcquireRequest {
                    scope_id: SCOPE.to_string(),
                });
                if let Some(id) = acquired.session_id {
                    let _ = service.release_session(SessionIdRequest {
                        session_id: id.clone(),
                    });
                    assert_ne!(id, terminal, "第 {round} 轮：收尾中的终端被选中");
                }
                probed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let sent = sender.await.expect("terminal_send 任务失败");
        assert!(probed, "第 {round} 轮未观察到收尾窗口: {sent}");
        assert!(stdout(&sent).contains(&format!("send-{round}")), "{sent}");
        assert!(!stdout(&sent).contains("__TIANGONG_"), "{sent}");
    }

    // 调用结束后守卫撤销登记，终端重新回到复用池。
    let acquired = service.acquire_session(AcquireRequest {
        scope_id: SCOPE.to_string(),
    });
    assert_eq!(acquired.session_id.as_deref(), Some(terminal.as_str()));

    let _ = service.kill_session(SessionIdRequest {
        session_id: terminal,
    });
}

/// issue #672 验证要求：终端里 `nohup … &` 起的进程（含孙进程），在终端
/// 关闭后仍能经 terminal_status 查到 pid，并能被结束；查询不越出本会话。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 终端关闭后遗留的后台作业可查询并结束() {
    let service = TerminalService::new();
    let (terminal, cwd) = ready_terminal(&service).await;
    let workspace = cwd.path().to_string_lossy().to_string();

    // 后台进程再派生子进程：覆盖孙进程与进程组。
    send(
        &service,
        &terminal,
        "nohup sh -c 'sleep 600 & sleep 600' >/dev/null 2>&1 &",
        true,
        5,
    )
    .await;
    service
        .kill_session(SessionIdRequest {
            session_id: terminal.clone(),
        })
        .expect("关闭终端失败");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let status = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_status",
                serde_json::json!({}),
                Some((SCOPE, workspace.as_str())),
            ))
            .await,
    );
    let payload: serde_json::Value = serde_json::from_str(stdout(&status)).expect("概要应为 JSON");
    let jobs = payload["background_jobs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let sleeps: Vec<i32> = jobs
        .iter()
        .filter(|job| job["name"] == "sleep" && job["terminal_id"] == terminal.as_str())
        .filter_map(|job| job["pid"].as_i64().map(|pid| pid as i32))
        .collect();
    assert!(sleeps.len() >= 2, "后台进程及其子进程都应列出: {status}");
    assert!(
        jobs.iter().all(|job| job["terminal_alive"] == false),
        "终端已关闭，作业应标记为孤儿: {status}"
    );
    assert!(summary(&status).contains("遗留后台进程"), "{status}");

    // 其他宿主会话看不到这些作业。
    let other = outcome_of(
        service
            .dispatch_test(tool_request(
                "terminal_status",
                serde_json::json!({}),
                Some(("session-other", workspace.as_str())),
            ))
            .await,
    );
    assert!(!summary(&other).contains("遗留后台进程"), "{other}");

    // 同一 sidecar 内可结束它们，结束后不再列出。
    for pid in &sleeps {
        // SAFETY: 结束本测试启动的进程。
        assert_eq!(
            unsafe { libc::kill(*pid, libc::SIGKILL) },
            0,
            "应能结束 {pid}"
        );
    }
    let mut remaining = Vec::new();
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        remaining = service.jobs.live_jobs(Some(SCOPE), |_| false);
        if remaining.iter().all(|job| !sleeps.contains(&job.pid)) {
            break;
        }
    }
    assert!(
        remaining.iter().all(|job| !sleeps.contains(&job.pid)),
        "结束后不应再列出: {remaining:?}"
    );
    for job in remaining {
        // SAFETY: 清理本测试遗留的 sh 包装进程。
        unsafe {
            libc::kill(job.pid, libc::SIGKILL);
        }
    }
}

/// 用法引导只提示、不拦截：nohup / & 与现写多行脚本照常执行，摘要末尾
/// 给出替代做法；普通命令不附提示。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 后台启动与多行脚本附带用法提示且照常执行() {
    let service = TerminalService::new();
    let (terminal, cwd) = ready_terminal(&service).await;
    let workspace = cwd.path().to_string_lossy().to_string();
    let run_shell = |script: &str| {
        tool_request(
            "run_shell",
            serde_json::json!({ "script": script }),
            Some((SCOPE, workspace.as_str())),
        )
    };

    let background = outcome_of(
        service
            .dispatch_test(run_shell("nohup sleep 30 >/dev/null 2>&1 & echo started"))
            .await,
    );
    assert_eq!(background["ok"], true, "{background}");
    assert!(stdout(&background).contains("started"), "命令照常执行");
    assert!(
        summary(&background).contains("terminal_open"),
        "后台启动应提示改用独立终端: {background}"
    );

    let script = outcome_of(
        service
            .dispatch_test(run_shell("echo one\necho two\necho three\necho four"))
            .await,
    );
    assert_eq!(script["ok"], true, "{script}");
    assert!(stdout(&script).contains("four"), "多行脚本照常执行");
    assert!(
        summary(&script).contains("逐步执行"),
        "多行脚本应提示拆分: {script}"
    );

    let plain = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo", "args": ["plain"]}),
                Some((SCOPE, workspace.as_str())),
            ))
            .await,
    );
    assert!(
        !summary(&plain).contains("提示"),
        "普通命令不附提示: {plain}"
    );

    for job in service.jobs.live_jobs(Some(SCOPE), |_| false) {
        // SAFETY: 清理本测试启动的后台进程。
        unsafe {
            libc::kill(job.pid, libc::SIGKILL);
        }
    }
    let ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
    let _ = terminal;
    for session_id in ids {
        let _ = service.kill_session(SessionIdRequest { session_id });
    }
}
