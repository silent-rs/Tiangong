//! issue #485：制品级黑盒测试。仅使用 Launcher CLI，不依赖 Runtime。
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tiangong_sandbox::SandboxPolicy;

static SERIAL: Mutex<()> = Mutex::new(());
const LAUNCHER: &str = env!("CARGO_BIN_EXE_tiangong-sandbox");

struct Process {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
    #[cfg(windows)]
    stop_event: WindowsStopEvent,
    #[cfg(unix)]
    liveness: std::os::unix::net::UnixStream,
}
impl Process {
    fn spawn(command: &mut Command, directory: &Path) -> Self {
        let id = scru128::new().to_string();
        let stdout = directory.join(format!("{id}.stdout"));
        let stderr = directory.join(format!("{id}.stderr"));
        command
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap());
        #[cfg(unix)]
        let (liveness, inherited) = std::os::unix::net::UnixStream::pair().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            liveness.set_nonblocking(true).unwrap();
            // stdin 由 Launcher 原样传给目标及后台子进程；宿主持有另一端，
            // EOF 用来确认整棵测试进程树退出，而不依赖心跳或进程名扫描。
            let fd: std::os::fd::OwnedFd = inherited.into();
            command.stdin(Stdio::from(fd)).process_group(0);
        }

        #[cfg(windows)]
        let stop_event = WindowsStopEvent::new(command);
        let child = command.spawn().unwrap();
        command.stdin(Stdio::null());
        Self {
            child,
            stdout,
            stderr,
            #[cfg(unix)]
            liveness,
            #[cfg(windows)]
            stop_event,
        }
    }
    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "Launcher 超时，{}",
                self.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    fn output(&self) -> String {
        fs::read_to_string(&self.stdout).unwrap()
    }
    fn diagnostics(&self) -> String {
        format!(
            "stdout={}\nstderr={}",
            self.output(),
            fs::read_to_string(&self.stderr).unwrap()
        )
    }
}
impl Process {
    #[cfg(unix)]
    fn stop(&mut self) -> std::io::Result<()> {
        // 当前对象由沙箱外宿主持有。只向本调用的独立进程组发送终止信号。
        if unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }
    #[cfg(unix)]
    fn assert_tree_closed(&mut self) {
        use std::io::Read;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.liveness.read(&mut [0u8; 1]) {
                Ok(0) => return,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                other => panic!("进程树存活探针异常: {other:?}"),
            }
            assert!(
                Instant::now() < deadline,
                "任务关闭后仍有进程持有继承描述符"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Err(error) = self.stop() {
            eprintln!("测试宿主关闭任务失败: {error}");
        }
        #[cfg(windows)]
        {
            self.stop_event.signal();
            // 正常路径由 Launcher 终止 Job 后回滚 ACL/容器身份，不直接杀首进程。
            let deadline = Instant::now() + Duration::from_secs(5);
            while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(3);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
#[cfg(windows)]
struct WindowsStopEvent(std::os::windows::io::OwnedHandle);
#[cfg(windows)]
impl WindowsStopEvent {
    fn new(command: &mut Command) -> Self {
        use std::os::windows::io::FromRawHandle;
        let name = format!("Local\\TiangongSandboxTest.{}", scru128::new());
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let handle = unsafe {
            windows_sys::Win32::System::Threading::CreateEventW(
                std::ptr::null(),
                1,
                0,
                wide.as_ptr(),
            )
        };
        assert!(
            !handle.is_null(),
            "创建宿主停止事件失败: {}",
            std::io::Error::last_os_error()
        );
        command
            .env(tiangong_sandbox::WINDOWS_STOP_EVENT_ENV, name)
            .env(
                tiangong_sandbox::HOST_PID_ENV,
                std::process::id().to_string(),
            );
        Self(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) })
    }
    fn signal(&self) {
        use std::os::windows::io::AsRawHandle;
        if unsafe { windows_sys::Win32::System::Threading::SetEvent(self.0.as_raw_handle()) } == 0 {
            eprintln!("宿主通知停止失败: {}", std::io::Error::last_os_error());
        }
    }
}
struct Fixture {
    root: tempfile::TempDir,
    workspace: PathBuf,
    temp: PathBuf,
    outside: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let canonical = tiangong_sandbox::canonicalize_path(root.path()).unwrap();
        let workspace = canonical.join("workspace");
        let temp = canonical.join("private-temp");
        let outside = canonical.join("outside");
        for path in [&workspace, &temp, &outside] {
            fs::create_dir(path).unwrap();
        }
        Self {
            root,
            workspace,
            temp,
            outside,
        }
    }
    fn policy(&self) -> SandboxPolicy {
        let mut policy = SandboxPolicy::workspace_write(&self.workspace);
        policy.extra_writable.push(self.temp.clone());
        policy
    }
    fn command(&self, policy: &SandboxPolicy) -> Command {
        let path = self.root.path().join(format!("{}.json", scru128::new()));
        fs::write(&path, serde_json::to_vec(policy).unwrap()).unwrap();
        let mut command = Command::new(LAUNCHER);
        command.args(["run", "--policy"]).arg(path).arg("--");
        command
            .env_remove(tiangong_sandbox::HOST_PID_ENV)
            .env_remove(tiangong_sandbox::WINDOWS_STOP_EVENT_ENV)
            .env_remove(tiangong_sandbox::POLICY_ENV);
        command
    }
    #[cfg(unix)]
    fn shell(&self, policy: &SandboxPolicy, script: &str) -> Process {
        Process::spawn(
            self.command(policy).args(["/bin/bash", "-c", script]),
            self.root.path(),
        )
    }
}

#[cfg(unix)]
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

#[test]
fn launcher_rejects_unsafe_policy_without_running_target() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let mut policy = fixture.policy();
    policy.mode = tiangong_sandbox::SandboxMode::FullAccess;
    let mut command = fixture.command(&policy);
    #[cfg(unix)]
    command.args(["/bin/sh", "-c", "echo TARGET_RAN"]);
    #[cfg(windows)]
    command.args(["cmd.exe", "/D", "/C", "echo TARGET_RAN"]);
    let mut process = Process::spawn(&mut command, fixture.root.path());
    assert_eq!(
        process.wait(Duration::from_secs(10)).code(),
        Some(78),
        "{}",
        process.diagnostics()
    );
    assert!(!process.output().contains("TARGET_RAN"));
}

