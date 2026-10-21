//! stdio 传输端到端测试（RFC 0017 S2）。
//!
//! 真实 spawn 测试用 echo sidecar 二进制，验证：
//! Auth 首帧 → 握手 → 请求往返 → 进程崩溃后自动换代重启。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tiangong_plugin_runtime::manifest::SidecarLifecycle;
use tiangong_plugin_runtime::sidecar::{
    SidecarConfig, SidecarConnection, SidecarInvocationContext, StdioSidecarConnection,
};
use tiangong_plugin_sidecar::protocol::PROTOCOL_VERSION;

fn echo_sidecar_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_test-stdio-sidecar"))
}

#[cfg(target_os = "macos")]
fn test_host_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_test-stdio-host"))
}

fn temp_paths(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base =
        std::env::temp_dir().join(format!("tiangong-stdio-e2e-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    (
        base.join("endpoint.json"),
        base.join("sidecar.log"),
        base.join("data"),
    )
}

fn connection(tag: &str) -> StdioSidecarConnection {
    let (endpoint, log, data_dir) = temp_paths(tag);
    let config = SidecarConfig::new(
        "test-stdio",
        "0.0.0",
        echo_sidecar_binary(),
        endpoint,
        log,
        data_dir,
        std::env::temp_dir(),
    )
    .with_timeouts(Duration::from_secs(10), Duration::from_secs(10));
    StdioSidecarConnection::new(config)
}

#[test]
#[serial_test::serial]
fn stdio_roundtrip_and_restart() {
    let can_terminate = tiangong_plugin_runtime::test_support::can_terminate_child_processes();
    let connection = connection("roundtrip");

    // 请求往返：echo 原样回显。
    let payload = serde_json::json!({"text": "hello stdio"});
    let raw = connection.invoke("echo", &payload.to_string()).unwrap();
    let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(response["text"], "hello stdio");

    // crash 使子进程退出；下一次请求应自动换代重启。
    let _ = connection.invoke("crash", "{}");
    std::thread::sleep(Duration::from_millis(200));
    let raw = connection.invoke("echo", r#"{"after":"restart"}"#).unwrap();
    let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(response["after"], "restart");

    tiangong_plugin_runtime::test_support::finish_stdio_connection(&connection, can_terminate);
}

#[test]
#[serial_test::serial]
fn resident_process_handshakes_once_across_calls() {
    let can_terminate = tiangong_plugin_runtime::test_support::can_terminate_child_processes();
    let (endpoint, log, data_dir) = temp_paths("handshake-once");
    let config = SidecarConfig::new(
        "test-stdio",
        "0.0.0",
        echo_sidecar_binary(),
        endpoint,
        log,
        data_dir,
        std::env::temp_dir(),
    )
    .with_lifecycle(SidecarLifecycle::Resident)
    .with_timeouts(Duration::from_secs(10), Duration::from_secs(10));
    let connection = StdioSidecarConnection::new(config);

    // 常驻进程：多次业务调用共享同一代次，就绪握手只发生一次。
    for index in 0..3 {
        let raw = connection
            .invoke("echo", &format!(r#"{{"index":{index}}}"#))
            .unwrap();
        assert!(raw.contains(&format!("\"index\":{index}")));
    }
    let raw = connection.invoke("handshake_count", "{}").unwrap();
    let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(response["count"], 1, "常驻进程多次调用不得重复握手");

    tiangong_plugin_runtime::test_support::finish_stdio_connection(&connection, can_terminate);
}

#[test]
#[serial_test::serial]
fn unresponsive_handshake_fails_within_start_timeout() {
    // 协议沉默对端：进程存活但不应答握手，验证启动/握手有限时限
    // 执行路径包含临时验证进程的强制清理：不能终止子进程的环境里
    // 清理错误会优先于握手错误返回，错误形态无法稳定断言——整体跳过。
    if !tiangong_plugin_runtime::test_support::can_terminate_child_processes() {
        return;
    }
    //（导入验证与就绪握手共用该保护）。
    // SAFETY: 本测试与所有会 spawn echo 子进程的测试经 #[serial] 互斥，
    // 设置与清除期间无并发 spawn，避免 MUTE 泄漏污染其他用例。
    unsafe { std::env::set_var("TIANGONG_TEST_STDIO_MUTE", "1") };
    let (endpoint, log, data_dir) = temp_paths("mute");
    let config = SidecarConfig::new(
        "test-stdio",
        "0.0.0",
        echo_sidecar_binary(),
        endpoint,
        log,
        data_dir,
        std::env::temp_dir(),
    )
    .with_timeouts(Duration::from_millis(500), Duration::from_secs(5));
    let connection = StdioSidecarConnection::new(config);
    let started = Instant::now();
    let error = connection
        .ensure_running()
        .expect_err("无响应握手必须在时限内失败");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "握手失败耗时 {} 超出时限预期",
        started.elapsed().as_millis()
    );
    assert!(
        error.to_string().contains("就绪握手失败"),
        "错误应指向就绪握手：{error:#}"
    );
    connection.stop().unwrap();
    // SAFETY: 与 spawn 类测试经 #[serial] 互斥，此处清除不会与其他
    // 测试的子进程环境竞争。
    unsafe { std::env::remove_var("TIANGONG_TEST_STDIO_MUTE") };
}

#[test]
#[serial_test::serial]
fn stdio_handshake_reports_identity() {
    let can_terminate = tiangong_plugin_runtime::test_support::can_terminate_child_processes();
    let connection = connection("handshake");
    // 握手经 runtime.handshake 完成（ensure_running 内部调用）；
    // 随后正常请求可用即证明身份校验通过。
    let raw = connection.invoke("echo", r#"{"probe":1}"#).unwrap();
    assert!(raw.contains("probe"));

    // 错误操作经 Response 错误信封返回（与 TCP 语义一致）。
    let error = connection.invoke("no-such-op", "{}").unwrap_err();
    assert!(error.to_string().contains("未知操作"));
    let _ = PROTOCOL_VERSION;
    tiangong_plugin_runtime::test_support::finish_stdio_connection(&connection, can_terminate);
}

#[test]
#[serial_test::serial]
fn resident_request_cancel_returns_immediately_and_keeps_sidecar_running() {
    let (endpoint, log, data_dir) = temp_paths("request-cancel");
    let config = SidecarConfig::new(
        "test-stdio",
        "0.0.0",
        echo_sidecar_binary(),
        endpoint,
        log,
        data_dir,
        std::env::temp_dir(),
    )
    .with_lifecycle(SidecarLifecycle::Resident)
    .with_timeouts(Duration::from_secs(10), Duration::from_secs(10));
    let connection = Arc::new(StdioSidecarConnection::new(config));
    let invoking = Arc::clone(&connection);
    let request = std::thread::spawn(move || {
        invoking.invoke_with_context(
            "delay",
            r#"{"millis":5000}"#,
            &SidecarInvocationContext {
                session_id: "cancel-target".to_string(),
                invocation_id: "tool-delay".to_string(),
                authoritative_workspace: std::env::temp_dir(),
            },
        )
    });

    // 等待业务请求（握手请求完成后）登记 pending；在它自然完成前取消。
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && connection.active_request_count() == 0 {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(connection.active_request_count(), 1, "delay 请求未及时登记");
    let cancelled_at = Instant::now();
    connection.cancel_session("cancel-target").unwrap();
    let result = request.join().unwrap();
    assert!(result.unwrap_err().to_string().contains("请求已取消"));
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(2),
        "请求级取消必须在 2 秒内唤醒调用方"
    );

    // Cancel 只结束目标请求；同一常驻 sidecar 继续处理后续请求。
    let raw = connection.invoke("echo", r#"{"after":"cancel"}"#).unwrap();
    let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(response["after"], "cancel");
    // 避免 fixture 的显式进程回收干扰请求级取消断言；进程生命周期由
    // 既有 stop/宿主崩溃用例单独覆盖。
    std::mem::forget(connection);
}

/// 会话级联取消：取消一个会话时，同一常驻 sidecar 上该会话的全部请求
///（含新版统一调用上下文的请求）一起取消；其他会话不受影响；常驻进程
/// 继续服务后续调用。
#[test]
#[serial_test::serial]
fn session_cancel_cascades_within_session_across_context_forms() {
    let (endpoint, log, data_dir) = temp_paths("session-cascade");
    let config = SidecarConfig::new(
        "test-stdio",
        "0.0.0",
        echo_sidecar_binary(),
        endpoint,
        log,
        data_dir,
        std::env::temp_dir(),
    )
    .with_lifecycle(SidecarLifecycle::Resident)
    .with_timeouts(Duration::from_secs(10), Duration::from_secs(10));
    let connection = Arc::new(StdioSidecarConnection::new(config));

    // 会话 A：旧版上下文 + 新版上下文各一个慢请求；会话 B：一个慢请求。
    let legacy = Arc::clone(&connection);
    let legacy_request = std::thread::spawn(move || {
        legacy.invoke_with_context(
            "delay",
            r#"{"millis":8000}"#,
            &SidecarInvocationContext {
                session_id: "session-a".to_string(),
                invocation_id: "legacy-delay".to_string(),
                authoritative_workspace: std::env::temp_dir(),
            },
        )
    });
    let runtime = Arc::clone(&connection);
    let runtime_request = std::thread::spawn(move || {
        runtime.invoke_with_invocation_context_and_progress(
            "delay",
            r#"{"millis":8000}"#,
            &tiangong_plugin_runtime::protocol::RequestInvocationContext {
                session_id: "session-a".to_string(),
                invocation_id: "runtime-delay".to_string(),
                workspace: std::env::temp_dir().to_string_lossy().into_owned(),
                actor_id: "agent".to_string(),
                deadline_ms: None,
            },
            &mut |_| {},
        )
    });
    let other = Arc::clone(&connection);
    let other_request = std::thread::spawn(move || {
        other.invoke_with_context(
            "delay",
            r#"{"millis":1500}"#,
            &SidecarInvocationContext {
                session_id: "session-b".to_string(),
                invocation_id: "other-delay".to_string(),
                authoritative_workspace: std::env::temp_dir(),
            },
        )
    });

    // 三个请求都登记后再取消，避免竞态漏掉慢启动的请求。
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && connection.active_request_count() < 3 {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        connection.active_request_count(),
        3,
        "三个并发请求未全部登记"
    );
    connection.cancel_session("session-a").unwrap();

    let legacy_result = legacy_request.join().unwrap();
    assert!(
        legacy_result
            .unwrap_err()
            .to_string()
            .contains("请求已取消"),
        "旧版上下文请求应随会话取消"
    );
    let runtime_result = runtime_request.join().unwrap();
    assert!(
        runtime_result
            .unwrap_err()
            .to_string()
            .contains("请求已取消"),
        "新版统一调用上下文请求应随会话取消"
    );
    let other_result = other_request.join().unwrap();
    assert!(other_result.is_ok(), "其他会话的请求不受级联取消影响");

    // 常驻进程不退出：取消后仍可正常执行后续调用。
    let raw = connection.invoke("echo", r#"{"after":"cascade"}"#).unwrap();
    let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(response["after"], "cascade");
    std::mem::forget(connection);
}

// 唯一使用者是 macOS 专属测试：非 macOS 编译时函数一并裁掉，
// 避免 Linux clippy 的 dead_code 告警。
#[cfg(target_os = "macos")]
fn native_sandbox_skip_reason() -> Option<String> {
    tiangong_plugin_runtime::test_support::native_sandbox_skip_reason()
}

#[cfg(target_os = "macos")]
#[test]
#[serial_test::serial]
fn macos_host_crash_kills_busy_sidecar_process_group() {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    if let Some(reason) = native_sandbox_skip_reason() {
        eprintln!("跳过：当前环境无法应用原生沙箱（kill 会被拒）：{reason}");
        return;
    }

    let base =
        std::env::temp_dir().join(format!("tiangong-stdio-host-crash-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let child = Command::new(test_host_binary())
        .arg(echo_sidecar_binary())
        .arg(&base)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = MacProcessGuard {
        host: child,
        sidecar_pid: None,
    };

    let sidecar_pid = wait_for_pid_file(&base.join("sidecar.pid"), Duration::from_secs(10));
    let child_pid = wait_for_pid_file(&base.join("child.pid"), Duration::from_secs(10));
    guard.sidecar_pid = Some(sidecar_pid);
    assert!(process_alive(sidecar_pid));
    assert!(process_alive(child_pid));

    unsafe { libc::kill(guard.host.id() as libc::pid_t, libc::SIGKILL) };
    let _ = guard.host.wait();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && (process_alive(sidecar_pid) || process_alive(child_pid)) {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!process_alive(sidecar_pid), "宿主退出后 sidecar 仍存活");
    assert!(!process_alive(child_pid), "宿主退出后后台子进程仍存活");
    guard.sidecar_pid = None;
}

#[cfg(target_os = "macos")]
struct MacProcessGuard {
    host: std::process::Child,
    sidecar_pid: Option<libc::pid_t>,
}

#[cfg(target_os = "macos")]
impl Drop for MacProcessGuard {
    fn drop(&mut self) {
        unsafe { libc::kill(self.host.id() as libc::pid_t, libc::SIGKILL) };
        let _ = self.host.wait();
        if let Some(pid) = self.sidecar_pid {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    }
}

#[cfg(target_os = "macos")]
fn wait_for_pid_file(path: &std::path::Path, timeout: Duration) -> libc::pid_t {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(raw) = std::fs::read_to_string(path)
            && let Ok(pid) = raw.trim().parse::<libc::pid_t>()
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("等待 PID 文件超时: {}", path.display());
}

#[cfg(target_os = "macos")]
fn process_alive(pid: libc::pid_t) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
