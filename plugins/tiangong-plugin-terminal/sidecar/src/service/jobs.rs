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
//!   不可用的 `ps` / `pgrep`。
//!
//! 只匹配本进程登记过的 sid，不会扩大到任意用户进程。

use std::sync::Mutex;

use serde::Serialize;

/// 登记的作业会话上限：防止反复新建终端时无界增长。
const MAX_TRACKED_JOB_SESSIONS: usize = 256;

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

/// 枚举到的一个进程。
struct ProcessInfo {
    pid: i32,
    pgid: i32,
    sid: i32,
    name: String,
}

/// macOS：`proc_listallpids` 枚举 + `getsid`/`getpgid` 读取会话与进程组。
/// 沙箱内 `ps` / `pgrep` 依赖的 sysmond 不可用，libproc 调用不受影响。
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
                pgid,
                sid,
                name,
            })
        })
        .collect()
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
            let pgid = fields.get(2)?.parse().ok()?;
            let sid = fields.get(3)?.parse().ok()?;
            Some(ProcessInfo {
                pid,
                pgid,
                sid,
                name,
            })
        })
        .collect()
}

/// 其他平台（Windows 等）没有 POSIX 会话概念，不跟踪。
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn list_processes() -> Vec<ProcessInfo> {
    Vec::new()
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
}
