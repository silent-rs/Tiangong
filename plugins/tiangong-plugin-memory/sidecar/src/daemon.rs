//! 后台 daemon 进程管理：`--daemon` 启动分离的后台进程，`--stop` 停止它。
//!
//! 后台子进程以 `--daemon --foreground` 运行实际服务，就绪后写入
//! `<storage>/memory/daemon.json`（pid、url、version，不含 token）。
//! 父进程等到发现文件出现且 pid 匹配后才返回，保证"命令成功 = 服务可用"。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const TOKEN_ENV: &str = "TIANGONG_MEMORY_TOKEN";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// daemon 发现文件内容（不含 token：token 由用户设置并自行保管）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonInfo {
    pub pid: u32,
    pub url: String,
    pub version: String,
    /// 可执行文件路径；`--stop` 前用于确认 pid 仍属于本服务（防 pid 复用误杀）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    /// 进程启动时刻（系统启动后的秒数 / Windows 为创建时间字符串），
    /// pid 复用后新进程的启动时间必然不同。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
}

impl DaemonInfo {
    /// 为当前进程生成发现信息。
    pub fn current(url: String) -> Self {
        Self {
            pid: std::process::id(),
            url,
            version: env!("CARGO_PKG_VERSION").to_string(),
            exe: std::env::current_exe()
                .ok()
                .map(|path| path.to_string_lossy().to_string()),
            started: process_identity(std::process::id()),
        }
    }

    /// 记录中的进程是否仍是本服务：pid 存活且身份标识一致。
    ///
    /// 记录缺少身份信息（旧版本写入）时只能退回 pid 存活判断。
    fn is_same_process(&self) -> bool {
        if !process_alive(self.pid) {
            return false;
        }
        match (&self.started, process_identity(self.pid)) {
            (Some(recorded), Some(current)) => recorded == &current,
            // 取不到身份标识时不阻断停止操作，避免无法收拾残留进程。
            _ => true,
        }
    }
}

/// 进程身份标识：用于识别 pid 复用。Unix 取 ps 的 lstart，Windows 取创建时间。
#[cfg(unix)]
fn process_identity(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(windows)]
fn process_identity(pid: u32) -> Option<String> {
    let mut command = Command::new("wmic");
    command.args([
        "process",
        "where",
        &format!("ProcessId={pid}"),
        "get",
        "CreationDate",
        "/value",
    ]);
    let output = tiangong_toolkit::configure_no_window(&mut command)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("CreationDate=")
                .map(str::to_string)
        })
        .filter(|value| !value.is_empty())
}

#[cfg(not(any(unix, windows)))]
fn process_identity(_pid: u32) -> Option<String> {
    None
}

pub fn memory_dir() -> PathBuf {
    tiangong_memory::default_memory_config_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn info_path() -> PathBuf {
    memory_dir().join("daemon.json")
}

pub fn log_path() -> PathBuf {
    memory_dir().join("daemon.log")
}

pub fn read_info() -> Option<DaemonInfo> {
    let body = std::fs::read(info_path()).ok()?;
    serde_json::from_slice(&body).ok()
}

/// 返回仍在运行的 daemon；发现文件指向已退出进程时顺手清理。
pub fn running() -> Option<DaemonInfo> {
    let info = read_info()?;
    if info.is_same_process() {
        Some(info)
    } else {
        // 进程已退出，或 pid 已被无关进程复用：只清理陈旧记录，不做其他处理。
        let _ = std::fs::remove_file(info_path());
        None
    }
}

/// 校验并取得 token：必须由用户通过 `--token` 或环境变量设置。
pub fn require_token(token: Option<String>) -> Result<String> {
    let token = token
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .with_context(|| {
            format!("daemon 需要访问令牌：请使用 --token <令牌> 或设置环境变量 {TOKEN_ENV}")
        })?;
    if token.len() < 8 {
        bail!("访问令牌至少 8 个字符");
    }
    if token
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        bail!("访问令牌不能包含空白或控制字符");
    }
    Ok(token)
}