#[test]
#[ignore = "必须由沙箱外宿主运行：cargo test --test launcher_execution -- --ignored --test-threads=1"]
fn launcher_attack_matrix_and_resource_self_check() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let mut process = Process::spawn(Command::new(LAUNCHER).arg("--self-check"), root.path());
    assert!(
        process.wait(Duration::from_secs(180)).success(),
        "{}",
        process.diagnostics()
    );
    let report: serde_json::Value = serde_json::from_str(&process.output()).unwrap();
    for field in [
        "workspace_write",
        "outside_write_blocked",
        "outside_delete_blocked",
        "outside_config_write_blocked",
        "git_metadata_write",
        "network_blocked",
        "sensitive_read_blocked",
        "symlink_escape_blocked",
        "path_traversal_blocked",
    ] {
        assert_eq!(report[field], true, "{field}: {report}");
    }
    // 自检上报当前平台支持的能力，宿主据此比对授予的能力。
    let reported: std::collections::BTreeSet<tiangong_sandbox::SandboxCapability> =
        serde_json::from_value(report["capabilities"].clone()).expect("自检必须上报能力");
    assert_eq!(reported, tiangong_sandbox::supported_capabilities());
    #[cfg(windows)]
    for field in [
        "outside_read_blocked",
        "dedicated_temp_write",
        "network_allowed",
        "child_restrictions_inherited",
        "resource_limits_applied",
        "process_limit_enforced",
        "memory_limit_enforced",
        "cpu_limit_enforced",
        "hardlink_escape_blocked",
        "temporary_acl_cleaned",
        "temporary_identity_cleaned",
    ] {
        assert_eq!(report[field], true, "{field}: {report}");
    }
    eprintln!("Launcher 自检报告: {report}");
}

#[cfg(unix)]
#[test]
#[ignore = "必须由沙箱外宿主运行：cargo test --test launcher_execution -- --ignored --test-threads=1"]
fn cli_workspace_temp_and_sensitive_authorization() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let secret = f.workspace.join("credentials");
    fs::write(&secret, "secret").unwrap();
    let mut policy = f.policy();
    policy.denied_read_paths.push(secret.clone());
    policy.protected_paths.push(secret.clone());
    let script = format!(
        "set -eu; for d in {} {}; do printf one > \"$d/file\"; test \"$(cat \"$d/file\")\" = one; printf two >> \"$d/file\"; test \"$(cat \"$d/file\")\" = onetwo; rm \"$d/file\"; done; if cat {} ; then exit 21; fi; if echo bad > {}/escape; then exit 22; fi; echo BOUNDARIES_OK",
        quote(&f.workspace),
        quote(&f.temp),
        quote(&secret),
        quote(&f.outside)
    );
    let mut process = f.shell(&policy, &script);
    assert!(
        process.wait(Duration::from_secs(15)).success(),
        "{}",
        process.diagnostics()
    );
    assert!(process.output().contains("BOUNDARIES_OK"));
    assert!(!f.outside.join("escape").exists());
    // 模拟宿主已验证身份后的策略豁免，不在沙箱内伪造宿主身份。
    policy.denied_read_paths.clear();
    let script = format!(
        "set -eu; test \"$(cat {})\" = secret; if echo changed > {}; then exit 23; fi; echo AUTHORIZED_READ_ONLY",
        quote(&secret),
        quote(&secret)
    );
    let mut process = f.shell(&policy, &script);
    assert!(
        process.wait(Duration::from_secs(15)).success(),
        "{}",
        process.diagnostics()
    );
    assert!(process.output().contains("AUTHORIZED_READ_ONLY"));
    assert_eq!(fs::read_to_string(secret).unwrap(), "secret");
}

