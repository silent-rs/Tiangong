//! 后台作业跟踪（issue #672）：终端里 `&` / `nohup` 起的进程留在登录
//! shell 的会话（sid）里，shell 或终端关闭后仍然存活。
//!
//! sidecar 空闲退出后这些进程会脱离管理：新 sidecar 运行在另一个沙箱
//! 实例中，沙箱只允许向同实例进程发信号（`signal (target same-sandbox)`），
//! 结果是端口一直被占、`kill` 返回 `Operation not permitted`。
//!
//! 因此：
//! - 每个登录 shell 启动时登记其 sid（portable-pty 以 setsid 启动 shell，
//!   shell 进程号即 sid）；
//! - 空闲退出前枚举仍存活的同 sid 进程，有则保持存活，让后续会话仍能
//!   结束它们（同一沙箱实例内 `kill` 可用）；
//! - `terminal_status` 列出这些进程，Agent 能定位并结束，不必依赖沙箱内
//!   不可用的 `ps` / `pgrep`；带 terminal_id 查询时还返回该终端会话内的
//!   进程树（pid、父进程、命令行，issue #677）——`ps`/`top` 是 setuid
//!   程序，沙箱禁止执行，这里直接读 libproc / `/proc`。
//!
//! 只匹配本进程登记过的 sid，不会扩大到任意用户进程。

use std::sync::Mutex;

use serde::Serialize;

/// 登记的作业会话上限：防止反复新建终端时无界增长。
const MAX_TRACKED_JOB_SESSIONS: usize = 256;
/// 进程命令行的最大字符数：防止超长参数撑爆工具输出。
const MAX_COMMAND_CHARS: usize = 512;

/// 一个登录 shell 的作业会话登记。
#[derive(Debug, Clone)]
struct TrackedJobSession {
    sid: i32,
    shell_pid: i32,
    terminal_id: String,
    scope_id: Option<String>,
}

/// 终端遗留的后台进程。
#[derive(Debug, Clone, Serialize)]
pub(super) struct BackgroundJob {
    pub(super) pid: i32,
    pub(super) pgid: i32,
    pub(super) name: String,
    /// 所属终端编号。
    pub(super) terminal_id: String,
    /// 所属终端是否仍存活；false 表示终端已关闭，进程成了孤儿作业。
    pub(super) terminal_alive: bool,
}

/// 终端会话内的一个进程（terminal_status 详情的进程树节点）。
#[derive(Debug, Clone, Serialize)]
pub(super) struct SessionProcess {
    pub(super) pid: i32,
    /// 父进程号；取不到时为 None。
    pub(super) ppid: Option<i32>,
    pub(super) pgid: i32,
    pub(super) name: String,
    /// 完整命令行（参数以空格连接，超长截断）；无权限或进程已退出时
    /// 为 None。不含环境变量。
    pub(super) command: Option<String>,
}

/// 作业会话登记表。
#[derive(Default)]
pub(super) struct JobTracker {
    sessions: Mutex<Vec<TrackedJobSession>>,
}

impl JobTracker {
    /// 登记一个新启动的登录 shell。
    pub(super) fn register(&self, shell_pid: u32, terminal_id: &str, scope_id: Option<&str>) {
        let Ok(pid) = i32::try_from(shell_pid) else {
            return;
        };
        let mut sessions = self.sessions.lock().expect("作业会话表锁损坏");
        sessions.push(TrackedJobSession {
            sid: pid,
            shell_pid: pid,
            terminal_id: terminal_id.to_string(),
            scope_id: scope_id.map(str::to_string),
        });
        if sessions.len() > MAX_TRACKED_JOB_SESSIONS {
            let overflow = sessions.len() - MAX_TRACKED_JOB_SESSIONS;
            sessions.drain(..overflow);
        }
    }

