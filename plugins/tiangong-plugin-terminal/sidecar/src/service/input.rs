//! `terminal_send` 交互输入：转义解码、写入，以及所发命令的完成判定。
//!
//! 固定时长等待无法反映命令真实状态：`sleep 60` 回显后就静默，旧实现
//! 静默 300ms 即返回「成功」，Agent 随即发送的下一条命令被排进 tty 输入
//! 缓冲、延后执行，输出整体错位且不会自行收敛（issue #673）。现在
//! `enter=true` 发出的命令会登记为待确认命令，`wait` 只是**上限**：
//!
//! - 本地 shell：前台进程组回到登录 shell 即完成（精确信号）；
//! - 交互程序内（SSH 远端 shell 等）：读不到对端进程状态，以「发送前的
//!   提示符重新出现在输出末尾」判定完成；程序退出、前台回到登录 shell
//!   同样判为完成；提示符变了（`cd` 改了带路径的提示符、REPL 续行 `...`、
//!   `[sudo] password:`）时，输出静默且光标停在一行未完成文本上即判为
//!   interactive 交还控制权，不让登记残留把终端永久锁住；
//! - 不带回车的 `\x03` 只是把中断键写给程序，不清除登记：程序真被中断
//!   会重绘提示符而按上面的规则完成；忽略中断（`trap '' INT`）仍在跑时
//!   继续拒绝新命令，否则新命令会被排进 tty 输入缓冲、等程序结束才执行；
//! - 命令拉起了新的交互程序（ssh 登录、vi、REPL）：前台切成非规范模式
//!   且输出静默即返回 interactive；
//! - 以上都不成立时等到 wait 上限，如实返回 still_running。
//!
//! 待确认命令未完成前，再次以 `enter=true` 发送会被拒绝（不写入），
//! 避免命令被排进输入缓冲延后执行；空输入用于继续等待并增量读取输出。

use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

use super::output::visible_text_from_raw;
use super::readiness::{Foreground, foreground_of, tty_canonical};
use super::types::{SendRequest, SendResponse};
use super::{COMMAND_POLL_INTERVAL_MS, PtySession, SessionPhase, TerminalService};
use crate::persist;

/// terminal_send 自动回车：正文写入后、单独补发回车前的间隔。SSH 等远端
/// 行编辑器在正文与回车同包到达时偶发吞掉回车，分开写入可稳定触发执行。
const SEND_ENTER_DELAY_MS: u64 = 80;

/// 未指定 wait 时的等待上限（秒）。wait 是上限而非固定时长：命令一旦确认
/// 完成即返回；未完成时返回 still_running，可用空输入继续等待。
const SEND_DEFAULT_WAIT_SECS: u64 = 10;

/// wait 的上限：须小于工具声明的 120 秒调用时限，保证如实返回
/// still_running，而不是被宿主按超时取消。
const SEND_MAX_WAIT_SECS: u64 = 110;

/// 判定完成后再等这么久的输出静默，让提示符绘制完整。
const SEND_SETTLE_MS: u64 = 300;

/// 命令拉起交互程序（ssh 登录、vi、REPL）后，输出静默达本时长即返回。
const SEND_INTERACTIVE_QUIET_MS: u64 = 500;

/// 规范模式程序等待输入的判定窗口：输出静默达本时长、且光标停在未完成
/// 的提示语行上。取 1.5 秒：进度条通常每秒刷新，不会被误判为在等待输入。
const SEND_PROMPT_QUIET_MS: u64 = 1500;

/// 只发送按键（不登记命令）时，输出静默达本时长即返回。
const SEND_KEYS_QUIET_MS: u64 = 300;

/// 单次返回的输出上限（字符），超出保留尾部。
const SEND_OUTPUT_LIMIT_CHARS: usize = 12_000;

/// 提示符行的最大长度：超过的尾行不当作提示符（避免把长输出行误作基线）。
const PROMPT_MAX_CHARS: usize = 200;