#[cfg(unix)]
#[test]
#[ignore = "必须由沙箱外宿主运行：cargo test --test launcher_execution -- --ignored --test-threads=1"]
fn cli_cpu_limit_readback_and_busy_loop_termination() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let mut policy = f.policy();
    policy.resource_limits.max_cpu_time_seconds = 2;
    let mut readback = f.shell(&policy, "ulimit -t");
    assert!(
        readback.wait(Duration::from_secs(10)).success(),
        "{}",
        readback.diagnostics()
    );
    assert_eq!(readback.output().trim(), "2");
    let mut busy = f.shell(&policy, "echo CPU_READY; while :; do :; done");
    let status = busy.wait(Duration::from_secs(20));
    assert!(
        busy.output().contains("CPU_READY"),
        "{}",
        busy.diagnostics()
    );
    use std::os::unix::process::ExitStatusExt;
    assert!(
        matches!(status.signal(), Some(libc::SIGXCPU | libc::SIGKILL))
            || matches!(status.code(), Some(137 | 152)),
        "CPU 未被限制: {status}, {}",
        busy.diagnostics()
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "必须由沙箱外宿主运行：cargo test --test launcher_execution -- --ignored --test-threads=1"]
fn cli_memory_limit_readback_and_enforcement() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let mut policy = f.policy();
    policy.resource_limits.max_memory_bytes = 256 * 1024 * 1024;
    let mut readback = f.shell(&policy, "ulimit -v");
    assert!(
        readback.wait(Duration::from_secs(15)).success(),
        "{}",
        readback.diagnostics()
    );
    let values = readback.output();
    let values: Vec<_> = values.lines().collect();
    assert_eq!(values[0], "262144");
    // 有界分配：不是 OOM 炸弹，最多尝试超出 256 MiB 的单次分配。
    let mut memory = Process::spawn(f.command(&policy).args(["python3", "-c",
        "import sys\ntry: bytearray(512*1024*1024)\nexcept MemoryError: print('MEMORY_BLOCKED'); sys.exit(0)\nsys.exit(24)"]), f.root.path());
    assert!(
        memory.wait(Duration::from_secs(15)).success(),
        "{}",
        memory.diagnostics()
    );
    assert!(memory.output().contains("MEMORY_BLOCKED"));
}
#[cfg(windows)]
#[test]
#[ignore = "必须由沙箱外宿主运行：cargo test --test launcher_execution -- --ignored --test-threads=1"]
fn windows_timeout_cancel_host_crash_and_concurrent_cleanup() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let mut process = Process::spawn(
        Command::new(LAUNCHER).arg("--windows-lifecycle-self-check"),
        root.path(),
    );
    assert!(
        process.wait(Duration::from_secs(180)).success(),
        "{}",
        process.diagnostics()
    );
    let report: serde_json::Value = serde_json::from_str(&process.output()).unwrap();
    for field in [
        "timeout_cleanup",
        "stop_event_cleanup",
        "host_exit_cleanup",
        "process_tree_cleanup",
        "concurrent_cleanup",
    ] {
        assert_eq!(report[field], true, "{field}: {report}");
    }
    eprintln!("Windows 生命周期报告: {report}");
}