    /// 列出登记会话中仍存活的后台进程（不含登录 shell 自身）。`scope_id`
    /// 为 Some 时只列该宿主会话的终端；`is_alive` 判断终端是否仍在。
    /// 枚举不到进程（非 macOS/Linux）时返回空列表。没有存活进程的登记
    /// 会被清理。
    pub(super) fn live_jobs(
        &self,
        scope_id: Option<&str>,
        is_alive: impl Fn(&str) -> bool,
    ) -> Vec<BackgroundJob> {
        let sessions = self.sessions.lock().expect("作业会话表锁损坏").clone();
        if sessions.is_empty() {
            return Vec::new();
        }
        let processes = list_processes();
        let mut jobs = Vec::new();
        let mut live_sids = Vec::new();
        for process in &processes {
            let Some(owner) = sessions.iter().find(|session| session.sid == process.sid) else {
                continue;
            };
            if !live_sids.contains(&owner.sid) {
                live_sids.push(owner.sid);
            }
            if process.pid == owner.shell_pid {
                continue;
            }
            if scope_id.is_some_and(|scope| owner.scope_id.as_deref() != Some(scope)) {
                continue;
            }
            jobs.push(BackgroundJob {
                pid: process.pid,
                pgid: process.pgid,
                name: process.name.clone(),
                terminal_id: owner.terminal_id.clone(),
                terminal_alive: is_alive(&owner.terminal_id),
            });
        }
        // 只在确实枚举到进程时清理：枚举失败（空列表）不能抹掉登记。
        if !processes.is_empty() {
            self.sessions
                .lock()
                .expect("作业会话表锁损坏")
                .retain(|session| live_sids.contains(&session.sid));
        }
        jobs.sort_by_key(|job| job.pid);
        jobs
    }
}

/// 列出会话 `sid`（登录 shell 进程号）内仍存活的全部进程，含 shell 自身，
/// 按 pid 排序。枚举不到进程（非 macOS/Linux）时返回空列表。
pub(super) fn session_processes(sid: u32) -> Vec<SessionProcess> {
    let Ok(sid) = i32::try_from(sid) else {
        return Vec::new();
    };
    let mut processes: Vec<SessionProcess> = list_processes()
        .into_iter()
        .filter(|process| process.sid == sid)
        .map(|process| SessionProcess {
            pid: process.pid,
            ppid: process.ppid,
            pgid: process.pgid,
            command: process_command(process.pid).map(|command| truncate_chars(&command)),
            name: process.name,
        })
        .collect();
    processes.sort_by_key(|process| process.pid);
    processes
}

fn truncate_chars(text: &str) -> String {
    if text.chars().count() <= MAX_COMMAND_CHARS {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(MAX_COMMAND_CHARS).collect();
    truncated.push('…');
    truncated
}

/// 枚举到的一个进程。
struct ProcessInfo {
    pid: i32,
    ppid: Option<i32>,
    pgid: i32,
    sid: i32,
    name: String,
}

/// macOS：`proc_listallpids` 枚举 + `getsid`/`getpgid` 读取会话与进程组，
/// `proc_pidinfo(PROC_PIDTBSDINFO)` 读取父进程。沙箱内 `ps` / `pgrep`
/// 依赖的 sysmond 不可用，libproc 调用不受影响。
#[cfg(target_os = "macos")]
fn list_processes() -> Vec<ProcessInfo> {
    // SAFETY: 传空缓冲只查询所需大小。
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    // 预留余量：两次调用之间可能有新进程出现。
    let mut pids = vec![0 as libc::c_int; count as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int;
    // SAFETY: 缓冲区大小以字节传入，与分配一致。
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if count <= 0 {
        return Vec::new();
    }
    pids.truncate(count as usize);
    pids.into_iter()
        .filter(|pid| *pid > 0)
        .filter_map(|pid| {
            // SAFETY: getsid/getpgid 只读取进程属性，进程已退出时返回 -1。
            let sid = unsafe { libc::getsid(pid) };
            let pgid = unsafe { libc::getpgid(pid) };
            if sid < 0 || pgid < 0 {
                return None;
            }
            let mut name = [0u8; 256];
            // SAFETY: 缓冲区长度与传入大小一致，返回值为写入字节数。
            let written =
                unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
            let name = String::from_utf8_lossy(&name[..written.max(0) as usize]).to_string();
            Some(ProcessInfo {
                pid,
                ppid: parent_pid(pid),
                pgid,
                sid,
                name,
            })
        })
        .collect()
}

/// macOS：经 `PROC_PIDTBSDINFO` 读父进程号；无权限或进程已退出时为 None。
#[cfg(target_os = "macos")]
fn parent_pid(pid: i32) -> Option<i32> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: 缓冲区为完整的 proc_bsdinfo，大小以字节传入。
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: 写入字节数等于结构大小，结构已完整初始化。
    let info = unsafe { info.assume_init() };
    i32::try_from(info.pbi_ppid).ok()
}

/// macOS：经 `sysctl(KERN_PROCARGS2)` 读命令行。只取 argv，不读其后的
/// 环境变量（可能含密钥）。
#[cfg(target_os = "macos")]
fn process_command(pid: i32) -> Option<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let mut argmax: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: 输出缓冲为一个 c_int，大小以 size 传入。
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut argmax as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || argmax <= 0 {
        return None;
    }
    let mut buffer = vec![0u8; argmax as usize];
    let mut size = buffer.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: 缓冲区长度以 size 传入，内核按其写入并回填实际长度。
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buffer.truncate(size);
    parse_procargs2(&buffer)
}

