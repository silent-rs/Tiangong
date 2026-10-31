//! PTY 会话服务：请求-响应操作 + 输出流通知。
//!
//! 按职责拆分（`impl TerminalService` 分布在各模块，状态集中在本文件）：
//! - [`session`]：PTY 会话的创建、选取复用与回收；
//! - [`readiness`]：Shell 就绪握手与前台进程/tty 模式探测；
//! - [`exec`]：Agent 命令执行（包装执行与夭折/交互/超时兜底收尾）；
//! - [`input`]：`terminal_send` 交互输入；
//! - [`frontend`]：前端标签附着、查找与存活集合回收；
//! - [`tools`]：Agent 工具入口与结果编排；
//! - [`dispatch`]：sidecar 协议分发与请求级取消；
//! - [`shell`] / [`protocol`] / [`output`]：Shell 方言、命令包装协议与
//!   PTY 输出处理（不持有服务状态的纯逻辑）；
//! - [`types`]：sidecar 操作的请求/响应载荷。

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use portable_pty::ChildKiller;

use crate::persist;
use shell::ShellKind;
use tools::ActiveToolRegistration;

mod advice;
mod dispatch;
mod exec;
mod frontend;
mod input;
mod jobs;
mod output;
mod protocol;
mod readiness;
mod session;
mod shell;
#[cfg(test)]
mod tests;
mod tools;
mod types;

/// 通知通道：PTY 输出流（负载 JSON 见 `OutputNotification`）。
pub const CHANNEL_OUTPUT: &str = "terminal.output";

/// 通知通道：会话退出（负载 JSON 见 `ExitNotification`）。
pub const CHANNEL_EXIT: &str = "terminal.exit";

/// 就绪握手失败的标记错误：探针超时后终端被标记为不可自动复用（但保持
/// 存活——里面可能有 `&` / `nohup` 起的长期作业和用户正在看的输出），
/// 调用方可据此换一个新终端重试（新终端走不发 Ctrl+C 的干净握手路径）。
#[derive(Debug)]
struct ShellNotReady;

impl std::fmt::Display for ShellNotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("等待 Shell 就绪超时，终端已保留但不再自动复用")
    }
}

impl std::error::Error for ShellNotReady {}

/// 轮询 PTY 输出的间隔。
const COMMAND_POLL_INTERVAL_MS: u64 = 50;

/// 最后一个 PTY 会话结束后保留短暂窗口，让关闭响应完成并容纳紧邻的新建请求。
const SIDECAR_IDLE_EXIT_SECS: u64 = 5;

/// 空闲时检查遗留后台作业的间隔：枚举进程有成本，不必每轮都做。
const BACKGROUND_JOB_CHECK_INTERVAL_SECS: u64 = 2;

/// 空闲 PTY 回收检查间隔：全表扫描 + 作业枚举有成本，粗粒度即可。
const IDLE_PTY_CHECK_INTERVAL_SECS: u64 = 60;

/// 空闲 PTY 回收阈值（秒，issue #697）：无运行命令、无等待输入、无后台
/// 作业且输出静默超过此时长的存活终端被终止，防止 CLI / Server 长会话与
/// 常驻 Server 下终端越积越多。环境变量
/// `TIANGONG_TERMINAL_IDLE_PTY_SECS` 可覆盖（0 = 关闭回收）。
const IDLE_PTY_RECYCLE_SECS: u64 = 1800;