/// 启动后台 daemon，等待就绪后返回其信息。
pub fn start_background(host: &str, port: u16, token: &str) -> Result<DaemonInfo> {
    if let Some(info) = running() {
        bail!(
            "daemon 已在运行（pid={}，{}），如需重启请先执行 --stop",
            info.pid,
            info.url
        );
    }
    let dir = memory_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("创建目录失败：{}", dir.display()))?;
    let log = log_path();
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("打开日志失败：{}", log.display()))?;
    let log_err = log_file.try_clone()?;
    let log_start = log_file.metadata().map(|meta| meta.len()).unwrap_or(0);

    let exe = std::env::current_exe().context("定位当前可执行文件失败")?;
    let mut command = Command::new(exe);
    command
        .args(["--daemon", "--foreground", "--host", host, "--port"])
        .arg(port.to_string())
        // token 走环境变量，避免出现在进程列表的命令行参数里。
        .env(TOKEN_ENV, token)
        .env_remove("TIANGONG_PLUGIN_TRANSPORT")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));
    detach(&mut command);
    let mut child = command.spawn().context("启动后台 daemon 失败")?;
    let pid = child.id();

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(info) = read_info().filter(|info| info.pid == pid) {
            return Ok(info);
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "后台 daemon 启动失败（{status}）：\n{}",
                log_tail(&log, log_start)
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            bail!(
                "后台 daemon 在 {} 秒内未就绪，已终止。日志：{}\n{}",
                STARTUP_TIMEOUT.as_secs(),
                log.display(),
                log_tail(&log, log_start)
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 停止后台 daemon。返回被停止的实例；没有运行中的实例返回 `None`。
pub fn stop() -> Result<Option<DaemonInfo>> {
    // running() 已核对 pid 身份：记录陈旧或 pid 被复用时返回 None，
    // 不会向无关进程发信号。
    let Some(info) = running() else {
        return Ok(None);
    };
    terminate(info.pid)?;
    let deadline = Instant::now() + STOP_TIMEOUT;
    while process_alive(info.pid) {
        if Instant::now() >= deadline {
            bail!(
                "daemon（pid={}）在 {} 秒内未退出",
                info.pid,
                STOP_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // 正常退出时 daemon 自行删除发现文件；被强制结束时由这里兜底。
    if read_info().is_some_and(|current| current.pid == info.pid) {
        let _ = std::fs::remove_file(info_path());
    }
    Ok(Some(info))
}

fn log_tail(path: &Path, from: u64) -> String {
    let Ok(body) = std::fs::read(path) else {
        return String::new();
    };
    let start = (from as usize).min(body.len());
    let text = String::from_utf8_lossy(&body[start..]);
    let lines = text.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: setsid 是 async-signal-safe 的系统调用，只在 fork 后的子进程中执行，
    // 使后台进程脱离当前终端会话，终端关闭或 Ctrl+C 不会波及它。
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) {}

#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: 信号 0 只检查进程是否存在，不会向目标进程发送信号。
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    let mut command = Command::new("tasklist");
    command.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
    tiangong_toolkit::configure_no_window(&mut command)
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")))
        .unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
pub fn process_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn terminate(pid: u32) -> Result<()> {
    let raw = i32::try_from(pid).context("pid 超出范围")?;
    // SAFETY: 向发现文件记录的 daemon 进程发送 SIGTERM，触发其优雅退出。
    let result = unsafe { libc::kill(raw, libc::SIGTERM) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).with_context(|| format!("停止 daemon（pid={pid}）失败"));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn terminate(pid: u32) -> Result<()> {
    // 分离的后台进程没有控制台，无法接收 Ctrl+C，只能结束进程；
    // Leader 租约会因心跳超时由下一个进程接管。
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/T", "/F"]);
    let status = tiangong_toolkit::configure_no_window(&mut command)
        .status()
        .context("执行 taskkill 失败")?;
    if !status.success() && process_alive(pid) {
        bail!("停止 daemon（pid={pid}）失败：{status}");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn terminate(_pid: u32) -> Result<()> {
    bail!("当前平台不支持 --stop")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_must_be_set_and_reasonable() {
        assert!(require_token(None).is_err());
        assert!(require_token(Some("   ".to_string())).is_err());
        assert!(require_token(Some("short".to_string())).is_err());
        assert!(require_token(Some("has space inside".to_string())).is_err());
        assert_eq!(
            require_token(Some("  secret-token  ".to_string())).unwrap(),
            "secret-token"
        );
    }

    #[test]
    fn current_process_is_alive() {
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn process_identity_is_stable_and_pid_specific() {
        let pid = std::process::id();
        let identity = process_identity(pid);
        if identity.is_none() {
            return; // 平台不支持时跳过
        }
        assert_eq!(identity, process_identity(pid), "同一进程身份标识应稳定");
        let info = DaemonInfo::current("http://127.0.0.1:1".to_string());
        assert!(info.is_same_process(), "当前进程应判定为同一进程");
        // 身份不符（pid 复用）时不认领：伪造一个不同的启动时间。
        let stale = DaemonInfo {
            started: Some("Thu Jan  1 00:00:00 1970".to_string()),
            ..info
        };
        assert!(!stale.is_same_process(), "启动时间不符应视为 pid 已复用");
    }
}