/// `enter=true` 发出、尚未确认完成的命令。
#[derive(Debug, Clone)]
pub(super) struct PendingSend {
    pub(super) command: String,
    pub(super) started_at: Instant,
    /// 回车写入前的原始输出偏移：完成判定只看此后的输出。
    start_offset: u64,
    /// 发送时前台是否为本地登录 shell（可按前台进程组精确判定完成）。
    from_shell: bool,
    /// 发送时 shell 停在行编辑器里（tty 为非规范模式，zsh ZLE / bash
    /// readline）。这类 shell 回到提示符必然重新切成非规范模式；执行
    /// `read` 等内建命令等待整行输入时前台仍是 shell，但 tty 是规范模式，
    /// 据此区分「已完成」与「内建命令在等输入」。
    line_editor: bool,
    /// 发送前的提示符行（交互程序内以其重新出现判定完成）。
    prompt: Option<String>,
}

/// 所发命令的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendStatus {
    /// 已确认执行完成。
    Completed,
    /// 仍在执行（或在等待输入）。
    Running,
    /// 命令拉起了交互程序，正在等待输入。
    Interactive,
}

impl SendStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Running => "still_running",
            Self::Interactive => "interactive",
        }
    }
}

pub(super) fn decode_terminal_escapes(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let Some(escaped) = chars.next() else {
            output.push('\\');
            break;
        };
        match escaped {
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            '0' => output.push('\0'),
            'e' | 'E' => output.push('\x1b'),
            '\\' => output.push('\\'),
            'x' => {
                let digits = chars.clone().take(2).collect::<String>();
                if digits.len() == 2
                    && let Ok(value) = u8::from_str_radix(&digits, 16)
                {
                    chars.next();
                    chars.next();
                    output.push(char::from(value));
                } else {
                    output.push_str("\\x");
                }
            }
            'u' => {
                let digits = chars.clone().take(4).collect::<String>();
                if digits.len() == 4
                    && let Ok(value) = u32::from_str_radix(&digits, 16)
                    && let Some(value) = char::from_u32(value)
                {
                    for _ in 0..4 {
                        chars.next();
                    }
                    output.push(value);
                } else {
                    output.push_str("\\u");
                }
            }
            other => {
                output.push('\\');
                output.push(other);
            }
        }
    }
    output
}

/// 去掉末尾一个行结束符（`\r\n`、`\n` 或 `\r`），避免与自动回车叠加成两次执行。
fn strip_trailing_line_ending(input: &str) -> &str {
    input
        .strip_suffix("\r\n")
        .or_else(|| input.strip_suffix('\n'))
        .or_else(|| input.strip_suffix('\r'))
        .unwrap_or(input)
}

/// 非空且只含控制键（C0 控制字符，不含构成命令行的 `\r` `\n` `\t`，
/// 也不含引出方向键等转义序列的 `\x1b`）。
pub(super) fn is_control_keys(input: &str) -> bool {
    !input.is_empty()
        && input
            .chars()
            .all(|ch| ch.is_ascii_control() && !matches!(ch, '\r' | '\n' | '\t' | '\x1b'))
}

/// 输出经终端仿真还原后的形态：已换行结束的行数，与光标所在的未完成行。
///
/// 提示符的特征是停在**未完成行**上等待输入；命令行回显虽然也以提示符
/// 开头，但回车后已换行结束，不会被当成提示符。
fn screen_tail(raw: &[u8]) -> (usize, String) {
    let mut processor = persist::TerminalLineProcessor::new();
    let completed = processor.process(&String::from_utf8_lossy(raw)).len();
    (completed, processor.current_line().trim_end().to_string())
}

/// 光标所在的未完成行是否是提示符：与基线相等，或以基线开头（zsh 右侧
/// 提示符、用户提前键入的内容会追加在行尾）。
fn ends_with_prompt(raw: &[u8], prompt: &str) -> bool {
    let prompt = prompt.trim_end();
    if prompt.is_empty() {
        return false;
    }
    let (_, current) = screen_tail(raw);
    // 忽略中断的程序结束后，提示符紧跟在 `^C` 回显之后（`^Cremote$`）。
    let current = strip_control_echo(&current);
    current == prompt || current.starts_with(prompt)
}

/// 提示符基线候选：只在终端已确认空闲时取样，取光标所在的未完成行；
/// 只接受短行且以常见提示符结尾字符收尾（启发式，排除明显的输出行）。
fn prompt_candidate(raw: &[u8]) -> Option<String> {
    let (_, line) = screen_tail(raw);
    let trimmed = line.trim_end();
    if trimmed.is_empty() || trimmed.chars().count() > PROMPT_MAX_CHARS {
        return None;
    }
    trimmed
        .ends_with(['$', '#', '%', '>', '❯', '»', '➜', ']', ')', ':'])
        .then(|| trimmed.to_string())
}