#[cfg(unix)]
fn wait_marker(path: &Path, process: &mut Process) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "目标提前退出: {}",
            process.diagnostics()
        );
        assert!(
            Instant::now() < deadline,
            "未收到进程树就绪标记: {}",
            process.diagnostics()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
#[test]
#[ignore = "必须由沙箱外宿主运行：cargo test --test launcher_execution -- --ignored --test-threads=1"]
fn cli_cancel_and_timeout_stop_entire_process_group() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    for label in ["cancel", "timeout"] {
        let marker = f.workspace.join(label);
        let heartbeat = f.workspace.join(format!("{label}-heartbeat"));
        let script = format!(
            "exec 9<&0; (while :; do echo tick >> {}; sleep 0.05; done) <&9 & echo ready > {}; wait",
            quote(&heartbeat),
            quote(&marker)
        );
        let mut process = f.shell(&f.policy(), &script);
        wait_marker(&marker, &mut process);
        if label == "timeout" {
            let deadline = Instant::now() + Duration::from_millis(200);
            while Instant::now() < deadline {
                assert!(
                    process.child.try_wait().unwrap().is_none(),
                    "超时前任务意外退出"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        process.stop().expect("沙箱外宿主关闭任务");
        assert!(!process.wait(Duration::from_secs(5)).success());
        process.assert_tree_closed();
        std::thread::sleep(Duration::from_millis(150));
        let before = fs::read(&heartbeat).unwrap_or_default();
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(
            before,
            fs::read(&heartbeat).unwrap_or_default(),
            "{label} 后后台进程仍在写入"
        );
    }
}

#[cfg(windows)]
#[test]
#[ignore = "必须由沙箱外宿主运行：cargo test --test launcher_execution -- --ignored --test-threads=1"]
fn windows_persistent_grants_preserve_file_access_and_private_temp() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let node = f.root.path().join("node.exe");
    fs::copy(
        std::env::var_os("SANDBOX_TEST_NODE").expect("需要 Node"),
        &node,
    )
    .unwrap();
    let managed = f.workspace.join("managed");
    fs::create_dir(&managed).unwrap();
    let cache = managed.join("grant.json");
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            tiangong_sandbox::sandbox::windows::revoke_persistent_grants(&self.0).unwrap();
        }
    }
    let _cleanup = Cleanup(cache.clone());
    let existing = f.workspace.join("existing.txt");
    fs::write(&existing, "one").unwrap();
    let mut policy = f.policy();
    policy.denied_read_paths.push(managed);
    let policy_file = f.root.path().join("policy.json");
    fs::write(&policy_file, serde_json::to_vec(&policy).unwrap()).unwrap();
    let script = "const fs=require('fs'),os=require('os'),path=require('path'); const p=process.argv[1]; if(!fs.readFileSync(p,'utf8'))process.exit(20); fs.appendFileSync(p,'two'); try{fs.readFileSync(process.argv[2]);process.exit(21);}catch(e){if(e.code!=='EPERM'&&e.code!=='EACCES')throw e;} const t=path.join(os.tmpdir(),'check.txt'); fs.writeFileSync(t,'temp');if(fs.readFileSync(t,'utf8')!=='temp')process.exit(22);fs.unlinkSync(t);console.log(JSON.stringify({dir:os.tmpdir()}));";
    let mut dirs = Vec::new();
    for reused in [false, true] {
        let mut command = Command::new(LAUNCHER);
        command
            .arg("run")
            .arg("--policy")
            .arg(&policy_file)
            .arg("--grant-cache")
            .arg(&cache)
            .arg("--")
            .arg(&node)
            .arg("-e")
            .arg(script)
            .arg(&existing)
            .arg(&cache)
            .env("TIANGONG_SANDBOX_DIAGNOSTICS", "1");
        let mut process = Process::spawn(&mut command, f.root.path());
        assert!(
            process.wait(Duration::from_secs(30)).success(),
            "{}",
            process.diagnostics()
        );
        assert!(
            process.diagnostics().contains(&format!("reused={reused}")),
            "{}",
            process.diagnostics()
        );
        dirs.push(
            serde_json::from_str::<serde_json::Value>(&process.output()).unwrap()["dir"].clone(),
        );
    }
    assert_ne!(dirs[0], dirs[1]);
}

#[cfg(windows)]
#[test]
#[ignore = "必须由沙箱外宿主运行：cargo test --test launcher_execution -- --ignored --test-threads=1"]
fn windows_cli_node_private_temp_roundtrip_and_identity_isolation() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let node =
        std::env::var_os("SANDBOX_TEST_NODE").expect("Windows CI 必须准备 SANDBOX_TEST_NODE");
    let script = "const fs=require('fs'),os=require('os'),path=require('path'); const dir=os.tmpdir(); const p=path.join(dir,'probe.txt'); fs.writeFileSync(p,'one'); if(fs.readFileSync(p,'utf8')!=='one')process.exit(20); fs.appendFileSync(p,'two'); if(fs.readFileSync(p,'utf8')!=='onetwo')process.exit(21); fs.unlinkSync(p); if(fs.existsSync(p))process.exit(22); console.log(JSON.stringify({dir}));";
    let mut directories = Vec::new();
    for _ in 0..2 {
        let mut command = f.command(&f.policy());
        command.arg(&node).args([
            "--preserve-symlinks",
            "--preserve-symlinks-main",
            "-e",
            script,
        ]);
        let mut process = Process::spawn(&mut command, f.root.path());
        assert!(
            process.wait(Duration::from_secs(30)).success(),
            "{}",
            process.diagnostics()
        );
        let result: serde_json::Value = serde_json::from_str(process.output().trim()).unwrap();
        let directory = result["dir"]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .to_lowercase();
        assert!(
            directory.contains("/packages/tiangongsandbox.") && directory.ends_with("/ac/temp"),
            "非私有临时目录: {directory}"
        );
        directories.push(directory);
    }
    assert_ne!(directories[0], directories[1]);
}