/// 启动时解析空闲 PTY 回收阈值（环境变量覆盖）。
fn idle_pty_recycle_after() -> Option<Duration> {
    let secs = std::env::var("TIANGONG_TERMINAL_IDLE_PTY_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(IDLE_PTY_RECYCLE_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

pub struct TerminalService {
    sessions: Arc<Mutex<HashMap<String, PtySession>>>,
    /// 已完成事件订阅、PTY 绑定并通过 terminalAttach 确认的终端。
    frontend_attached: Mutex<HashSet<String>>,
    /// 已被前端至少一次确认存在的标签。并行挂载时集合会逐步变完整，只有
    /// 曾确认存在、后来消失的终端才允许被 GC 回收。
    confirmed_frontend_tabs: Mutex<HashMap<String, HashSet<String>>>,
    last_activity: Arc<Mutex<Option<Instant>>>,
    sequence: Mutex<u64>,
    spawn_lock: Mutex<()>,
    /// 进行中或等待正式取消处理的 Agent 工具调用（sidecar request_id → 执行终端）。
    /// 执行 future 可以先被丢弃，本表必须保留到正常完成或 cancel() 主动清除。
    active_tools: Arc<Mutex<HashMap<String, ActiveTool>>>,
    /// 登录 shell 的作业会话登记：识别 `&` / `nohup` 遗留的后台进程，
    /// 空闲退出前确认它们已全部结束（issue #672，见 [`jobs`]）。
    jobs: Arc<jobs::JobTracker>,
}

/// 活动工具调用的取消所需信息。
#[derive(Debug, Clone)]
struct ActiveTool {
    terminal_id: String,
    /// 本次调用请求了交互模式（vi/ssh 等）。执行期间 phase 仍是 Running，
    /// 只有返回结果后才置 Interactive，因此取消时不能只看 phase：调用自己
    /// 拉起的交互程序必须靠本标志识别，否则会被中断确认流误杀。
    interactive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionPhase {
    Idle,
    Reserved,
    Running,
    Interactive,
    /// 就绪确认失败：Ctrl+C 之后 shell 没在预算内回到提示符（前台进程树
    /// 仍在收尾、前台程序吃掉了探针等）。终端**保持存活**——用户用 `&`
    /// 或 `nohup` 起的后台作业、可见输出和可继续输入的会话都不受影响，
    /// 只是不再被自动复用；由用户自行继续使用或关闭。
    Unresponsive,
}

/// 会话相对死亡/回收的三态，见 [`TerminalService::session_end_state`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionEndState {
    /// 会话在表且 shell 存活。
    Alive,
    /// shell 已退出（携带退出码），等待命令收尾后回收。
    Ended(i32),
    /// 会话已被移出表（手动关闭或回收路径先一步完成）。
    Reaped,
}

struct PtySession {
    shell: ShellKind,
    shell_ready: bool,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// 同一终端内的 Agent 命令串行执行，避免边界标记相互穿插。
    exec_lock: Arc<tokio::sync::Mutex<()>>,
    /// 宿主会话标识：终端归属与隔离的锚点（同 scope 取最新会话）。
    scope_id: Option<String>,
    /// 创建序号：单调递增，同 scope 多会话时分辨最新。
    sequence: u64,
    /// Agent 命令选择与交互状态；选择时只复用空闲终端。
    phase: SessionPhase,
    /// 原始 PTY 输出：仅供命令边界识别与结果收集，包含内部标记。
    raw_history: Vec<u8>,
    /// 原始缓冲首字节在会话输出流中的绝对偏移。
    raw_history_start: u64,
    /// 原始输出累计字节数，不受环形缓冲截断影响。
    raw_bytes_total: u64,
    /// 最近一次收到 PTY 输出的时刻（判定输出静默）。
    last_output_at: Instant,
    /// 用户可见输出：过滤内部标记后供 UI 重放。
    display_history: Vec<u8>,
    /// 本终端是否曾有前端页面附着过（会话级事实，非宿主形态）：没有
    /// 前端的终端由 sidecar 屏幕模拟器生成快照（issue #697）。
    frontend_attached_once: bool,
    /// sidecar 内的终端屏幕模拟（headless 画面快照来源，issue #697）。
    /// 桌面模式同样维护（供终端恢复与后续统一解析评估），快照仍由前端
    /// 回传为准；headless 模式由它生成 `screen_snapshot`。
    screen: Arc<Mutex<crate::screen::ScreenGrid>>,
    /// xterm 回传的当前可见画面，供 terminal_send 返回交互结果。
    screen_snapshot: String,
    screen_updates: u64,
    /// shell 已退出（信号等异常退出时取不到退出码记 -1）。标记死亡而非
    /// 立即出表：执行中的命令仍需读取输出与退出码正常收尾；出表由命令
    /// 收尾或下次选终端时完成。
    exited_code: Option<i32>,
    /// 就绪确认超时时遗留在 shell 输入缓冲里的探针 marker。前台进程树
    /// 收尾后 shell 会把它执行掉并回显；选终端时据此判断 Unresponsive
    /// 终端是否已自行恢复，避免长任务结束后终端永久退出复用池。
    pending_ready_marker: Option<String>,
    /// terminal_send 以 enter=true 发出、尚未确认完成的命令（见
    /// [`input::PendingSend`]）。
    pending_send: Option<input::PendingSend>,
    /// 进行中的 terminal_send 调用数：判定完成后清除登记、到真正返回之间
    /// 还要等提示符绘制完整，此期间选终端不得把它交给 run_command，否则
    /// 包装脚本会混进本次 terminal_send 的输出。
    send_in_flight: u32,
    /// terminal_send 增量读取游标：重复等待时只返回上次之后的新输出。
    send_read_offset: Option<u64>,
    /// 最近确认回到提示符时的提示符行：只补发回车（input 为空）时，
    /// 输入行里混着先前键入的文本，以它作为完成判定的提示符基线。
    prompt_hint: Option<String>,
    /// 正在执行的命令文本（进入执行态时记录，供 terminal_status 查询）。
    /// 回到空闲时清空；交互中与未就绪（收尾中）保留——查询时能看到
    /// 终端里跑的是什么程序对排查有直接价值。
    running_command: Option<String>,
    /// 本次执行为交互模式且命令已写入 PTY（程序真正启动）。取消据此
    /// 区分「程序已在前台须保留转交互」与「尚未启动只须释放预留」。
    interactive_started: bool,
    /// 登录 shell 自身的进程号：前台进程组与其不同即说明 PTY 前台被
    /// 子程序占据（交互程序在跑），用于识别「非交互命令实际拉起了
    /// 交互程序并停在等待输入」的静默挂起。
    shell_pid: Option<u32>,
    /// 输出持久化日志（按 scope 分文件）：打开失败为 None（优雅降级）。
    logger: Option<Arc<persist::OutputLogger>>,
    /// 待回显的命令文本（start marker → 命令）。与输出过滤线程共享：
    /// 命令写入 PTY 前登记，过滤器在 start marker 到达时取出，把 marker
    /// 行替换成命令行——命令与其输出因此严格相邻。
    pending_echo: Arc<Mutex<HashMap<String, String>>>,
}

impl Default for TerminalService {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalService {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            frontend_attached: Mutex::new(HashSet::new()),
            confirmed_frontend_tabs: Mutex::new(HashMap::new()),
            last_activity: Arc::new(Mutex::new(None)),
            sequence: Mutex::new(0),
            spawn_lock: Mutex::new(()),
            active_tools: Arc::new(Mutex::new(HashMap::new())),
            jobs: Arc::new(jobs::JobTracker::default()),
        }
    }

    /// 登记一次执行中的工具调用。正常完成时显式清除；future 被丢弃时
    /// 保留映射，供随后到达的正式取消处理定位 PTY。
    fn track_active_tool(
        &self,
        request_id: &str,
        terminal_id: &str,
        interactive: bool,
    ) -> ActiveToolRegistration {
        self.active_tools.lock().expect("活动工具表锁损坏").insert(
            request_id.to_string(),
            ActiveTool {
                terminal_id: terminal_id.to_string(),
                interactive,
            },
        );
        ActiveToolRegistration {
            active_tools: Arc::clone(&self.active_tools),
            request_id: request_id.to_string(),
        }
    }

    /// terminal 不随 App 常驻：首次业务请求由宿主按需拉起，最后一个 PTY
    /// 会话结束且空闲窗口届满后退出；共享连接会在下次操作时重新启动。
    ///
    /// 仍有终端遗留的后台作业（`&` / `nohup`）时不退出：退出后它们落在
    /// 旧沙箱实例里，新 sidecar 无权向其发信号，只能请用户手动结束
    /// （issue #672）。保持存活期间 Agent 可经 terminal_status 定位并结束。
    pub fn start_idle_exit_monitor(&self) {
        let sessions = Arc::clone(&self.sessions);
        let last_activity = Arc::clone(&self.last_activity);
        let jobs = Arc::clone(&self.jobs);
        let recycle_after = idle_pty_recycle_after();
        if recycle_after.is_some() {
            tracing::info!(
                after_secs = recycle_after
                    .map(|duration| duration.as_secs())
                    .unwrap_or_default(),
                "空闲 PTY 回收已启用（环境变量 TIANGONG_TERMINAL_IDLE_PTY_SECS 可调整）"
            );
        }
        if let Err(error) = std::thread::Builder::new()
            .name("terminal-idle-exit".to_string())
            .spawn(move || {
                let mut last_job_check: Option<Instant> = None;
                let mut reported_jobs = false;
                let mut last_recycle_check: Option<Instant> = None;
                loop {
                    std::thread::sleep(Duration::from_millis(250));
                    // 空闲 PTY 回收（issue #697）：Server 长期运行时无主终端
                    // 不会自己退出，靠本检查兜底；无候选时为纯表扫描。
                    if recycle_after.is_some()
                        && last_recycle_check.is_none_or(|at| {
                            at.elapsed() >= Duration::from_secs(IDLE_PTY_CHECK_INTERVAL_SECS)
                        })
                    {
                        last_recycle_check = Some(Instant::now());
                        recycle_idle_ptys(&sessions, &jobs, recycle_after);
                    }
                    // 已死亡未回收的会话不算活跃：否则 shell 退出后 sidecar
                    // 永远等不到空闲退出。
                    let has_sessions = sessions
                        .lock()
                        .map(|sessions| {
                            sessions
                                .values()
                                .any(|session| session.exited_code.is_none())
                        })
                        .unwrap_or(true);
                    if has_sessions {
                        continue;
                    }
                    let idle = last_activity
                        .lock()
                        .ok()
                        .and_then(|activity| *activity)
                        .is_some_and(|activity| {
                            activity.elapsed() >= Duration::from_secs(SIDECAR_IDLE_EXIT_SECS)
                        });
                    if !idle {
                        continue;
                    }
                    if last_job_check.is_some_and(|at| {
                        at.elapsed() < Duration::from_secs(BACKGROUND_JOB_CHECK_INTERVAL_SECS)
                    }) {
                        continue;
                    }
                    last_job_check = Some(Instant::now());
                    let remaining = jobs.live_jobs(None, |_| false);
                    if !remaining.is_empty() {
                        if !reported_jobs {
                            let pids: Vec<i32> = remaining.iter().map(|job| job.pid).collect();
                            tracing::info!(
                                ?pids,
                                "terminal sidecar 无 PTY 会话但仍有后台作业，保持存活以便后续结束"
                            );
                            reported_jobs = true;
                        }
                        continue;
                    }
                    tracing::info!("terminal sidecar 已无 PTY 会话，退出空闲进程");
                    std::process::exit(0);
                }
            })
        {
            tracing::warn!(%error, "启动 terminal 空闲退出监视失败");
        }
    }

    fn mark_activity(&self) {
        *self.last_activity.lock().expect("终端活动时间锁损坏") = Some(Instant::now());
    }

    fn next_sequence(&self) -> u64 {
        let mut sequence = self.sequence.lock().expect("会话序号锁损坏");
        *sequence += 1;
        *sequence
    }

    fn with_session<R>(
        &self,
        session_id: &str,
        operation: impl FnOnce(&mut PtySession) -> Result<R>,
    ) -> Result<R> {
        let mut sessions = self.sessions.lock().expect("会话表锁损坏");
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在: {session_id}"))?;
        operation(session)
    }

    fn raw_output_since(&self, session_id: &str, offset: u64) -> Result<Vec<u8>> {
        self.with_session(session_id, |session| {
            let available_start = offset.max(session.raw_history_start);
            let index = available_start
                .saturating_sub(session.raw_history_start)
                .min(session.raw_history.len() as u64) as usize;
            Ok(session.raw_history[index..].to_vec())
        })
    }
}

/// 空闲 PTY 回收（issue #697）：CLI / Server 下没有前端标签关闭路径，
/// 交互 shell 挂着就会让 sidecar 永不空闲退出，长期运行终端越积越多。
///
/// 回收条件（全部满足）：
/// - shell 存活且处于空闲/不可复用状态（执行中、交互中、已预留不碰）；
/// - 没有 `terminal_send` 待确认命令，也没有进行中的等待；
/// - 输出静默超过阈值（`last_output_at`）；
/// - 该终端没有登记存活的后台作业（`&` / `nohup`，#672 语义优先）。
fn recycle_idle_ptys(
    sessions: &Arc<Mutex<HashMap<String, PtySession>>>,
    jobs: &Arc<jobs::JobTracker>,
    recycle_after: Option<Duration>,
) {
    let Some(recycle_after) = recycle_after else {
        return;
    };
    // 候选收集与作业核查分离：jobs 枚举较重，只对候选做。
    let candidates: Vec<String> = {
        let Ok(table) = sessions.lock() else {
            return;
        };
        table
            .iter()
            .filter(|(_, session)| {
                session.exited_code.is_none()
                    && matches!(
                        session.phase,
                        SessionPhase::Idle | SessionPhase::Unresponsive
                    )
                    && session.pending_send.is_none()
                    && session.send_in_flight == 0
                    && session.running_command.is_none()
                    && session.last_output_at.elapsed() >= recycle_after
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    if candidates.is_empty() {
        return;
    }
    // 有存活后台作业的终端不回收：回收后沙箱换代，作业再也收不掉（#672）。
    let live_jobs = jobs.live_jobs(None, |_| true);
    let removed: Vec<(String, PtySession)> = {
        let Ok(mut table) = sessions.lock() else {
            return;
        };
        candidates
            .into_iter()
            .filter(|id| !live_jobs.iter().any(|job| &job.terminal_id == id))
            .filter_map(|id| table.remove(&id).map(|session| (id, session)))
            .collect()
    };
    if removed.is_empty() {
        return;
    }
    for (id, mut session) in removed {
        let _ = session.killer.kill();
        tracing::info!(terminal_id = %id, "空闲 PTY 回收：终端超时无活动，已终止");
    }
}