/// 进行中的 terminal_send 登记守卫（见 [`PtySession::send_in_flight`]）。
struct SendInFlight<'a> {
    sessions: &'a Mutex<HashMap<String, PtySession>>,
    session_id: &'a str,
}

impl<'a> SendInFlight<'a> {
    fn enter(sessions: &'a Mutex<HashMap<String, PtySession>>, session_id: &'a str) -> Self {
        if let Some(session) = sessions.lock().expect("会话表锁损坏").get_mut(session_id) {
            session.send_in_flight = session.send_in_flight.saturating_add(1);
        }
        Self {
            sessions,
            session_id,
        }
    }
}

impl Drop for SendInFlight<'_> {
    fn drop(&mut self) {
        // 锁中毒时放弃撤销：Drop 内不能再 panic。
        if let Ok(mut sessions) = self.sessions.lock()
            && let Some(session) = sessions.get_mut(self.session_id)
        {
            session.send_in_flight = session.send_in_flight.saturating_sub(1);
        }
    }
}

/// 截取尾部，避免一次返回过长输出。
fn tail_chars(text: String, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text;
    }
    let skip = count - limit;
    let tail: String = text.chars().skip(skip).collect();
    format!("…（已省略前 {skip} 个字符）\n{tail}")
}

impl TerminalService {
    pub(super) async fn send_to_session(&self, request: SendRequest) -> Result<SendResponse> {
        // 整个调用期间（含判定完成后等提示符绘制完整的收尾窗口）登记为
        // 进行中，选终端据此跳过本终端：并行发出的 run_command 若在此
        // 窗口复用它，包装脚本会混进本次返回的输出。守卫在 future 被丢弃
        //（调用取消）时同样撤销登记。
        let session_id = request.session_id.clone();
        let _in_flight = SendInFlight::enter(&self.sessions, &session_id);
        self.send_to_session_inner(request).await
    }