/// 解析 `KERN_PROCARGS2` 缓冲：argc（int）、可执行文件路径、若干 NUL
/// 填充、argv 各项（NUL 结尾）、环境变量。返回以空格连接的 argv。
#[cfg(any(target_os = "macos", test))]
fn parse_procargs2(buffer: &[u8]) -> Option<String> {
    let argc_bytes: [u8; 4] = buffer.get(..4)?.try_into().ok()?;
    let argc = usize::try_from(i32::from_ne_bytes(argc_bytes)).ok()?;
    let rest = &buffer[4..];
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    let args_start = path_end + rest[path_end..].iter().position(|byte| *byte != 0)?;
    let args: Vec<String> = rest[args_start..]
        .split(|byte| *byte == 0)
        .take(argc)
        .map(|arg| String::from_utf8_lossy(arg).to_string())
        .collect();
    if args.is_empty() {
        return None;
    }
    Some(args.join(" "))
}

/// Linux：读 `/proc/<pid>/stat`（字段 5 进程组、6 会话；进程名在括号内）。
#[cfg(target_os = "linux")]
fn list_processes() -> Vec<ProcessInfo> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let pid: i32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let open = stat.find('(')?;
            let close = stat.rfind(')')?;
            let name = stat[open + 1..close].to_string();
            let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
            // close 之后：状态(0) 父进程(1) 进程组(2) 会话(3)
            let ppid = fields.get(1).and_then(|value| value.parse().ok());
            let pgid = fields.get(2)?.parse().ok()?;
            let sid = fields.get(3)?.parse().ok()?;
            Some(ProcessInfo {
                pid,
                ppid,
                pgid,
                sid,
                name,
            })
        })
        .collect()
}

/// Linux：读 `/proc/<pid>/cmdline`（argv 以 NUL 分隔）。
#[cfg(target_os = "linux")]
fn process_command(pid: i32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = raw
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).to_string())
        .collect();
    if args.is_empty() {
        return None;
    }
    Some(args.join(" "))
}

