//! sidecar 子进程的启动准备与平台生命周期。
//!
//! 从 `mod`（连接本体）拆出：策略 FD 注入（平台差异经条件编译三实现）、
//! 用户环境策略、进程组/会话配置、跨平台进程树终止（Windows Job 对象
//! 含内存与进程数限额）。连接协议不在此层——本模块只负责「把进程拉起
//! 并可靠停下」。

use std::process::{Child, Command};
use std::time::Duration;

use anyhow::{Result, anyhow};
// `Context` 与 `bail!` 仅被 unix 与非 unix/windows 兜底分支使用，
// Windows 下无引用，需按平台条件导入以避免 unused_imports 警告。
#[cfg(not(windows))]
use anyhow::{Context, bail};

use super::StdioProcess;
use crate::sidecar::SidecarConfig;

/// sidecar 启动尝试的错误分类：只有进程创建失败才允许失效解释器缓存
/// 并重试，前置准备失败与解释器发现无关。
pub(super) enum SpawnAttemptError {
    /// `Command::spawn()` 失败（文件被删、无执行权限、程序格式无效等），
    /// 携带实际尝试的程序路径。
    ProcessCreation {
        program: std::path::PathBuf,
        source: anyhow::Error,
    },
    /// 前置准备失败（目录/日志/清单校验/生命周期配置等）。
    Preparation(anyhow::Error),
}

pub(super) fn preparation<T>(
    result: anyhow::Result<T>,
) -> std::result::Result<T, SpawnAttemptError> {
    result.map_err(SpawnAttemptError::Preparation)
}

/// 为沙箱程序准备策略描述符：匿名管道写端写入长度前缀和策略正文后立即
/// 关闭；读端经 pre_exec 复制到 fd3 并关闭原描述符。
///
/// 返回的读端守卫必须存活到 `spawn` 返回——父进程随后正常关闭（无泄漏）；
/// 标准库管道两端在返回调用方前均已设置 FD_CLOEXEC，避免并发 spawn 继承
/// 尚未关闭的写端，导致 Launcher 永远等不到策略 EOF。
#[cfg(unix)]
pub(super) struct PolicyFdGuard(std::io::PipeReader);

#[cfg(not(unix))]
pub(super) struct PolicyFdGuard;