    async fn send_to_session_inner(&self, mut request: SendRequest) -> Result<SendResponse> {
        let decoded = decode_terminal_escapes(&request.input);
        // 只含控制键（`\x03` Ctrl-C、`\x04` Ctrl-D、`\x1a` Ctrl-Z 等）的输入
        // 是按键而非命令：即使调用方带了 enter=true，也按不设 enter 原样
        // 写入。否则上一条命令仍在执行时它会被当作新命令整条拒绝，中断键
        // 根本送不到程序；补发的回车也会在程序退出后多执行一个空行。
        if request.enter && is_control_keys(strip_trailing_line_ending(&decoded)) {
            request.enter = false;
        }
        let data = if request.enter {
            strip_trailing_line_ending(&decoded)
        } else {
            decoded.as_str()
        };
        let wait = Duration::from_secs(
            request
                .wait
                .unwrap_or(SEND_DEFAULT_WAIT_SECS)
                .clamp(1, SEND_MAX_WAIT_SECS),
        );

        // 写入前先确认上一条命令的状态：仍在执行时拒绝新命令，避免它被
        // 排进 tty 输入缓冲、在不可预期的时机执行。
        let (read_offset, baseline_updates, rejected, initial) = {
            let mut sessions = self.sessions.lock().expect("会话表锁损坏");
            let session = sessions
                .get_mut(&request.session_id)
                .filter(|session| session.scope_id.as_deref() == Some(request.scope_id.as_str()))
                .ok_or_else(|| anyhow::anyhow!("目标终端不存在或不属于当前会话"))?;
            if session.exited_code.is_some() {
                bail!("终端 {} 已退出，无法继续输入", request.session_id);
            }
            let had_pending = session.pending_send.is_some();
            let initial = had_pending.then(|| refresh_pending(session));
            let read_offset = session
                .send_read_offset
                .filter(|_| session.pending_send.is_some())
                .unwrap_or(session.raw_bytes_total);
            // 待确认命令已在预检时完成：本次从它的未读位置返回剩余输出。
            let read_offset = if initial == Some(SendStatus::Completed) {
                session.send_read_offset.take().unwrap_or(read_offset)
            } else {
                read_offset
            };
            let baseline_updates = session.screen_updates;
            if request.enter && session.pending_send.is_some() {
                (read_offset, baseline_updates, true, initial)
            } else {
                if request.enter {
                    let from_shell = matches!(foreground_of(session), Some(Foreground::Shell));
                    let line_editor = from_shell && tty_canonical(session) == Some(false);
                    let tail = tail_bytes(session);
                    // 只补发回车（input 为空）时输入行里是先前键入的文本，
                    // 尾行不是干净的提示符，退回最近一次记录的提示符。
                    let prompt = if data.is_empty() {
                        session.prompt_hint.clone()
                    } else {
                        prompt_candidate(&tail).or_else(|| session.prompt_hint.clone())
                    };
                    if let Some(prompt) = &prompt {
                        session.prompt_hint = Some(prompt.clone());
                    }
                    session.pending_send = Some(PendingSend {
                        command: data.trim().to_string(),
                        started_at: Instant::now(),
                        start_offset: session.raw_bytes_total,
                        from_shell,
                        line_editor,
                        prompt,
                    });
                }
                session
                    .writer
                    .write_all(data.as_bytes())
                    .context("写入交互输入失败")?;
                session.writer.flush().context("刷新交互输入失败")?;
                // 新命令从此刻起读；继续等待（不写新命令）沿用旧命令的读取位置。
                let offset = if request.enter || initial.is_none() {
                    session.raw_bytes_total
                } else {
                    read_offset
                };
                (offset, baseline_updates, false, initial)
            }
        };
        if rejected {
            return self.busy_response(&request.session_id, read_offset);
        }
        if request.enter {
            if !data.is_empty() {
                tokio::time::sleep(Duration::from_millis(SEND_ENTER_DELAY_MS)).await;
            }
            self.with_session(&request.session_id, |session| {
                if session.scope_id.as_deref() != Some(request.scope_id.as_str()) {
                    bail!("目标终端不存在或不属于当前会话");
                }
                session.writer.write_all(b"\r").context("写入回车失败")?;
                session.writer.flush().context("刷新回车失败")
            })?;
        }

        // 预检时已完成的命令（继续等待场景）：只需等提示符绘制完整。
        let initial = if request.enter { None } else { initial };
        let status = self
            .wait_for_send(&request.session_id, wait, initial)
            .await?;
        self.send_response(&request.session_id, read_offset, baseline_updates, status)
    }