/// 其他平台（Windows 等）没有 POSIX 会话概念，不跟踪。
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn list_processes() -> Vec<ProcessInfo> {
    Vec::new()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_command(_pid: i32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 登录 shell 退出后，它放到后台的进程必须仍能被识别出来（issue #672
    /// 的孤儿作业形态）；不在登记会话里的进程不得出现在结果中。
    #[cfg(unix)]
    #[test]
    fn 识别登记会话里的后台作业且不含无关进程() {
        use std::process::{Command, Stdio};

        // `setsid sh -c 'sleep 30 & echo $$ $!'`：sh 成为新会话首进程（与
        // portable-pty 启动的登录 shell 同构），起后台 sleep 后立即退出。
        // macOS 没有 setsid 命令，用 perl 的 POSIX::setsid 等价实现。
        let script = "sleep 30 </dev/null >/dev/null 2>&1 & echo $$ $!";
        let output = Command::new("perl")
            .args([
                "-MPOSIX=setsid",
                "-e",
                "setsid() or die $!; exec '/bin/sh', '-c', $ARGV[0]",
                script,
            ])
            .stdin(Stdio::null())
            .output()
            .expect("启动测试会话失败");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut parts = stdout.split_whitespace();
        let shell: u32 = parts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("未取得会话首进程号: {stdout:?} {output:?}"));
        let job: i32 = parts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("未取得后台进程号: {stdout:?}"));

        let tracker = JobTracker::default();
        tracker.register(shell, "tty-test", Some("scope-a"));
        let jobs = tracker.live_jobs(None, |_| false);
        let scoped_other = tracker.live_jobs(Some("scope-b"), |_| true);
        // SAFETY: 测试清理自己启动的进程。
        unsafe {
            libc::kill(job, libc::SIGKILL);
        }

        let found = jobs
            .iter()
            .find(|item| item.pid == job)
            .unwrap_or_else(|| panic!("应识别出 shell 退出后遗留的后台进程 {job}: {jobs:?}"));
        assert_eq!(found.terminal_id, "tty-test");
        assert!(!found.terminal_alive, "所属终端已不存在");
        assert!(
            jobs.iter().all(|item| item.terminal_id == "tty-test"),
            "不得包含未登记会话的进程: {jobs:?}"
        );
        assert!(scoped_other.is_empty(), "按宿主会话过滤: {scoped_other:?}");
    }

    /// 后台进程全部结束后登记被清理，空闲退出判定随之放行。
    #[cfg(unix)]
    #[test]
    fn 作业全部结束后登记被清理() {
        let tracker = JobTracker::default();
        // 不存在的会话号：没有任何进程属于它。
        tracker.register(i32::MAX as u32 - 7, "tty-gone", None);
        assert!(tracker.live_jobs(None, |_| false).is_empty());
        assert!(
            tracker.sessions.lock().expect("锁损坏").is_empty(),
            "无存活进程的登记应被清理"
        );
    }

    /// KERN_PROCARGS2 布局：只取 argc 个参数，跳过路径填充，不带出环境变量。
    #[test]
    fn 解析_procargs2_只取_argv() {
        let mut buffer = 2i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/bin/sleep\0\0\0\0sleep\0");
        buffer.extend_from_slice(b"300\0SECRET=token\0\0");
        assert_eq!(parse_procargs2(&buffer).as_deref(), Some("sleep 300"));
        assert_eq!(parse_procargs2(&[1, 0]), None, "截断缓冲不应 panic");
        let mut empty = 0i32.to_ne_bytes().to_vec();
        empty.extend_from_slice(b"/bin/x\0\0");
        assert_eq!(parse_procargs2(&empty), None);
    }

    /// 会话进程树：含会话首进程与其子进程，父子关系与命令行正确，
    /// 不含其他会话的进程。
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn 列出会话内进程树含父子关系与命令行() {
        use std::process::{Command, Stdio};

        let mut leader = Command::new("perl")
            .args([
                "-MPOSIX=setsid",
                "-e",
                "setsid() or die $!; exec '/bin/sh', '-c', $ARGV[0]",
                "sleep 37 & wait",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("启动测试会话失败");
        let sid = leader.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let processes = loop {
            let processes = session_processes(sid);
            if processes.iter().any(|process| process.pid != sid as i32)
                || std::time::Instant::now() > deadline
            {
                break processes;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        // SAFETY: 测试清理自己创建的进程组（会话首进程即组长）。
        unsafe {
            libc::kill(-(sid as i32), libc::SIGKILL);
        }
        let _ = leader.wait();

        assert!(
            processes.iter().any(|process| process.pid == sid as i32),
            "应含会话首进程: {processes:?}"
        );
        let child = processes
            .iter()
            .find(|process| process.pid != sid as i32)
            .unwrap_or_else(|| panic!("应含子进程: {processes:?}"));
        assert_eq!(child.ppid, Some(sid as i32), "父进程应为会话首进程");
        assert_eq!(child.command.as_deref(), Some("sleep 37"));
        assert!(session_processes(i32::MAX as u32 - 7).is_empty());
    }
}
