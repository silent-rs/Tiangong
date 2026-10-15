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
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 交互程序内的命令以提示符重新出现判定完成() {
    let service = TerminalService::new();
    let (terminal, _cwd) = ready_terminal(&service).await;

    // 进入一个固定提示符的嵌套 shell，模拟 SSH 远端 shell。
    let entered = send(
        &service,
        &terminal,
        "env -i PS1='remote$ ' TERM=dumb /bin/sh",
        true,
        5,
    )
    .await;
    assert!(
        summary(&entered).contains("交互程序"),
        "进入嵌套 shell 应判为交互程序: {entered}"
    );

    let running = send(&service, &terminal, "sleep 4; echo remote-done", true, 1).await;
    assert!(
        summary(&running).contains("仍在执行"),
        "远端 sleep 不得被判为完成: {running}"
    );
    let waited = send(&service, &terminal, "", false, 10).await;
    assert!(
        summary(&waited).contains("执行完成"),
        "提示符重新出现后应完成: {waited}"
    );
    assert!(stdout(&waited).contains("remote-done"), "{waited}");

    let echoed = send(&service, &terminal, "echo hello-remote", true, 5).await;
    assert!(summary(&echoed).contains("执行完成"), "{echoed}");
    assert!(
        stdout(&echoed).contains("hello-remote"),
        "后续命令结果必须当次返回，不得滞后: {echoed}"
    );

    service
        .kill_session(SessionIdRequest {
            session_id: terminal,
        })
        .expect("清理测试终端失败");
}

/// run_command 不得复用前台被程序占据的「空闲」终端：terminal_send 进入
/// ssh / REPL 后 phase 仍是 Idle，往里写本地包装脚本会被远端当输入执行。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 选终端跳过前台被程序占据的终端() {
    let service = TerminalService::new();
    let (terminal, cwd) = ready_terminal(&service).await;
    let workspace = cwd.path().to_string_lossy().to_string();

    send(
        &service,
        &terminal,
        "env -i PS1='remote$ ' /bin/sh",
        true,
        5,
    )
    .await;
    let executed = outcome_of(
        service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "echo", "args": ["local-ok"]}),
                Some((SCOPE, workspace.as_str())),
            ))
            .await,
    );
    assert_eq!(executed["ok"], true, "{executed}");
    assert!(
        summary(&executed).contains("新终端") && !summary(&executed).contains(&terminal),
        "前台被占据的终端不得复用: {executed}"
    );
    assert!(stdout(&executed).contains("local-ok"), "{executed}");

    let other = summary(&executed)
        .split("新终端 ")
        .nth(1)
        .and_then(|rest| rest.split('（').next())
        .map(str::to_string);
    for id in [Some(terminal), other].into_iter().flatten() {
        let _ = service.kill_session(SessionIdRequest { session_id: id });
    }
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