    /// 等待所发命令完成或交出控制权；没有待确认命令时按输出静默返回。
    async fn wait_for_send(
        &self,
        session_id: &str,
        wait: Duration,
        initial: Option<SendStatus>,
    ) -> Result<Option<SendStatus>> {
        let deadline = Instant::now() + wait;
        let started = Instant::now();
        // 完成判定会清除登记：一旦判为完成就锁定结果，后续轮询只等提示符
        // 绘制完整，不能因登记已清除退回「没有待确认命令」的语义。
        let mut tracked = initial.is_some()
            || self.with_session(session_id, |session| Ok(session.pending_send.is_some()))?;
        let mut completed = initial == Some(SendStatus::Completed);
        loop {
            let (status, quiet_for, ended) = self.with_session(session_id, |session| {
                let quiet_for = session.last_output_at.elapsed();
                let status = if completed {
                    Some(SendStatus::Completed)
                } else if session.pending_send.is_some() {
                    Some(refresh_pending(session))
                } else {
                    None
                };
                Ok((status, quiet_for, session.exited_code.is_some()))
            })?;
            if status == Some(SendStatus::Completed) {
                completed = true;
            }
            if ended {
                return Ok(tracked.then_some(SendStatus::Completed));
            }
            let quiet = |ms: u64| quiet_for >= Duration::from_millis(ms);
            match status {
                // 确认完成后等提示符绘制完整（输出静默一个短窗口）。
                Some(SendStatus::Completed) if quiet(SEND_SETTLE_MS) => {
                    return Ok(Some(SendStatus::Completed));
                }
                Some(SendStatus::Interactive) if quiet(SEND_INTERACTIVE_QUIET_MS) => {
                    return Ok(Some(SendStatus::Interactive));
                }
                // 只发送按键（没有待确认命令）：沿用「输出静默」语义，但至少
                // 观察一个窗口，让回显与程序响应有机会到达。
                None if !tracked
                    && started.elapsed() >= Duration::from_millis(SEND_KEYS_QUIET_MS)
                    && quiet(SEND_KEYS_QUIET_MS) =>
                {
                    return Ok(None);
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Ok(status);
            }
            // 登记可能在等待中途被其他路径清除（如终端被关闭重建）。
            tracked |= status.is_some();
            tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
        }
    }

    fn send_response(
        &self,
        session_id: &str,
        read_offset: u64,
        baseline_updates: u64,
        status: Option<SendStatus>,
    ) -> Result<SendResponse> {
        let raw = self.raw_output_since(session_id, read_offset)?;
        self.with_session(session_id, |session| {
            // 命令已把终端交给交互程序（ssh 登录、vi、REPL）：命令本身视为
            // 已交接，清除登记，后续在程序内发送的命令重新登记、按提示符判定。
            if status == Some(SendStatus::Interactive) {
                session.pending_send = None;
            }
            let pending = session.pending_send.clone();
            // 还在执行时记下读取位置，下次空输入等待只返回新增部分。
            session.send_read_offset = pending.as_ref().map(|_| session.raw_bytes_total);
            let raw_output = visible_text_from_raw(&raw);
            // 有待确认命令时只返回增量输出：画面快照会混入已返回过的内容。
            let stdout = if status.is_some() {
                raw_output
            } else if session.screen_updates > baseline_updates
                && !session.screen_snapshot.trim().is_empty()
            {
                session.screen_snapshot.clone()
            } else if !raw_output.trim().is_empty() {
                raw_output
            } else {
                session.screen_snapshot.clone()
            };
            let command = pending
                .as_ref()
                .map(|pending| pending.command.clone())
                .or_else(|| status.map(|_| session.running_command.clone().unwrap_or_default()));
            Ok(SendResponse {
                session_id: session_id.to_string(),
                stdout: tail_chars(stdout, SEND_OUTPUT_LIMIT_CHARS),
                exit_code: 0,
                interactive_mode: session.phase == SessionPhase::Interactive
                    || status == Some(SendStatus::Interactive),
                status: status.map(SendStatus::label),
                command: command.filter(|command| !command.is_empty()),
                elapsed_secs: pending.map(|pending| pending.started_at.elapsed().as_secs()),
                rejected: false,
            })
        })
    }

    /// 上一条命令仍在执行：不写入本次输入，返回新增输出与运行态。
    fn busy_response(&self, session_id: &str, read_offset: u64) -> Result<SendResponse> {
        let mut response =
            self.send_response(session_id, read_offset, u64::MAX, Some(SendStatus::Running))?;
        response.rejected = true;
        Ok(response)
    }
}

/// 原始输出尾部（提示符基线取样用）：只取最后 4KB，避免整段历史做终端仿真。
fn tail_bytes(session: &PtySession) -> Vec<u8> {
    let start = session.raw_history.len().saturating_sub(4096);
    session.raw_history[start..].to_vec()
}

/// 判定待确认命令的状态；已完成时清除登记并记录新的提示符基线。
fn refresh_pending(session: &mut PtySession) -> SendStatus {
    let Some(pending) = session.pending_send.as_ref() else {
        return SendStatus::Completed;
    };
    let foreground = foreground_of(session);
    let since = output_since(session, pending.start_offset);
    let completed = if pending.from_shell {
        // 本地 shell：命令占据前台期间前台进程组不是 shell。但回车刚写入、
        // shell 还没读到回车，或已换行但尚未 fork 出命令时，前台同样是
        // shell——所以还要求命令行已换行结束、且光标行上已重绘出内容
        // （提示符）。不比对提示符文本：`cd` 等命令会改变提示符。
        let (completed_lines, current) = screen_tail(since);
        matches!(foreground, Some(Foreground::Shell))
            && completed_lines > 0
            && !current.is_empty()
            && !(pending.line_editor && tty_canonical(session) == Some(true))
    } else {
        // 交互程序内读不到对端进程状态，判据是提示符重新停在光标行上。
        // 程序退出（嵌套 shell 的 `exit`、REPL 的 `exit()`）后回到登录
        // shell，出现的是登录 shell 的提示符而不是程序的提示符：此时前台
        // 已回到登录 shell，且命令行已换行、光标行已重绘，同样判为完成。
        let (completed_lines, current) = screen_tail(since);
        let back_to_shell = matches!(foreground, Some(Foreground::Shell))
            && completed_lines > 0
            && !current.is_empty();
        back_to_shell
            || pending
                .prompt
                .as_deref()
                .is_some_and(|prompt| completed_lines > 0 && ends_with_prompt(since, prompt))
    };
    if completed {
        let tail = tail_bytes(session);
        if let Some(prompt) = prompt_candidate(&tail) {
            session.prompt_hint = Some(prompt);
        }
        session.pending_send = None;
        session.send_read_offset = None;
        return SendStatus::Completed;
    }
    match foreground {
        // 本地 shell 发出的命令拉起了交互程序（ssh 登录、vi、REPL）。
        Some(Foreground::Program { interactive: true }) if pending.from_shell => {
            SendStatus::Interactive
        }
        // 规范模式下等待输入的程序（dash 等无行编辑器的 shell、`read`、
        // `[y/N]` 确认）不会切 tty 模式，只能看输出形态：静默且光标停在
        // 一行未完成的文本（提示语）上。`sleep` 这类安静的长命令回显后光标
        // 在空行，不会命中；进度条持续刷新，达不到静默窗口。
        // 前台仍是 shell 但 tty 停在规范模式：`read` 等内建命令在等整行输入。
        Some(Foreground::Program { interactive: false }) | Some(Foreground::Shell)
            if pending.from_shell
                && session.last_output_at.elapsed()
                    >= Duration::from_millis(SEND_PROMPT_QUIET_MS)
                && waiting_at_prompt(since) =>
        {
            SendStatus::Interactive
        }
        // 交互程序内等不到发送前的提示符：提示符可能已经变了（`cd` 改了
        // 带路径的提示符、REPL 续行 `...`、`[sudo] password:`）。输出静默且
        // 光标停在一行未完成文本上，说明程序在等输入，交还控制权；与本地
        // 规范模式的判定同一标准，`sleep` 等安静命令光标在空行，不会命中。
        _ if !pending.from_shell
            && session.last_output_at.elapsed() >= Duration::from_millis(SEND_PROMPT_QUIET_MS)
            && waiting_at_program_prompt(since) =>
        {
            SendStatus::Interactive
        }
        _ => SendStatus::Running,
    }
}

/// 命令行已换行结束、光标停在一行非空的未完成文本上（提示符或提示语）。
/// 控制键回显（`\x03` 回显的 `^C` 等）不算提示语：程序仍可能在跑。
fn waiting_at_prompt(since: &[u8]) -> bool {
    let (completed_lines, current) = screen_tail(since);
    completed_lines > 0 && !strip_control_echo(&current).is_empty()
}

/// 交互程序内的等待输入判定，比 [`waiting_at_prompt`] 更严：光标行须
/// 呈提示符形态。程序内写入的按键会被 tty 回显到光标行——`\x03` 回显
/// 成 `^C`、不带回车键入的文本原样出现——而程序仍在跑，不能据此交还
/// 控制权，否则下一条命令会被排进输入缓冲、等程序结束才执行。
fn waiting_at_program_prompt(since: &[u8]) -> bool {
    let (completed_lines, current) = screen_tail(since);
    completed_lines > 0 && looks_like_prompt(&current)
}

/// 去掉行首的控制键回显（`^C`、`^D`、`^Z` 等）。
fn strip_control_echo(line: &str) -> &str {
    let mut rest = line.trim();
    while let Some(stripped) = rest.strip_prefix('^') {
        let mut chars = stripped.chars();
        match chars.next() {
            Some(key) if key.is_ascii_uppercase() || "@[\\]^_?".contains(key) => {
                rest = chars.as_str().trim_start();
            }
            _ => break,
        }
    }
    rest
}

/// 去掉控制键回显后，以常见提示符结尾字符或 REPL 续行 `...` 收尾。
fn looks_like_prompt(line: &str) -> bool {
    let rest = strip_control_echo(line);
    !rest.is_empty()
        && (rest.ends_with("...")
            || rest.ends_with(['$', '#', '%', '>', '❯', '»', '➜', ']', ')', ':', '?']))
}

/// 自 `offset` 起的原始输出（被环形缓冲截掉的部分从可用起点读）。
fn output_since(session: &PtySession, offset: u64) -> &[u8] {
    let available_start = offset.max(session.raw_history_start);
    let index = available_start
        .saturating_sub(session.raw_history_start)
        .min(session.raw_history.len() as u64) as usize;
    &session.raw_history[index..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 自动回车只去掉末尾一个换行() {
        assert_eq!(strip_trailing_line_ending("ls -la"), "ls -la");
        assert_eq!(strip_trailing_line_ending("ls\n"), "ls");
        assert_eq!(strip_trailing_line_ending("ls\r"), "ls");
        assert_eq!(strip_trailing_line_ending("ls\r\n"), "ls");
        assert_eq!(strip_trailing_line_ending("a\n\n"), "a\n");
        assert_eq!(strip_trailing_line_ending(""), "");
    }

    #[test]
    fn 提示符基线只接受短行且以提示符字符收尾() {
        assert_eq!(
            prompt_candidate(b"output\r\nuser@host:~$ ").as_deref(),
            Some("user@host:~$")
        );
        assert_eq!(
            prompt_candidate(b"\x1b[1;32m\xe2\x9d\xaf\x1b[0m ").as_deref(),
            Some("❯")
        );
        assert_eq!(
            prompt_candidate(b"Pulling layer 3/9"),
            None,
            "输出行不是提示符"
        );
        let long = format!("{}$", "x".repeat(PROMPT_MAX_CHARS + 1));
        assert_eq!(prompt_candidate(long.as_bytes()), None, "过长行不作基线");
    }

    /// 远端（SSH）命令完成判定：提示符必须在命令输出**之后**重新出现，
    /// 命令行回显本身以提示符开头，不能算完成。
    #[test]
    fn 提示符重新出现才算远端命令完成() {
        let prompt = "root@spark:~#";
        assert!(
            !ends_with_prompt(b"root@spark:~# sleep 60\r\n", prompt),
            "只有回显时仍在执行"
        );
        assert!(
            ends_with_prompt(b"root@spark:~# sleep 60\r\nroot@spark:~# ", prompt),
            "提示符重新出现即完成"
        );
        assert!(
            ends_with_prompt(
                b"root@spark:~# echo hi\r\nhi\r\nroot@spark:~# \x1b[K",
                prompt
            ),
            "提示符后的擦除序列不影响判定"
        );
        assert!(
            !ends_with_prompt(b"root@spark:~# docker pull x\r\nPulling layer 3/9", prompt),
            "进度输出中不算完成"
        );
    }

    #[test]
    fn 超长输出保留尾部() {
        let text = "a".repeat(20) + "TAIL";
        let tail = tail_chars(text, 4);
        assert!(tail.ends_with("TAIL") && tail.contains("已省略前 20 个字符"));
        assert_eq!(tail_chars("short".to_string(), 10), "short");
    }

    #[test]
    fn 只含控制键的输入识别() {
        assert!(is_control_keys("\x03"));
        assert!(is_control_keys("\x03\x03"));
        assert!(is_control_keys("\x04"));
        assert!(is_control_keys("\x1a"));
        assert!(!is_control_keys(""));
        assert!(!is_control_keys("ls"));
        assert!(!is_control_keys("\x03ls"));
        assert!(!is_control_keys("\x1b[A"));
        assert!(!is_control_keys("\t"));
    }

    #[test]
    fn 控制键回显不算提示语() {
        assert_eq!(strip_control_echo("^C"), "");
        assert_eq!(strip_control_echo("^C^C"), "");
        assert_eq!(strip_control_echo("^Cremote$"), "remote$");
        assert_eq!(strip_control_echo("^ caret"), "^ caret");
        assert!(!waiting_at_prompt(b"sleep 30\r\n^C"));
        assert!(waiting_at_prompt(b"read\r\nname? "));
        assert!(ends_with_prompt(b"sleep 4\r\n^Cremote$ ", "remote$ "));
    }

    #[test]
    fn 程序内只认提示符形态的光标行() {
        assert!(looks_like_prompt("/tmp$"));
        assert!(looks_like_prompt("..."));
        assert!(looks_like_prompt("[sudo] password for me:"));
        assert!(looks_like_prompt("^C>>>"));
        assert!(!looks_like_prompt("^C"));
        assert!(!looks_like_prompt("echo released"));
        assert!(!waiting_at_program_prompt(b"sleep 30\r\n^Cecho released"));
    }
}
