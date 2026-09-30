//! PTY 会话服务：请求-响应操作 + 输出流通知。

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use portable_pty::{ChildKiller, CommandBuilder, NativePtySystem, PtySize, PtySystem};
use serde::{Deserialize, Serialize};
use tiangong_plugin_runtime::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, PROTOCOL_VERSION, Request, RequestInvocationContext, Response,
};
use tiangong_plugin_sidecar::server::emit_notification;

use crate::persist;

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

/// PTY 输出节流：读取线程按此间隔批量推送（约 60fps 上限）。
const OUTPUT_FLUSH_INTERVAL_MS: u64 = 16;
/// 会话最近输出环形缓冲上限（字节）：UI 重新附着时重放历史（含控制序列原样字节）。
const HISTORY_LIMIT_BYTES: usize = 128 * 1024;
/// 轮询 PTY 输出的间隔。
const COMMAND_POLL_INTERVAL_MS: u64 = 50;
/// 登录 shell 清理残留输入并确认重新接管终端的最长等待时间。
const SHELL_READY_TIMEOUT_SECS: u64 = 3;
/// 最后一个 PTY 会话结束后保留短暂窗口，让关闭响应完成并容纳紧邻的新建请求。
const SIDECAR_IDLE_EXIT_SECS: u64 = 5;
/// 当前会话有前端标签时，等待隐藏页面完成事件订阅与精确附着的最长时间
/// （宿主统一的实例资源后台挂载上限）。
const FRONTEND_ATTACH_WAIT_MS: u64 = tiangong_plugin_runtime::protocol::FRONTEND_ATTACH_WAIT_MS;
/// 非交互命令的静默挂起判定窗口：命令已写入 PTY 后持续无新输出、
/// 前台被子程序占据、且该程序已把 tty 切成非规范模式（交互程序特征）
/// 达到本时长，即认定它在等待用户按键。三个条件同时成立时信号很强，
/// 窗口不需要很长——不必让调用方空等。
const SILENT_INTERACTIVE_HANG_SECS: u64 = 5;
/// 包装脚本夭折的兜底判定：start marker 已出现、退出码标记还没出现、
/// 输出静默达本时长后，才开始探测「登录 shell 是否已回到提示符」。
///
/// 这里守的**不是** issue #571 的原始场景——用户命令已隔离进内层脚本
/// （见 [`prepare_posix_command`]），解析期错误只终止内层 `source`，
/// wrapper 照常输出 cwd/rc/end marker，那类命令不会走到这里。
///
/// 本兜底覆盖的是剩下两类：
/// 1. **shell 仍存活、但 marker 永远不会来**。shell 随命令一起死亡由
///    [`SessionEndState::Ended`] 分支接管；此处专收 shell 被顶替/替换后
///    新 shell 还活着的情形（如内层 `exec sh -i`），以及 marker 被前台
///    程序吞掉、shell 插件干扰等无法穷举的意外。
/// 2. **带显式 timeout 的命令**。交互程序兜底分支带 `deadline.is_none()`
///    前置条件，命令一旦设了 timeout 就完全不生效；本分支不受该约束，
///    是 timeout 路径下唯一能提前收尾的兜底。否则长 timeout 命令一旦丢
///    marker，就要轮询满整个超时，且终端会被标记 Unresponsive 退出复用池。
///
/// 因此即便 #571 已根治，本兜底仍不可移除。
const SILENT_ABORTED_WRAPPER_SECS: u64 = 3;
/// 两次夭折探测的最小间隔：探测要往 shell 输入缓冲写一行，不能每轮都写。
const ABORTED_WRAPPER_PROBE_INTERVAL_SECS: u64 = 5;
/// 夭折探测的最大次数。连续未被执行说明前台确实还在消费 stdin，
/// 继续写入只会不断污染它的输入，到达上限后回到纯等待。
const MAX_ABORTED_WRAPPER_PROBES: u32 = 3;
/// 单次夭折探测等待就绪 marker 的时间预算。
const ABORTED_WRAPPER_PROBE_TIMEOUT_MS: u64 = 800;
/// zsh 默认开启 `EQUALS`：以 `=` 开头的裸词被当作 equals-expansion
/// 展开为命令路径，`echo ===` 会在**解析期**报 `== not found`，
/// 并直接终止被 `source` 的整个脚本——包装脚本的 end marker 永不输出，
/// 调用方只能轮询到超时（issue #571）。解析期错误无法靠 `set -e`/`trap`
/// 补救，只能从源头关掉该选项。
///
/// 写法约束：
/// - 必须对 bash/dash/sh 无害，因此 `setopt` 只在 `ZSH_VERSION` 存在时
///   经 `eval` 执行（非 zsh 连 `[[ -o equals ]]` 都不会被解析到）；
/// - 记录原值并在用户命令后恢复，不改变用户 shell 的既有偏好。
const ZSH_EQUALS_DISABLE_SNIPPET: &str = "__tiangong_equals_restore=''\nif [ -n \"${ZSH_VERSION-}\" ]; then eval 'if [[ -o equals ]]; then __tiangong_equals_restore=1; fi; setopt noequals'; fi\n";
/// 与 [`ZSH_EQUALS_DISABLE_SNIPPET`] 配对：仅当原本开启时才恢复，
/// 用户主动 `unsetopt equals` 的偏好不会被打开。
const ZSH_EQUALS_RESTORE_SNIPPET: &str = "if [ -n \"$__tiangong_equals_restore\" ]; then eval 'setopt equals'; fi\nunset __tiangong_equals_restore\n";
/// 内部命令边界标记公共前缀；这些行不得显示给用户。
const MARKER_PREFIX: &str = "__TIANGONG_";

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
    /// 用户可见输出：过滤内部标记后供 UI 重放。
    display_history: Vec<u8>,
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

#[derive(Debug, Clone, Serialize)]
struct OutputNotification<'a> {
    session_id: &'a str,
    data: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct ExitNotification<'a> {
    session_id: &'a str,
    exit_code: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnRequest {
    /// App 实例编号。UI 创建终端时由调用方指定，保证 App 与 PTY 一一对应。
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cmd: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    /// 宿主会话标识：终端跟随会话切换的锚点。
    #[serde(default)]
    scope_id: Option<String>,
    /// Agent 选择后立即预留，避免并行命令复用同一终端。
    #[serde(default)]
    reserve: bool,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecRequest {
    session_id: String,
    #[serde(default)]
    cmd: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    interactive: bool,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Posix,
    Cmd,
    PowerShell,
}

impl ShellKind {
    fn of(program: &str) -> Self {
        let name = std::path::Path::new(program)
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or(program);
        if name.eq_ignore_ascii_case("cmd") {
            Self::Cmd
        } else if name.eq_ignore_ascii_case("powershell") || name.eq_ignore_ascii_case("pwsh") {
            Self::PowerShell
        } else {
            Self::Posix
        }
    }

    fn quote(self, value: &str) -> String {
        match self {
            Self::PowerShell => format!("'{}'", value.replace('\'', "''")),
            Self::Cmd => {
                if value.is_empty()
                    || value.contains(|ch: char| ch.is_whitespace() || "\"&|<>()^".contains(ch))
                {
                    format!("\"{}\"", value.replace('"', "\"\""))
                } else {
                    value.to_string()
                }
            }
            Self::Posix => posix_quote(value),
        }
    }
}

fn posix_quote(value: &str) -> String {
    if value.is_empty()
        || value.contains(|ch: char| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '\'' | '"'
                        | '\\'
                        | '$'
                        | '`'
                        | '!'
                        | '*'
                        | '?'
                        | '['
                        | ']'
                        | '('
                        | ')'
                        | '{'
                        | '}'
                        | '|'
                        | '&'
                        | ';'
                        | '<'
                        | '>'
                        | '~'
                )
        })
    {
        format!("'{}'", value.replace('\'', "'\\''"))
    } else {
        value.to_string()
    }
}

fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                std::path::PathBuf::from(
                    std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
                )
                .join("System32/WindowsPowerShell/v1.0/powershell.exe")
                .to_string_lossy()
                .into_owned()
            } else {
                "sh".to_string()
            }
        })
}

fn login_shell_args(shell: &str) -> Vec<&'static str> {
    match ShellKind::of(shell) {
        // 禁用第三方交互扩展及个人启动脚本，确保代理命令边界可预测。
        ShellKind::PowerShell => vec![
            "-NoLogo",
            "-NoProfile",
            "-NoExit",
            "-Command",
            "Remove-Module PSReadLine -ErrorAction SilentlyContinue",
        ],
        ShellKind::Cmd => vec!["/d"],
        ShellKind::Posix => {
            let basename = std::path::Path::new(shell)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(shell);
            match basename {
                "bash" | "zsh" => vec!["--login"],
                "sh" => vec!["-l"],
                _ => Vec::new(),
            }
        }
    }
}

/// 就绪探针命令：各 Shell 都以「拼接」形式书写，探针文本本身绝不含
/// 完整 marker。PTY 回显开启时写进去的字节会原样回流，若探针里带完整
/// marker，未就绪终端（残留前台进程占用 stdin）的纯回显也会命中匹配，
/// 就绪判定退化成恒真——`cancel()` 的中断确认将失去意义。
fn shell_ready_probe(shell: ShellKind, marker: &str) -> String {
    let suffix = marker.strip_prefix(MARKER_PREFIX).unwrap_or(marker);
    match shell {
        ShellKind::Cmd => format!("echo {MARKER_PREFIX}^{suffix}\r"),
        ShellKind::PowerShell => format!("Write-Output ('{MARKER_PREFIX}' + '{suffix}')\r"),
        // 相邻引用串在 POSIX shell 中天然拼接：执行后输出完整 marker，
        // 回显的命令行则被 `'` 断开，不构成完整 marker。
        ShellKind::Posix => format!("echo '{MARKER_PREFIX}''{suffix}'\r"),
    }
}

