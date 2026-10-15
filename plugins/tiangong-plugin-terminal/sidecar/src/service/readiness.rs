//! Shell 就绪与前台探测：命令执行前的就绪握手、超时未收尾时的自愈
//! 标记，以及 PTY 前台进程组/tty 模式判定。
//!
//! 执行（[`super::exec`]）与取消（[`super::dispatch`]）共用这里的判定，
//! 保证「shell 是否已重新接管终端」只有一种口径。

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use super::shell::{ShellKind, shell_ready_probe, shell_ready_probe_completed};
use super::{COMMAND_POLL_INTERVAL_MS, PtySession, SessionPhase, ShellNotReady, TerminalService};

/// 登录 shell 清理残留输入并确认重新接管终端的最长等待时间。
const SHELL_READY_TIMEOUT_SECS: u64 = 3;

impl TerminalService {
    pub(super) async fn prepare_shell_for_command(&self, session_id: &str) -> Result<()> {
        let initial_deadline = Instant::now() + Duration::from_secs(SHELL_READY_TIMEOUT_SECS);
        while self.with_session(session_id, |session| Ok(session.raw_bytes_total == 0))?
            && Instant::now() < initial_deadline
        {
            tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
        }
        // ConPTY 可能先交付半段提示符；此时立即写入 Ctrl+C 会让刚启动的
        // cmd.exe 退出。等待一个短稳定窗口后再清理残留输入。
        tokio::time::sleep(Duration::from_millis(300)).await;

        let marker = format!("__TIANGONG_READY_{}__", scru128::new());
        let initialized = self.with_session(session_id, |session| Ok(session.shell_ready))?;
        if initialized {
            let start_offset =
                self.with_session(session_id, |session| Ok(session.raw_bytes_total))?;
            self.with_session(session_id, |session| {
                session
                    .writer
                    .write_all(b"\x03")
                    .context("清理终端残留输入失败")?;
                session.writer.flush().context("刷新终端中断输入失败")
            })?;

            // 提示符空闲时部分 shell 处理 Ctrl+C 不会产生完整新行。短暂等待
            // 任意输出变化；即使没有回显也继续由下面的精确探针判断是否就绪。
            let interrupt_deadline = Instant::now() + Duration::from_millis(300);
            loop {
                let raw = self.raw_output_since(session_id, start_offset)?;
                if !raw.is_empty() || Instant::now() >= interrupt_deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
            }
        }
        if initialized
            && self.with_session(session_id, |session| {
                Ok(session.shell == ShellKind::PowerShell)
            })?
        {
            // PowerShell 的 Ctrl+C 处理完成前送入的下一行可能被丢弃。
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let probe_offset = self.with_session(session_id, |session| Ok(session.raw_bytes_total))?;
        self.with_session(session_id, |session| {
            session
                .writer
                .write_all(shell_ready_probe(session.shell, &marker).as_bytes())
                .context("发送 Shell 就绪探针失败")?;
            session.writer.flush().context("刷新 Shell 就绪探针失败")
        })?;

        let probe_deadline = Instant::now() + Duration::from_secs(SHELL_READY_TIMEOUT_SECS);
        loop {
            let raw = self.raw_output_since(session_id, probe_offset)?;
            let probe_completed =
                shell_ready_probe_completed(&String::from_utf8_lossy(&raw), &marker);
            if probe_completed {
                self.with_session(session_id, |session| {
                    session.shell_ready = true;
                    session.pending_ready_marker = None;
                    Ok(())
                })?;
                return Ok(());
            }
            if Instant::now() >= probe_deadline {
                let output = String::from_utf8_lossy(&raw);
                let reversed_tail = output.chars().rev().take(2048).collect::<String>();
                let output_tail = reversed_tail.chars().rev().collect::<String>();
                // 终端**不关闭**：前台进程树可能仍在收尾，用户也可能用
                // `&` / `nohup` 起了要长期运行的作业，或终端里就是一个
                // 用户正在看的程序。杀 PTY 会给整个进程组发 SIGHUP，
                // 把这些作业和输出一起带走。改为标记不可自动复用：
                // 终端保持存活可见可输入，只是不再被选终端选中。
                let _ = self.with_session(session_id, |session| {
                    session.phase = SessionPhase::Unresponsive;
                    // shell 是否还认账已不确定，下次使用重新走完整握手。
                    session.shell_ready = false;
                    // 探针还躺在 shell 的输入缓冲里：前台进程树收尾后
                    // shell 会执行它并回显 marker，选终端据此判断恢复。
                    session.pending_ready_marker = Some(marker.clone());
                    Ok(())
                });
                tracing::warn!(
                    session_id,
                    probe_output = ?output_tail,
                    "等待 Shell 就绪超时，终端保留但不再自动复用"
                );
                return Err(anyhow::Error::new(ShellNotReady));
            }
            tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
        }
    }

    /// 超时未确认收尾：终端保持存活但移出自动复用池，并往输入缓冲写入
    /// 一条就绪探针——前台命令收尾后 shell 会执行它并回显 marker，
    /// 下次选终端时据此把终端恢复回池（复用就绪确认超时的自愈机制）。
    pub(super) fn mark_unresponsive_with_probe(&self, session_id: &str) {
        let marker = format!("__TIANGONG_READY_{}__", scru128::new());
        let marked = self.with_session(session_id, |session| {
            let probe = shell_ready_probe(session.shell, &marker);
            session.writer.write_all(probe.as_bytes()).ok();
            session.writer.flush().ok();
            session.phase = SessionPhase::Unresponsive;
            session.shell_ready = false;
            session.pending_ready_marker = Some(marker);
            Ok(())
        });
        match marked {
            Ok(()) => tracing::warn!(session_id, "命令超时未确认收尾，终端暂不自动复用"),
            Err(error) => {
                tracing::debug!(session_id, %error, "标记未收尾终端时会话已不可用")
            }
        }
    }

    /// PTY 前台进程组归属：登录 shell 自身，还是被子程序占据。
    ///
    /// 「前台是 shell」是命令已结束、shell 已回到提示符的必要条件——命令
    /// 运行期间 shell 把前台让给命令的进程组。被占据时再读 tty 模式：
    /// 交互程序（ssh、vi、REPL）为逐键读取必然清除 `ICANON`，而
    /// `cargo build`、`pytest`、`sleep` 这类命令从不碰 tty 模式。
    ///
    /// 取不到信息（非 Unix、fd 不可用）时返回 None，调用方按「无法判定」
    /// 处理。
    #[cfg(unix)]
    pub(super) fn foreground(&self, session_id: &str) -> Option<Foreground> {
        self.with_session(session_id, |session| Ok(foreground_of(session)))
            .ok()
            .flatten()
    }

    /// Windows（ConPTY）没有前台进程组与 termios，无法判定。
    #[cfg(not(unix))]
    pub(super) fn foreground(&self, _session_id: &str) -> Option<Foreground> {
        None
    }

    /// PTY 前台是否是一个正在等待按键的交互程序（见 [`Self::foreground`]）。
    ///
    /// 前台被占据单独不足以判定：任何前台命令都会占据前台进程组。tty
    /// 非规范模式才是区分「交互程序在等输入」与「普通命令在安静地跑」
    /// 的依据，因此长时间编译、测试不会被误判。
    pub(super) fn foreground_is_interactive(&self, session_id: &str) -> Option<bool> {
        self.foreground(session_id)
            .map(|foreground| matches!(foreground, Foreground::Program { interactive: true }))
    }
}

/// PTY 前台进程组归属（见 [`TerminalService::foreground`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Foreground {
    /// 前台是登录 shell 自身：没有命令占据终端。
    Shell,
    /// 前台被子程序占据；`interactive` 表示 tty 已切成非规范模式。
    Program { interactive: bool },
}

/// 直接基于会话判定前台归属：供已持有会话表锁的调用方（选终端、
/// terminal_send 的完成判定）使用，避免重入加锁。
#[cfg(unix)]
pub(super) fn foreground_of(session: &PtySession) -> Option<Foreground> {
    let shell_pid = session.shell_pid? as i64;
    let leader = session.master.process_group_leader()? as i64;
    if leader == shell_pid {
        return Some(Foreground::Shell);
    }
    let fd = session.master.as_raw_fd()?;
    let mut termios = std::mem::MaybeUninit::<libc::termios>::zeroed();
    // SAFETY: fd 来自存活会话持有的 PTY master，tcgetattr 只读取属性。
    if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: tcgetattr 返回 0 表示结构已被完整初始化。
    let termios = unsafe { termios.assume_init() };
    Some(Foreground::Program {
        interactive: termios.c_lflag & libc::ICANON == 0,
    })
}

/// Windows（ConPTY）没有前台进程组与 termios，无法判定。
#[cfg(not(unix))]
pub(super) fn foreground_of(_session: &PtySession) -> Option<Foreground> {
    None
}