#[cfg(unix)]
pub(super) fn prepare_policy_fd(
    command: &mut Command,
    policy_json: String,
) -> Result<PolicyFdGuard> {
    use std::io::Write;
    use std::os::fd::AsRawFd;

    let policy_bytes = policy_json.as_bytes();
    if policy_bytes.len() > tiangong_sandbox::MAX_POLICY_FRAME_BYTES {
        bail!(
            "Launcher 策略超过长度上限: actual={}, max={}",
            policy_bytes.len(),
            tiangong_sandbox::MAX_POLICY_FRAME_BYTES
        );
    }
    let length = u32::try_from(policy_bytes.len()).context("Launcher 策略长度无法编码")?;
    let (read_fd, mut writer) = std::io::pipe().context("创建策略管道失败")?;
    writer
        .write_all(&length.to_be_bytes())
        .and_then(|_| writer.write_all(policy_bytes))
        .and_then(|_| writer.flush())
        .context("写入策略管道失败")?;
    drop(writer);

    let guard = PolicyFdGuard(read_fd);
    let raw_read = guard.0.as_raw_fd();
    // pre_exec（fork 后、exec 前）：复制到 fd3 并关闭原描述符（若非 3）。
    // dup2 会清除目标 fd 的 CLOEXEC；原描述符恰好已经是 3 时则必须显式
    // 清除，否则并发 spawn 中拿到 fd3 的 Launcher 会在 exec 后读到 EBADF。
    // SAFETY: pre_exec 限制内仅调用异步信号安全函数。
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(move || {
            if raw_read == 3 {
                let flags = libc::fcntl(raw_read, libc::F_GETFD);
                if flags < 0 || libc::fcntl(raw_read, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            } else {
                if libc::dup2(raw_read, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(raw_read);
            }
            Ok(())
        });
    }
    Ok(guard)
}

#[cfg(windows)]
pub(super) fn prepare_policy_fd(
    command: &mut Command,
    policy_json: String,
) -> Result<PolicyFdGuard> {
    command.env(tiangong_sandbox::POLICY_ENV, policy_json);
    Ok(PolicyFdGuard)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn prepare_policy_fd(
    _command: &mut Command,
    _policy_json: String,
) -> Result<PolicyFdGuard> {
    bail!("当前平台没有可用的 Launcher 策略传输通道")
}

pub(super) fn sanitize_spawn_environment(command: &mut Command) {
    // 解释器启动注入类（NODE_OPTIONS/PYTHON*/PERL5OPT/RUBY*/JAVA_TOOL_OPTIONS/
    // ZDOTDIR）能让目标后续拉起的解释器在启动前加载额外代码，与动态加载
    // 前缀同层级拒绝（对齐 octos 危险环境清单）。
    for (key, _) in std::env::vars_os() {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        if crate::BUILTIN_DENIED_ENV_KEYS.contains(&upper.as_str())
            || crate::BUILTIN_DENIED_ENV_PREFIXES
                .iter()
                .any(|prefix| upper.starts_with(prefix))
        {
            command.env_remove(key);
        }
    }
}

pub(super) fn apply_user_environment_policy(command: &mut Command, config: &SidecarConfig) {
    for (key, _) in std::env::vars_os() {
        let key_text = key.to_string_lossy();
        if config
            .sandbox_environment_blocklist
            .iter()
            .any(|item| key_text.eq_ignore_ascii_case(item))
        {
            command.env_remove(&key);
        }
    }
}

pub(super) fn configure_process_lifecycle(command: &mut Command) -> Result<()> {
    #[cfg(windows)]
    crate::platform::configure_no_window(command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // 每个 stdio sidecar 独占进程组，正常取消时可连同 Shell 后台进程清理。
        command.process_group(0);
    }
    let _ = command;
    Ok(())
}

/// 在常驻线程上创建子进程。
///
/// Linux 的父进程死亡信号（`PR_SET_PDEATHSIG`）以**创建子进程的线程**为
/// 「父」：该线程一退出就触发。Linux 沙箱经 bubblewrap `--die-with-parent`
/// 启动，若在短命线程（如 WASM 宿主每次调用 sidecar 时开的线程）里
/// spawn，线程结束时常驻 sidecar 会被 SIGKILL，下次调用只能重启，且进行中
/// 的请求无法收到取消帧。统一交给进程内常驻线程创建即可避免；其他平台
/// 没有该语义，直接在当前线程创建。
#[cfg(target_os = "linux")]
pub(super) fn spawn_on_persistent_thread(mut command: Command) -> std::io::Result<Child> {
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Mutex, OnceLock};

    type Job = (Command, Sender<std::io::Result<Child>>);
    static SPAWNER: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
    let spawner = SPAWNER.get_or_init(|| {
        let (sender, receiver) = channel::<Job>();
        std::thread::Builder::new()
            .name("stdio-sidecar-spawner".to_string())
            .spawn(move || {
                for (mut command, reply) in receiver {
                    let _ = reply.send(command.spawn());
                }
            })
            .expect("启动 sidecar 创建线程失败");
        Mutex::new(sender)
    });
    let (reply, result) = channel();
    let sent = spawner
        .lock()
        .map_err(|_| std::io::Error::other("sidecar 创建线程通道已损坏"))?
        .send((command, reply));
    match sent {
        Ok(()) => result
            .recv()
            .map_err(|_| std::io::Error::other("sidecar 创建线程已退出"))?,
        // 创建线程不可用时退回当前线程，保证仍能启动。
        Err(std::sync::mpsc::SendError((returned, _))) => {
            command = returned;
            command.spawn()
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn spawn_on_persistent_thread(mut command: Command) -> std::io::Result<Child> {
    command.spawn()
}

/// 终止后等待子进程退出的上限：正常 SIGKILL 后毫秒级完成；超时说明
/// 信号被外层环境保护拦截或进程陷入不可中断状态，放弃回收防止调用方
/// 永久阻塞（所有 stop/取消/换代路径都持有 child 锁调用本函数）。
const TERMINATE_WAIT_LIMIT: Duration = Duration::from_secs(5);

/// 终止子进程进程树并等待直接子进程退出。
///
/// 返回 Err 表示进程未被可靠终止/回收（信号被外层沙箱拒绝、限时内
/// 未退出）：严格验证路径必须把它当作清理失败；尽力而为的路径
///（取消、换代覆盖）可以忽略错误但要知道进程可能残留。
pub(super) fn terminate_process_tree(process: &StdioProcess, child: &mut Child) -> Result<()> {
    let pid = child.id();
    #[cfg(unix)]
    unsafe {
        // 进程组 ID 在 spawn 前固定为直接子进程 PID；即使组长先退出，仍可清理后代。
        // 组信号失败不阻断：直接子进程的回收由下方 child.kill 决定。
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    // Windows 侧 Job Object 整组终止（KILL_ON_JOB_CLOSE + 显式 Terminate），
    // 不需要子进程句柄；随后的 child.kill/wait 对已死进程为 no-op。
    #[cfg(windows)]
    process.lifecycle.terminate(child);
    #[cfg(not(windows))]
    let _ = process;
    // kill 被拒（如外层 Seatbelt 未放行 process-signal）时子进程不会退出，
    // 此时 wait 必然永久挂起——放弃回收并返回清理失败。
    if let Err(error) = child.kill()
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, pid, "终止 stdio sidecar 子进程失败（信号可能被外层沙箱拦截），放弃等待其退出");
        return Err(anyhow!(error).context(format!(
            "终止 stdio sidecar 子进程失败（pid={pid}，信号可能被外层沙箱拦截）"
        )));
    }
    let deadline = std::time::Instant::now() + TERMINATE_WAIT_LIMIT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return Ok(()),
            Ok(None) if std::time::Instant::now() >= deadline => {
                tracing::warn!(pid, "stdio sidecar 子进程终止后未在时限内退出，放弃等待");
                return Err(anyhow!(
                    "stdio sidecar 子进程终止后未在 {TERMINATE_WAIT_LIMIT:?} 内退出（pid={pid}）"
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(windows)]
pub(super) enum WindowsLifecycle {
    Job(WindowsJob),
    Sandbox(WindowsStopEvent),
}

#[cfg(windows)]
impl WindowsLifecycle {
    pub(super) fn assign(&self, child: &Child) -> std::io::Result<()> {
        match self {
            Self::Job(job) => job.assign(child),
            // Sandbox Launcher 在恢复目标线程前自行创建并应用内层 Job，避免
            // spawn 与宿主 AssignProcessToJobObject 之间出现逃逸窗口。
            Self::Sandbox(_) => Ok(()),
        }
    }

    fn terminate(&self, child: &mut Child) {
        match self {
            Self::Job(job) => job.terminate(),
            Self::Sandbox(stop) => stop.signal_and_wait(child),
        }
    }
}

#[cfg(windows)]
pub(super) struct WindowsStopEvent {
    handle: std::os::windows::io::OwnedHandle,
    name: String,
}

#[cfg(windows)]
impl WindowsStopEvent {
    /// 停止事件的内核对象名，供宿主经环境变量传递给 Sandbox Launcher。
    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn new() -> std::io::Result<Self> {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateEventW;

        let name = format!("Local\\TiangongSandboxStop-{}", scru128::new());
        let wide = std::ffi::OsStr::new(&name)
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "Sandbox 停止事件名称冲突",
            ));
        }
        Ok(Self {
            handle: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) },
            name,
        })
    }

    fn signal_and_wait(&self, child: &mut Child) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Threading::SetEvent;

        unsafe {
            SetEvent(self.handle.as_raw_handle());
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if child.try_wait().is_ok_and(|status| status.is_some()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(windows)]
pub(super) struct WindowsJob {
    handle: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl WindowsJob {
    pub(super) fn new(
        resource_limits: Option<tiangong_sandbox::SandboxResourceLimits>,
    ) -> std::io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
            JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW 成功后返回由当前对象独占的有效句柄。
        let job = Self {
            handle: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) },
        };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(resource_limits) = resource_limits {
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_JOB_MEMORY
                | JOB_OBJECT_LIMIT_JOB_TIME;
            limits.BasicLimitInformation.PerJobUserTimeLimit = resource_limits
                .max_cpu_time_seconds
                .saturating_mul(10_000_000)
                as i64;
            limits.BasicLimitInformation.ActiveProcessLimit = resource_limits.max_processes;
            limits.JobMemoryLimit = resource_limits.max_memory_bytes as usize;
        }
        let configured = unsafe {
            SetInformationJobObject(
                job.raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if configured == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }

    pub(super) fn assign(&self, child: &Child) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        let process = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        if unsafe { AssignProcessToJobObject(self.raw_handle(), process) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        unsafe {
            TerminateJobObject(self.raw_handle(), 1);
        }
    }

    fn raw_handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        use std::os::windows::io::AsRawHandle;
        self.handle.as_raw_handle()
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn job_object_applies_process_and_memory_limits() {
        use windows_sys::Win32::System::JobObjects::{
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
            JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            QueryInformationJobObject,
        };

        let expected = tiangong_sandbox::SandboxResourceLimits::default();
        let job = WindowsJob::new(Some(expected)).unwrap();
        let mut actual: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        let queried = unsafe {
            QueryInformationJobObject(
                job.raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw mut actual).cast(),
                std::mem::size_of_val(&actual) as u32,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(queried, 0);
        assert_ne!(
            actual.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            0
        );
        assert_ne!(
            actual.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            0
        );
        assert_ne!(
            actual.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_JOB_MEMORY,
            0
        );
        assert_ne!(
            actual.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_JOB_TIME,
            0
        );
        assert_eq!(
            actual.BasicLimitInformation.PerJobUserTimeLimit,
            expected.max_cpu_time_seconds as i64 * 10_000_000
        );
        assert_eq!(
            actual.BasicLimitInformation.ActiveProcessLimit,
            expected.max_processes
        );
        assert_eq!(actual.JobMemoryLimit, expected.max_memory_bytes as usize);
    }
}