/// 就绪判定：把原始字节经终端仿真还原成「终端上真实呈现的行」后，再要求
/// 某一整行等于 marker。
///
/// 不能在原始字节流上做子串匹配。原始流里混着 ZLE 重绘序列、光标回退、
/// `\r` 覆写以及 zsh-autosuggestions / zsh-syntax-highlighting 的预测文本
/// 与擦除——这些字节从未作为「终端最终显示的内容」存在过，只是绘制过程的
/// 中间态。在中间态上 `contains` 会把行编辑器重绘出的、**尚未执行**的命令
/// 行当成执行结果：autosuggest 重绘时引号被消化，完整 marker 随之出现，
/// 就绪判定退化成恒真，正常运行的命令被误判为「包装脚本夭折」。
///
/// 还原后改用整行相等匹配，[`shell_ready_probe`] 的相邻引用串拼接防护才真
/// 正生效：回显的 `echo '前缀''后缀'` 整行不可能等于纯 marker 行，唯有 shell
/// 实际执行过 echo 才会产生独占一行的 marker。
fn shell_ready_probe_completed(raw: &str, marker: &str) -> bool {
    let mut processor = persist::TerminalLineProcessor::new();
    let mut lines = processor.process(raw);
    let current = processor.current_line();
    if !current.trim().is_empty() {
        lines.push(current);
    }
    lines.iter().any(|line| line.trim() == marker)
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

#[derive(Debug, Serialize)]
struct SpawnResponse {
    session_id: String,
    /// 首批输出（shell 提示符等）：随响应返回给 UI 作渲染基线。
    /// 冷启动窗口内宿主通知监听可能尚未连上（sidecar 通知无订阅者即
    /// 丢弃），首批走通知会永久丢失——终端黑屏的根因；随响应走则
    /// 不依赖通知时序。首批已落盘/入历史，聚合线程不再重发。
    #[serde(skip_serializing_if = "Option::is_none")]
    boot_output: Option<String>,
}

#[derive(Debug, Serialize)]
struct ExecResponse {
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
    cwd_after: String,
    interactive_mode: bool,
    /// 命令使 shell 本身退出（如 exit/登出）：终端已不可复用，
    /// 下次调用将自动新建终端。
    session_ended: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteRequest {
    session_id: String,
    data: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScreenUpdateRequest {
    session_id: String,
    screen: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcquireRequest {
    scope_id: String,
}

#[derive(Debug, Serialize)]
struct AcquireResponse {
    session_id: Option<String>,
    history: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendRequest {
    scope_id: String,
    session_id: String,
    input: String,
    #[serde(default)]
    wait: Option<u64>,
}

#[derive(Debug, Serialize)]
struct SendResponse {
    session_id: String,
    stdout: String,
    exit_code: i32,
    interactive_mode: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResizeRequest {
    session_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionIdRequest {
    session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseRequest {
    #[serde(default)]
    session_id: Option<String>,
    scope_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalGcRequest {
    session_id: String,
    live_terminal_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalBindingRequest {
    scope_id: String,
    session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindRequest {
    scope_id: String,
    /// App 工具打开时携带的 PTY 实例编号；存在时必须精确附着。
    #[serde(default)]
    session_id: Option<String>,
}

/// terminalFind 结果：会话不存在时 session_id 为 null（调用方据此新建）。
#[derive(Debug, Serialize)]
struct FindResponse {
    session_id: Option<String>,
    history: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListByScopeRequest {
    scope_id: String,
}

/// 宿主统一实例生命周期协议：`session_id` 为宿主会话，`instance_id` 为
/// 标签编号（终端即 PTY 编号）。与终端内部字段命名（session_id=PTY）不同，
/// 这里单独建模后映射到既有关闭逻辑。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceClosedRequest {
    session_id: String,
    instance_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListInstancesRequest {
    session_id: String,
}

#[derive(Debug, Serialize)]
struct InstanceEntry {
    instance_id: String,
}

#[derive(Debug, Serialize)]
struct ListInstancesResponse {
    instances: Vec<InstanceEntry>,
}

/// terminalListByScope 条目：宿主会话下仍存活的终端（前端切换会话时
/// 恢复可见标签，历史输出由页面经 terminalFind/幂等 spawn 自行回放）。
#[derive(Debug, Serialize)]
struct ScopeTerminalEntry {
    session_id: String,
}

#[derive(Debug, Serialize)]
struct ListByScopeResponse {
    terminals: Vec<ScopeTerminalEntry>,
}

#[derive(Debug, Serialize)]
struct OkResponse {
    ok: bool,
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
    pub fn start_idle_exit_monitor(&self) {
        let sessions = Arc::clone(&self.sessions);
        let last_activity = Arc::clone(&self.last_activity);
        if let Err(error) = std::thread::Builder::new()
            .name("terminal-idle-exit".to_string())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(250));
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
                    if idle {
                        tracing::info!("terminal sidecar 已无 PTY 会话，退出空闲进程");
                        std::process::exit(0);
                    }
                }
            })
        {
            tracing::warn!(%error, "启动 terminal 空闲退出监视失败");
        }
    }

    fn mark_activity(&self) {
        *self.last_activity.lock().expect("终端活动时间锁损坏") = Some(Instant::now());
    }

    fn mark_frontend_attached(&self, session_id: &str) {
        self.frontend_attached
            .lock()
            .expect("前端附着表锁损坏")
            .insert(session_id.to_string());
    }

    fn attach_frontend(&self, request: TerminalBindingRequest) -> Result<OkResponse> {
        let session_id = request.session_id.trim();
        let scope_id = request.scope_id.trim();
        if session_id.is_empty() || scope_id.is_empty() {
            bail!("终端附着状态需要有效的 scope_id 与 session_id");
        }
        let sessions = self.sessions.lock().expect("会话表锁损坏");
        let session = sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在: {session_id}"))?;
        if session.scope_id.as_deref() != Some(scope_id) {
            bail!("终端 {session_id} 不属于当前会话");
        }
        drop(sessions);

        self.mark_frontend_attached(session_id);
        self.confirmed_frontend_tabs
            .lock()
            .expect("前端终端标签表锁损坏")
            .entry(scope_id.to_string())
            .or_default()
            .insert(session_id.to_string());
        Ok(OkResponse { ok: true })
    }

    async fn wait_for_frontend_attach(&self, session_id: &str) -> bool {
        let deadline = Instant::now() + Duration::from_millis(FRONTEND_ATTACH_WAIT_MS);
        loop {
            if self
                .frontend_attached
                .lock()
                .expect("前端附着表锁损坏")
                .contains(session_id)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn next_sequence(&self) -> u64 {
        let mut sequence = self.sequence.lock().expect("会话序号锁损坏");
        *sequence += 1;
        *sequence
    }

    fn spawn_session(&self, mut request: SpawnRequest) -> Result<SpawnResponse> {
        if let Some(cwd) = &mut request.cwd {
            *cwd = shell_compatible_cwd(cwd).to_string();
        }
        let _spawn_guard = self.spawn_lock.lock().expect("终端创建锁损坏");
        let requested_session_id = request
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if let Some(session_id) = requested_session_id.as_deref() {
            let mut sessions = self.sessions.lock().expect("会话表锁损坏");
            if let Some(session) = sessions.get_mut(session_id) {
                if session.scope_id.as_deref() != request.scope_id.as_deref() {
                    bail!("终端 {session_id} 已属于其他会话");
                }
                if request.reserve {
                    if session.phase != SessionPhase::Idle {
                        bail!("终端 {session_id} 正在使用");
                    }
                    session.phase = SessionPhase::Reserved;
                }
                return Ok(SpawnResponse {
                    session_id: session_id.to_string(),
                    boot_output: Some(
                        String::from_utf8_lossy(&session.display_history).to_string(),
                    ),
                });
            }
        }

        // 编号全局唯一（scru128）：sidecar 换代（崩溃重启/空闲退出/升级）
        // 后序号不回卷，Agent 持有的旧编号只会明确失败，不会误命中新终端。
        // sequence 与编号身份无关，仅用于同 scope 多终端时分辨最新。
        let (sequence, session_id) = loop {
            let sequence = self.next_sequence();
            let session_id = requested_session_id
                .clone()
                .unwrap_or_else(|| format!("tty-{}", scru128::new()));
            if !self
                .sessions
                .lock()
                .expect("会话表锁损坏")
                .contains_key(&session_id)
            {
                break (sequence, session_id);
            }
        };
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: request.rows,
                cols: request.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("创建 PTY 失败")?;

        let shell_program = if request.cmd.is_empty() || request.script.is_some() {
            default_shell()
        } else {
            request.cmd.clone()
        };
        let shell_kind = ShellKind::of(&shell_program);
        // 脚本参数和交互协议均跟随实际 Shell。
        let mut command = if let Some(script) = request.script.as_deref() {
            let shell = shell_program.clone();
            let mut builder = CommandBuilder::new(shell);
            if shell_kind == ShellKind::PowerShell {
                builder.args(["-NoLogo", "-NoProfile", "-Command"]);
            } else if shell_kind == ShellKind::Cmd {
                builder.args(["/d", "/c"]);
            } else {
                builder.arg("-c");
            }
            builder.arg(script);
            builder
        } else if request.cmd.is_empty() {
            // 登录 shell 启动（对齐原版终端与 Terminal.app 行为）：source
            // /etc/profile 与 ~/.zprofile 等，拿到用户真实 PATH；并设置
            // TERM 让 zsh/zle 以全功能终端运行（否则语法高亮/补全降级错乱）。
            let shell = shell_program.clone();
            let mut builder = CommandBuilder::new(&shell);
            for arg in login_shell_args(&shell) {
                builder.arg(arg);
            }
            builder.env("TERM", "xterm-256color");
            builder.env("COLORTERM", "truecolor");
            builder
        } else {
            let mut builder = CommandBuilder::new(&request.cmd);
            builder.env("TERM", "xterm-256color");
            builder
        };
        for arg in &request.args {
            command.arg(arg);
        }
        // cwd 失效（目录被删等）不阻断会话创建：回退到继承进程工作目录
        if let Some(cwd) = request
            .cwd
            .as_deref()
            .filter(|cwd| std::path::Path::new(cwd).is_dir())
        {
            command.cwd(cwd);
        }

        let mut child = pair
            .slave
            .spawn_command(command)
            .context("启动 PTY 子进程失败")?;
        drop(pair.slave);

        let shell_pid = child.process_id();
        let killer = child.clone_killer();
        // 命令回显登记表：执行侧写入、输出过滤线程消费。
        let pending_echo: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // 输出持久化（按 scope 分文件）：仅供运行诊断和关闭时清理
        let logger = request
            .scope_id
            .as_deref()
            .and_then(persist::scope_log_path)
            .and_then(persist::OutputLogger::open)
            .map(Arc::new);
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("克隆 PTY 读取端失败")?;
        let writer = pair.master.take_writer().context("获取 PTY 写入端失败")?;
        let master = pair.master;

        // 必须先登记会话再启动输出搬运；否则启动提示符可能先于登记到达，
        // 原始输出和重放历史都会永久丢失。
        self.sessions.lock().expect("会话表锁损坏").insert(
            session_id.clone(),
            PtySession {
                shell: shell_kind,
                shell_ready: false,
                writer,
                killer,
                master,
                exec_lock: Arc::new(tokio::sync::Mutex::new(())),
                scope_id: request.scope_id,
                sequence,
                phase: if request.reserve {
                    SessionPhase::Reserved
                } else {
                    SessionPhase::Idle
                },
                raw_history: Vec::new(),
                raw_history_start: 0,
                raw_bytes_total: 0,
                display_history: Vec::new(),
                screen_snapshot: String::new(),
                screen_updates: 0,
                exited_code: None,
                pending_ready_marker: None,
                running_command: None,
                interactive_started: false,
                shell_pid,
                logger,
                pending_echo: Arc::clone(&pending_echo),
            },
        );

        // 输出管道：原始流保留给命令完成协议，用户可见流先过滤内部
        // marker 和包含 marker 的 shell 回显，再落盘、推送与重放。
        let output_session = session_id.clone();
        let output_sessions = Arc::clone(&self.sessions);
        let boot_sessions = Arc::clone(&self.sessions);
        let (chunk_tx, chunk_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(bytes) => {
                        if chunk_tx.send(buffer[..bytes].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let mut output_filter = RawOutputFilter::new(Arc::clone(&pending_echo));
        // 首批提示符随响应返回，避免 UI 订阅尚未建立时永久丢帧。
        let boot_output = match chunk_rx.recv_timeout(Duration::from_millis(40)) {
            Ok(first) => {
                record_raw_output(&boot_sessions, &output_session, &first);
                let filtered = output_filter.filter(&String::from_utf8_lossy(&first));
                record_display_output(&boot_sessions, &output_session, filtered.as_bytes());
                (!filtered.is_empty()).then_some(filtered)
            }
            Err(_) => None,
        };
        std::thread::spawn(move || {
            while let Ok(first) = chunk_rx.recv() {
                let mut pending = first;
                while let Ok(more) =
                    chunk_rx.recv_timeout(Duration::from_millis(OUTPUT_FLUSH_INTERVAL_MS))
                {
                    pending.extend_from_slice(&more);
                    if pending.len() >= 64 * 1024 {
                        break;
                    }
                }
                record_raw_output(&output_sessions, &output_session, &pending);
                let filtered = output_filter.filter(&String::from_utf8_lossy(&pending));
                if !filtered.is_empty() {
                    publish_display_output(&output_sessions, &output_session, &filtered);
                }
            }
        });

        // 等待线程：退出通知 + 标记死亡（不出表）。正在执行的命令仍需
        // 从会话读取输出与退出码正常收尾；出表由命令收尾或下次选终端
        // 时完成，write/resize 等操作在此期间按会话状态自行判断。
        let exit_session = session_id.clone();
        let exit_sequence = sequence;
        let exit_sessions = Arc::clone(&self.sessions);
        std::thread::spawn(move || {
            let status = child.wait();
            let exit_code = status.as_ref().ok().map(|status| status.exit_code());
            let payload = serde_json::to_string(&ExitNotification {
                session_id: &exit_session,
                exit_code,
            })
            .unwrap_or_default();
            emit_notification(CHANNEL_EXIT, payload);
            let mut sessions = exit_sessions.lock().expect("会话表锁损坏");
            if sessions
                .get(&exit_session)
                .is_some_and(|session| session.sequence == exit_sequence)
                && let Some(session) = sessions.get_mut(&exit_session)
            {
                session.exited_code = Some(exit_code.map(|code| code as i32).unwrap_or(-1));
            }
        });

        Ok(SpawnResponse {
            session_id,
            boot_output,
        })
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

    /// 回收已死亡的会话（出表并清前端附着标记）。命令收尾时调用，
    /// 防御性保留给选终端等路径复用。
    fn reap_ended_session(&self, session_id: &str) {
        let removed = self
            .sessions
            .lock()
            .expect("会话表锁损坏")
            .remove(session_id);
        if removed.is_some() {
            self.frontend_attached
                .lock()
                .expect("前端附着表锁损坏")
                .remove(session_id);
        }
    }

    /// 会话相对死亡/回收的三态：调用方需区分「活着」（正常继续）、
    /// 「已退出带码」（按死亡收尾）与「已被回收」（手动关闭或回收路径
    /// 先一步出表，读输出只会撞「会话不存在」，须给出明确错误）。
    fn session_end_state(&self, session_id: &str) -> SessionEndState {
        let sessions = self.sessions.lock().expect("会话表锁损坏");
        match sessions.get(session_id) {
            Some(session) => match session.exited_code {
                Some(code) => SessionEndState::Ended(code),
                None => SessionEndState::Alive,
            },
            None => SessionEndState::Reaped,
        }
    }

    /// 命令使 shell 本身退出时的收尾：已解析出的命令退出码优先于
    /// shell 退出码（结束标记先于 shell 退出的组合场景），超时语义下
    /// 固定 -1；已收集输出照常返回，并回收死亡会话。
    fn ended_session_response(
        &self,
        session_id: &str,
        start_offset: u64,
        markers: &CommandMarkers,
        timed_out: bool,
    ) -> Result<ExecResponse> {
        let shell_exit = match self.session_end_state(session_id) {
            SessionEndState::Ended(code) => Some(code),
            SessionEndState::Alive => None,
            SessionEndState::Reaped => {
                bail!("终端会话已被关闭或回收，命令结果不可恢复：{session_id}")
            }
        };
        let raw = self.raw_output_since(session_id, start_offset)?;
        let parsed = parse_command_output(&raw, markers);
        let parsed_exit_code = parsed.exit_code;
        let mut response = parsed.into_response(timed_out);
        if timed_out {
            response.exit_code = -1;
            response.stderr = "命令执行超时".to_string();
        } else {
            response.exit_code = parsed_exit_code.or(shell_exit).unwrap_or(0);
        }
        response.session_ended = true;
        if shell_exit.is_some() {
            self.reap_ended_session(session_id);
        }
        Ok(response)
    }

    fn kill_session(&self, request: SessionIdRequest) -> Result<OkResponse> {
        let session_id = request.session_id;
        let removed = self
            .sessions
            .lock()
            .expect("会话表锁损坏")
            .remove(&session_id);
        match removed {
            Some(mut session) => {
                self.frontend_attached
                    .lock()
                    .expect("前端附着表锁损坏")
                    .remove(&session_id);
                let _ = session.killer.kill();
                Ok(OkResponse { ok: true })
            }
            None => Ok(OkResponse { ok: false }),
        }
    }

    fn close_session(&self, request: CloseRequest) -> Result<OkResponse> {
        let closing_session_id = request.session_id.clone();
        let (removed, has_other_scope_sessions) = {
            let mut sessions = self.sessions.lock().expect("会话表锁损坏");
            let removed = if let Some(session_id) = request.session_id.as_deref() {
                if let Some(session) = sessions.get(session_id)
                    && session.scope_id.as_deref() != Some(request.scope_id.as_str())
                {
                    bail!("终端 {session_id} 不属于当前会话");
                }
                sessions.remove(session_id)
            } else {
                None
            };
            let has_other_scope_sessions = sessions
                .values()
                .any(|session| session.scope_id.as_deref() == Some(request.scope_id.as_str()));
            (removed, has_other_scope_sessions)
        };
        if let Some(mut session) = removed {
            if let Some(session_id) = closing_session_id {
                self.frontend_attached
                    .lock()
                    .expect("前端附着表锁损坏")
                    .remove(&session_id);
            }
            let _ = session.killer.kill();
        }
        if !has_other_scope_sessions {
            persist::clear_scope_log(&request.scope_id).context("清理终端恢复记录失败")?;
        }
        Ok(OkResponse { ok: true })
    }

    /// 前端在标签新建或关闭后提交当前会话的存活集合。页面并行挂载期间
    /// 集合可能暂时不完整，因此只回收曾被确认、后来从集合消失的 PTY。
    /// 调用只影响指定 scope，不增加后台周期任务。
    fn gc_terminals(&self, request: TerminalGcRequest) -> Result<OkResponse> {
        let scope_id = request.session_id.trim();
        if scope_id.is_empty() {
            bail!("terminalGc 需要有效的 session_id");
        }
        if request.live_terminal_ids.len() > 1024 {
            bail!("terminalGc 的 live_terminal_ids 数量超过上限 1024");
        }
        let mut live = HashSet::with_capacity(request.live_terminal_ids.len());
        for terminal_id in request.live_terminal_ids {
            let terminal_id = terminal_id.trim();
            if terminal_id.is_empty() {
                bail!("terminalGc 的 live_terminal_ids 不能包含空编号");
            }
            live.insert(terminal_id.to_string());
        }

        let stale_confirmed_ids = {
            let mut confirmed = self
                .confirmed_frontend_tabs
                .lock()
                .expect("前端终端标签表锁损坏");
            let stale = confirmed
                .get(scope_id)
                .map(|previous| previous.difference(&live).cloned().collect::<HashSet<_>>())
                .unwrap_or_default();
            if live.is_empty() {
                confirmed.remove(scope_id);
            } else {
                confirmed.insert(scope_id.to_string(), live.clone());
            }
            stale
        };

        let (removed, has_remaining) = {
            let mut sessions = self.sessions.lock().expect("会话表锁损坏");
            let stale_ids = sessions
                .iter()
                .filter(|(session_id, session)| {
                    session.scope_id.as_deref() == Some(scope_id)
                        && stale_confirmed_ids.contains(*session_id)
                })
                .map(|(session_id, _)| session_id.clone())
                .collect::<Vec<_>>();
            let removed = stale_ids
                .into_iter()
                .filter_map(|session_id| {
                    sessions
                        .remove(&session_id)
                        .map(|session| (session_id, session))
                })
                .collect::<Vec<_>>();
            let has_remaining = sessions
                .values()
                .any(|session| session.scope_id.as_deref() == Some(scope_id));
            (removed, has_remaining)
        };

        let removed_ids = removed
            .iter()
            .map(|(session_id, _)| session_id.clone())
            .collect::<HashSet<_>>();
        if !removed_ids.is_empty() {
            self.frontend_attached
                .lock()
                .expect("前端附着表锁损坏")
                .retain(|terminal_id| !removed_ids.contains(terminal_id));
            self.active_tools
                .lock()
                .expect("活动工具表锁损坏")
                .retain(|_, active| !removed_ids.contains(&active.terminal_id));
        }
        for (session_id, mut session) in removed {
            let _ = session.killer.kill();
            tracing::info!(session_id, scope_id, "Terminal GC 已回收孤立终端");
        }
        if !has_remaining && live.is_empty() {
            persist::clear_scope_log(scope_id).context("Terminal GC 清理恢复记录失败")?;
        }
        Ok(OkResponse { ok: true })
    }

    fn acquire_session(&self, request: AcquireRequest) -> AcquireResponse {
        let mut sessions = self.sessions.lock().expect("会话表锁损坏");
        let had_live_terminal = sessions.values().any(|session| {
            session.scope_id.as_deref() == Some(request.scope_id.as_str())
                && session.exited_code.is_none()
        });
        // 防御性回收已死亡但尚未被命令收尾清理的会话（出表并清附着标记）。
        // 仅限本会话（scope）且空闲的死亡终端：执行中/交互中的死亡会话
        // 正被它自己的命令等待收尾读取（感知窗口最长一个轮询周期），
        // 选终端不得抢先摘表，否则该命令撞回「会话不存在」；其他会话的
        // 死终端同理留给其收尾路径。
        let ended_ids: Vec<String> = sessions
            .iter()
            .filter(|(_, session)| {
                session.exited_code.is_some()
                    && session.phase == SessionPhase::Idle
                    && session.scope_id.as_deref() == Some(request.scope_id.as_str())
            })
            .map(|(session_id, _)| session_id.clone())
            .collect();
        sessions.retain(|_, session| {
            session.exited_code.is_none()
                || session.phase != SessionPhase::Idle
                || session.scope_id.as_deref() != Some(request.scope_id.as_str())
        });
        if !ended_ids.is_empty() {
            let mut attached = self.frontend_attached.lock().expect("前端附着表锁损坏");
            for session_id in &ended_ids {
                attached.remove(session_id);
            }
        }
        // 曾就绪确认超时的终端自行恢复后重新可用：遗留在 shell 输入缓冲
        // 里的探针会在前台进程树收尾后被执行并回显 marker。长任务跑完
        // （或后台作业转入后台）后终端就该回到复用池，而不是永久闲置。
        let recovered: Vec<String> = sessions
            .iter()
            .filter(|(_, session)| {
                session.phase == SessionPhase::Unresponsive
                    && session.exited_code.is_none()
                    && session.scope_id.as_deref() == Some(request.scope_id.as_str())
                    && session
                        .pending_ready_marker
                        .as_deref()
                        .is_some_and(|marker| {
                            shell_ready_probe_completed(
                                &String::from_utf8_lossy(&session.raw_history),
                                marker,
                            )
                        })
            })
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in recovered {
            if let Some(session) = sessions.get_mut(&session_id) {
                session.phase = SessionPhase::Idle;
                session.shell_ready = true;
                session.pending_ready_marker = None;
                session.running_command = None;
                tracing::info!(session_id, "终端前台命令已收尾，提示符恢复，重新加入复用池");
            }
        }
        let selected = sessions
            .iter()
            .filter(|(_, session)| {
                session.scope_id.as_deref() == Some(request.scope_id.as_str())
                    && session.phase == SessionPhase::Idle
            })
            .min_by_key(|(_, session)| session.sequence)
            .map(|(session_id, _)| session_id.clone());
        let Some(session_id) = selected else {
            return AcquireResponse {
                session_id: None,
                history: String::new(),
                reason: if had_live_terminal {
                    "all_busy".to_string()
                } else {
                    "no_available".to_string()
                },
            };
        };
        let session = sessions.get_mut(&session_id).expect("刚选择的终端不存在");
        session.phase = SessionPhase::Reserved;
        AcquireResponse {
            session_id: Some(session_id),
            history: String::from_utf8_lossy(&session.display_history).to_string(),
            reason: "reused_idle".to_string(),
        }
    }

    fn release_session(&self, request: SessionIdRequest) -> Result<OkResponse> {
        self.with_session(&request.session_id, |session| {
            if session.phase == SessionPhase::Reserved {
                session.phase = SessionPhase::Idle;
            }
            Ok(OkResponse { ok: true })
        })
    }

    fn update_screen(&self, request: ScreenUpdateRequest) -> Result<OkResponse> {
        self.with_session(&request.session_id, |session| {
            session.screen_snapshot = request.screen;
            session.screen_updates = session.screen_updates.saturating_add(1);
            Ok(OkResponse { ok: true })
        })
    }

    async fn send_to_session(&self, request: SendRequest) -> Result<SendResponse> {
        let data = decode_terminal_escapes(&request.input);
        let (session_id, start_offset, baseline_updates) = {
            let mut sessions = self.sessions.lock().expect("会话表锁损坏");
            let selected = sessions
                .get(&request.session_id)
                .filter(|session| session.scope_id.as_deref() == Some(request.scope_id.as_str()))
                .map(|_| request.session_id.clone())
                .ok_or_else(|| anyhow::anyhow!("目标终端不存在或不属于当前会话"))?;
            let session = sessions.get_mut(&selected).expect("刚选择的终端不存在");
            let start_offset = session.raw_bytes_total;
            let baseline_updates = session.screen_updates;
            session
                .writer
                .write_all(data.as_bytes())
                .context("写入交互输入失败")?;
            session.writer.flush().context("刷新交互输入失败")?;
            (selected, start_offset, baseline_updates)
        };

        let deadline = Instant::now() + Duration::from_secs(request.wait.unwrap_or(3).max(1));
        let stable_window = Duration::from_millis(300);
        let mut last_raw_total = start_offset;
        let mut last_screen_updates = baseline_updates;
        let mut last_change_at = None;
        let mut saw_screen_update = false;
        loop {
            let (raw_total, screen_updates) = self.with_session(&session_id, |session| {
                Ok((session.raw_bytes_total, session.screen_updates))
            })?;
            if raw_total != last_raw_total {
                last_raw_total = raw_total;
                last_change_at = Some(Instant::now());
            }
            if screen_updates != last_screen_updates {
                last_screen_updates = screen_updates;
                last_change_at = Some(Instant::now());
                saw_screen_update = true;
            }
            let required_stability = if saw_screen_update {
                stable_window
            } else {
                stable_window * 2
            };
            if last_change_at.is_some_and(|changed| changed.elapsed() >= required_stability)
                || Instant::now() >= deadline
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
        }

        let raw = self.raw_output_since(&session_id, start_offset)?;
        let (screen, screen_updates, interactive_mode) =
            self.with_session(&session_id, |session| {
                Ok((
                    session.screen_snapshot.clone(),
                    session.screen_updates,
                    session.phase == SessionPhase::Interactive,
                ))
            })?;
        let raw_output = visible_text_from_raw(&raw);
        Ok(SendResponse {
            session_id,
            stdout: if screen_updates > baseline_updates && !screen.trim().is_empty() {
                screen
            } else if !raw_output.trim().is_empty() {
                raw_output
            } else {
                screen
            },
            exit_code: 0,
            interactive_mode,
        })
    }

    async fn exec_in_session(&self, request: ExecRequest) -> Result<ExecResponse> {
        let shell = self.with_session(&request.session_id, |session| Ok(session.shell))?;
        let command = command_from_request(&request, shell)?;
        let exec_lock = self.with_session(&request.session_id, |session| {
            Ok(Arc::clone(&session.exec_lock))
        })?;
        let _guard = exec_lock.lock().await;
        self.with_session(&request.session_id, |session| {
            if session.phase == SessionPhase::Interactive {
                bail!("终端正在运行交互程序");
            }
            session.phase = SessionPhase::Running;
            session.running_command = Some(command.clone());
            session.interactive_started = false;
            Ok(())
        })?;

        let result = async {
            // 与正式版一致：先取消 shell 中的残留输入或前台命令，再用隐藏
            // 探针确认登录 shell 已重新接管，最后才发送 Agent 的真实命令。
            self.prepare_shell_for_command(&request.session_id).await?;
            if request.interactive {
                self.exec_interactive(
                    &request.session_id,
                    &command,
                    request.cwd.as_deref(),
                    request.timeout,
                )
                .await
            } else {
                self.exec_non_interactive(&request, &command).await
            }
        }
        .await;
        let next_phase = if result
            .as_ref()
            .is_ok_and(|response| response.interactive_mode)
        {
            SessionPhase::Interactive
        } else {
            SessionPhase::Idle
        };
        let _ = self.with_session(&request.session_id, |session| {
            // 就绪确认失败的终端保持 Unresponsive：终端仍存活可用（后台
            // 作业、用户输入都在），但不得被选终端自动复用，否则下一次
            // 调用又会撞在同一个未回到提示符的 shell 上。
            if session.phase != SessionPhase::Unresponsive {
                session.phase = next_phase;
                // 回到空闲的命令已结束；交互中的程序名对查询仍有价值，
                // 与 Unresponsive（收尾中的命令）一并保留。
                if next_phase == SessionPhase::Idle {
                    session.running_command = None;
                }
            }
            Ok(())
        });
        result
    }

    async fn prepare_shell_for_command(&self, session_id: &str) -> Result<()> {
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

    async fn exec_non_interactive(
        &self,
        request: &ExecRequest,
        command: &str,
    ) -> Result<ExecResponse> {
        let marker_id = scru128::new().to_string();
        let markers = CommandMarkers::new(&marker_id);
        let shell = self.with_session(&request.session_id, |session| Ok(session.shell))?;
        let prepared =
            prepare_non_interactive_command(command, request.cwd.as_deref(), &markers, shell)?;
        let start_offset =
            self.with_session(&request.session_id, |session| Ok(session.raw_bytes_total))?;

        // 命令回显交给输出过滤器：它在 start marker 到达时把 marker 行
        // 替换成命令文本。此处若直接推给 UI，命令会先于 shell 为就绪
        // 探针重绘的提示符落地，用户看到的就是「命令、提示符、输出」
        // 三段分离——与手敲命令时命令紧贴输出的形态不一致。
        self.with_session(&request.session_id, |session| {
            session
                .pending_echo
                .lock()
                .expect("命令回显表锁损坏")
                .insert(markers.start.clone(), command.trim_end().to_string());
            Ok(())
        })?;
        self.with_session(&request.session_id, |session| {
            session
                .writer
                .write_all(prepared.input.as_bytes())
                .context("写入终端命令失败")?;
            session.writer.flush().context("刷新终端命令失败")
        })?;

        let deadline = request
            .timeout
            .map(|timeout| Instant::now() + Duration::from_secs(timeout.max(1)));
        let mut exit_code_seen_at = None;
        // 静默挂起检测基线：命令写入后输出停止增长的时刻。
        let mut last_raw_total = start_offset;
        let mut last_output_at = Instant::now();
        // 夭折探测的节流状态。
        let mut aborted_probes: u32 = 0;
        let mut aborted_probe_at: Option<Instant> = None;
        loop {
            // 会话状态先行：已被回收（手动关闭或选终端回收先一步出表）时
            // 给出明确错误，不再读输出撞「会话不存在」内部错误。
            let end_state = match self.session_end_state(&request.session_id) {
                SessionEndState::Reaped => bail!(
                    "终端会话已被关闭或回收，命令结果不可恢复：{}",
                    request.session_id
                ),
                state => state,
            };
            let raw = self.raw_output_since(&request.session_id, start_offset)?;
            let parsed = parse_command_output(&raw, &markers);
            // 输出仍在增长说明命令在正常推进，刷新静默基线。
            let raw_total = start_offset.saturating_add(raw.len() as u64);
            if raw_total != last_raw_total {
                last_raw_total = raw_total;
                last_output_at = Instant::now();
            }
            // 完成判断优先于死亡收尾：结束标记已闭合时以命令结果为准，
            // shell 随后的退出只补「终端已退出」标记并回收，不覆盖命令
            // 退出码（命令带 exit 的组合、shell 自身异常退出等场景）。
            if parsed.completed {
                // end marker 已闭合后让登录 shell 完成提示符绘制，避免紧随其后的
                // terminal_send 与 wrapper 收尾交错，造成输入多字或终端状态异常。
                tokio::time::sleep(Duration::from_millis(100)).await;
                return Ok(self.finish_parsed(&request.session_id, parsed, end_state));
            }
            if parsed.exit_code.is_some() {
                let seen_at = exit_code_seen_at.get_or_insert_with(Instant::now);
                // 少数 TTY 程序会污染 end marker。退出码已出现但 500ms 内仍
                // 未见 end 时按已完成兜底，不能固定等到整个命令超时。
                if seen_at.elapsed() >= Duration::from_millis(500) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    return Ok(self.finish_parsed(&request.session_id, parsed, end_state));
                }
            }
            // 命令使 shell 本身退出（如 exit/登出）：end marker 不会出现，
            // 以已解析退出码（无则 shell 退出码）收尾，已收集输出照常返回。
            if matches!(end_state, SessionEndState::Ended(_)) {
                return self.ended_session_response(
                    &request.session_id,
                    start_offset,
                    &markers,
                    false,
                );
            }
            // 没有显式 timeout 的非交互命令：Agent 可能把交互程序
            //（vi/ssh/python 等）当普通命令执行，程序停在等待输入，
            // end marker 永不出现——原实现会在此无限轮询，工具调用
            // 直到会话取消才结束，用户侧表现为"一直卡住"。
            //
            // 判据核心是 tty 模式而非静默时长：交互程序必然把 tty 切成
            // 非规范模式逐键读取，而长时间的编译、测试从不碰 tty 模式，
            // 因此跑多久都不会被误判打断（见 foreground_is_interactive）。
            if deadline.is_none()
                && last_output_at.elapsed() >= Duration::from_secs(SILENT_INTERACTIVE_HANG_SECS)
                && self.foreground_is_interactive(&request.session_id) == Some(true)
            {
                tracing::info!(
                    session_id = request.session_id,
                    "命令静默且前台程序已切换 tty 为非规范模式，按交互程序收尾"
                );
                let mut response = parsed.into_response(false);
                response.interactive_mode = true;
                response.stderr =
                    "命令长时间无输出，判定为交互程序正在等待输入；终端已转入交互状态，可用 terminal_send 继续操作"
                        .to_string();
                return Ok(response);
            }
            // 包装脚本夭折兜底：start marker 已出现，但 cwd/rc/end 边界
            // 永远不会再来了。
            //
            // 注意这里**不是** issue #571 的主修复路径：用户命令已隔离进
            // 内层脚本，`echo ===`、语法错误、命令不存在等都只会终止内层
            // `source`，wrapper 照常吐出 end marker 走正常收尾。实测只有
            // 「shell 被顶替但新 shell 仍存活」（内层 `exec sh -i`）这类
            // 才会落到这里；shell 随命令一起死亡的情况由上面的 Ended 分支
            // 接管。此外 marker 被前台程序吞掉、shell 插件干扰等无法穷举
            // 的意外也归本分支兜底。
            //
            // 本分支刻意不加 `deadline.is_none()` 前置条件（对比上面的
            // 交互程序分支）：带显式 timeout 的长命令若丢了 marker，没有
            // 本兜底就要轮询满整个超时，终端还会被标记 Unresponsive 退出
            // 复用池——正是 #571 让用户难受的那种表现。
            //
            // 判据不能只看静默时长——安静跑几分钟的编译同样静默。真正的
            // 区别是「shell 是否已回到提示符」：向 shell 写一条就绪探针，
            // 前台还有命令在跑时它只会躺在输入缓冲里不被执行；夭折后
            // shell 已回到提示符，探针立刻被执行并回显 marker。
            if last_output_at.elapsed() >= Duration::from_secs(SILENT_ABORTED_WRAPPER_SECS)
                && parsed.exit_code.is_none()
                && aborted_probes < MAX_ABORTED_WRAPPER_PROBES
                && aborted_probe_at.is_none_or(|at: Instant| {
                    at.elapsed() >= Duration::from_secs(ABORTED_WRAPPER_PROBE_INTERVAL_SECS)
                })
            {
                aborted_probes += 1;
                aborted_probe_at = Some(Instant::now());
                if self.wrapper_aborted(&request.session_id).await? {
                    tracing::warn!(
                        session_id = request.session_id,
                        "命令包装脚本未正常结束，shell 已回到提示符，按异常结束收尾"
                    );
                    let mut response = parsed.into_response(false);
                    response.exit_code = -1;
                    response.stderr =
                        "命令包装脚本未正常结束（命令可能存在语法/解析错误），已返回其结束前的输出"
                            .to_string();
                    return Ok(response);
                }
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                // 会话已在等待中死亡：不能再向其发送中断键，按死亡收尾。
                if matches!(
                    self.session_end_state(&request.session_id),
                    SessionEndState::Ended(_)
                ) {
                    return self.ended_session_response(
                        &request.session_id,
                        start_offset,
                        &markers,
                        true,
                    );
                }
                self.with_session(&request.session_id, |session| {
                    session
                        .writer
                        .write_all(b"\x03")
                        .context("中断超时命令失败")?;
                    session.writer.flush().context("刷新中断输入失败")
                })?;
                return self
                    .collect_after_interrupt(&request.session_id, start_offset, &markers)
                    .await;
            }
            tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
        }
    }

    /// 探测「包装脚本是否已夭折」：向 shell 写一条就绪探针，在预算内
    /// 看到它被**执行**（而非仅被 tty 回显）即说明 shell 已回到提示符，
    /// 当前命令的边界标记不会再出现了。
    ///
    /// 探针写法保证回显不含完整 marker（见 [`shell_ready_probe`]），
    /// 因此前台仍有命令在跑时不会误判：那种情况下探针只会躺在输入
    /// 缓冲里，等前台结束后才被执行——而那时命令的 end marker 早已
    /// 先一步输出，主循环会以正常完成收尾。
    ///
    /// 之所以用探针而非 `foreground_is_interactive` 取反：后者不是充分
    /// 条件，命令在两条前台进程之间的空档会瞬时成立，足以造成误判。
    async fn wrapper_aborted(&self, session_id: &str) -> Result<bool> {
        let marker = format!("__TIANGONG_ABORTCHK_{}__", scru128::new());
        let probe_offset = self.with_session(session_id, |session| Ok(session.raw_bytes_total))?;
        self.with_session(session_id, |session| {
            session
                .writer
                .write_all(shell_ready_probe(session.shell, &marker).as_bytes())
                .context("发送夭折探针失败")?;
            session.writer.flush().context("刷新夭折探针失败")
        })?;

        let deadline = Instant::now() + Duration::from_millis(ABORTED_WRAPPER_PROBE_TIMEOUT_MS);
        loop {
            let raw = self.raw_output_since(session_id, probe_offset)?;
            if shell_ready_probe_completed(&String::from_utf8_lossy(&raw), &marker) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
        }
    }

    /// 命令正常完成时的收尾：shell 若已随之退出（Ended），补「终端已
    /// 退出」标记并回收会话，命令退出码保持解析结果不被覆盖。
    fn finish_parsed(
        &self,
        session_id: &str,
        parsed: ParsedCommandOutput,
        end_state: SessionEndState,
    ) -> ExecResponse {
        let mut response = parsed.into_response(false);
        if matches!(end_state, SessionEndState::Ended(_)) {
            response.session_ended = true;
            self.reap_ended_session(session_id);
        }
        response
    }

    async fn collect_after_interrupt(
        &self,
        session_id: &str,
        start_offset: u64,
        markers: &CommandMarkers,
    ) -> Result<ExecResponse> {
        let grace_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.session_end_state(session_id) {
                // 中断后 shell 也可能随命令退出：按死亡收尾，不再等满宽限期。
                SessionEndState::Ended(_) => {
                    return self.ended_session_response(session_id, start_offset, markers, true);
                }
                // 已被回收：给出明确错误，不再读输出撞「会话不存在」。
                SessionEndState::Reaped => {
                    bail!("终端会话已被关闭或回收，命令结果不可恢复：{session_id}")
                }
                SessionEndState::Alive => {}
            }
            let raw = self.raw_output_since(session_id, start_offset)?;
            let parsed = parse_command_output(&raw, markers);
            if parsed.completed || Instant::now() >= grace_deadline {
                let confirmed = parsed.completed;
                let mut response = parsed.into_response(true);
                response.exit_code = -1;
                response.stderr = "命令执行超时".to_string();
                if !confirmed {
                    // 宽限耗尽不等于命令结束（前台可能忽略 SIGINT 仍在跑）：
                    // 置回空闲会让状态查询错报空闲、命令信息丢失，下一条
                    // 命令还会撞上仍在跑的前台命令。标记不可复用并遗留
                    // 就绪探针，命令收尾后经恢复检测自动回池。
                    self.mark_unresponsive_with_probe(session_id);
                }
                return Ok(response);
            }
            tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
        }
    }

    /// 超时未确认收尾：终端保持存活但移出自动复用池，并往输入缓冲写入
    /// 一条就绪探针——前台命令收尾后 shell 会执行它并回显 marker，
    /// 下次选终端时据此把终端恢复回池（复用就绪确认超时的自愈机制）。
    fn mark_unresponsive_with_probe(&self, session_id: &str) {
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

    async fn exec_interactive(
        &self,
        session_id: &str,
        command: &str,
        cwd: Option<&str>,
        wait_secs: Option<u64>,
    ) -> Result<ExecResponse> {
        let start_offset = self.with_session(session_id, |session| Ok(session.raw_bytes_total))?;
        // 交互命令原样写进 PTY（用户看到的就是 Agent 请求的命令本身）。
        // 工作目录改由前置的独立一行 `cd` 完成，不再污染命令文本。
        let shell = self.with_session(session_id, |session| Ok(session.shell))?;
        if let Some(enter) = interactive_cwd_command(cwd, shell) {
            self.with_session(session_id, |session| {
                session
                    .writer
                    .write_all(enter.as_bytes())
                    .context("切换交互命令工作目录失败")?;
                session.writer.flush().context("刷新工作目录切换失败")
            })?;
            tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
        }
        self.with_session(session_id, |session| {
            session
                .writer
                .write_all(format!("{}\r", command.trim_end()).as_bytes())
                .context("写入交互命令失败")?;
            session.writer.flush().context("刷新交互命令失败")?;
            // 程序已真正进入前台：此后取消须保留终端并转交互状态。
            session.interactive_started = true;
            Ok(())
        })?;

        let deadline = Instant::now() + Duration::from_secs(wait_secs.unwrap_or(3).max(1));
        let mut changed_at = None;
        let mut last_len = 0;
        loop {
            // 已被回收：给出明确错误，不再读输出撞「会话不存在」。
            if let SessionEndState::Reaped = self.session_end_state(session_id) {
                bail!("终端会话已被关闭或回收，命令结果不可恢复：{session_id}");
            }
            let raw = self.raw_output_since(session_id, start_offset)?;
            if raw.len() != last_len {
                last_len = raw.len();
                changed_at = Some(Instant::now());
            }
            if changed_at.is_some_and(|changed| changed.elapsed() >= Duration::from_millis(300))
                || Instant::now() >= deadline
            {
                let mut processor = persist::TerminalLineProcessor::new();
                let mut lines = processor.process(&String::from_utf8_lossy(&raw));
                let current = processor.current_line();
                if !current.trim().is_empty() {
                    lines.push(current);
                }
                // 交互命令也可能结束 shell 本身（如 exit）：标记并回收，
                // 让下次调用自然新建终端。
                let session_ended = matches!(
                    self.session_end_state(session_id),
                    SessionEndState::Ended(_)
                );
                if session_ended {
                    self.reap_ended_session(session_id);
                }
                return Ok(ExecResponse {
                    stdout: lines.join("\n"),
                    stderr: String::new(),
                    exit_code: 0,
                    timed_out: false,
                    cwd_after: String::new(),
                    interactive_mode: true,
                    session_ended,
                });
            }
            tokio::time::sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MS)).await;
        }
    }

    /// PTY 前台是否是一个正在等待按键的交互程序。
    ///
    /// 两个条件同时成立才算：
    /// 1. 前台进程组不是登录 shell 自身（`tcgetpgrp`）——shell 把前台
    ///    让给了子程序；
    /// 2. tty 已被切成非规范模式（`ICANON` 被清除）——交互程序为了逐键
    ///    读取必然这么做，而 `cargo build`、`pytest`、`sleep` 这类命令
    ///    从不碰 tty 模式，ICANON 始终保持开启。
    ///
    /// 条件 1 单独不足以判定：任何前台命令都会占据前台进程组。条件 2
    /// 才是区分「交互程序在等输入」与「普通命令在安静地跑」的依据，
    /// 因此长时间编译、测试不会被误判。
    ///
    /// 取不到信息（非 Unix、fd 不可用）时返回 None，调用方按「无法判定」
    /// 处理，保持原有等待行为。
    #[cfg(unix)]
    fn foreground_is_interactive(&self, session_id: &str) -> Option<bool> {
        let (shell_pid, leader, raw_fd) = self
            .with_session(session_id, |session| {
                Ok((
                    session.shell_pid,
                    session.master.process_group_leader(),
                    session.master.as_raw_fd(),
                ))
            })
            .ok()?;
        let shell_pid = shell_pid? as i64;
        let leader = leader? as i64;
        if leader == shell_pid {
            // 前台是 shell 自身：没有命令在跑，谈不上等待输入。
            return Some(false);
        }
        let fd = raw_fd?;
        let mut termios = std::mem::MaybeUninit::<libc::termios>::zeroed();
        // SAFETY: fd 来自存活会话持有的 PTY master，tcgetattr 只读取属性。
        if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: tcgetattr 返回 0 表示结构已被完整初始化。
        let termios = unsafe { termios.assume_init() };
        Some(termios.c_lflag & libc::ICANON == 0)
    }

    /// Windows（ConPTY）没有前台进程组与 termios，无法判定。
    #[cfg(not(unix))]
    fn foreground_is_interactive(&self, _session_id: &str) -> Option<bool> {
        None
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

    /// 按宿主会话标识找活跃终端。精确编号存在时附着同一 PTY；精确编号
    /// 不存在时返回空结果，由页面创建全新 PTY，不把其他终端的会话日志
    /// 回放进新标签。
    fn find_by_scope(&self, request: &FindRequest) -> FindResponse {
        let sessions = self.sessions.lock().expect("会话表锁损坏");
        if let Some(session_id) = request.session_id.as_deref()
            && let Some(session) = sessions.get(session_id)
            && session.scope_id.as_deref() == Some(request.scope_id.as_str())
        {
            return FindResponse {
                session_id: Some(session_id.to_string()),
                history: String::from_utf8_lossy(&session.display_history).to_string(),
            };
        }
        if request.session_id.is_some() {
            return FindResponse {
                session_id: None,
                history: String::new(),
            };
        }
        if let Some((session_id, session)) = sessions
            .iter()
            .filter(|(_, session)| session.scope_id.as_deref() == Some(request.scope_id.as_str()))
            .max_by_key(|(_, session)| session.sequence)
        {
            return FindResponse {
                session_id: Some(session_id.clone()),
                history: String::from_utf8_lossy(&session.display_history).to_string(),
            };
        }
        FindResponse {
            session_id: None,
            history: String::new(),
        }
    }

    /// 列出宿主会话下全部存活终端（按创建序）。前端切换会话时恢复可见
    /// 标签用；已退出的终端（含死亡未出表）不返回——那些由既有收尾与
    /// GC 机制回收，恢复展示没有意义。
    fn list_by_scope(&self, request: &ListByScopeRequest) -> Result<ListByScopeResponse> {
        let scope_id = request.scope_id.trim();
        if scope_id.is_empty() {
            bail!("terminalListByScope 需要有效的 scope_id");
        }
        let sessions = self.sessions.lock().expect("会话表锁损坏");
        let mut terminals: Vec<(u64, String)> = sessions
            .iter()
            .filter(|(_, session)| {
                session.scope_id.as_deref() == Some(scope_id) && session.exited_code.is_none()
            })
            .map(|(session_id, session)| (session.sequence, session_id.clone()))
            .collect();
        terminals.sort_unstable();
        Ok(ListByScopeResponse {
            terminals: terminals
                .into_iter()
                .map(|(_, session_id)| ScopeTerminalEntry { session_id })
                .collect(),
        })
    }
}

/// 记录原始 PTY 输出，供命令边界检测和 stdout 收集。
fn record_raw_output(
    sessions: &Arc<Mutex<HashMap<String, PtySession>>>,
    session_id: &str,
    bytes: &[u8],
) {
    let mut sessions = sessions.lock().expect("会话表锁损坏");
    let Some(session) = sessions.get_mut(session_id) else {
        return;
    };
    session.raw_history.extend_from_slice(bytes);
    session.raw_bytes_total = session.raw_bytes_total.saturating_add(bytes.len() as u64);
    let overflow = session
        .raw_history
        .len()
        .saturating_sub(HISTORY_LIMIT_BYTES);
    if overflow > 0 {
        session.raw_history.drain(..overflow);
        session.raw_history_start = session.raw_history_start.saturating_add(overflow as u64);
    }
}

/// 记录过滤后的用户可见输出。日志写在会话表锁外，避免阻塞输入与 resize。
fn record_display_output(
    sessions: &Arc<Mutex<HashMap<String, PtySession>>>,
    session_id: &str,
    bytes: &[u8],
) {
    if bytes.is_empty() {
        return;
    }
    let logger = {
        let mut sessions = sessions.lock().expect("会话表锁损坏");
        let Some(session) = sessions.get_mut(session_id) else {
            return;
        };
        session.display_history.extend_from_slice(bytes);
        let overflow = session
            .display_history
            .len()
            .saturating_sub(HISTORY_LIMIT_BYTES);
        if overflow > 0 {
            session.display_history.drain(..overflow);
        }
        session.logger.clone()
    };
    if let Some(logger) = logger {
        logger.append(&String::from_utf8_lossy(bytes));
    }
}

fn publish_display_output(
    sessions: &Arc<Mutex<HashMap<String, PtySession>>>,
    session_id: &str,
    data: &str,
) {
    record_display_output(sessions, session_id, data.as_bytes());
    let payload =
        serde_json::to_string(&OutputNotification { session_id, data }).unwrap_or_default();
    emit_notification(CHANNEL_OUTPUT, payload);
}

fn contains_marker(text: &str) -> bool {
    text.contains(MARKER_PREFIX)
}

/// 行级 marker 过滤器：内部 wrapper 的 shell 回显和边界行不进入 UI。
///
/// 同时承担命令回显：start marker 是命令真正开始执行的位置，过滤器把
/// 它**替换**成 Agent 的命令文本，而不是丢弃。命令行因此与自己的输出
/// 严格相邻，中间不会再夹进 shell 为就绪探针重绘的提示符——与用户手敲
/// 命令时看到的形态一致。
struct RawOutputFilter {
    pending: String,
    suppress_until_newline: bool,
    /// start marker → 该命令的可见文本。命令写入 PTY 前登记，
    /// 命中后消费一次。
    pending_echo: Arc<Mutex<HashMap<String, String>>>,
}

impl RawOutputFilter {
    fn new(pending_echo: Arc<Mutex<HashMap<String, String>>>) -> Self {
        Self {
            pending: String::new(),
            suppress_until_newline: false,
            pending_echo,
        }
    }

    /// marker 行命中登记时取出命令文本，作为可见行替换它。
    fn echo_for(&self, segment: &str) -> Option<String> {
        let trimmed = segment.trim();
        let mut pending = self.pending_echo.lock().expect("命令回显表锁损坏");
        if pending.is_empty() {
            return None;
        }
        let marker = pending
            .keys()
            .find(|marker| trimmed.contains(marker.as_str()))
            .cloned()?;
        pending.remove(&marker)
    }

    fn filter(&mut self, chunk: &str) -> String {
        // 终端应答序列先行剥离：它们与 marker 无关，且可能出现在任意位置，
        // 在进入行边界切分前去掉，避免混进 UI 输出。
        self.pending.push_str(&strip_terminal_reports(chunk));
        let mut result = String::new();
        while let Some((end, newline)) = next_output_boundary(&self.pending) {
            let segment = self.pending[..end].to_string();
            self.pending.drain(..end);
            if contains_marker(&segment) {
                // start marker 就地换成命令文本：命令行与其输出相邻。
                if let Some(echo) = self.echo_for(&segment) {
                    result.push_str(&echo);
                    result.push_str("\r\n");
                }
                self.suppress_until_newline = !newline;
            } else if !self.suppress_until_newline {
                result.push_str(&segment);
            }
            if newline {
                self.suppress_until_newline = false;
            }
        }
        if self.pending.is_empty() {
            return result;
        }
        if self.suppress_until_newline {
            return result;
        }
        if contains_marker(&self.pending) {
            if self.pending.len() > 8192 {
                result.push_str(&self.pending);
                self.pending.clear();
            }
            return result;
        }
        let split = marker_safe_split(&self.pending);
        if split > 0 {
            result.push_str(&self.pending[..split]);
            self.pending.drain(..split);
        }
        result
    }
}

/// 剥离终端**应答**序列：这类字节是终端回答应用查询的结果，方向是
/// 终端 → 应用，本就不该出现在给用户看的输出里。
///
/// 触发场景：zsh 的 ZLE 行编辑器被唤醒重绘时（夭折探针写入、窗口尺寸
/// 变化等），zsh-syntax-highlighting 与主题会发出 OSC 11（查询背景色）
/// 和 DSR 6n（查询光标位置），终端的应答随即混入 PTY 输出流。过滤器
/// 若不处理，用户就会在长命令输出中途看到 `]11;rgb:1e1e/1e1e/2e2e` 与
/// `[24;1R` 这类乱码。
///
/// 只剥离两类明确的应答，不动任何正常渲染序列（SGR 颜色、光标移动等）：
/// - OSC 颜色应答：`ESC ] 1[01] ; rgb:... (ESC \ | BEL)`
/// - DSR 光标位置上报：`ESC [ <行> ; <列> R`
fn strip_terminal_reports(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\x1b' && index + 1 < bytes.len() {
            // DSR 光标位置上报：ESC [ digits ; digits R
            if bytes[index + 1] == b'[' {
                let mut end = index + 2;
                while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
                    end += 1;
                }
                if end < bytes.len() && bytes[end] == b'R' && end > index + 2 {
                    index = end + 1;
                    continue;
                }
            }
            // OSC 颜色应答：ESC ] 10/11 ; ... 以 ESC \ 或 BEL 结束
            if bytes[index + 1] == b']' {
                let mut end = index + 2;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                let code = &value[index + 2..end];
                if matches!(code, "10" | "11" | "12") && end < bytes.len() && bytes[end] == b';' {
                    let mut scan = end;
                    while scan < bytes.len() {
                        if bytes[scan] == 0x07 {
                            scan += 1;
                            break;
                        }
                        if bytes[scan] == b'\x1b'
                            && scan + 1 < bytes.len()
                            && bytes[scan + 1] == b'\\'
                        {
                            scan += 2;
                            break;
                        }
                        scan += 1;
                    }
                    // 未见终止符说明序列尚未收全，留待下个分片再处理。
                    if scan < bytes.len() || value[end..].contains('\u{7}') {
                        index = scan;
                        continue;
                    }
                }
            }
        }
        let ch_len = value[index..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&value[index..index + ch_len]);
        index += ch_len;
    }
    out
}

fn next_output_boundary(value: &str) -> Option<(usize, bool)> {
    let newline = value.find('\n').map(|index| (index + 1, true));
    let bytes = value.as_bytes();
    let mut cursor = None;
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] != b'\x1b' || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }
        let mut end = index + 2;
        while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
            end += 1;
        }
        if end < bytes.len() && matches!(bytes[end], b'H' | b'f') {
            cursor = Some((end + 1, false));
            break;
        }
        index += 1;
    }
    match (newline, cursor) {
        (Some(newline), Some(cursor)) => Some(if newline.0 <= cursor.0 {
            newline
        } else {
            cursor
        }),
        (Some(boundary), None) | (None, Some(boundary)) => Some(boundary),
        (None, None) => None,
    }
}

fn marker_safe_split(value: &str) -> usize {
    for prefix_len in (1..MARKER_PREFIX.len()).rev() {
        if value.len() < prefix_len {
            continue;
        }
        let split = value.len() - prefix_len;
        if value.is_char_boundary(split) && MARKER_PREFIX.starts_with(&value[split..]) {
            return split;
        }
    }
    value.len()
}

impl Default for TerminalService {
    fn default() -> Self {
        Self::new()
    }
}

struct CommandMarkers {
    start: String,
    end: String,
    cwd: String,
    exit_code: String,
}

impl CommandMarkers {
    fn new(id: &str) -> Self {
        Self {
            start: format!("__TIANGONG_START_{id}__"),
            end: format!("__TIANGONG_END_{id}__"),
            cwd: format!("__TIANGONG_CWD_{id}__"),
            exit_code: format!("__TIANGONG_RC_{id}__"),
        }
    }
}

#[derive(Default)]
struct ParsedCommandOutput {
    stdout: String,
    exit_code: Option<i32>,
    cwd_after: String,
    completed: bool,
}

impl ParsedCommandOutput {
    fn into_response(self, timed_out: bool) -> ExecResponse {
        ExecResponse {
            stdout: self.stdout,
            stderr: String::new(),
            exit_code: self.exit_code.unwrap_or(if timed_out { -1 } else { 0 }),
            timed_out,
            cwd_after: self.cwd_after,
            interactive_mode: false,
            session_ended: false,
        }
    }
}

/// 把 Agent 的请求拼成要执行的命令文本。
///
/// **不含 cwd 切换**：工作目录由包装脚本负责（见 [`prepare_posix_command`]），
/// 不能拼进命令文本。命令文本会原样回显给用户、记入 `running_command`
/// 并出现在失败摘要里，混入 `cd ... &&` 会让用户看到自己绝不会这样敲的
/// 命令；宿主每次调用都传会话工作目录，拼进去等于每条命令都平白多一截
/// 前缀，用户命令与 Agent 命令的形态就不一致了。
fn command_from_request(request: &ExecRequest, shell: ShellKind) -> Result<String> {
    if let Some(script) = request.script.as_deref() {
        if script.trim().is_empty() {
            bail!("script 不能为空");
        }
        return Ok(script.to_string());
    }
    if request.cmd.trim().is_empty() {
        bail!("cmd 不能为空");
    }
    // cmd 可含参数（与 command 插件的 run_command 语义一致）：先按引号
    // 感知规则拆分成程序名 + 内联参数，再逐词 quote 拼接，避免
    // "git status" 被整体 quote 成单个命令名导致找不到命令。
    let (program, inline_args) = split_command(request.cmd.trim());
    let mut command = shell.quote(&program);
    if shell == ShellKind::PowerShell {
        command.insert_str(0, "& ");
    }
    for arg in inline_args.iter().chain(request.args.iter()) {
        command.push(' ');
        command.push_str(&shell.quote(arg));
    }
    Ok(command)
}

/// 交互模式的工作目录切换：作为独立一行写入 PTY，不并入命令文本。
fn interactive_cwd_command(cwd: Option<&str>, shell: ShellKind) -> Option<String> {
    let cwd = cwd
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(shell_compatible_cwd)?;
    let quoted = shell.quote(cwd);
    Some(match shell {
        ShellKind::Cmd => format!("cd /d {quoted}\r"),
        ShellKind::PowerShell => format!("Set-Location -LiteralPath {quoted}\r"),
        ShellKind::Posix => format!("cd {quoted}\r"),
    })
}

/// 包装脚本里的工作目录切换语句（不进入用户可见的命令文本）。
/// 切换失败必须让命令整体失败，不能在错误的目录里执行。
fn cwd_prelude(cwd: Option<&str>, shell: ShellKind) -> String {
    let Some(cwd) = cwd
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(shell_compatible_cwd)
    else {
        return String::new();
    };
    match shell {
        ShellKind::Cmd => format!(
            "cd /d {}\r\nif errorlevel 1 goto :eof\r\n",
            shell.quote(cwd)
        ),
        ShellKind::PowerShell => format!(
            "Set-Location -LiteralPath {} -ErrorAction Stop\n",
            shell.quote(cwd)
        ),
        ShellKind::Posix => format!("cd {} || return $?\n", shell.quote(cwd)),
    }
}

/// Windows 的本地扩展路径可用于文件 API，但 cmd 不支持将其作为工作目录。
/// 仅去掉本地盘符路径的前缀，不把真实 UNC 网络路径伪装成本地路径。
fn shell_compatible_cwd(cwd: &str) -> &str {
    if cfg!(windows)
        && let Some(local) = cwd.strip_prefix(r"\\?\")
        && local.as_bytes().get(1) == Some(&b':')
    {
        return local;
    }
    cwd
}

/// 按引号感知规则拆分命令字符串为（程序名, 参数列表）。
///
/// 与 command 插件 sidecar 的 `split_command` 语义一致：单双引号成组、
/// 反斜杠转义，空白分隔；无引号的裸词按空白切分。
fn split_command(raw: &str) -> (String, Vec<String>) {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single && !cfg!(windows) => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    if parts.is_empty() {
        return (raw.to_string(), Vec::new());
    }
    let cmd = parts.remove(0);
    (cmd, parts)
}

fn decode_terminal_escapes(input: &str) -> String {
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

fn visible_text_from_raw(raw: &[u8]) -> String {
    let mut processor = persist::TerminalLineProcessor::new();
    let mut lines = processor.process(&String::from_utf8_lossy(raw));
    let current = processor.current_line();
    if !current.trim().is_empty() {
        lines.push(current);
    }
    lines.join("\n")
}

struct PreparedCommand {
    input: String,
    /// Unix 下保持临时脚本存活到命令结束；drop 后自动删除。
    /// POSIX 路径有两个文件：wrapper 与承载用户命令的内层脚本。
    _files: Vec<tempfile::TempPath>,
}

/// 生成 POSIX 包装：wrapper 负责 marker 边界与环境保存/恢复，
/// **用户命令单独落在内层脚本**，由 wrapper `source` 执行。
///
/// 分成两层是 issue #571 的根治点：内联时用户命令的任何**解析期错误**
///（zsh equals 展开、未闭合引号、非法重定向等）都会直接终止整份脚本，
/// end marker 永不输出，调用方只能轮询到超时；错误发生在解析阶段，
/// `set -e`、`trap`、`||` 兜底一律无效——脚本余下部分根本不会被执行。
/// 拆成内层文件后，解析失败只终止内层 `source`（shell 把它折算成非零
/// 退出码，zsh 126 / bash 1），wrapper 的控制流完好，照常吐出 cwd、
/// 退出码与 end marker，调用方立刻拿到结果和 shell 的原始报错。
///
/// 同时关闭 zsh `EQUALS` 选项，让 `echo ===`、`awk =x` 这类最常见的形态
/// 直接按字面量正常执行，而不是「稳定报错但能收尾」。
fn prepare_posix_command(
    command: &str,
    cwd: Option<&str>,
    markers: &CommandMarkers,
) -> Result<PreparedCommand> {
    let mut inner = tempfile::Builder::new()
        .prefix(&markers.start)
        .suffix(".inner.sh")
        .tempfile()
        .context("创建终端命令临时文件失败")?;
    inner
        .write_all(format!("{command}\n").as_bytes())
        .context("写入终端命令临时文件失败")?;
    inner.flush().context("刷新终端命令临时文件失败")?;
    let inner_path = ShellKind::Posix.quote(&inner.path().to_string_lossy());

    let script = posix_wrapper_script(&inner_path, cwd, markers);
    let mut file = tempfile::Builder::new()
        .prefix(&markers.start)
        .suffix(".sh")
        .tempfile()
        .context("创建终端命令临时文件失败")?;
    file.write_all(script.as_bytes())
        .context("写入终端命令临时文件失败")?;
    file.flush().context("刷新终端命令临时文件失败")?;
    let path = file.path().to_string_lossy();
    Ok(PreparedCommand {
        // marker 放在输入行最前面，reader 从首个 chunk 起就会暂存并过滤
        // shell/ZLE 回显，不会因路径换行或语法高亮把内部 source 命令漏到 UI。
        input: format!("__TIANGONG_= . {}\r", ShellKind::Posix.quote(&path)),
        _files: vec![file.into_temp_path(), inner.into_temp_path()],
    })
}

/// wrapper 脚本正文：`inner_path` 是已 quote 的内层脚本路径。
/// 独立成函数便于测试直接对脚本文本断言。
fn posix_wrapper_script(inner_path: &str, cwd: Option<&str>, markers: &CommandMarkers) -> String {
    format!(
        "echo '{}'\n{}{}__tiangong_had_PAGER=${{PAGER+x}}\n__tiangong_old_PAGER=${{PAGER-}}\n__tiangong_had_GIT_PAGER=${{GIT_PAGER+x}}\n__tiangong_old_GIT_PAGER=${{GIT_PAGER-}}\n__tiangong_had_GH_PAGER=${{GH_PAGER+x}}\n__tiangong_old_GH_PAGER=${{GH_PAGER-}}\n__tiangong_had_LESS=${{LESS+x}}\n__tiangong_old_LESS=${{LESS-}}\nexport PAGER=cat GIT_PAGER=cat GH_PAGER=cat LESS=FRX\n. {}\n__tiangong_rc=$?\n{}if [ -n \"$__tiangong_had_PAGER\" ]; then PAGER=\"$__tiangong_old_PAGER\"; export PAGER; else unset PAGER; fi\nif [ -n \"$__tiangong_had_GIT_PAGER\" ]; then GIT_PAGER=\"$__tiangong_old_GIT_PAGER\"; export GIT_PAGER; else unset GIT_PAGER; fi\nif [ -n \"$__tiangong_had_GH_PAGER\" ]; then GH_PAGER=\"$__tiangong_old_GH_PAGER\"; export GH_PAGER; else unset GH_PAGER; fi\nif [ -n \"$__tiangong_had_LESS\" ]; then LESS=\"$__tiangong_old_LESS\"; export LESS; else unset LESS; fi\nprintf '\\n{}'; pwd\necho '{}'$__tiangong_rc\necho '{}'\n",
        markers.start,
        cwd_prelude(cwd, ShellKind::Posix),
        ZSH_EQUALS_DISABLE_SNIPPET,
        inner_path,
        ZSH_EQUALS_RESTORE_SNIPPET,
        markers.cwd,
        markers.exit_code,
        markers.end,
    )
}

fn prepare_cmd_command(
    command: &str,
    cwd: Option<&str>,
    markers: &CommandMarkers,
) -> Result<PreparedCommand> {
    let script = format!(
        "@echo off\r\necho {}\r\n{}{}\r\nset \"__TIANGONG_RC_VALUE=%errorlevel%\"\r\necho {}\r\necho %cd%\r\necho {}%__TIANGONG_RC_VALUE%\r\necho {}\r\n",
        markers.start,
        cwd_prelude(cwd, ShellKind::Cmd),
        command,
        markers.cwd,
        markers.exit_code,
        markers.end,
    );
    let mut file = tempfile::Builder::new()
        .prefix(&markers.start)
        .suffix(".cmd")
        .tempfile()
        .context("创建 Windows 终端命令临时文件失败")?;
    file.write_all(script.as_bytes())
        .context("写入 Windows 终端命令临时文件失败")?;
    file.flush().context("刷新 Windows 终端命令临时文件失败")?;
    let path = file.path().to_string_lossy();
    Ok(PreparedCommand {
        input: format!(
            "echo {}>nul & call {}\r",
            markers.start,
            ShellKind::Cmd.quote(&path)
        ),
        _files: vec![file.into_temp_path()],
    })
}

fn prepare_non_interactive_command(
    command: &str,
    cwd: Option<&str>,
    markers: &CommandMarkers,
    shell: ShellKind,
) -> Result<PreparedCommand> {
    match shell {
        ShellKind::Posix => prepare_posix_command(command, cwd, markers),
        ShellKind::Cmd => prepare_cmd_command(command, cwd, markers),
        ShellKind::PowerShell => {
            // Out-Default 强制格式化在结束标记前完成，否则表格会延迟到提示符才输出。
            let result_variable = format!("{}VALUE", markers.exit_code);
            let script = format!(
                "try {{\nWrite-Output '{start}'\n{prelude}${state} = [pscustomobject]@{{ Result = 0; Before = $LASTEXITCODE; Errors = $Error.Count }}\ntry {{ . {{\n{command}\n${state}.Result = if ($?) {{ 0 }} elseif ($LASTEXITCODE -ne ${state}.Before) {{ $LASTEXITCODE }} elseif ($Error.Count -gt ${state}.Errors) {{ 1 }} else {{ ${state}.Before }}\n}} | Out-Default }} catch {{ ${state}.Result = 1; $_ | Out-Default }}\nWrite-Output ''\nWrite-Output '{cwd}'\nWrite-Output (Get-Location).Path\nWrite-Output ('{rc}' + ${state}.Result)\nWrite-Output '{end}'\n}} finally {{ Remove-Variable -Name '{state}' -ErrorAction SilentlyContinue }}\n",
                start = markers.start,
                prelude = cwd_prelude(cwd, ShellKind::PowerShell),
                cwd = markers.cwd,
                rc = markers.exit_code,
                end = markers.end,
                state = result_variable,
            );
            let mut file = tempfile::Builder::new()
                .prefix(&markers.start)
                .suffix(".ps1")
                .tempfile()
                .context("创建 PowerShell 命令临时文件失败")?;
            file.write_all(script.as_bytes())?;
            file.flush()?;
            // 用 UTF-8 读取已生成内容，不修改用户的执行策略。
            let path = ShellKind::PowerShell.quote(&file.path().to_string_lossy());
            Ok(PreparedCommand {
                input: format!(
                    "$null='__TIANGONG_'; try {{ . ([scriptblock]::Create([IO.File]::ReadAllText({path}))) }} catch {{ Write-Output '{start}'; $_ | Out-Default; Write-Output '{rc}1'; Write-Output '{end}' }}\r",
                    start = markers.start,
                    rc = markers.exit_code,
                    end = markers.end
                ),
                _files: vec![file.into_temp_path()],
            })
        }
    }
}

fn parse_command_output(raw: &[u8], markers: &CommandMarkers) -> ParsedCommandOutput {
    let mut processor = persist::TerminalLineProcessor::new();
    let mut lines = processor.process(&String::from_utf8_lossy(raw));
    let current = processor.current_line();
    if !current.trim().is_empty() {
        lines.push(current);
    }

    let mut start_seen = false;
    let mut collecting_cwd = false;
    let mut output = Vec::new();
    let mut parsed = ParsedCommandOutput::default();
    for line in lines {
        let trimmed = line.trim();
        if trimmed == markers.start {
            start_seen = true;
            continue;
        }
        if trimmed == markers.cwd {
            parsed.cwd_after.clear();
            collecting_cwd = true;
            continue;
        }
        if let Some(value) = trimmed.strip_prefix(&markers.cwd) {
            parsed.cwd_after = value.trim().to_string();
            collecting_cwd = true;
            continue;
        }
        if let Some(value) = trimmed.strip_prefix(&markers.exit_code) {
            collecting_cwd = false;
            parsed.exit_code = value.trim().parse().ok();
            continue;
        }
        if trimmed == markers.end {
            parsed.completed = start_seen;
            break;
        }
        if collecting_cwd {
            parsed.cwd_after.push_str(trimmed);
            continue;
        }
        if start_seen && !contains_marker(trimmed) {
            output.push(line.trim_end().to_string());
        }
    }
    parsed.stdout = output.join("\n");
    parsed
}

// ── Agent 工具入口（宿主直连；每会话唯一调度者在本进程内）─────────────
//
// 工具编排（选终端、新建、开标签、执行、收结果）全部在 sidecar 内完成：
// 会话归属与权威工作目录来自请求帧携带的宿主上下文（Session.cwd 真相源），
// 不接受插件页面上下文；terminal_id 归属 scope 校验在操作内完成。

/// 工具调用的宿主权威会话上下文（请求帧 context，缺省拒绝执行）。
struct ToolScope {
    session_id: String,
    workspace: String,
    request_id: String,
}

/// 活动工具登记。正常执行结束时由 `complete` 清除；dispatch future 先被
/// 丢弃时有意保留映射，随后到达的 `cancel()` 仍能定位并中断 PTY。
struct ActiveToolRegistration {
    active_tools: Arc<Mutex<HashMap<String, ActiveTool>>>,
    request_id: String,
}

impl ActiveToolRegistration {
    fn complete(self) {
        if let Ok(mut active) = self.active_tools.lock() {
            active.remove(&self.request_id);
        }
    }
}

fn tool_scope(request_id: &str, context: Option<&RequestInvocationContext>) -> Result<ToolScope> {
    let context = context.ok_or_else(|| {
        anyhow::anyhow!("终端工具调用缺少宿主会话上下文（session_id 与工作目录由宿主注入）")
    })?;
    Ok(ToolScope {
        session_id: context.session_id.clone(),
        workspace: context.workspace.clone(),
        request_id: request_id.to_string(),
    })
}

/// 工具结果（对齐宿主 ToolOutcome 形状）。
#[derive(Debug, Serialize)]
struct ToolOutcome {
    ok: bool,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
    exit_code: i32,
}

impl ToolOutcome {
    fn ok(summary: String) -> Self {
        Self {
            ok: true,
            summary,
            stdout: None,
            stderr: None,
            exit_code: 0,
        }
    }

    fn fail(summary: String) -> Self {
        Self {
            ok: false,
            summary,
            stdout: None,
            stderr: None,
            exit_code: 1,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RunCommandArgs {
    cmd: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RunShellArgs {
    script: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    interactive: bool,
}

#[derive(Debug, Deserialize)]
struct TerminalSendArgs {
    input: String,
    terminal_id: String,
    #[serde(default)]
    wait: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TerminalCloseArgs {
    terminal_id: String,
}

#[derive(Debug, Deserialize)]
struct TerminalStatusArgs {
    /// 缺省查询当前会话全部终端；指定则返回该终端详情。
    #[serde(default)]
    terminal_id: String,
}

/// 终端状态对外标签（Agent 查询与摘要呈现用）。
fn phase_label(phase: SessionPhase, exited_code: Option<i32>) -> &'static str {
    if exited_code.is_some() {
        return "exited";
    }
    match phase {
        SessionPhase::Idle => "idle",
        SessionPhase::Reserved => "reserved",
        SessionPhase::Running => "running",
        SessionPhase::Interactive => "interactive",
        SessionPhase::Unresponsive => "unresponsive",
    }
}

/// 摘要中的中文表述。
fn phase_label_cn(label: &str) -> &'static str {
    match label {
        "idle" => "空闲",
        "reserved" => "已预留",
        "running" => "执行中",
        "interactive" => "交互中",
        "unresponsive" => "未就绪（暂不自动复用）",
        _ => "已退出",
    }
}

/// 经进度帧请求宿主执行 App 原语（建立/收起本插件标签）。
async fn request_host_app_open(scope_id: &str, instance_id: &str, focus: bool) {
    let mut payload = serde_json::json!({
        "session_id": scope_id,
        "instance_id": instance_id,
    });
    if !focus {
        // 静默建标签（不弹面板）与 shell 时代 showPanel:false 对齐；
        // 用户明确请求展示的 terminal_open 才展开面板。
        payload["mode"] = serde_json::json!("background");
    }
    let message = serde_json::json!({
        "host_action": {
            "method": "app.open",
            "payload": payload,
        }
    });
    tiangong_plugin_sidecar::emit_progress(message.to_string()).await;
}

async fn request_host_app_close(scope_id: &str, instance_id: &str) {
    let message = serde_json::json!({
        "host_action": {
            "method": "app.close",
            "payload": {
                "session_id": scope_id,
                "instance_id": instance_id,
            },
        }
    });
    tiangong_plugin_sidecar::emit_progress(message.to_string()).await;
}

impl TerminalService {
    /// run_command / run_shell：优先复用当前会话的空闲长期终端，没有则
    /// 新建；终端对用户可见可关，实例编号与 PTY 一致，幂等聚焦同一标签。
    async fn tool_run_command(
        &self,
        scope: &ToolScope,
        is_shell: bool,
        payload: serde_json::Value,
    ) -> ToolOutcome {
        // run_command 与 run_shell 共用执行链，仅命令形态不同：前者是
        // cmd + args，后者是 script + interactive。
        let (command, args, cwd_arg, timeout, interactive) = if is_shell {
            match serde_json::from_value::<RunShellArgs>(payload) {
                Ok(parsed) => (
                    parsed.script,
                    Vec::new(),
                    parsed.cwd,
                    parsed.timeout,
                    parsed.interactive,
                ),
                Err(error) => return ToolOutcome::fail(format!("run_shell 参数无效：{error}")),
            }
        } else {
            match serde_json::from_value::<RunCommandArgs>(payload) {
                Ok(parsed) => (parsed.cmd, parsed.args, parsed.cwd, parsed.timeout, false),
                Err(error) => return ToolOutcome::fail(format!("run_command 参数无效：{error}")),
            }
        };

        let acquired = self.acquire_session(AcquireRequest {
            scope_id: scope.session_id.clone(),
        });
        let mut session_id = acquired.session_id;
        let mut created_new = session_id.is_none();
        // cwd 优先级：Agent 显式指定 > 宿主权威会话工作目录。
        let workspace = cwd_arg
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let workspace = scope.workspace.trim();
                (!workspace.is_empty()).then(|| workspace.to_string())
            });
        if session_id.is_none() {
            let spawned = self.spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: workspace.clone(),
                scope_id: Some(scope.session_id.clone()),
                reserve: true,
                cols: default_cols(),
                rows: default_rows(),
            });
            match spawned {
                Ok(spawned) => session_id = Some(spawned.session_id),
                Err(error) => return ToolOutcome::fail(format!("终端会话创建失败：{error:#}")),
            }
        }
        let mut session_id = match session_id {
            Some(session_id) => session_id,
            None => return ToolOutcome::fail("终端会话创建失败".to_string()),
        };

        // 命令执行静默建立前台标签（不弹面板）：终端对用户可见可关，
        // 重复调用幂等聚焦同一标签。精确页面通过 terminalFind 完成事件
        // 订阅和附着后才开始输出；后台会话没有前台标签时有限等待后继续。
        // 复用终端的就绪握手失败时自动新建终端重试一次：新终端走不发
        // Ctrl+C 的干净握手路径，对上层透明。旧终端不被关闭，只是标记为
        // 不可自动复用（里面可能有 `&` / `nohup` 起的长期作业），用户仍
        // 可在标签里继续查看和操作。
        let mut retried_after_handshake = false;
        let mut handed_off_terminal = None;
        let executed = loop {
            // 从首次异步等待开始登记取消目标。执行 future 若先被丢弃，
            // 登记对象虽随 future 销毁，映射仍保留给随后到达的 cancel() 使用。
            let active = self.track_active_tool(&scope.request_id, &session_id, interactive);
            request_host_app_open(&scope.session_id, &session_id, false).await;
            if !self.wait_for_frontend_attach(&session_id).await {
                tracing::debug!(
                    terminal_id = session_id,
                    scope_id = scope.session_id,
                    "未等待到前端终端附着，按无前台页面的后台会话继续执行"
                );
            }
            let executed = self
                .exec_in_session(ExecRequest {
                    session_id: session_id.clone(),
                    cmd: if is_shell {
                        String::new()
                    } else {
                        command.clone()
                    },
                    args: if is_shell { Vec::new() } else { args.clone() },
                    script: if is_shell {
                        Some(command.clone())
                    } else {
                        None
                    },
                    timeout,
                    interactive,
                    cwd: workspace.clone().filter(|value| !value.is_empty()),
                })
                .await;
            active.complete();
            match executed {
                Ok(executed) => break executed,
                Err(error)
                    if !created_new
                        && !retried_after_handshake
                        && error.downcast_ref::<ShellNotReady>().is_some() =>
                {
                    // 首次新建仍握手失败说明 shell 环境异常，重试无益，
                    // 只对复用场景兜底一次。
                    tracing::warn!(
                        request_id = %scope.request_id,
                        terminal = %session_id,
                        "复用终端就绪握手失败，保留原终端并新建终端重试一次"
                    );
                    retried_after_handshake = true;
                    handed_off_terminal = Some(session_id.clone());
                    match self.spawn_session(SpawnRequest {
                        session_id: None,
                        cmd: String::new(),
                        args: Vec::new(),
                        script: None,
                        cwd: workspace.clone(),
                        scope_id: Some(scope.session_id.clone()),
                        reserve: true,
                        cols: default_cols(),
                        rows: default_rows(),
                    }) {
                        Ok(spawned) => {
                            session_id = spawned.session_id;
                            created_new = true;
                            continue;
                        }
                        Err(error) => {
                            return ToolOutcome::fail(format!(
                                "自动重试创建终端会话失败：{error:#}"
                            ));
                        }
                    }
                }
                Err(error) => {
                    // 执行失败时释放预留（幂等；会话已退出则忽略）。
                    let _ = self.release_session(SessionIdRequest {
                        session_id: session_id.clone(),
                    });
                    return ToolOutcome::fail(format!("终端执行失败：{error:#}"));
                }
            }
        };
        let exit_code = executed.exit_code;
        let cwd_note = if executed.cwd_after.is_empty() {
            String::new()
        } else {
            format!("，cwd: {}", executed.cwd_after)
        };
        let reason = if acquired.reason == "all_busy" {
            "当前会话已有终端都在忙"
        } else {
            "当前会话没有可用终端"
        };
        let selection = if let Some(previous) = handed_off_terminal.as_deref() {
            format!(
                "新终端 {session_id}（终端 {previous} 未在预算内回到提示符，已保留其内容与后台作业，改用新终端）"
            )
        } else if created_new {
            format!("新终端 {session_id}（{reason}，没有写入旧终端）")
        } else {
            format!("终端 {session_id}（复用空闲终端）")
        };
        // 失败/超时场景把退出码等失败事实放在摘要最前：调度解释（都在
        // 忙、没有写入旧终端）对定位错误没有帮助，只保留终端编号与是否
        // 新开供回溯输出位置；超时的中断语义（会话保留、可继续输入）
        // 也不与 stderr 的「命令执行超时」重复表述。
        let terminal_note = if created_new {
            format!("新终端 {session_id}")
        } else {
            format!("终端 {session_id}")
        };
        // 命令使 shell 本身退出：终端已不可复用，明示下次调用将新建。
        let ended_note = if executed.session_ended {
            "，终端已随命令退出，下次调用将新建终端"
        } else {
            ""
        };
        let summary = if executed.interactive_mode {
            format!("命令已在{selection}进入交互状态{ended_note}")
        } else if executed.timed_out {
            // 超时与 shell 退出同时命中时终端已不可输入，两种说法不能并存。
            if executed.session_ended {
                format!(
                    "命令超时已中断（{terminal_note} 已随命令退出，输出见 stdout/stderr）{cwd_note}，下次调用将新建终端"
                )
            } else {
                format!(
                    "命令超时已中断（{terminal_note} 仍可继续输入，此前输出见 stdout/stderr）{cwd_note}"
                )
            }
        } else if exit_code == 0 {
            format!("命令已在{selection}执行完成{cwd_note}{ended_note}")
        } else {
            format!(
                "命令失败，退出码 {exit_code}（{terminal_note}，完整输出见 stdout/stderr）{cwd_note}{ended_note}"
            )
        };
        ToolOutcome {
            ok: !executed.timed_out && (executed.interactive_mode || exit_code == 0),
            summary,
            stdout: Some(executed.stdout),
            stderr: Some(executed.stderr),
            exit_code,
        }
    }

    /// terminal_open：创建独立编号的终端并展开面板聚焦（用户明确请求
    /// 展示时使用）。
    async fn tool_terminal_open(&self, scope: &ToolScope) -> ToolOutcome {
        let instance_id = format!("terminal-{}", scru128::new());
        let spawned = self.spawn_session(SpawnRequest {
            session_id: Some(instance_id.clone()),
            cmd: String::new(),
            args: Vec::new(),
            script: None,
            cwd: (!scope.workspace.trim().is_empty()).then(|| scope.workspace.trim().to_string()),
            scope_id: Some(scope.session_id.clone()),
            reserve: false,
            cols: default_cols(),
            rows: default_rows(),
        });
        if let Err(error) = spawned {
            return ToolOutcome::fail(format!("终端面板操作失败：{error:#}"));
        }
        request_host_app_open(&scope.session_id, &instance_id, true).await;
        // 打开面板后等待页面完成事件订阅与 PTY 附着再返回：调用方拿到
        // 编号往往紧接着 terminal_send，附着未完成时输入会写在页面订阅
        // 之前，用户在标签里看不到这段交互。等待有上限，前端未接应
        //（后台会话等）不阻塞返回。
        if !self.wait_for_frontend_attach(&instance_id).await {
            tracing::debug!(
                terminal_id = instance_id,
                scope_id = scope.session_id,
                "terminal_open 未等到前端附着，按无前台页面继续"
            );
        }
        ToolOutcome::ok(format!("已打开终端 {instance_id}"))
    }

    /// terminal_send：向指定终端发送输入；目标必须属于当前会话。
    async fn tool_terminal_send(
        &self,
        scope: &ToolScope,
        payload: serde_json::Value,
    ) -> ToolOutcome {
        let parsed = match serde_json::from_value::<TerminalSendArgs>(payload) {
            Ok(parsed) => parsed,
            Err(error) => return ToolOutcome::fail(format!("terminal_send 参数无效：{error}")),
        };
        let target = parsed.terminal_id.trim().to_string();
        if target.is_empty() {
            return ToolOutcome::fail("terminal_send 缺少 terminal_id".to_string());
        }
        let sent = self
            .send_to_session(SendRequest {
                scope_id: scope.session_id.clone(),
                session_id: target,
                input: parsed.input,
                wait: parsed.wait,
            })
            .await;
        match sent {
            Ok(sent) => ToolOutcome {
                ok: true,
                summary: format!("终端 {} 已更新，当前内容见 stdout", sent.session_id),
                stdout: Some(sent.stdout),
                stderr: None,
                exit_code: sent.exit_code,
            },
            Err(error) => ToolOutcome::fail(format!("{error:#}")),
        }
    }

    /// terminal_close：精确关闭指定终端及其面板（归属校验在关闭操作内）。
    async fn tool_terminal_close(
        &self,
        scope: &ToolScope,
        payload: serde_json::Value,
    ) -> ToolOutcome {
        let parsed = match serde_json::from_value::<TerminalCloseArgs>(payload) {
            Ok(parsed) => parsed,
            Err(error) => return ToolOutcome::fail(format!("terminal_close 参数无效：{error}")),
        };
        let target = parsed.terminal_id.trim().to_string();
        if target.is_empty() {
            return ToolOutcome::fail(
                "terminal_close 缺少 terminal_id，未关闭任何终端".to_string(),
            );
        }
        // App 标签可能先于 PTY 退出被用户关闭，关闭操作幂等；但归属
        // 校验失败（终端属于其他会话）必须明确拒绝，不回退当前可见会话。
        match self.close_session(CloseRequest {
            session_id: Some(target.clone()),
            scope_id: scope.session_id.clone(),
        }) {
            Ok(_) => {}
            Err(error) => return ToolOutcome::fail(format!("终端面板操作失败：{error:#}")),
        }
        // 标签已不在前台时仍保证收起；操作幂等。
        request_host_app_close(&scope.session_id, &target).await;
        ToolOutcome::ok(format!("已关闭终端 {target}"))
    }

    /// terminal_status：查询当前会话的终端状态（只读，不触碰任何 PTY）。
    /// 不带 terminal_id 返回全部终端概要（按创建序）；带 terminal_id 返回
    /// 该终端详情（含最近可见输出尾部），归属校验与 terminal_send 一致。
    fn tool_terminal_status(&self, scope: &ToolScope, payload: serde_json::Value) -> ToolOutcome {
        let parsed = match serde_json::from_value::<TerminalStatusArgs>(payload) {
            Ok(parsed) => parsed,
            Err(error) => return ToolOutcome::fail(format!("terminal_status 参数无效：{error}")),
        };
        let target = parsed.terminal_id.trim().to_string();
        let sessions = self.sessions.lock().expect("会话表锁损坏");
        if target.is_empty() {
            // 概要含已退出待回收的终端：Agent 排查「为什么复用失败」时
            // 需要看到完整在表集合，与前端恢复列表（只列存活）目的不同。
            let mut entries: Vec<(u64, &String, &PtySession)> = sessions
                .iter()
                .filter(|(_, session)| {
                    session.scope_id.as_deref() == Some(scope.session_id.as_str())
                })
                .map(|(id, session)| (session.sequence, id, session))
                .collect();
            entries.sort_by_key(|(sequence, _, _)| *sequence);
            let mut status_counts: Vec<(&str, usize)> = Vec::new();
            let terminals: Vec<serde_json::Value> = entries
                .iter()
                .map(|(_, id, session)| {
                    let label = phase_label(session.phase, session.exited_code);
                    if let Some(count) = status_counts.iter_mut().find(|(name, _)| *name == label) {
                        count.1 += 1;
                    } else {
                        status_counts.push((label, 1));
                    }
                    serde_json::json!({
                        "terminal_id": id,
                        "status": label,
                        "command": session.running_command,
                        "exited_code": session.exited_code,
                    })
                })
                .collect();
            let summary = if terminals.is_empty() {
                "当前会话没有终端".to_string()
            } else {
                let breakdown = status_counts
                    .iter()
                    .map(|(label, count)| format!("{}×{}", phase_label_cn(label), count))
                    .collect::<Vec<_>>()
                    .join("、");
                format!("当前会话有 {} 个终端：{}", terminals.len(), breakdown)
            };
            return ToolOutcome {
                ok: true,
                summary,
                stdout: Some(
                    serde_json::to_string(&serde_json::json!({ "terminals": terminals }))
                        .unwrap_or_default(),
                ),
                stderr: None,
                exit_code: 0,
            };
        }
        let Some(session) = sessions.get(&target) else {
            return ToolOutcome::fail(format!("终端 {target} 不存在"));
        };
        if session.scope_id.as_deref() != Some(scope.session_id.as_str()) {
            return ToolOutcome::fail(format!("终端 {target} 不属于当前会话"));
        }
        let label = phase_label(session.phase, session.exited_code);
        let visible = String::from_utf8_lossy(&session.display_history).to_string();
        let reversed_tail = visible.chars().rev().take(2000).collect::<String>();
        let recent_output = reversed_tail.chars().rev().collect::<String>();
        let running_command = session.running_command.clone();
        let summary = match &running_command {
            Some(command) => format!("终端 {target} 状态：{}（{command}）", phase_label_cn(label)),
            None => format!("终端 {target} 状态：{}", phase_label_cn(label)),
        };
        ToolOutcome {
            ok: true,
            summary,
            stdout: Some(
                serde_json::to_string(&serde_json::json!({
                    "terminal_id": target,
                    "status": label,
                    "command": running_command,
                    "exited_code": session.exited_code,
                    "recent_output": recent_output,
                }))
                .unwrap_or_default(),
            ),
            stderr: None,
            exit_code: 0,
        }
    }
}

impl TerminalService {
    async fn dispatch_with_context(
        &self,
        request: Request,
        context: Option<RequestInvocationContext>,
    ) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "terminal 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }
        if request.operation != HANDSHAKE_OPERATION {
            self.mark_activity();
        }
        let payload = match dispatch_operation(
            self,
            &request_id,
            &request.operation,
            request.payload.clone(),
            context.as_ref(),
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return Response::error(
                    &request_id,
                    ErrorCode::ServiceError,
                    format!("{error:#}"),
                    false,
                );
            }
        };
        Response::success(&request_id, payload)
    }
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for TerminalService {
    async fn dispatch(&self, request: Request) -> Response {
        let context = tiangong_plugin_sidecar::invocation_context();
        self.dispatch_with_context(request, context).await
    }

    /// 请求级取消：宿主工具超时或会话取消经 Cancel 帧到达（stdio runner
    /// 已丢弃执行 future），从独立保留的映射定位具体 PTY，发送 Ctrl+C
    /// 中断正在执行的命令；经就绪探针确认登录 shell 重新接管后才放回
    /// 空闲池，确认失败直接关闭不放回（Ctrl+C 只送达信号，带子进程树的
    /// 长任务收尾需要时间，立即置 Idle 会让下一次调用在未收尾的终端上
    /// 握手，探针进入残留进程的 stdin，marker 永不回显）。
    async fn cancel(&self, request: &Request) -> Result<()> {
        let request_id = request.request_id.as_str();
        let active = self
            .active_tools
            .lock()
            .expect("活动工具表锁损坏")
            .remove(request_id);
        let Some(active) = active else {
            // 非执行型调用（send 的画面等待等）：等待 future 已被 runner
            // 丢弃，无 PTY 资源需要释放。
            return Ok(());
        };
        let terminal_id = active.terminal_id;
        // 与可能仍在丢弃收尾中的执行 future 串行化，避免写交错；future
        // 被 runner 丢弃后锁即释放，不会久等。
        let exec_lock =
            match self.with_session(&terminal_id, |session| Ok(Arc::clone(&session.exec_lock))) {
                Ok(exec_lock) => exec_lock,
                Err(error) => {
                    // 终端已退出（会话结束）等场景：无需中断，登记已清除。
                    tracing::debug!(
                        request_id,
                        terminal = %terminal_id,
                        %error,
                        "取消时终端已不可写"
                    );
                    return Ok(());
                }
            };
        let _guard = exec_lock.lock().await;
        // 按取消时刻的执行进度收尾，不能只凭请求的 interactive 参数：
        //
        // - Reserved：命令尚未写入 PTY（前端附着等待等阶段），终端干净，
        //   直接放回空闲池，无需中断确认——否则预留永远挂着不被复用；
        // - 交互程序已写入（interactive_started）：程序在前台且对用户
        //   可见，保留终端并转交互状态（探针进不了交互程序，确认流只会
        //   误杀）；由用户在标签内自行收尾；
        // - 更早进入的 Interactive：同上保持不动；
        // - 其余（命令执行中、交互调用的握手阶段）：走中断确认流。
        let state = self.with_session(&terminal_id, |session| {
            Ok((session.phase, session.interactive_started))
        });
        let (phase, interactive_started) = match state {
            Ok(state) => state,
            Err(error) => {
                // 拿到 exec_lock 后终端才被回收：无需中断，登记已清除。
                tracing::debug!(
                    request_id,
                    terminal = %terminal_id,
                    %error,
                    "取消时终端已不可用"
                );
                return Ok(());
            }
        };
        match phase {
            SessionPhase::Reserved => {
                let _ = self.with_session(&terminal_id, |session| {
                    session.phase = SessionPhase::Idle;
                    session.running_command = None;
                    Ok(())
                });
                tracing::info!(
                    request_id,
                    terminal = %terminal_id,
                    "取消发生在命令启动前，终端直接回到空闲"
                );
                return Ok(());
            }
            SessionPhase::Interactive => {
                tracing::info!(
                    request_id,
                    terminal = %terminal_id,
                    "取消交互调用：保留交互终端，不做中断确认"
                );
                return Ok(());
            }
            _ if active.interactive && interactive_started => {
                let _ = self.with_session(&terminal_id, |session| {
                    session.phase = SessionPhase::Interactive;
                    Ok(())
                });
                tracing::info!(
                    request_id,
                    terminal = %terminal_id,
                    "取消交互调用：程序保留在前台，终端转入交互状态"
                );
                return Ok(());
            }
            _ => {}
        }
        let interrupted = self.with_session(&terminal_id, |session| {
            session
                .writer
                .write_all(b"\x03")
                .context("取消写入中断输入失败")?;
            session.writer.flush().context("取消刷新中断输入失败")
        });
        if let Err(error) = interrupted {
            // 终端已退出（会话结束）等场景：无需中断，登记已清除。
            tracing::debug!(
                request_id,
                terminal = %terminal_id,
                %error,
                "取消时终端已不可写"
            );
            return Ok(());
        }
        // 就绪握手复用命令前置路径：shell_ready 终端会再发一次 Ctrl+C
        // 清理残留输入并用探针确认提示符已恢复。确认成功才放回复用池；
        // 失败时超时路径已把终端标记为 Unresponsive（终端仍存活，后台
        // 作业和用户输入不受影响），不放回池即可。
        match self.prepare_shell_for_command(&terminal_id).await {
            Ok(()) => {
                let _ = self.with_session(&terminal_id, |session| {
                    session.phase = SessionPhase::Idle;
                    session.running_command = None;
                    Ok(())
                });
                tracing::info!(
                    request_id,
                    terminal = %terminal_id,
                    "已按取消请求中断终端命令并确认 Shell 就绪"
                );
            }
            Err(error) => {
                tracing::warn!(
                    request_id,
                    terminal = %terminal_id,
                    %error,
                    "取消后 Shell 未在预算内回到提示符，终端保留但不再自动复用"
                );
            }
        }
        Ok(())
    }
}

async fn dispatch_operation(
    service: &TerminalService,
    request_id: &str,
    operation: &str,
    payload: serde_json::Value,
    context: Option<&RequestInvocationContext>,
) -> Result<serde_json::Value> {
    // Agent 工具入口（宿主直连）：会话归属来自宿主注入的请求上下文，
    // 缺失即拒绝——不允许脱离会话真相源执行终端操作。
    if matches!(
        operation,
        "run_command"
            | "run_shell"
            | "terminal_open"
            | "terminal_send"
            | "terminal_close"
            | "terminal_status"
    ) {
        let scope = tool_scope(request_id, context)?;
        let outcome = match operation {
            "run_command" => service.tool_run_command(&scope, false, payload).await,
            "run_shell" => service.tool_run_command(&scope, true, payload).await,
            "terminal_open" => service.tool_terminal_open(&scope).await,
            "terminal_send" => service.tool_terminal_send(&scope, payload).await,
            "terminal_status" => service.tool_terminal_status(&scope, payload),
            _ => service.tool_terminal_close(&scope, payload).await,
        };
        return Ok(serde_json::to_value(outcome)?);
    }
    match operation {
        HANDSHAKE_OPERATION => Ok(serde_json::json!({
            "plugin_id": "terminal",
            "plugin_version": env!("CARGO_PKG_VERSION"),
            "sidecar_version": env!("CARGO_PKG_VERSION"),
            "protocol_version": PROTOCOL_VERSION,
            "business_protocol": 1,
            "capabilities": [
                "tool:run_command",
                "tool:run_shell",
                "tool:terminal_open",
                "tool:terminal_send",
                "tool:terminal_close",
                "tool:terminal_status",
            ],
            "instance_id": format!("terminal-sidecar-{}", std::process::id()),
            "status": "ready",
        })),
        "terminalSpawn" => {
            let request: SpawnRequest =
                serde_json::from_value(payload).context("terminalSpawn 参数无效")?;
            Ok(serde_json::to_value(service.spawn_session(request)?)?)
        }
        "terminalAcquire" => {
            let request: AcquireRequest =
                serde_json::from_value(payload).context("terminalAcquire 参数无效")?;
            Ok(serde_json::to_value(service.acquire_session(request))?)
        }
        "terminalRelease" => {
            let request: SessionIdRequest =
                serde_json::from_value(payload).context("terminalRelease 参数无效")?;
            Ok(serde_json::to_value(service.release_session(request)?)?)
        }
        "terminalExec" => {
            let request: ExecRequest =
                serde_json::from_value(payload).context("terminalExec 参数无效")?;
            Ok(serde_json::to_value(
                service.exec_in_session(request).await?,
            )?)
        }
        "terminalWrite" => {
            let request: WriteRequest =
                serde_json::from_value(payload).context("terminalWrite 参数无效")?;
            service.with_session(&request.session_id, |session| {
                session
                    .writer
                    .write_all(request.data.as_bytes())
                    .context("写入 PTY 失败")?;
                session.writer.flush().context("刷新 PTY 失败")
            })?;
            Ok(serde_json::to_value(OkResponse { ok: true })?)
        }
        "terminalSend" => {
            let request: SendRequest =
                serde_json::from_value(payload).context("terminalSend 参数无效")?;
            Ok(serde_json::to_value(
                service.send_to_session(request).await?,
            )?)
        }
        "terminalAttach" => {
            let request: TerminalBindingRequest =
                serde_json::from_value(payload).context("终端附着状态参数无效")?;
            Ok(serde_json::to_value(service.attach_frontend(request)?)?)
        }
        "terminalScreenUpdate" => {
            let request: ScreenUpdateRequest =
                serde_json::from_value(payload).context("terminalScreenUpdate 参数无效")?;
            Ok(serde_json::to_value(service.update_screen(request)?)?)
        }
        "terminalResize" => {
            let request: ResizeRequest =
                serde_json::from_value(payload).context("terminalResize 参数无效")?;
            service.with_session(&request.session_id, |session| {
                session
                    .master
                    .resize(PtySize {
                        rows: request.rows,
                        cols: request.cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .context("调整 PTY 尺寸失败")
            })?;
            Ok(serde_json::to_value(OkResponse { ok: true })?)
        }
        "terminalKill" => {
            let request: SessionIdRequest =
                serde_json::from_value(payload).context("terminalKill 参数无效")?;
            Ok(serde_json::to_value(service.kill_session(request)?)?)
        }
        "terminalClose" => {
            let request: CloseRequest =
                serde_json::from_value(payload).context("terminalClose 参数无效")?;
            Ok(serde_json::to_value(service.close_session(request)?)?)
        }
        "terminalGc" => {
            let request: TerminalGcRequest =
                serde_json::from_value(payload).context("terminalGc 参数无效")?;
            Ok(serde_json::to_value(service.gc_terminals(request)?)?)
        }
        // 宿主统一实例生命周期：标签移除后宿主保证发出一次（页面未就绪或
        // 已卸载时 beforeClose 无人接收，也由此回收）。幂等，归属校验同
        // terminalClose——实例编号与终端编号一一对应。
        "instanceClosed" => {
            let request: InstanceClosedRequest =
                serde_json::from_value(payload).context("instanceClosed 参数无效")?;
            let scope_id = request.session_id.trim().to_string();
            let instance_id = request.instance_id.trim().to_string();
            if scope_id.is_empty() || instance_id.is_empty() {
                bail!("instanceClosed 需要有效的 session_id 与 instance_id");
            }
            Ok(serde_json::to_value(service.close_session(
                CloseRequest {
                    session_id: Some(instance_id),
                    scope_id,
                },
            )?)?)
        }
        // 宿主统一实例核查：列出会话下持有资源的实例（切换会话恢复标签、
        // 核查多余资源、逻辑删除会话时释放）。
        "listInstances" => {
            let request: ListInstancesRequest =
                serde_json::from_value(payload).context("listInstances 参数无效")?;
            let listed = service.list_by_scope(&ListByScopeRequest {
                scope_id: request.session_id,
            })?;
            Ok(serde_json::to_value(ListInstancesResponse {
                instances: listed
                    .terminals
                    .into_iter()
                    .map(|entry| InstanceEntry {
                        instance_id: entry.session_id,
                    })
                    .collect(),
            })?)
        }
        "terminalFind" => {
            let request: FindRequest =
                serde_json::from_value(payload).context("terminalFind 参数无效")?;
            Ok(serde_json::to_value(service.find_by_scope(&request))?)
        }
        "terminalListByScope" => {
            let request: ListByScopeRequest =
                serde_json::from_value(payload).context("terminalListByScope 参数无效")?;
            Ok(serde_json::to_value(service.list_by_scope(&request)?)?)
        }
        other => bail!("未知操作: {other}"),
    }
}

// PtySystem trait object 供 openpty 使用（NativePtySystem 已是具体类型，此处不需）
#[allow(dead_code)]
fn _pty_system_type_check(_: &dyn PtySystem) {}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use tiangong_plugin_sidecar::SidecarService as _;

    #[test]
    fn split_command_拆分含参数的cmd() {
        let (program, args) = split_command("git status --short");
        assert_eq!(program, "git");
        assert_eq!(args, vec!["status".to_string(), "--short".to_string()]);
    }

    #[test]
    fn split_command_引号成组() {
        let (program, args) = split_command("echo 'a b' \"c d\"");
        assert_eq!(program, "echo");
        assert_eq!(args, vec!["a b".to_string(), "c d".to_string()]);
    }

    #[test]
    fn split_command_裸命令无参数() {
        let (program, args) = split_command("ls");
        assert_eq!(program, "ls");
        assert!(args.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn shell_ready_probe_uses_cmd_syntax() {
        assert_eq!(
            shell_ready_probe(ShellKind::Cmd, "__TIANGONG_READY_test__"),
            "echo __TIANGONG_^READY_test__\r"
        );
    }

    /// 探针文本本身绝不能含完整 marker：PTY 回显会把写入的字节原样回流，
    /// 若探针里带完整 marker，未就绪终端（残留前台进程占着 stdin、一个字节
    /// 都没执行）的纯回显也会命中子串匹配，就绪判定退化成恒真。
    #[test]
    fn shell_ready_probe_文本不含完整_marker() {
        let marker = "__TIANGONG_READY_abc__";
        for shell in [ShellKind::Posix, ShellKind::Cmd, ShellKind::PowerShell] {
            let probe = shell_ready_probe(shell, marker);
            assert!(
                !probe.contains(marker),
                "{shell:?} 探针回显会被误判为就绪: {probe}"
            );
        }
        assert_eq!(
            shell_ready_probe(ShellKind::Posix, marker),
            "echo '__TIANGONG_''READY_abc__'\r",
            "POSIX 探针须用相邻引用串拼接，执行后才输出完整 marker"
        );
    }

    #[test]
    fn 就绪判定_未执行的纯回显不算就绪() {
        let marker = "__TIANGONG_READY_abc__";
        // 残留前台进程占用 stdin：探针整行被 tty 回显，但无人执行。
        let echo_only = format!(
            "\u{1b}[?2004h{}\r\n",
            shell_ready_probe(ShellKind::Posix, marker).trim_end_matches('\r')
        );
        assert!(
            !shell_ready_probe_completed(&echo_only, marker),
            "纯回显必须判为未就绪: {echo_only:?}"
        );
    }

    /// 富提示符下**真正执行过**探针：marker 独占一行（前面可能有 starship
    /// 提示符与 OSC 颜色查询等噪声行），必须判为就绪。
    #[test]
    fn 就绪判定_命中富提示符噪声中独占一行的_marker() {
        let marker = "__TIANGONG_READY_abc__";
        let noisy = format!(
            "\u{1b}]11;rgb:1e1e/1e1e/2e2e\u{1b}\\\r\n\
             \u{1b}[1;32m❯\u{1b}[0m echo '__TIANGONG_READY_''abc__'\r\n\
             {marker}\u{1b}[K\r\n"
        );
        assert!(
            shell_ready_probe_completed(&noisy, marker),
            "执行后独占一行的 marker 必须判为就绪"
        );
        assert!(
            !shell_ready_probe_completed(&noisy, "__TIANGONG_READY_other__"),
            "不同随机 marker 不得互相命中"
        );
    }

    /// zsh-autosuggestions / zsh-syntax-highlighting 等 ZLE widget 会在**行编辑
    /// 阶段**重绘输入行，重绘时引号被消化，完整 marker 随之出现在原始字节流
    /// 中——但命令一个字都还没执行。
    ///
    /// 这是 #571 兜底假阴性的真正根因：旧实现在未经终端仿真的原始字节上做
    /// `contains`，把重绘中间态当成执行结果，导致 git fetch 等正常命令被误判
    /// 为「包装脚本夭折」，实际执行成功却回报失败，诱导调用方重复执行。
    #[test]
    fn 就绪判定_zsh行编辑器重绘不算就绪() {
        let marker = "__TIANGONG_READY_abc__";
        // autosuggest 的 xtrace 前缀里带完整 marker：命令尚未执行。
        let xtrace = format!("_zsh_autosuggest_bind_widgets:18> echo {marker}\r\n");
        assert!(
            !shell_ready_probe_completed(&xtrace, marker),
            "xtrace 回显的未执行命令行不得判为就绪: {xtrace:?}"
        );

        // 语法高亮重绘：marker 与提示符同行，且行尾被擦除序列覆盖。
        let redraw = format!("\u{1b}[1;32m❯\u{1b}[0m echo {marker}\u{1b}[K\r");
        assert!(
            !shell_ready_probe_completed(&redraw, marker),
            "行编辑器重绘出的命令行不得判为就绪: {redraw:?}"
        );
    }

    #[test]
    fn marker_filter_hides_wrapper_and_keeps_command_output() {
        let mut filter = RawOutputFilter::new(Arc::new(Mutex::new(HashMap::new())));
        let visible = filter.filter(
            "$ __TIANGONG_= echo '__TIANGONG_START_x__'; ls\r\n\
             __TIANGONG_START_x__\r\nCargo.toml\r\n\
             __TIANGONG_RC_x__0\r\n",
        );
        assert_eq!(visible, "Cargo.toml\r\n");
    }

    /// 终端应答序列（方向是终端 → 应用）绝不能进入给用户看的输出。
    ///
    /// zsh 的 ZLE 被唤醒重绘时（夭折探针写入等），zsh-syntax-highlighting
    /// 与主题会发 OSC 11 查背景色、DSR 6n 查光标位置，终端应答随即混入 PTY
    /// 输出流。过滤器不处理的话，用户会在长命令输出中途看到
    /// `]11;rgb:1e1e/1e1e/2e2e` 与 `[24;1R` 这类乱码。
    #[test]
    fn marker_filter_剥离终端应答序列() {
        let mut filter = RawOutputFilter::new(Arc::new(Mutex::new(HashMap::new())));
        let visible = filter
            .filter("\u{1b}]11;rgb:1e1e/1e1e/2e2e\u{1b}\\building...\r\n\u{1b}[24;1Rdone\r\n");
        assert_eq!(
            visible, "building...\r\ndone\r\n",
            "OSC 颜色应答与 DSR 光标上报必须被剥离: {visible:?}"
        );
    }

    /// 剥离只针对应答序列，正常渲染序列（SGR 颜色、光标移动、擦除）必须
    /// 原样保留——它们是 UI 正确着色与重绘的依据。
    #[test]
    fn marker_filter_不误伤正常渲染序列() {
        let kept = strip_terminal_reports(
            "\u{1b}[1;32mgreen\u{1b}[0m\u{1b}[2J\u{1b}[H\u{1b}[K\u{1b}[10Dmoved",
        );
        assert_eq!(
            kept, "\u{1b}[1;32mgreen\u{1b}[0m\u{1b}[2J\u{1b}[H\u{1b}[K\u{1b}[10Dmoved",
            "正常渲染序列不得被剥离: {kept:?}"
        );
        // OSC 0/2（设置标题）是应用 → 终端方向，不属于应答，保留。
        let title = strip_terminal_reports("\u{1b}]0;my title\u{7}text");
        assert_eq!(
            title, "\u{1b}]0;my title\u{7}text",
            "设置标题序列不得被剥离"
        );
    }

    /// 命令行必须由 start marker 就地替换产生，紧贴自己的输出。
    /// 早前实现是在写入 PTY 前抢先推送命令文本，shell 为就绪探针重绘的
    /// 提示符会插在命令和输出之间，与用户手敲命令的形态不一致。
    #[test]
    fn marker_filter_把start_marker换成命令行并紧贴输出() {
        let pending = Arc::new(Mutex::new(HashMap::from([(
            "__TIANGONG_START_x__".to_string(),
            "cargo clippy --workspace".to_string(),
        )])));
        let mut filter = RawOutputFilter::new(Arc::clone(&pending));
        // shell 先回显 wrapper、重绘提示符，随后才是 start marker 与输出。
        let visible = filter.filter(
            "$ __TIANGONG_= . /tmp/x.sh\r\n\
             __TIANGONG_START_x__\r\n\
             Finished `dev` profile\r\n\
             __TIANGONG_RC_x__0\r\n",
        );
        assert_eq!(
            visible, "cargo clippy --workspace\r\nFinished `dev` profile\r\n",
            "命令行应紧邻其输出，中间不得夹入提示符或 wrapper 回显"
        );
        assert!(
            pending.lock().expect("锁损坏").is_empty(),
            "命令回显只消费一次"
        );
    }

    #[test]
    fn marker_filter_handles_windows_cursor_moves_and_wrapping() {
        let mut filter = RawOutputFilter::new(Arc::new(Mutex::new(HashMap::new())));
        let visible = filter.filter(
            "__TIANGONG_START_x__\x1b[10;1HC:\\work>echo __TIANGONG_STA\x1b[11;80HRT_x__>nul & dir /b\r\nresult.txt\x1b[13;1HC:\\work>set __TIANGONG_RC_VALUE=0\x1b[14;1H",
        );
        assert_eq!(visible, "result.txt\x1b[13;1H");
    }

    /// cwd 绝不能拼进命令文本：命令文本会原样回显给用户、记入
    /// running_command 并出现在失败摘要里，混入 `cd ... &&` 会让用户看到
    /// 自己绝不会这样敲的命令（宿主每次调用都带会话工作目录，等于每条
    /// 命令都平白多一截前缀）。目录切换由包装脚本承担。
    #[test]
    fn cwd不进入命令文本而由包装脚本承担() {
        let request = ExecRequest {
            session_id: "s".to_string(),
            cmd: "cargo".to_string(),
            args: vec!["clippy".to_string()],
            script: None,
            timeout: None,
            interactive: false,
            cwd: Some("/work/repo".to_string()),
        };
        let command = command_from_request(&request, ShellKind::Posix).expect("拼装命令失败");
        assert_eq!(command, "cargo clippy", "命令文本不得含 cd 前缀");

        let markers = CommandMarkers::new("x");
        let quoted = ShellKind::Posix.quote("/work/repo");
        let wrapper = posix_wrapper_script("'/tmp/inner.sh'", Some("/work/repo"), &markers);
        assert!(
            wrapper.contains(&format!("cd {quoted} || return $?")),
            "包装脚本须负责切换目录且失败即终止: {wrapper}"
        );
        let cd_at = wrapper.find(&format!("cd {quoted}")).expect("缺少 cd");
        let source_at = wrapper.find(". '/tmp/inner.sh'").expect("缺少 source");
        assert!(cd_at < source_at, "必须先切目录再执行命令");

        // 交互模式同样不污染命令文本，改为独立一行写入。
        assert_eq!(
            interactive_cwd_command(Some("/work/repo"), ShellKind::Posix),
            Some(format!("cd {quoted}\r"))
        );
        assert_eq!(interactive_cwd_command(None, ShellKind::Posix), None);
    }

    #[test]
    fn command_parser_returns_stdout_exit_code_and_cwd() {
        let markers = CommandMarkers::new("x");
        let parsed = parse_command_output(
            b"prompt wrapper\r\n__TIANGONG_START_x__\r\none\r\ntwo\r\n__TIANGONG_CWD_x__/tmp\r\n__TIANGONG_RC_x__7\r\n__TIANGONG_END_x__\r\n",
            &markers,
        );
        assert!(parsed.completed);
        assert_eq!(parsed.stdout, "one\ntwo");
        assert_eq!(parsed.exit_code, Some(7));
        assert_eq!(parsed.cwd_after, "/tmp");
    }

    #[test]
    fn command_parser_joins_wrapped_cwd_without_polluting_stdout() {
        let markers = CommandMarkers::new("x");
        let parsed = parse_command_output(
            b"__TIANGONG_START_x__\r\nagain\r\n__TIANGONG_CWD_x__\r\nC:\\Users\\Test\\very-long\r\n                                                                               path\r\n__TIANGONG_RC_x__0\r\n__TIANGONG_END_x__\r\n",
            &markers,
        );
        assert!(parsed.completed);
        assert_eq!(parsed.stdout, "again");
        assert_eq!(parsed.cwd_after, r"C:\Users\Test\very-longpath");
    }

    /// issue #571：用户命令必须落在独立的内层脚本里，由 wrapper `source`。
    /// 内联时用户命令的解析期错误会连带终止 wrapper，end marker 永不输出。
    #[test]
    fn posix_包装器把用户命令隔离在内层脚本() {
        let markers = CommandMarkers::new("x");
        let prepared = prepare_posix_command("echo A; echo ===; echo B", None, &markers)
            .expect("生成包装失败");
        assert_eq!(prepared._files.len(), 2, "应生成 wrapper 与内层两个脚本");
        let wrapper = std::fs::read_to_string(&prepared._files[0]).expect("读取 wrapper 失败");
        let inner = std::fs::read_to_string(&prepared._files[1]).expect("读取内层脚本失败");
        assert_eq!(inner, "echo A; echo ===; echo B\n");
        assert!(
            !wrapper.contains("echo A; echo ===; echo B"),
            "用户命令不得内联进 wrapper: {wrapper}"
        );
        assert!(
            wrapper.contains(&markers.end),
            "wrapper 必须自带 end marker: {wrapper}"
        );
    }

    /// zsh `EQUALS` 必须在用户命令前关闭、命令后按原值恢复；
    /// 且片段对 bash/dash 无害（`setopt` 只在 ZSH_VERSION 下经 eval 执行）。
    #[test]
    fn posix_包装器关闭并恢复_zsh_equals() {
        let markers = CommandMarkers::new("x");
        let wrapper = posix_wrapper_script("'/tmp/inner.sh'", None, &markers);
        let disable = wrapper.find("setopt noequals").expect("缺少关闭 equals");
        let source = wrapper.find(". '/tmp/inner.sh'").expect("缺少内层 source");
        let restore = wrapper
            .find("__tiangong_equals_restore\" ]; then eval 'setopt equals'")
            .expect("缺少恢复 equals");
        assert!(disable < source, "必须在执行用户命令前关闭 equals");
        assert!(source < restore, "必须在用户命令之后再恢复 equals");
        assert!(
            wrapper.contains("if [ -n \"${ZSH_VERSION-}\" ]; then eval"),
            "setopt 必须由 ZSH_VERSION 守卫并经 eval 执行，避免 bash/dash 解析失败"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn command_exec_returns_output_and_keeps_shell_alive() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        std::fs::write(cwd.path().join("terminal-result.txt"), "ok").expect("创建测试文件失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        let first = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: "ls".to_string(),
                args: Vec::new(),
                script: None,
                timeout: Some(10),
                interactive: false,
                cwd: None,
            })
            .await
            .expect("第一次命令执行失败");
        let raw = service
            .with_session(&spawned.session_id, |session| {
                Ok(String::from_utf8_lossy(&session.raw_history).to_string())
            })
            .unwrap_or_default();
        assert_eq!(first.exit_code, 0, "第一次命令结果: {first:?}\nraw={raw:?}");
        assert_eq!(first.stdout, "terminal-result.txt");
        assert_eq!(
            first.cwd_after,
            cwd.path()
                .canonicalize()
                .expect("规范化测试目录失败")
                .to_string_lossy()
        );

        let second = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: String::new(),
                args: Vec::new(),
                script: Some("false".to_string()),
                timeout: Some(10),
                interactive: false,
                cwd: None,
            })
            .await
            .expect("第二次命令执行失败");
        assert_eq!(second.exit_code, 1);

        let timed_out = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: String::new(),
                args: Vec::new(),
                script: Some("sleep 5".to_string()),
                timeout: Some(1),
                interactive: false,
                cwd: None,
            })
            .await
            .expect("超时命令执行失败");
        assert!(timed_out.timed_out);
        assert_eq!(timed_out.exit_code, -1);

        let after_timeout = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: String::new(),
                args: Vec::new(),
                script: Some("printf 'still-alive\\n'".to_string()),
                timeout: Some(10),
                interactive: false,
                cwd: None,
            })
            .await
            .expect("超时后的命令执行失败");
        assert_eq!(after_timeout.exit_code, 0);
        assert_eq!(after_timeout.stdout, "still-alive");
        assert!(
            service
                .with_session(&spawned.session_id, |_| Ok(()))
                .is_ok()
        );

        service
            .kill_session(SessionIdRequest {
                session_id: spawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    /// issue #571 的验证标准：`echo A; echo ===; echo B` 必须秒级返回、
    /// 三行输出齐全、退出码 0，终端保持可复用。
    /// 修复前 zsh 的 equals 展开会在解析期终止整份包装脚本，end marker
    /// 永不出现，本用例会一直轮询到超时。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zsh_裸等号命令不再挂起并返回完整输出() {
        let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh", "/usr/local/bin/zsh"]
            .into_iter()
            .find(|path| std::path::Path::new(path).exists())
        else {
            eprintln!("跳过：本机没有 zsh");
            return;
        };
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: zsh.to_string(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建 zsh 测试终端失败");

        let result = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: String::new(),
                args: Vec::new(),
                script: Some("echo A; echo ===; echo B".to_string()),
                timeout: Some(15),
                interactive: false,
                cwd: None,
            })
            .await
            .expect("裸等号命令执行失败");
        assert!(!result.timed_out, "裸等号命令不应超时: {result:?}");
        assert_eq!(result.exit_code, 0, "裸等号命令应成功: {result:?}");
        let lines: Vec<&str> = result.stdout.lines().map(str::trim).collect();
        assert_eq!(lines, vec!["A", "===", "B"], "输出不完整: {result:?}");

        // 终端仍可复用：下一条命令照常执行。
        let next = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: String::new(),
                args: Vec::new(),
                script: Some("echo still-usable".to_string()),
                timeout: Some(15),
                interactive: false,
                cwd: None,
            })
            .await
            .expect("后续命令执行失败");
        assert_eq!(next.exit_code, 0, "终端应保持可复用: {next:?}");
        assert!(next.stdout.contains("still-usable"), "{next:?}");

        service
            .kill_session(SessionIdRequest {
                session_id: spawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    /// P1 兜底：用户命令自身有解析期错误（未闭合引号）时，包装脚本
    /// 仍须正常收尾——返回非零退出码与 shell 的原始报错，而不是挂到超时。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 用户命令语法错误仍按非零退出码正常收尾() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        let result = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: String::new(),
                args: Vec::new(),
                script: Some("echo before-error\necho \"unclosed\necho never".to_string()),
                timeout: Some(15),
                interactive: false,
                cwd: None,
            })
            .await
            .expect("语法错误命令执行失败");
        assert!(!result.timed_out, "语法错误不应表现为超时: {result:?}");
        assert_ne!(result.exit_code, 0, "语法错误应返回非零退出码: {result:?}");
        assert!(
            result.stdout.contains("before-error"),
            "错误前已执行的输出应保留: {result:?}"
        );

        service
            .kill_session(SessionIdRequest {
                session_id: spawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    /// 夭折探针带公共 marker 前缀，其回显与执行结果都不得混进命令 stdout。
    #[test]
    fn 夭折探针不污染命令输出() {
        let markers = CommandMarkers::new("x");
        let probe_marker = "__TIANGONG_ABORTCHK_abc__";
        let probe_echo = shell_ready_probe(ShellKind::Posix, probe_marker);
        let raw = format!(
            "__TIANGONG_START_x__\r\nreal-output\r\n{}\r\n{probe_marker}\r\n__TIANGONG_CWD_x__/tmp\r\n__TIANGONG_RC_x__0\r\n__TIANGONG_END_x__\r\n",
            probe_echo.trim_end_matches('\r')
        );
        let parsed = parse_command_output(raw.as_bytes(), &markers);
        assert!(parsed.completed);
        assert_eq!(parsed.stdout, "real-output", "探针行不得进入命令输出");
    }

    /// 静默但仍在运行的长命令绝不能被夭折兜底误判：探针躺在输入缓冲里
    /// 不被执行，`wrapper_aborted` 必须返回 false，命令照常跑到结束。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 静默长命令不被夭折兜底误判() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        // 静默 6 秒（超过 SILENT_ABORTED_WRAPPER_SECS，会触发多轮探测）
        // 后才输出，必须拿到正常退出码与完整输出。
        let result = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: String::new(),
                args: Vec::new(),
                script: Some("sleep 6; echo slow-done".to_string()),
                timeout: Some(30),
                interactive: false,
                cwd: None,
            })
            .await
            .expect("静默长命令执行失败");
        assert!(!result.timed_out, "静默长命令不应超时: {result:?}");
        assert_eq!(result.exit_code, 0, "静默长命令应正常完成: {result:?}");
        assert!(
            result.stdout.contains("slow-done"),
            "应拿到完整输出: {result:?}"
        );

        service
            .kill_session(SessionIdRequest {
                session_id: spawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    /// P1 兜底必须真能触发：构造一个 end marker 永不出现、但 shell 已回到
    /// 提示符的场景——内层脚本用 `exec` 顶掉当前 shell 进程执行别的程序，
    /// wrapper 余下的 marker 输出随之全部丢失。
    /// 没有兜底时本用例只能等到 timeout；有兜底时秒级返回明确语义。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 包装脚本夭折时按异常结束收尾而非超时() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        let started = Instant::now();
        let result = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: String::new(),
                args: Vec::new(),
                // 子 shell 里 exec 一个新登录 shell：当前 shell 被顶替，
                // wrapper 的 cwd/rc/end marker 全部不会再输出。
                script: Some("echo before-abort; exec sh -i".to_string()),
                // 不设 timeout：没有兜底就会一路轮询到工具层超时。
                timeout: None,
                interactive: false,
                cwd: None,
            })
            .await
            .expect("夭折命令执行失败");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "兜底应在静默窗口后很快收尾，实际耗时 {:?}",
            started.elapsed()
        );
        assert!(
            result.stderr.contains("包装脚本未正常结束"),
            "应按夭折给出可识别语义: {result:?}"
        );
        assert_eq!(result.exit_code, -1, "夭折应返回 -1: {result:?}");
        assert!(
            !result.timed_out,
            "夭折不应被表述为超时（超时会另行标记不可复用）: {result:?}"
        );
        assert!(
            result.stdout.contains("before-abort"),
            "夭折前的输出应保留: {result:?}"
        );

        service
            .kill_session(SessionIdRequest {
                session_id: spawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn command_exiting_shell_returns_code_output_and_reaps_session() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        // 命令先输出内容再 exit 7：退出码、此前输出与会话结束标记都应保留。
        let result = service
            .exec_in_session(ExecRequest {
                session_id: spawned.session_id.clone(),
                cmd: String::new(),
                args: Vec::new(),
                script: Some("echo before-exit; exit 7".to_string()),
                timeout: Some(10),
                interactive: false,
                cwd: None,
            })
            .await
            .expect("结束 shell 的命令执行失败");
        assert_eq!(result.exit_code, 7, "应以 shell 退出码收尾: {result:?}");
        assert!(
            result.stdout.contains("before-exit"),
            "应保留命令输出: {result:?}"
        );
        assert!(result.session_ended, "应标记终端已随命令退出: {result:?}");
        assert!(
            service
                .sessions
                .lock()
                .expect("会话表锁损坏")
                .get(&spawned.session_id)
                .is_none(),
            "死亡会话应已出表"
        );

        // 同一会话紧接着执行：选终端判定无可用（旧死终端已清、不占用
        // all_busy 名额），新建终端后正常返回。
        let acquired = service.acquire_session(AcquireRequest {
            scope_id: "terminal-sidecar-test".to_string(),
        });
        assert_eq!(
            acquired.reason, "no_available",
            "死终端不应计入可用或占用名额: {acquired:?}"
        );
        let respawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("重建测试终端失败");
        let second = service
            .exec_in_session(ExecRequest {
                session_id: respawned.session_id.clone(),
                cmd: String::new(),
                args: Vec::new(),
                script: Some("echo recovered".to_string()),
                timeout: Some(10),
                interactive: false,
                cwd: None,
            })
            .await
            .expect("死终端后的命令执行失败");
        assert_eq!(second.exit_code, 0);
        assert_eq!(second.stdout, "recovered");

        service
            .kill_session(SessionIdRequest {
                session_id: respawned.session_id,
            })
            .expect("清理测试终端失败");
    }

    #[cfg(unix)]
    #[test]
    fn acquire_does_not_reap_running_ended_session() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("terminal-sidecar-test".to_string()),
                reserve: false,
                cols: 80,
                rows: 24,
            })
            .expect("创建测试终端失败");

        // 模拟「shell 刚死、命令尚未收尾」：执行中的死亡会话不被选终端回收，
        // 留给它自己的收尾路径。
        {
            let mut sessions = service.sessions.lock().expect("会话表锁损坏");
            let session = sessions.get_mut(&spawned.session_id).expect("会话应在表");
            session.exited_code = Some(3);
            session.phase = SessionPhase::Running;
        }
        let acquired = service.acquire_session(AcquireRequest {
            scope_id: "terminal-sidecar-test".to_string(),
        });
        assert!(
            service
                .sessions
                .lock()
                .expect("会话表锁损坏")
                .get(&spawned.session_id)
                .is_some(),
            "执行中的死亡会话不应被选终端回收"
        );
        assert_ne!(
            acquired.session_id.as_deref(),
            Some(spawned.session_id.as_str())
        );
        if let Some(new_session) = acquired.session_id {
            service
                .kill_session(SessionIdRequest {
                    session_id: new_session,
                })
                .expect("清理新终端失败");
        }

        // 空闲后（命令已收尾）：同会话再次选终端时应被回收。
        {
            let mut sessions = service.sessions.lock().expect("会话表锁损坏");
            let session = sessions.get_mut(&spawned.session_id).expect("会话应在表");
            session.phase = SessionPhase::Idle;
        }
        let _ = service.acquire_session(AcquireRequest {
            scope_id: "terminal-sidecar-test".to_string(),
        });
        assert!(
            service
                .sessions
                .lock()
                .expect("会话表锁损坏")
                .get(&spawned.session_id)
                .is_none(),
            "空闲的死亡会话应被选终端回收"
        );
    }

    #[derive(Clone)]
    struct ScopedRequest {
        request: Request,
        context: Option<RequestInvocationContext>,
    }

    impl TerminalService {
        async fn dispatch_test(&self, scoped: ScopedRequest) -> Response {
            self.dispatch_with_context(scoped.request, scoped.context)
                .await
        }
    }

    /// 构造带宿主权威上下文的工具请求（scope 为 None 即缺上下文场景）。
    fn tool_request(
        operation: &str,
        payload: serde_json::Value,
        scope: Option<(&str, &str)>,
    ) -> ScopedRequest {
        let request_id = format!("test-{}", scru128::new());
        ScopedRequest {
            request: Request {
                protocol_version: PROTOCOL_VERSION.to_string(),
                request_id: request_id.clone(),
                operation: operation.to_string(),
                payload,
            },
            context: scope.map(|(session_id, workspace)| RequestInvocationContext {
                session_id: session_id.to_string(),
                invocation_id: request_id,
                workspace: workspace.to_string(),
                actor_id: "test-agent".to_string(),
                deadline_ms: None,
            }),
        }
    }

    fn outcome_of(response: Response) -> serde_json::Value {
        assert!(response.success, "工具调用失败: {response:?}");
        response.payload.expect("成功响应必须带 payload")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 工具调用缺少宿主上下文被拒绝() {
        let service = TerminalService::new();
        let response = service
            .dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "ls"}),
                None,
            ))
            .await;
        assert!(!response.success, "缺上下文必须拒绝: {response:?}");
        let message = response.error_message.unwrap_or_default();
        assert!(message.contains("宿主会话上下文"), "拒绝原因: {message}");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_command_建立会话终端_复用空闲终端() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        std::fs::write(cwd.path().join("marker.txt"), "ok").expect("写测试文件失败");
        let service = TerminalService::new();

        let first = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "ls", "args": []}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        let diagnostic = service
            .sessions
            .lock()
            .unwrap()
            .values()
            .map(|session| String::from_utf8_lossy(&session.raw_history).to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            first["ok"], true,
            "首次执行结果: {first}; raw={diagnostic:?}"
        );
        assert_eq!(first["exit_code"], 0);
        assert_eq!(first["stdout"], "marker.txt");
        // 权威 cwd 生效：命令在宿主注入的会话工作目录执行（shell 回显的
        // 目录即传入的工作目录——逻辑路径原样，不做物理化）。
        assert!(
            first["summary"]
                .as_str()
                .unwrap_or_default()
                .contains(&workspace),
            "summary 应体现权威工作目录: {}",
            first["summary"]
        );
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
        assert!(!terminal.is_empty());
        assert!(
            terminal.starts_with("tty-"),
            "自动创建的终端编号应带 tty- 前缀: {terminal}"
        );

        let second = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "echo", "args": ["again"], "timeout": 10}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(second["ok"], true);
        assert!(
            second["summary"]
                .as_str()
                .unwrap_or_default()
                .contains("复用空闲终端"),
            "第二次应复用空闲终端: {}",
            second["summary"]
        );

        service
            .kill_session(SessionIdRequest {
                session_id: terminal,
            })
            .expect("清理测试终端失败");
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_command_windows_首次执行并复用同一终端() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        for explicit_shell in [None, Some("cmd.exe")] {
            let cwd = tempfile::tempdir().expect("创建 Windows 测试目录失败");
            let workspace = cwd
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .to_string();
            std::fs::write(cwd.path().join("terminal-ready-windows.txt"), "ok")
                .expect("写 Windows 测试文件失败");
            let service = TerminalService::new();
            if let Some(shell) = explicit_shell {
                service
                    .spawn_session(SpawnRequest {
                        session_id: None,
                        cmd: shell.into(),
                        args: vec!["/d".into()],
                        script: None,
                        cwd: Some(workspace.clone()),
                        scope_id: Some("windows-session".into()),
                        reserve: false,
                        cols: 80,
                        rows: 24,
                    })
                    .unwrap();
            }

            let first = outcome_of(
                service
                    .dispatch_test(tool_request(
                        "run_command",
                        serde_json::json!({"cmd": "dir", "args": [], "timeout": 10}),
                        Some(("windows-session", workspace.as_str())),
                    ))
                    .await,
            );
            let diagnostic = service
                .sessions
                .lock()
                .unwrap()
                .values()
                .map(|session| String::from_utf8_lossy(&session.raw_history).to_string())
                .collect::<Vec<_>>();
            assert_eq!(
                first["ok"], true,
                "首次执行结果: {first}; raw={diagnostic:?}"
            );
            assert_eq!(first["exit_code"], 0, "首次执行结果: {first}");
            assert!(
                first["stdout"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("terminal-ready-windows.txt"),
                "首次执行未返回目录内容: {first}"
            );
            let terminal = first["summary"]
                .as_str()
                .and_then(|summary| {
                    summary
                        .split("终端 ")
                        .nth(1)
                        .and_then(|rest| rest.split('（').next())
                })
                .expect("首次执行摘要应含终端编号")
                .to_string();

            let second = outcome_of(
                service
                    .dispatch_test(tool_request(
                        "run_command",
                        serde_json::json!({"cmd": "echo", "args": ["again"], "timeout": 10}),
                        Some(("windows-session", workspace.as_str())),
                    ))
                    .await,
            );
            assert_eq!(second["ok"], true, "第二次执行结果: {second}");
            assert_eq!(second["exit_code"], 0, "第二次执行结果: {second}");
            let raw = service
                .with_session(&terminal, |session| {
                    Ok(String::from_utf8_lossy(&session.raw_history).to_string())
                })
                .unwrap_or_default();
            assert_eq!(
                second["stdout"], "again",
                "第二次执行结果: {second}\nraw={raw:?}"
            );
            assert!(
                second["summary"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("复用空闲终端"),
                "第二次执行未复用终端: {second}"
            );

            let failed = outcome_of(
                service
                    .dispatch_test(tool_request(
                        "run_command",
                        serde_json::json!({"cmd":"cmd.exe","args":["/c","exit","7"],"timeout":5}),
                        Some(("windows-session", workspace.as_str())),
                    ))
                    .await,
            );
            assert_eq!(failed["exit_code"], 7, "失败返回: {failed}");
            assert_eq!(failed["ok"], false, "非零退出状态不能报告工具成功");

            if explicit_shell.is_none() {
                let native_error = outcome_of(
                    service
                        .dispatch_test(tool_request(
                            "run_shell",
                            serde_json::json!({"script": "Write-Error 'bad'", "timeout": 5}),
                            Some(("windows-session", workspace.as_str())),
                        ))
                        .await,
                );
                assert_eq!(
                    native_error["exit_code"], 1,
                    "PowerShell 错误不能沿用上一条退出码: {native_error}"
                );
                assert_eq!(native_error["ok"], false);
                for script in [
                    "if ($LASTEXITCODE -ne 7) { throw 'last exit lost' }; $value = 123; function Get-ReviewValue { $value }; Set-Variable -Name ('__' + 'TIANGONG_RC_VALUE') -Value 'user-owned'; $__TG_COMMAND_BEFORE = 'before-owned'; $__TG_COMMAND_ERRORS = 'errors-owned'",
                    "if ($value -ne 123 -or (Get-ReviewValue) -ne 123) { throw 'scope lost' }; $value += 1",
                    "if ($value -ne 124 -or (Get-ReviewValue) -ne 124) { throw 'state update lost' }; if ((Get-Variable ('__' + 'TIANGONG_RC_VALUE')).Value -ne 'user-owned') { throw 'user variable overwritten' }; if ($__TG_COMMAND_BEFORE -ne 'before-owned' -or $__TG_COMMAND_ERRORS -ne 'errors-owned') { throw 'fixed helper variable overwritten' }; $helpers = @(Get-Variable ('__' + 'TIANGONG_*') | Where-Object { $_.Name -ne ('__' + 'TIANGONG_RC_VALUE') }); if ($helpers.Count -ne 1) { throw ('helper leaked: ' + ($helpers.Name -join ',')) }",
                ] {
                    let result = outcome_of(
                        service
                            .dispatch_test(tool_request(
                                "run_shell",
                                serde_json::json!({"script": script, "timeout": 10}),
                                Some(("windows-session", workspace.as_str())),
                            ))
                            .await,
                    );
                    assert_eq!(
                        result["ok"], true,
                        "PowerShell 状态应在连续命令中保留: {result}"
                    );
                }
                let invalid = outcome_of(
                    service
                        .dispatch_test(tool_request(
                            "run_shell",
                            serde_json::json!({"script": "if (", "timeout": 5}),
                            Some(("windows-session", workspace.as_str())),
                        ))
                        .await,
                );
                assert_eq!(invalid["exit_code"], 1, "语法错误必须及时返回: {invalid}");
                assert_eq!(invalid["ok"], false);
            }
            let script = if explicit_shell.is_some() {
                "echo content>roundtrip.txt & type roundtrip.txt & del roundtrip.txt"
            } else {
                "Set-Content -LiteralPath 'roundtrip.txt' -Value 'content'; Get-Content -LiteralPath 'roundtrip.txt'; Remove-Item -LiteralPath 'roundtrip.txt'"
            };
            let roundtrip = outcome_of(
                service
                    .dispatch_test(tool_request(
                        "run_shell",
                        serde_json::json!({"script": script, "timeout": 10}),
                        Some(("windows-session", workspace.as_str())),
                    ))
                    .await,
            );
            assert_eq!(roundtrip["ok"], true, "文件读写: {roundtrip}");
            assert!(
                roundtrip["stdout"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("content")
            );
            assert!(!cwd.path().join("roundtrip.txt").exists());

            tokio::time::sleep(Duration::from_millis(100)).await;
            let display = service
                .with_session(&terminal, |session| {
                    Ok(String::from_utf8_lossy(&session.display_history).to_string())
                })
                .expect("读取 Windows 终端可见历史失败");
            assert!(display.contains("terminal-ready-windows.txt"));
            assert!(
                !contains_marker(&display),
                "可见输出泄露内部标记: {display:?}"
            );
            service
                .kill_session(SessionIdRequest {
                    session_id: terminal,
                })
                .expect("清理 Windows 测试终端失败");
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_send_同会话输入送达并返回画面() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let service = TerminalService::new();

        let first = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "echo", "args": ["ready"]}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(first["ok"], true);
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

        // 命令结束后 shell 回到提示符：向该终端发送新命令，输入应送达
        // 且返回内容包含命令回显与输出（视图在线走画面快照，离线走
        // 原始输出回显——测试无视图，验证回显路径）。
        let sent = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_send",
                    serde_json::json!({
                        "terminal_id": terminal,
                        "input": "echo sent-ok\r",
                        "wait": 3,
                    }),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(sent["ok"], true, "terminal_send 结果: {sent}");
        assert!(
            sent["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("sent-ok"),
            "回显应包含发送的命令输出: {}",
            sent["stdout"]
        );

        service
            .kill_session(SessionIdRequest {
                session_id: terminal,
            })
            .expect("清理测试终端失败");
    }

    /// 判据必须能区分「交互程序在等按键」与「长命令在安静地跑」。
    ///
    /// 这是本修复的核心安全性质：长时间的编译、测试不能被误判打断。
    /// 用 `sleep` 代表安静的长命令——它占据前台但不碰 tty 模式，
    /// 与 `cargo build` / `pytest` 的 tty 行为一致。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 交互程序与静默长命令的前台判定必须可区分() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let service = TerminalService::new();
        let spawned = service
            .spawn_session(SpawnRequest {
                session_id: None,
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(workspace.clone()),
                scope_id: Some("session-hang".to_string()),
                reserve: false,
                cols: default_cols(),
                rows: default_rows(),
            })
            .expect("创建终端失败");
        let terminal = spawned.session_id;

        let wait_until = |expected: bool| {
            let service = &service;
            let terminal = terminal.clone();
            async move {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if service.foreground_is_interactive(&terminal) == Some(expected) {
                        return true;
                    }
                    if Instant::now() >= deadline {
                        return false;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        };
        let write = |data: &'static [u8]| {
            service
                .with_session(&terminal, |session| {
                    session.writer.write_all(data)?;
                    session.writer.flush()?;
                    Ok(())
                })
                .expect("写入终端失败");
        };

        // 空闲 shell：没有命令在跑。
        assert!(wait_until(false).await, "空闲终端不应判定为交互等待");

        // 安静的长命令：占据前台，但不改 tty 模式 —— 不得判为交互。
        // 这一条挂掉就意味着长时间编译/测试会被错误打断。
        write(b"sleep 30\r");
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            service.foreground_is_interactive(&terminal),
            Some(false),
            "安静的长命令（编译/测试同类）不得被判定为交互等待"
        );

        // 交互程序：切换 tty 为非规范模式逐键读取 —— 应判为交互。
        write(b"\x03");
        assert!(wait_until(false).await, "中断后应回到 shell");
        write(b"python3 -q\r");
        assert!(wait_until(true).await, "交互 REPL 应被判定为正在等待输入");

        write(b"exit()\r");
        let _ = service.kill_session(SessionIdRequest {
            session_id: terminal,
        });
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_open后的唯一编号可继续交互() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let service = TerminalService::new();

        let opened = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_open",
                    serde_json::json!({}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(opened["ok"], true, "terminal_open 结果: {opened}");
        let terminal = opened["summary"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches("已打开终端 ")
            .to_string();
        assert!(
            terminal.starts_with("terminal-"),
            "terminal_open 应返回独立编号: {terminal}"
        );

        // 空闲终端建立后即可按编号发送输入（用户面板的等价路径）。
        let sent = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_send",
                    serde_json::json!({
                        "terminal_id": terminal,
                        "input": "echo opened\r",
                        "wait": 3,
                    }),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(sent["ok"], true, "send 结果: {sent}");
        assert!(
            sent["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("opened"),
            "回显应包含输出: {}",
            sent["stdout"]
        );

        service
            .kill_session(SessionIdRequest {
                session_id: terminal,
            })
            .expect("清理测试终端失败");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 跨会话操作被明确拒绝() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let service = TerminalService::new();

        let first = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "sleep", "args": ["30"], "timeout": 60}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        // 长命令超时返回 ok（命令状态），终端保留运行中。
        assert_eq!(first["ok"], true);
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

        // 会话 B 不得发送输入或关闭 A 的终端。
        let sent = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_send",
                    serde_json::json!({"terminal_id": terminal, "input": "ls"}),
                    Some(("session-b", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(sent["ok"], false, "跨会话 terminal_send 必须拒绝");
        let closed = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_close",
                    serde_json::json!({"terminal_id": terminal}),
                    Some(("session-b", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(closed["ok"], false, "跨会话 terminal_close 必须拒绝");

        // 归属会话关闭正常，且关闭未知终端幂等成功。
        let closed_by_owner = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_close",
                    serde_json::json!({"terminal_id": terminal}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(closed_by_owner["ok"], true);
        let closed_missing = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_close",
                    serde_json::json!({"terminal_id": "tty-not-exist"}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(closed_missing["ok"], true);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 多会话各自持有独立终端() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        std::fs::write(cwd.path().join("shared.txt"), "ok").expect("写测试文件失败");
        let service = TerminalService::new();

        let (a, b) = tokio::join!(
            service.dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "ls"}),
                Some(("session-a", workspace.as_str())),
            )),
            service.dispatch_test(tool_request(
                "run_command",
                serde_json::json!({"cmd": "ls"}),
                Some(("session-b", workspace.as_str())),
            )),
        );
        let (a, b) = (outcome_of(a), outcome_of(b));
        assert_eq!(a["ok"], true);
        assert_eq!(b["ok"], true);
        let terminal_of = |value: &serde_json::Value| {
            value["summary"]
                .as_str()
                .and_then(|summary| {
                    summary
                        .split("终端 ")
                        .nth(1)
                        .and_then(|rest| rest.split('（').next())
                })
                .expect("summary 应含终端编号")
                .to_string()
        };
        let (terminal_a, terminal_b) = (terminal_of(&a), terminal_of(&b));
        assert_ne!(terminal_a, terminal_b, "两个会话必须各建独立终端");

        // 关闭 A 的终端不影响 B 的终端继续执行。
        let closed = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_close",
                    serde_json::json!({"terminal_id": terminal_a}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(closed["ok"], true);
        let b_again = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "echo", "args": ["still"]}),
                    Some(("session-b", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(b_again["ok"], true);
        assert!(
            b_again["summary"]
                .as_str()
                .unwrap_or_default()
                .contains("复用空闲终端"),
            "B 的终端不受 A 关闭影响: {}",
            b_again["summary"]
        );

        service
            .kill_session(SessionIdRequest {
                session_id: terminal_b,
            })
            .expect("清理测试终端失败");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_open_返回独立编号终端() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let service = TerminalService::new();
        let opened = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_open",
                    serde_json::json!({}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(opened["ok"], true, "terminal_open 结果: {opened}");
        let terminal = opened["summary"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches("已打开终端 ")
            .to_string();
        assert!(
            terminal.starts_with("terminal-"),
            "terminal_open 应返回独立编号: {terminal}"
        );
        service
            .kill_session(SessionIdRequest {
                session_id: terminal,
            })
            .expect("清理测试终端失败");
    }

    /// 取消链路：宿主超时/会话取消后 Cancel 帧到达，dispatch future 已被
    /// runner 丢弃——cancel 钩子必须把 Ctrl+C 落到执行中的终端并复位
    /// phase，命令不脱离调用方无限运行、终端可被后续调用复用。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 取消中断执行中的命令并复位终端() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let service = TerminalService::new();

        // 长命令在独立 task 中执行（对齐 runner：dispatch 由任务持有）。
        let service_for_task = std::sync::Arc::new(service);
        let request = tool_request(
            "run_command",
            serde_json::json!({"cmd": "sleep", "args": ["60"]}),
            Some(("session-a", workspace.as_str())),
        );
        let task_service = std::sync::Arc::clone(&service_for_task);
        let task_request = request.clone();
        let running = tokio::spawn(async move {
            task_service.dispatch_test(task_request).await;
        });
        // 先等待取消目标登记并取得实际终端编号。
        let terminal = {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(active) = service_for_task.active_tools.lock()
                    && let Some((_, active)) = active.iter().next()
                {
                    break active.terminal_id.clone();
                }
                assert!(Instant::now() < deadline, "长命令应登记为活动工具调用");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };

        // 登记发生在前端附着等待之前；继续等到 PTY 已输出命令起始标记，
        // 确保 sleep 长命令确实开始执行后再模拟宿主终止 future。
        let command_start_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let started = service_for_task
                .with_session(&terminal, |session| {
                    Ok(session.phase == SessionPhase::Running
                        && String::from_utf8_lossy(&session.raw_history)
                            .contains("__TIANGONG_START_"))
                })
                .unwrap_or(false);
            if started {
                break;
            }
            assert!(
                Instant::now() < command_start_deadline,
                "长命令应在取消前进入 PTY 执行"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // 模拟生产顺序：stdio runner 先触发取消信号并丢弃 dispatch
        // future，随后才调用服务的 cancel 钩子。
        let request_id = request.request.request_id.clone();
        running.abort();
        let aborted = running.await.expect_err("执行任务应被取消");
        assert!(aborted.is_cancelled(), "执行任务应先于 cancel 钩子终止");
        assert_eq!(
            service_for_task
                .active_tools
                .lock()
                .expect("活动工具表锁损坏")
                .get(&request_id)
                .map(|active| active.terminal_id.clone()),
            Some(terminal.clone()),
            "执行 future 销毁后必须保留取消目标"
        );

        service_for_task
            .cancel(&request.request)
            .await
            .expect("取消钩子不应失败");
        assert!(
            !service_for_task
                .active_tools
                .lock()
                .expect("活动工具表锁损坏")
                .contains_key(&request_id),
            "cancel 完成后应清除取消目标"
        );

        // 终端 phase 已复位：后续调用可立即复用该终端并成功执行。
        let after = outcome_of(
            service_for_task
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "echo", "args": ["recovered"], "timeout": 5}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(after["ok"], true, "取消后终端应可复用: {after}");
        assert_eq!(after["stdout"], "recovered");
        assert!(
            after["summary"]
                .as_str()
                .unwrap_or_default()
                .contains("复用空闲终端"),
            "取消后应复用被中断的终端: {}",
            after["summary"]
        );
        assert!(
            service_for_task.with_session(&terminal, |_| Ok(())).is_ok(),
            "终端应存活（取消中断命令而非杀死终端）"
        );

        service_for_task
            .kill_session(SessionIdRequest {
                session_id: terminal,
            })
            .expect("清理测试终端失败");
    }

    /// 测试辅助：起一条长命令并在其真正进入 PTY 后模拟 runner 丢弃
    /// dispatch future，返回（服务, 执行终端编号, 原请求）。
    #[cfg(unix)]
    async fn 取消执行中的调用(
        payload: serde_json::Value,
        operation: &str,
        workspace: &str,
    ) -> (std::sync::Arc<TerminalService>, String, ScopedRequest) {
        let service = std::sync::Arc::new(TerminalService::new());
        let request = tool_request(operation, payload, Some(("session-a", workspace)));
        let task_service = std::sync::Arc::clone(&service);
        let task_request = request.clone();
        let running = tokio::spawn(async move {
            task_service.dispatch_test(task_request).await;
        });
        let terminal = {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(active) = service.active_tools.lock()
                    && let Some((_, active)) = active.iter().next()
                {
                    break active.terminal_id.clone();
                }
                assert!(Instant::now() < deadline, "长命令应登记为活动工具调用");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        // 等到命令真正进入 PTY（交互命令没有 start marker，退而求其次等
        // 到 phase 进入执行态且有输出产生）。
        let started_deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let started = service
                .with_session(&terminal, |session| {
                    let raw = String::from_utf8_lossy(&session.raw_history).to_string();
                    Ok(session.phase == SessionPhase::Running
                        && (raw.contains("__TIANGONG_START_") || raw.contains("READY_TO_CANCEL")))
                })
                .unwrap_or(false);
            if started {
                break;
            }
            assert!(
                Instant::now() < started_deadline,
                "长命令应在取消前进入 PTY"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        running.abort();
        let aborted = running.await.expect_err("执行任务应被取消");
        assert!(aborted.is_cancelled(), "执行任务应先于 cancel 钩子终止");
        (service, terminal, request)
    }

    /// issue #552 主场景：被取消的命令带子进程树，SIGINT 送达后收尾需要
    /// 时间。cancel 必须确认 shell 真正回到提示符才放回复用池，否则下一条
    /// 命令会在未收尾的终端上握手、探针进入残留进程 stdin 而永不回显。
    ///
    /// 这里用 trap 让 SIGINT 后仍占用前台若干秒，精确复现「账面空闲、实际
    /// 仍忙」的窗口；验收点是下一条命令必须正常执行，而不是超时失败。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 取消收尾缓慢的命令后下一条命令仍能正常执行() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        // SIGINT 后还要占用前台 4 秒（> SHELL_READY_TIMEOUT_SECS），
        // 模拟 cargo nextest 之类带子进程树的长任务收尾。
        let (service, terminal, request) = 取消执行中的调用(
            serde_json::json!({
                "script": "trap 'sleep 4; exit 130' INT; echo READY_TO_CANCEL; sleep 60",
            }),
            "run_shell",
            &workspace,
        )
        .await;

        service
            .cancel(&request.request)
            .await
            .expect("取消钩子不应失败");

        // 未就绪终端绝不能以 Idle 放回池——那正是 issue 的根因。
        let phase = service
            .with_session(&terminal, |session| Ok(session.phase))
            .expect("终端应保留");
        assert_ne!(
            phase,
            SessionPhase::Idle,
            "shell 未确认回到提示符时不得放回复用池"
        );

        // 下一条命令必须成功：要么复用已确认就绪的终端，要么自动换新终端。
        let after = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "echo", "args": ["recovered"], "timeout": 10}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(after["ok"], true, "取消后下一条命令必须可执行: {after}");
        assert_eq!(after["stdout"], "recovered", "结果: {after}");

        // 先收集再循环：for 头部表达式里的锁守卫会活到整个循环结束，
        // 循环体内的 kill_session 将重入同一把非重入锁，自死锁。
        let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
        for session_id in session_ids {
            let _ = service.kill_session(SessionIdRequest { session_id });
        }
    }

    /// 取消不得杀终端：`&` / `nohup` 起的后台作业必须活下来。
    ///
    /// 终端被 kill 会给整个进程组发 SIGHUP，用户特意放到后台长期运行的
    /// 进程会被一起带走。就绪确认失败只允许把终端移出复用池，PTY 本身
    /// 必须保留（用户仍可查看输出、继续输入）。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 取消不得杀死终端或其后台作业() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let flag = cwd.path().join("nohup-alive.txt");
        let flag_path = flag.to_string_lossy().to_string();
        // 后台作业：持续写标记文件；前台故意忽略 SIGINT 撑过就绪预算，
        // 逼出「确认失败」这条最危险的分支。
        let script = format!(
            "nohup sh -c 'while true; do echo alive >> {flag_path}; sleep 0.2; done' \
             >/dev/null 2>&1 &\n\
             trap '' INT\n\
             echo READY_TO_CANCEL\n\
             sleep 60\n"
        );
        let (service, terminal, request) = 取消执行中的调用(
            serde_json::json!({ "script": script }),
            "run_shell",
            &workspace,
        )
        .await;

        service
            .cancel(&request.request)
            .await
            .expect("取消钩子不应失败");

        // 终端必须存活：这是后台作业与用户可见输出的载体。
        assert!(
            service.with_session(&terminal, |_| Ok(())).is_ok(),
            "就绪确认失败不得关闭终端（会连带 SIGHUP 杀掉 nohup/& 后台作业）"
        );
        assert!(
            service
                .with_session(&terminal, |session| Ok(session.exited_code))
                .expect("终端应保留")
                .is_none(),
            "终端 shell 不应被终止"
        );

        // 后台作业仍在持续写入：取消只放弃本次工具等待，不影响长期进程。
        let before = std::fs::metadata(&flag).map(|meta| meta.len()).unwrap_or(0);
        assert!(before > 0, "后台作业应已开始写入标记文件");
        tokio::time::sleep(Duration::from_millis(800)).await;
        let after = std::fs::metadata(&flag).map(|meta| meta.len()).unwrap_or(0);
        assert!(
            after > before,
            "nohup 后台作业必须在取消后继续运行: before={before} after={after}"
        );

        // 但该终端不再被自动复用，下一条命令换用新终端并正常返回。
        let next = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "echo", "args": ["fresh"], "timeout": 10}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(next["ok"], true, "应换用新终端执行: {next}");
        assert_eq!(next["stdout"], "fresh");
        assert!(
            !next["summary"]
                .as_str()
                .unwrap_or_default()
                .contains(&terminal),
            "未回到提示符的终端不得被复用: {}",
            next["summary"]
        );

        // 先收集再循环：for 头部表达式里的锁守卫会活到整个循环结束，
        // 循环体内的 kill_session 将重入同一把非重入锁，自死锁。
        let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
        for session_id in session_ids {
            let _ = service.kill_session(SessionIdRequest { session_id });
        }
    }

    /// 前台长任务收尾后，终端应自行回到复用池，而不是永久闲置。
    ///
    /// 就绪确认超时时遗留在输入缓冲里的探针会在前台进程退出后被 shell
    /// 执行并回显，选终端据此识别恢复。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 收尾完成的终端重新回到复用池() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        // SIGINT 后占用前台 5 秒（超过就绪预算）。trap 内不得 exit——那会
        // 退出登录 shell 本身，shell 死后无人执行排队的探针，恢复检测
        // 永不命中；只睡眠，收尾后 shell 回提示符执行遗留探针。
        let (service, terminal, request) = 取消执行中的调用(
            serde_json::json!({
                "script": "trap 'sleep 5' INT; echo READY_TO_CANCEL; sleep 60",
            }),
            "run_shell",
            &workspace,
        )
        .await;
        service
            .cancel(&request.request)
            .await
            .expect("取消钩子不应失败");
        assert_eq!(
            service
                .with_session(&terminal, |session| Ok(session.phase))
                .expect("终端应保留"),
            SessionPhase::Unresponsive,
            "确认失败的终端应标记为不可自动复用"
        );

        // 等待前台命令自行收尾，遗留探针被 shell 执行。
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let acquired = service.acquire_session(AcquireRequest {
                scope_id: "session-a".to_string(),
            });
            if acquired.session_id.as_deref() == Some(terminal.as_str()) {
                break;
            }
            // 未选中则把预留状态还原，避免影响下一轮判断。
            if let Some(session_id) = acquired.session_id {
                let _ = service.release_session(SessionIdRequest { session_id });
            }
            assert!(
                Instant::now() < deadline,
                "前台命令收尾后终端应重新进入复用池"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let _ = service.release_session(SessionIdRequest {
            session_id: terminal.clone(),
        });
        let after = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "echo", "args": ["reusable"], "timeout": 10}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(after["ok"], true, "恢复后的终端应能正常执行: {after}");
        assert_eq!(after["stdout"], "reusable");

        // 先收集再循环：for 头部表达式里的锁守卫会活到整个循环结束，
        // 循环体内的 kill_session 将重入同一把非重入锁，自死锁。
        let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
        for session_id in session_ids {
            let _ = service.kill_session(SessionIdRequest { session_id });
        }
    }

    /// 取消交互调用（vi/ssh）不得动用户的终端。
    ///
    /// 执行期间 phase 还是 Running（只有返回结果后才置 Interactive），
    /// 所以取消判据必须带上本次调用的 interactive 入参，否则调用自己
    /// 拉起的交互程序会走中断确认流：探针进不了 vi，超时后整个终端被
    /// 处置，用户正在编辑的内容随之丢失。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 取消交互调用保留用户终端且不发中断() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let (service, terminal, request) = 取消执行中的调用(
            serde_json::json!({
                "script": "echo READY_TO_CANCEL; cat",
                "interactive": true,
            }),
            "run_shell",
            &workspace,
        )
        .await;

        let before = service
            .with_session(&terminal, |session| Ok(session.raw_bytes_total))
            .expect("终端应存活");
        service
            .cancel(&request.request)
            .await
            .expect("取消钩子不应失败");

        assert!(
            service.with_session(&terminal, |_| Ok(())).is_ok(),
            "交互终端必须保留给用户继续使用"
        );
        // 程序已启动（cat 在前台）：取消后状态必须收尾为交互中且命令
        // 保留，不得滞留 Running（收尾 future 已丢弃，不会再有人置态）。
        let (phase, command) = service
            .with_session(&terminal, |session| {
                Ok((session.phase, session.running_command.clone()))
            })
            .expect("终端应存活");
        assert_eq!(phase, SessionPhase::Interactive, "取消后应转交互状态");
        assert!(
            command
                .as_deref()
                .is_some_and(|command| command.contains("cat")),
            "交互中的终端应保留命令供 terminal_status 查询: {command:?}"
        );
        // 不发 Ctrl+C、不发探针：取消后终端不应因中断确认流产生新输出。
        let after = service
            .with_session(&terminal, |session| Ok(session.raw_bytes_total))
            .expect("终端应存活");
        assert_eq!(
            before,
            after,
            "取消交互调用不应向终端写入中断或探针（新增输出 {} 字节）",
            after.saturating_sub(before)
        );
        // before == after 已覆盖「不发 Ctrl+C、不发探针」：任一写入都会
        // 产生新回显字节。不再检查整段历史——首次命令前的就绪握手
        // 合法地发过一次探针，其回显留在历史里属预期。

        // 先收集再循环：for 头部表达式里的锁守卫会活到整个循环结束，
        // 循环体内的 kill_session 将重入同一把非重入锁，自死锁。
        let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
        for session_id in session_ids {
            let _ = service.kill_session(SessionIdRequest { session_id });
        }
    }

    /// 取消发生在命令启动前（前端附着等待阶段）：终端尚未收到任何输入，
    /// 取消必须直接释放预留回到空闲——否则预留永远挂着，终端不再被复用。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 取消发生在命令启动前时终端回到空闲() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let service = std::sync::Arc::new(TerminalService::new());
        let request = tool_request(
            "run_shell",
            serde_json::json!({ "script": "echo never-runs", "interactive": true }),
            Some(("session-a", workspace.as_str())),
        );
        let task_service = std::sync::Arc::clone(&service);
        let task_request = request.clone();
        let running = tokio::spawn(async move {
            task_service.dispatch_test(task_request).await;
        });
        // 登记发生在前端附着等待之前：拿到终端编号时它必处于 Reserved
        // （命令尚未写入 PTY），立即取消即命中「启动前取消」窗口。
        let terminal = {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(active) = service.active_tools.lock()
                    && let Some((_, active)) = active.iter().next()
                {
                    break active.terminal_id.clone();
                }
                assert!(Instant::now() < deadline, "调用应登记为活动工具");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        running.abort();
        let aborted = running.await.expect_err("执行任务应被取消");
        assert!(aborted.is_cancelled());
        service
            .cancel(&request.request)
            .await
            .expect("取消钩子不应失败");

        let phase = service
            .with_session(&terminal, |session| Ok(session.phase))
            .expect("终端应存活");
        assert_eq!(phase, SessionPhase::Idle, "启动前取消必须释放预留回到空闲");

        // 后续命令应能复用该终端（它干净且空闲）。
        let after = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "echo", "args": ["reused"], "timeout": 10}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(after["ok"], true, "{after}");
        assert_eq!(after["stdout"], "reused");
        assert!(
            after["summary"]
                .as_str()
                .unwrap_or_default()
                .contains("复用空闲终端"),
            "应复用被释放的终端: {}",
            after["summary"]
        );

        // 先收集再循环：避免 for 头部锁守卫跨循环体重入自死锁。
        let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
        for session_id in session_ids {
            let _ = service.kill_session(SessionIdRequest { session_id });
        }
    }

    /// 命令超时但忽略 SIGINT 仍在前台运行：超时返回后终端必须标记为
    /// 不可自动复用并保留命令（状态查询不得错报空闲、丢命令），下一条
    /// 命令换新终端执行；前台收尾后遗留探针回显，终端自动恢复回池。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 超时未收尾的终端不得错报空闲() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let service = TerminalService::new();

        let timed = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({
                        "cmd": "bash",
                        "args": ["-c", "trap '' INT; echo RUNNING; sleep 6"],
                        "timeout": 2,
                    }),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(timed["ok"], false, "超时命令应返回失败: {timed}");

        // 超时返回时命令仍在跑：状态必须是不可复用且命令可见。
        let status = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_status",
                    serde_json::json!({}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        let overview: serde_json::Value =
            serde_json::from_str(status["stdout"].as_str().expect("概要应有 stdout"))
                .expect("概要应为 JSON");
        let stale = overview["terminals"]
            .as_array()
            .expect("概要应为数组")
            .iter()
            .find(|terminal| terminal["status"] == "unresponsive")
            .expect("未收尾终端应标记为不可复用")
            .clone();
        let stale_id = stale["terminal_id"].as_str().expect("应有编号").to_string();
        assert!(
            stale["command"]
                .as_str()
                .is_some_and(|command| command.contains("sleep 6")),
            "未收尾终端应保留命令: {stale}"
        );

        // 下一条命令换新终端正常执行，不撞仍在跑的前台命令。
        let next = outcome_of(
            service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "echo", "args": ["fresh"], "timeout": 10}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(next["ok"], true, "应换新终端执行: {next}");
        assert_eq!(next["stdout"], "fresh");
        assert!(
            !next["summary"]
                .as_str()
                .unwrap_or_default()
                .contains(&stale_id),
            "未收尾终端不得被复用: {}",
            next["summary"]
        );

        // 前台命令自行收尾（sleep 6 结束）后，遗留探针被 shell 执行回显；
        // 恢复检测在选终端时进行，经 acquire 轮询驱动。
        let recovered_deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let acquired = service.acquire_session(AcquireRequest {
                scope_id: "session-a".to_string(),
            });
            if let Some(selected) = acquired.session_id {
                let recovered = selected == stale_id;
                let _ = service.release_session(SessionIdRequest {
                    session_id: selected,
                });
                if recovered {
                    break;
                }
            }
            assert!(
                Instant::now() < recovered_deadline,
                "前台收尾后终端应自动恢复回池"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
        }

        // 先收集再循环：避免 for 头部锁守卫跨循环体重入自死锁。
        let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
        for session_id in session_ids {
            let _ = service.kill_session(SessionIdRequest { session_id });
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_send_close_缺少terminal_id明确失败() {
        let service = TerminalService::new();
        let scope = Some(("session-a", "/tmp"));
        let sent = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_send",
                    serde_json::json!({"input": "ls"}),
                    scope,
                ))
                .await,
        );
        assert_eq!(sent["ok"], false);
        assert!(
            sent["summary"]
                .as_str()
                .unwrap_or_default()
                .contains("terminal_id")
        );
        let closed = outcome_of(
            service
                .dispatch_test(tool_request("terminal_close", serde_json::json!({}), scope))
                .await,
        );
        assert_eq!(closed["ok"], false);
    }

    /// 宿主实例移除兜底（instanceClosed）：页面未就绪时由宿主补发；
    /// 幂等（重复/未知编号成功）、跨会话拒绝，与 terminal_close 一致。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn instance_closed_兜底回收幂等且校验归属() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let service = TerminalService::new();

        // 模拟手动新建标签：终端编号由调用方指定（tab 编号）。
        let opened = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_open",
                    serde_json::json!({}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        let terminal = opened["summary"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches("已打开终端 ")
            .to_string();

        // 实例通知不携带工具上下文（宿主生命周期操作）：
        // session_id=宿主会话，instance_id=标签（终端）编号。
        let host_notice = |scope_id: &str, instance_id: &str| Request {
            protocol_version: PROTOCOL_VERSION.to_string(),
            request_id: format!("test-{}", scru128::new()),
            operation: "instanceClosed".to_string(),
            payload: serde_json::json!({
                "session_id": scope_id,
                "instance_id": instance_id,
            }),
        };
        let list_instances = |scope_id: &str| Request {
            protocol_version: PROTOCOL_VERSION.to_string(),
            request_id: format!("test-{}", scru128::new()),
            operation: "listInstances".to_string(),
            payload: serde_json::json!({ "session_id": scope_id }),
        };
        let listed = service.dispatch(list_instances("session-a")).await;
        assert!(listed.success, "listInstances 应成功: {listed:?}");
        assert_eq!(
            listed
                .payload
                .as_ref()
                .and_then(|p| p["instances"][0]["instance_id"].as_str()),
            Some(terminal.as_str()),
            "listInstances 应返回会话下存活终端"
        );
        // 跨会话兜底通知：拒绝（终端不属于该会话）。
        let denied = service.dispatch(host_notice("session-b", &terminal)).await;
        assert!(!denied.success, "跨会话兜底必须拒绝: {denied:?}");

        // 归属会话：回收成功；重复补发与未知编号幂等成功。
        let closed = service.dispatch(host_notice("session-a", &terminal)).await;
        assert!(closed.success, "归属会话兜底应成功: {closed:?}");
        assert!(
            service.with_session(&terminal, |_| Ok(())).is_err(),
            "终端应已被兜底回收"
        );
        let again = service.dispatch(host_notice("session-a", &terminal)).await;
        assert!(again.success, "重复兜底应幂等成功");
        let unknown = service
            .dispatch(host_notice("session-a", "tty-not-exist"))
            .await;
        assert!(unknown.success, "未知编号兜底应幂等成功");
        let emptied = service.dispatch(list_instances("session-a")).await;
        assert_eq!(
            emptied
                .payload
                .as_ref()
                .and_then(|p| p["instances"].as_array().map(Vec::len)),
            Some(0)
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_command_等待前端精确附着后才执行() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let service = Arc::new(TerminalService::new());
        let running_service = Arc::clone(&service);
        let running = tokio::spawn(async move {
            running_service
                .dispatch_test(tool_request(
                    "run_command",
                    serde_json::json!({"cmd": "printf", "args": ["attached"]}),
                    Some(("session-attached", workspace.as_str())),
                ))
                .await
        });

        let terminal_id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(terminal_id) = service
                    .sessions
                    .lock()
                    .expect("会话表锁损坏")
                    .keys()
                    .next()
                    .cloned()
                {
                    break terminal_id;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("等待 Agent 终端创建超时");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!running.is_finished(), "前端尚未附着时命令不应开始并完成");

        let found = service.find_by_scope(&FindRequest {
            scope_id: "session-attached".to_string(),
            session_id: Some(terminal_id.clone()),
        });
        assert_eq!(found.session_id.as_deref(), Some(terminal_id.as_str()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !running.is_finished(),
            "仅查到 PTY 不能代替前端完成订阅与附着"
        );
        service
            .attach_frontend(TerminalBindingRequest {
                scope_id: "session-attached".to_string(),
                session_id: terminal_id.clone(),
            })
            .expect("确认前端附着失败");

        let response = tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .expect("前端附着后命令未完成")
            .expect("命令任务异常");
        let outcome = outcome_of(response);
        assert_eq!(outcome["stdout"], "attached");
        service
            .kill_session(SessionIdRequest {
                session_id: terminal_id,
            })
            .expect("清理测试终端失败");
    }

    #[cfg(unix)]
    #[test]
    fn terminal_list_by_scope_只列同会话存活终端并按创建序() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawn = |session_id: &str, scope_id: &str| SpawnRequest {
            session_id: Some(session_id.to_string()),
            cmd: String::new(),
            args: Vec::new(),
            script: None,
            cwd: Some(cwd.path().to_string_lossy().to_string()),
            scope_id: Some(scope_id.to_string()),
            reserve: false,
            cols: default_cols(),
            rows: default_rows(),
        };
        service
            .spawn_session(spawn("terminal-first", "session-list"))
            .unwrap();
        service
            .spawn_session(spawn("terminal-second", "session-list"))
            .unwrap();
        service
            .spawn_session(spawn("terminal-other", "session-b"))
            .unwrap();
        service
            .spawn_session(spawn("terminal-dead", "session-list"))
            .unwrap();
        // 死亡未出表：shell 已退出但等命令收尾，列表不得恢复其标签。
        service
            .with_session("terminal-dead", |session| {
                session.exited_code = Some(-1);
                Ok(())
            })
            .unwrap();

        let listed = service
            .list_by_scope(&ListByScopeRequest {
                scope_id: "session-list".to_string(),
            })
            .expect("terminalListByScope 查询失败");
        let ids: Vec<&str> = listed
            .terminals
            .iter()
            .map(|terminal| terminal.session_id.as_str())
            .collect();
        assert_eq!(ids, vec!["terminal-first", "terminal-second"]);

        // 前端经 bridgeCall 走 dispatch 通道，协议形状一并验证。
        let response = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("创建测试 runtime 失败")
            .block_on(service.dispatch(Request::new(
                "terminalListByScope",
                serde_json::json!({ "scope_id": "session-list" }),
            )));
        assert!(
            response.success,
            "terminalListByScope dispatch 失败: {response:?}"
        );
        assert_eq!(
            serde_json::to_value(&listed).expect("序列化失败"),
            response.payload.expect("dispatch 成功响应缺少 payload"),
        );

        for terminal in [
            "terminal-first",
            "terminal-second",
            "terminal-other",
            "terminal-dead",
        ] {
            service
                .kill_session(SessionIdRequest {
                    session_id: terminal.to_string(),
                })
                .unwrap();
        }
    }

    /// terminal_status：概要（仅本会话、含已退出终端与状态映射、按创建
    /// 序）与详情（含最近输出尾部）；跨会话与不存在终端明确失败。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_status_概要详情与跨会话拒绝() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let workspace = cwd.path().to_string_lossy().to_string();
        let service = TerminalService::new();
        let spawn = |terminal_id: &str, scope_id: &str| SpawnRequest {
            session_id: Some(terminal_id.to_string()),
            cmd: String::new(),
            args: Vec::new(),
            script: None,
            cwd: Some(workspace.clone()),
            scope_id: Some(scope_id.to_string()),
            reserve: false,
            cols: default_cols(),
            rows: default_rows(),
        };
        service
            .spawn_session(spawn("status-idle", "session-a"))
            .unwrap();
        service
            .spawn_session(spawn("status-busy", "session-a"))
            .unwrap();
        service
            .spawn_session(spawn("status-other", "session-b"))
            .unwrap();
        service
            .spawn_session(spawn("status-dead", "session-a"))
            .unwrap();
        // 覆盖状态映射：交互中（带命令）与已退出（待回收）。
        service
            .with_session("status-busy", |session| {
                session.phase = SessionPhase::Interactive;
                session.running_command = Some("vi notes.txt".to_string());
                Ok(())
            })
            .unwrap();
        service
            .with_session("status-dead", |session| {
                session.exited_code = Some(130);
                Ok(())
            })
            .unwrap();

        let listed = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_status",
                    serde_json::json!({}),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(listed["ok"], true, "概要查询失败: {listed}");
        let overview: serde_json::Value =
            serde_json::from_str(listed["stdout"].as_str().expect("概要应有 stdout"))
                .expect("概要 stdout 应为 JSON");
        let terminals = overview["terminals"].as_array().expect("概要应为数组");
        let ids: Vec<&str> = terminals
            .iter()
            .map(|terminal| terminal["terminal_id"].as_str().expect("应有编号"))
            .collect();
        assert_eq!(
            ids,
            vec!["status-idle", "status-busy", "status-dead"],
            "概要应仅含本会话终端（含已退出）并按创建序"
        );
        let status_of = |id: &str| {
            terminals
                .iter()
                .find(|terminal| terminal["terminal_id"] == id)
                .and_then(|terminal| terminal["status"].as_str())
                .unwrap_or_default()
        };
        assert_eq!(status_of("status-idle"), "idle");
        assert_eq!(status_of("status-busy"), "interactive");
        assert_eq!(status_of("status-dead"), "exited");
        let command_of = |id: &str| {
            terminals
                .iter()
                .find(|terminal| terminal["terminal_id"] == id)
                .and_then(|terminal| terminal["command"].as_str())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(
            command_of("status-busy"),
            "vi notes.txt",
            "执行中/交互中的终端应带正在执行的命令"
        );
        assert_eq!(
            command_of("status-idle"),
            "",
            "空闲终端不应残留命令（null 序列化为空断言兜底）"
        );
        assert_eq!(
            terminals
                .iter()
                .find(|terminal| terminal["terminal_id"] == "status-dead")
                .and_then(|terminal| terminal["exited_code"].as_i64()),
            Some(130)
        );

        let detail = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_status",
                    serde_json::json!({ "terminal_id": "status-idle" }),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(detail["ok"], true, "详情查询失败: {detail}");
        let detail_payload: serde_json::Value =
            serde_json::from_str(detail["stdout"].as_str().expect("详情应有 stdout"))
                .expect("详情 stdout 应为 JSON");
        assert_eq!(detail_payload["status"], "idle");
        assert!(
            detail_payload.get("recent_output").is_some(),
            "详情应含最近输出字段: {detail_payload}"
        );

        // 跨会话与不存在终端：明确失败，与其他工具的归属语义一致。
        let denied = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_status",
                    serde_json::json!({ "terminal_id": "status-idle" }),
                    Some(("session-b", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(denied["ok"], false, "跨会话查询应被拒绝: {denied}");
        assert!(
            denied["summary"]
                .as_str()
                .unwrap_or_default()
                .contains("不属于当前会话"),
            "拒绝原因应明示归属: {}",
            denied["summary"]
        );
        let missing = outcome_of(
            service
                .dispatch_test(tool_request(
                    "terminal_status",
                    serde_json::json!({ "terminal_id": "status-none" }),
                    Some(("session-a", workspace.as_str())),
                ))
                .await,
        );
        assert_eq!(missing["ok"], false, "查询不存在的终端应失败: {missing}");

        // 先收集再循环：for 头部表达式里的锁守卫会活到整个循环结束。
        let session_ids: Vec<String> = service.sessions.lock().unwrap().keys().cloned().collect();
        for session_id in session_ids {
            let _ = service.kill_session(SessionIdRequest { session_id });
        }
    }

    #[cfg(unix)]
    #[test]
    fn terminal_find_新编号不继承其他终端输出() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let old = service
            .spawn_session(SpawnRequest {
                session_id: Some("terminal-old".to_string()),
                cmd: String::new(),
                args: Vec::new(),
                script: None,
                cwd: Some(cwd.path().to_string_lossy().to_string()),
                scope_id: Some("session-fresh".to_string()),
                reserve: false,
                cols: default_cols(),
                rows: default_rows(),
            })
            .expect("创建旧终端失败");
        record_display_output(&service.sessions, &old.session_id, b"old agent output\r\n");

        let found = service.find_by_scope(&FindRequest {
            scope_id: "session-fresh".to_string(),
            session_id: Some("terminal-new".to_string()),
        });
        assert!(found.session_id.is_none());
        assert!(found.history.is_empty(), "新终端不得继承旧终端输出");
        service
            .kill_session(SessionIdRequest {
                session_id: old.session_id,
            })
            .expect("清理旧终端失败");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_gc_按前端存活集合回收且不跨会话() {
        let cwd = tempfile::tempdir().expect("创建测试目录失败");
        let service = TerminalService::new();
        let spawn = |session_id: &str, scope_id: &str| SpawnRequest {
            session_id: Some(session_id.to_string()),
            cmd: String::new(),
            args: Vec::new(),
            script: None,
            cwd: Some(cwd.path().to_string_lossy().to_string()),
            scope_id: Some(scope_id.to_string()),
            reserve: false,
            cols: default_cols(),
            rows: default_rows(),
        };
        service
            .spawn_session(spawn("terminal-live", "session-a"))
            .unwrap();
        service
            .spawn_session(spawn("terminal-closed", "session-a"))
            .unwrap();
        service
            .spawn_session(spawn("terminal-other-scope", "session-b"))
            .unwrap();
        service
            .spawn_session(spawn("terminal-closed-before-gc", "session-a"))
            .unwrap();
        service
            .attach_frontend(TerminalBindingRequest {
                scope_id: "session-a".to_string(),
                session_id: "terminal-closed-before-gc".to_string(),
            })
            .unwrap();

        let initial = service
            .dispatch(Request::new(
                "terminalGc",
                serde_json::json!({
                    "session_id": "session-a",
                    "live_terminal_ids": ["terminal-live", "terminal-closed"],
                }),
            ))
            .await;
        assert!(initial.success, "首次 Terminal GC 对账失败: {initial:?}");
        assert!(
            service
                .with_session("terminal-closed-before-gc", |_| Ok(()))
                .is_err(),
            "已附着终端即使在首次定时对账前关闭也必须被回收"
        );
        service
            .spawn_session(spawn("terminal-mounting", "session-a"))
            .unwrap();
        service
            .with_session("terminal-closed", |session| {
                session.phase = SessionPhase::Running;
                Ok(())
            })
            .unwrap();
        service.active_tools.lock().unwrap().insert(
            "request-a".to_string(),
            ActiveTool {
                terminal_id: "terminal-closed".to_string(),
                interactive: false,
            },
        );

        let response = service
            .dispatch(Request::new(
                "terminalGc",
                serde_json::json!({
                    "session_id": "session-a",
                    "live_terminal_ids": ["terminal-live"],
                }),
            ))
            .await;
        assert!(response.success, "Terminal GC 对账失败: {response:?}");

        assert!(service.with_session("terminal-live", |_| Ok(())).is_ok());
        assert!(service.with_session("terminal-closed", |_| Ok(())).is_err());
        assert!(
            service
                .with_session("terminal-mounting", |_| Ok(()))
                .is_ok(),
            "尚未完成前端挂载的新终端不得被局部存活集合误回收"
        );
        assert!(
            service
                .with_session("terminal-other-scope", |_| Ok(()))
                .is_ok(),
            "GC 不得影响其他会话"
        );
        assert!(service.active_tools.lock().unwrap().is_empty());
        service
            .kill_session(SessionIdRequest {
                session_id: "terminal-live".to_string(),
            })
            .unwrap();
        service
            .kill_session(SessionIdRequest {
                session_id: "terminal-other-scope".to_string(),
            })
            .unwrap();
        service
            .kill_session(SessionIdRequest {
                session_id: "terminal-mounting".to_string(),
            })
            .unwrap();
    }
}