#[cfg(windows)]
#[test]
#[ignore = "必须由沙箱外宿主运行：cargo test --test launcher_execution -- --ignored --test-threads=1"]
fn windows_cli_sensitive_read_exemption_preserves_write_denial() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let node = std::env::var_os("SANDBOX_TEST_NODE").expect("Windows CI 必须准备 Node");
    let secret = f.workspace.join("credentials.txt");
    fs::write(&secret, "secret").unwrap();
    let mut policy = f.policy();
    policy.protected_paths.push(secret.clone());
    policy.denied_read_paths.push(secret.clone());
    for allowed in [false, true] {
        if allowed {
            policy.denied_read_paths.clear();
        }
        let script = "const fs=require('fs');const [secret,outside,allowed]=process.argv.slice(1);let read=false;try{read=fs.readFileSync(secret,'utf8')==='secret'}catch(e){if(!['EACCES','EPERM'].includes(e.code))throw e}if(read!==(allowed==='true'))process.exit(20);for(const p of [secret,outside]){try{fs.writeFileSync(p,'bad');process.exit(21)}catch(e){if(!['EACCES','EPERM'].includes(e.code))throw e}}console.log('BOUNDARIES_OK')";
        let mut command = f.command(&policy);
        command
            .arg(&node)
            .args(["-e", script])
            .arg(&secret)
            .arg(f.outside.join("escape"))
            .arg(allowed.to_string());
        let mut process = Process::spawn(&mut command, f.root.path());
        assert!(
            process.wait(Duration::from_secs(30)).success(),
            "{}",
            process.diagnostics()
        );
        assert!(process.output().contains("BOUNDARIES_OK"));
        assert_eq!(fs::read_to_string(&secret).unwrap(), "secret");
        assert!(!f.outside.join("escape").exists());
    }
}

#[cfg(unix)]
#[test]
#[ignore = "必须由沙箱外宿主运行：cargo test --test launcher_execution -- --ignored --test-threads=1"]
fn cli_concurrent_cancellation_does_not_stop_other_invocation() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let first = f.workspace.join("first");
    let second = f.workspace.join("second");
    let script = |marker: &Path| {
        format!(
            "echo ready > {}; while :; do sleep 0.05; done",
            quote(marker)
        )
    };
    let mut cancelled = f.shell(&f.policy(), &script(&first));
    let mut survivor = f.shell(&f.policy(), &script(&second));
    wait_marker(&first, &mut cancelled);
    wait_marker(&second, &mut survivor);
    cancelled.stop().expect("沙箱外宿主取消指定调用");
    assert!(!cancelled.wait(Duration::from_secs(5)).success());
    cancelled.assert_tree_closed();
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        survivor.child.try_wait().unwrap().is_none(),
        "取消影响了另一调用"
    );
    survivor.stop().expect("沙箱外宿主清理剩余调用");
    assert!(!survivor.wait(Duration::from_secs(5)).success());
    survivor.assert_tree_closed();
}

// 不施加沙箱、不发送 kill：验证测试宿主的描述符归属和有限回收逻辑。
#[cfg(unix)]
#[test]
fn host_liveness_probe_waits_for_descendant_exit() {
    use std::io::{Read, Write};
    let root = tempfile::tempdir().unwrap();
    let mut process = Process::spawn(
        Command::new("/bin/sh").args(["-c", "exec 9<&0; (read token <&9) <&9 & exit 0"]),
        root.path(),
    );
    assert!(process.wait(Duration::from_secs(5)).success());
    assert!(
        matches!(process.liveness.read(&mut [0u8; 1]), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "首进程退出不应误判后台子进程也已退出"
    );
    // 显式释放后台进程，不依赖 sleep 时长或 CI 调度速度。
    process.liveness.write_all(b"stop\n").unwrap();
    process.assert_tree_closed();
}
