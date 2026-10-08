//! CLI Adapter 执行引擎：managed JSONL 子进程的启动、注入、中断与终止。
//!
//! 协议约定（首版 CLI 后端）：
//! - sidecar 以 `sh -c <command>`（Windows `cmd /C`）启动子进程，cwd 为绑定的
//!   Workspace（或独立 worktree），Unix 下 setsid 自立进程组；
//! - stdin 每行一个 JSON：`begin` → 后续 `user_message` / `interrupt` / `cancel`，
//!   `begin` 与 `user_message` 可选携带 `attachments`（本地路径原样透传）；
//! - stdout 每行一个 JSON 事件（见 [`CliEvent`]），非 JSON 行按 `message` 处理；
//! - stderr 逐行收进运行日志；
//! - 子进程在 stdin EOF 时应自行退出（宿主异常退出的级联兜底）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tiangong_plugin_subagent_protocol::ops::AttachmentPayload;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// 子进程 stdout 行事件（归一化前的后端原始事件）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CliEvent {
    /// 普通输出（有价值的消息，非高频进度）。
    Message { text: String },
    /// 状态上报：working / ready。
    Status {
        status: String,
        #[serde(default)]
        text: Option<String>,
    },
    /// 阻塞，等待外部输入。
    Blocked { text: String },
    /// 等待审批。
    ApprovalRequired { text: String },
    /// 任务完成。
    Completed {
        #[serde(default)]
        result: Option<String>,
    },
    /// 失败。
    Failed { error: String },
}

impl CliEvent {
    /// 解析一行 stdout：JSON 事件优先，非 JSON 行按 message 处理。
    pub fn parse_line(line: &str) -> Option<Self> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed.starts_with('{')
            && let Ok(event) = serde_json::from_str(trimmed)
        {
            return Some(event);
        }
        Some(Self::Message {
            text: trimmed.to_string(),
        })
    }
}

/// 传给子进程的启动帧。
#[derive(Debug, Clone, Serialize)]
pub struct BeginFrame<'a> {
    pub r#type: &'a str,
    pub agent_id: &'a str,
    pub agent_name: &'a str,
    pub instructions: &'a str,
    /// 长期记忆摘要（memory/ 注入，可为空串）。
    pub memory: &'a str,
    pub activation_id: &'a str,
    pub session_id: &'a str,
    pub workspace: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_criteria: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<&'a str>,
    /// 触发本次运行的用户输入（消息内容或任务补充说明）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<&'a str>,
    /// 随消息携带的附件（本地路径原样透传，成员自行读取；无附件省略字段）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<&'a [AttachmentPayload]>,
    /// 天工协作 MCP 接入引导（仅 CLI 后端携带；CLI 工具自行决定注册方式）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpGuide>,
}

/// 引导 CLI 成员接入天工协作的 MCP 端点信息（begin 帧透传，不代为注册：
/// 用户级/项目级注册由 CLI 工具与其用户决定，沙箱内也不可写外部配置）。
#[derive(Debug, Clone, Serialize)]
pub struct McpGuide {
    /// MCP server 名（注册时使用）。
    pub server: &'static str,
    /// sidecar 可执行文件绝对路径（直接作为 stdio MCP server 命令启动）。
    pub command: String,
    /// 建议 MCP server 的工作目录（成员协作域按此路由）。
    pub workspace: String,
    /// 给 CLI agent 的注册与用途说明。
    pub hint: &'static str,
}

#[derive(Debug, Clone)]
pub struct ExitInfo {
    pub code: Option<i32>,
}

/// 运行事件回调（由 service 注入，同步执行、必须非阻塞快路径）。
pub type EventCallback = Arc<dyn Fn(&str, CliEvent) + Send + Sync>;
pub type LogCallback = Arc<dyn Fn(&str, &str) + Send + Sync>;
pub type ExitCallback = Arc<dyn Fn(&str, ExitInfo) + Send + Sync>;

#[derive(Clone)]
pub struct RunHooks {
    pub on_event: EventCallback,
    pub on_log: LogCallback,
    pub on_exit: ExitCallback,
}

struct ManagedProcess {
    pid: u32,
    stdin: Arc<Mutex<Option<tokio::process::ChildStdin>>>,
    interrupt_sent: std::sync::atomic::AtomicBool,
}

/// 全部活跃 managed 子进程的注册表。
#[derive(Clone)]
pub struct RunnerHub {
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
}

impl RunnerHub {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 启动一个 managed 运行进程并注入 begin 帧。
    pub async fn spawn(
        &self,
        run_id: &str,
        command: &str,
        cwd: &std::path::Path,
        env: &[(String, String)],
        begin: &BeginFrame<'_>,
        hooks: RunHooks,
    ) -> Result<u32> {
        let mut cmd = build_command(command);
        cmd.current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in env {
            cmd.env(key, value);
        }
        apply_platform_window_suppression(&mut cmd);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 Agent 子进程失败（cwd={}）", cwd.display()))?;
        let pid = child.id().context("子进程启动后立即退出，无法获取 PID")?;
        let mut stdin = child.stdin.take().context("子进程未提供 stdin")?;
        let stdout = child.stdout.take().context("子进程未提供 stdout")?;
        let stderr = child.stderr.take().context("子进程未提供 stderr")?;

        let begin_line = serde_json::to_string(begin).context("序列化 begin 帧失败")?;
        stdin
            .write_all(format!("{begin_line}\n").as_bytes())
            .await
            .context("写入 begin 帧失败")?;
        stdin.flush().await.context("flush begin 帧失败")?;

        let stdin_handle = Arc::new(Mutex::new(Some(stdin)));
        self.processes.lock().await.insert(
            run_id.to_string(),
            ManagedProcess {
                pid,
                stdin: stdin_handle,
                interrupt_sent: std::sync::atomic::AtomicBool::new(false),
            },
        );

        let run_id_owned = run_id.to_string();
        let registry = self.processes.clone();
        // 子进程退出即从注册表移除（进程对象由本 task 独占持有）。
        tokio::spawn(async move {
            let exit_code = supervise_process(&run_id_owned, child, stdout, stderr, &hooks).await;
            registry.lock().await.remove(&run_id_owned);
            (hooks.on_exit)(&run_id_owned, ExitInfo { code: exit_code });
        });
        Ok(pid)
    }

    /// 向运行中的子进程注入一行 JSON（user_message / interrupt / cancel）。
    pub async fn write_line(&self, run_id: &str, payload: &serde_json::Value) -> Result<()> {
        let guard = self.processes.lock().await;
        let Some(process) = guard.get(run_id) else {
            bail!("运行已不存在: {run_id}");
        };
        let mut stdin_slot = process.stdin.lock().await;
        let Some(stdin) = stdin_slot.as_mut() else {
            bail!("运行进程的 stdin 已关闭: {run_id}");
        };
        let line = serde_json::to_string(payload).context("序列化注入帧失败")?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .context("写入注入帧失败")?;
        stdin.flush().await.context("flush 注入帧失败")?;
        Ok(())
    }

    /// 中断（SIGINT 组信号，保留现场；对应工具 interrupt_agent_run）。
    pub async fn interrupt(&self, run_id: &str) -> Result<()> {
        let guard = self.processes.lock().await;
        let Some(process) = guard.get(run_id) else {
            bail!("运行已不存在: {run_id}");
        };
        process
            .interrupt_sent
            .store(true, std::sync::atomic::Ordering::Release);
        signal_group(process.pid, Signal::Interrupt).context("发送中断信号失败")
    }

    /// 是否已向该运行发送过中断信号。
    pub async fn interrupt_sent(&self, run_id: &str) -> bool {
        self.processes
            .lock()
            .await
            .get(run_id)
            .is_some_and(|process| {
                process
                    .interrupt_sent
                    .load(std::sync::atomic::Ordering::Acquire)
            })
    }

    /// 终止：先发终止信号，宽限内未退出则强杀，并确认实际退出后才返回成功
    ///（同步等待，供取消与退出清理使用）；强杀后仍存活（沙箱拒绝信号等）
    /// 返回错误——未确认停止不能向调用方宣告已结束。
    pub async fn terminate(&self, run_id: &str, grace: Duration) -> Result<()> {
        let pid = {
            let guard = self.processes.lock().await;
            let Some(process) = guard.get(run_id) else {
                return Ok(());
            };
            process.pid
        };
        let _ = signal_group(pid, Signal::Terminate);
        let deadline = tokio::time::Instant::now() + grace;
        while tokio::time::Instant::now() < deadline {
            if !process_alive(pid) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = signal_group(pid, Signal::Kill);
        let kill_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < kill_deadline {
            if !process_alive(pid) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("进程 {pid} 在终止与强杀后仍未退出（沙箱可能拒绝信号），未确认停止")
    }

    /// 关闭 stdin（通知子进程不再有输入，协议要求其自行退出）。
    pub async fn close_stdin(&self, run_id: &str) -> Result<()> {
        let guard = self.processes.lock().await;
        let Some(process) = guard.get(run_id) else {
            return Ok(());
        };
        *process.stdin.lock().await = None;
        Ok(())
    }

    /// 优雅关闭全部 managed 运行：中断 → 宽限 → 强杀（宿主退出流程调用）。
    pub async fn shutdown_all(&self, grace: Duration) {
        let pids: Vec<u32> = {
            let guard = self.processes.lock().await;
            guard.values().map(|process| process.pid).collect()
        };
        if pids.is_empty() {
            return;
        }
        for pid in &pids {
            let _ = signal_group(*pid, Signal::Interrupt);
        }
        let deadline = tokio::time::Instant::now() + grace;
        while tokio::time::Instant::now() < deadline && pids.iter().any(|pid| process_alive(*pid)) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for pid in pids.iter().filter(|pid| process_alive(**pid)) {
            let _ = signal_group(*pid, Signal::Kill);
        }
    }
}

/// 读 stdout/stderr、轮询进程退出，返回退出码。
async fn supervise_process(
    run_id: &str,
    mut child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    hooks: &RunHooks,
) -> Option<i32> {
    let event_run_id = run_id.to_string();
    let event_hook = hooks.on_event.clone();
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(event) = CliEvent::parse_line(&line) {
                event_hook(&event_run_id, event);
            }
        }
    });
    let log_run_id = run_id.to_string();
    let log_hook = hooks.on_log.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            log_hook(&log_run_id, &line);
        }
    });
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => tokio::time::sleep(Duration::from_millis(200)).await,
            Err(error) => {
                tracing::warn!(run_id, %error, "轮询子进程退出状态失败");
                break None;
            }
        }
    };
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    exit_code
}

/// 宿主注入的「宿主 ↔ 本 sidecar」私有通道变量（传输标识、认证令牌、端点）。
/// 不得随 CLI 成员子进程外传：成员据此启动的下游进程（典型是同二进制以
/// MCP 形态被成员注册为 server）会误判传输形态——带着
/// `TIANGONG_PLUGIN_TRANSPORT=stdio` 就走进宿主 stdio 协议分支，而它并没有
/// 宿主连接，MCP 握手直接不通；同时也避免把不属于成员的凭据交给外部工具。
/// 存储根与服务连接信息（TIANGONG_STORAGE_ROOT / TIANGONG_SERVER_*）保留：
/// 成员及其 MCP 进程需要访问同一份存储与本机服务。
const HOST_CHANNEL_ENV: &[&str] = &[
    "TIANGONG_PLUGIN_TRANSPORT",
    "TIANGONG_PLUGIN_ID",
    "TIANGONG_PLUGIN_STDIO_TOKEN",
    "TIANGONG_PLUGIN_ENDPOINT",
    "TIANGONG_PLUGIN_DATA_DIR",
    "TIANGONG_PLUGIN_VERSION",
];

fn build_command(command: &str) -> tokio::process::Command {
    #[cfg(unix)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    };
    for key in HOST_CHANNEL_ENV {
        cmd.env_remove(key);
    }
    cmd
}

/// Windows 抑制控制台窗口；Unix 不 setsid——子进程留在 sidecar 进程组内。
///
/// Seatbelt 沙箱默认拒绝 process-signal（实测 macOS 26），sidecar 对子进程的
/// 组信号不可用；保留同组关系让宿主退出时的 kill(-sidecar_pid, SIGKILL)
/// 级联清理 managed 子进程（不依赖后端遵守 stdin EOF 自退出约定）。
/// 运行期控制走 JSONL 协议帧（interrupt/cancel）与 stdin EOF。
fn apply_platform_window_suppression(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

enum Signal {
    Interrupt,
    Terminate,
    Kill,
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: Signal) -> Result<()> {
    let (sig, name) = match signal {
        Signal::Interrupt => (libc::SIGINT, "SIGINT"),
        Signal::Terminate => (libc::SIGTERM, "SIGTERM"),
        Signal::Kill => (libc::SIGKILL, "SIGKILL"),
    };
    // 单进程信号（子进程与 sidecar 同组，宿主级联负责整组回收）。
    // 沙箱内可能被 Seatbelt 拒绝（process-signal 默认不放行）——
    // 进程已消失（ESRCH）视为成功，其余失败由调用方降级处理。
    let result = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if result == 0 || std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
        Ok(())
    } else {
        bail!("向进程 {pid} 发送 {name} 失败")
    }
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    unsafe {
        let result = libc::kill(pid as libc::pid_t, 0);
        // EPERM = 进程存在但当前沙箱拒绝信号（Seatbelt process-signal 默认不放行）。
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(windows)]
fn signal_group(pid: u32, _signal: Signal) -> Result<()> {
    // Windows 无组信号且 CTRL_BREAK 依赖共享控制台：中断与终止均降级为
    // 进程树终止（能力差异记录在 Adapter 声明中）。taskkill 抑制窗口闪烁。
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Ok(result) if result.status.success() => Ok(()),
        Ok(result) => bail!(
            "taskkill 终止进程树 {pid} 失败: {}",
            String::from_utf8_lossy(&result.stderr)
        ),
        Err(error) => {
            Err(anyhow::Error::new(error).context(format!("启动 taskkill 失败（pid={pid}）")))
        }
    }
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}")])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Ok(result) => String::from_utf8_lossy(&result.stdout).contains(&pid.to_string()),
        Err(_) => false,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn no_hooks() -> RunHooks {
        RunHooks {
            on_event: Arc::new(|_, _| {}),
            on_log: Arc::new(|_, _| {}),
            on_exit: Arc::new(|_, _| {}),
        }
    }

    fn sample_attachments() -> Vec<AttachmentPayload> {
        vec![AttachmentPayload {
            path: "/tmp/屏幕截图 2026.png".to_string(),
            kind: "image".to_string(),
            mime_type: Some("image/png".to_string()),
            name: Some("截图.png".to_string()),
        }]
    }

    fn begin_frame<'a>(attachments: Option<&'a [AttachmentPayload]>) -> BeginFrame<'a> {
        BeginFrame {
            r#type: "begin",
            agent_id: "agent-1",
            agent_name: "测试成员",
            instructions: "",
            memory: "",
            activation_id: "act-1",
            session_id: "sess-1",
            workspace: "/tmp/ws",
            task_id: None,
            goal: None,
            completion_criteria: None,
            message: Some("看图"),
            input: Some("看图"),
            attachments,
            mcp: None,
        }
    }

    /// 启动帧附件 wire 形状：无附件省略字段（老协议实现兼容）；有附件为
    /// [{path, kind, mime_type, name}]，路径原样透传（含空格中文不转义破坏）。
    #[test]
    fn 启动帧附件序列化形状() {
        let without = serde_json::to_value(begin_frame(None)).unwrap();
        assert!(without.get("attachments").is_none(), "无附件必须省略字段");
        assert!(without.get("mcp").is_none(), "无 MCP 引导必须省略字段");

        let with = serde_json::to_value(begin_frame(Some(&sample_attachments()))).unwrap();
        let attachments = with["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0]["path"], "/tmp/屏幕截图 2026.png");
        assert_eq!(attachments[0]["kind"], "image");
        assert_eq!(attachments[0]["mime_type"], "image/png");
        assert_eq!(attachments[0]["name"], "截图.png");
    }

    /// 端到端：真实子进程收到带附件的启动帧与补充帧（验收口径「读取真实
    /// 子进程收到的帧」）。cat 把 stdin 落盘到 cwd 下的文件，进程在 stdin
    /// EOF 后退出，再核对两行 JSON。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 真实子进程接收启动帧与补充帧附件() {
        let temp = tempfile::tempdir().unwrap();
        let hub = RunnerHub::new();
        let attachments = sample_attachments();
        let guide = McpGuide {
            server: "tiangong-subagent",
            command: "/opt/部署路径/tiangong-subagent-sidecar".to_string(),
            workspace: temp.path().to_string_lossy().into_owned(),
            hint: "注册为本工具的 MCP server 后可与天工协作。",
        };
        let begin = begin_frame(Some(&attachments));
        let begin = BeginFrame {
            mcp: Some(guide),
            ..begin
        };
        let pid = hub
            .spawn(
                "run-test",
                "cat > frames.jsonl",
                temp.path(),
                &[],
                &begin,
                no_hooks(),
            )
            .await
            .unwrap();
        assert!(pid > 0);

        let mut supplement = serde_json::json!({
            "type": "user_message",
            "content": "补充：再核对第二张",
        });
        supplement["attachments"] = serde_json::to_value(&attachments).unwrap();
        hub.write_line("run-test", &supplement).await.unwrap();
        hub.close_stdin("run-test").await.unwrap();

        // cat 在 EOF 后退出；轮询落盘文件直至两行齐。
        let frames_path = temp.path().join("frames.jsonl");
        let mut lines: Vec<serde_json::Value> = Vec::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Ok(content) = std::fs::read_to_string(&frames_path)
                && content.lines().count() >= 2
            {
                lines = content
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                break;
            }
        }
        assert_eq!(lines.len(), 2, "应收到启动帧与补充帧两行");

        let begin = &lines[0];
        assert_eq!(begin["type"], "begin");
        assert_eq!(begin["attachments"][0]["path"], "/tmp/屏幕截图 2026.png");
        assert_eq!(begin["attachments"][0]["name"], "截图.png");
        // MCP 接入引导随帧到达：注册名、命令路径与工作目录。
        assert_eq!(begin["mcp"]["server"], "tiangong-subagent");
        assert_eq!(
            begin["mcp"]["command"],
            "/opt/部署路径/tiangong-subagent-sidecar"
        );
        assert_eq!(
            begin["mcp"]["workspace"].as_str().unwrap(),
            temp.path().to_string_lossy()
        );

        let supplement = &lines[1];
        assert_eq!(supplement["type"], "user_message");
        assert_eq!(supplement["content"], "补充：再核对第二张");
        assert_eq!(
            supplement["attachments"][0]["path"],
            "/tmp/屏幕截图 2026.png"
        );
    }

    /// 端到端：CLI 成员子进程不继承宿主插件通道变量（传输标识、认证令牌），
    /// 但保留存储根与服务连接信息。成员据 begin 帧的 mcp 引导把 sidecar 注册
    /// 为 MCP server 时，带 `TIANGONG_PLUGIN_TRANSPORT=stdio` 会误走宿主 stdio
    /// 分支、MCP 握手不通——子进程侧实测环境变量取值。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn 成员子进程不继承插件通道变量() {
        // 模拟宿主 spawn 本 sidecar 时注入的通道环境。
        unsafe {
            std::env::set_var("TIANGONG_PLUGIN_TRANSPORT", "stdio");
            std::env::set_var("TIANGONG_PLUGIN_STDIO_TOKEN", "host-token");
            std::env::set_var("TIANGONG_STORAGE_ROOT", "/tmp/kept-root");
        }
        let temp = tempfile::tempdir().unwrap();
        let hub = RunnerHub::new();
        let begin = begin_frame(None);
        // 先读空 stdin：子进程不得在宿主写入 begin 帧前退出，否则写入遇 Broken pipe。
        hub.spawn(
            "run-env",
            r#"cat > /dev/null; printf '%s|%s|%s' "${TIANGONG_PLUGIN_TRANSPORT-<unset>}" "${TIANGONG_PLUGIN_STDIO_TOKEN-<unset>}" "$TIANGONG_STORAGE_ROOT" > env.txt"#,
            temp.path(),
            &[],
            &begin,
            no_hooks(),
        )
        .await
        .unwrap();
        hub.close_stdin("run-env").await.unwrap();

        let env_path = temp.path().join("env.txt");
        let mut observed = String::new();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Ok(content) = std::fs::read_to_string(&env_path)
                && !content.is_empty()
            {
                observed = content;
                break;
            }
        }
        unsafe {
            std::env::remove_var("TIANGONG_PLUGIN_TRANSPORT");
            std::env::remove_var("TIANGONG_PLUGIN_STDIO_TOKEN");
            std::env::remove_var("TIANGONG_STORAGE_ROOT");
        }
        let fields: Vec<&str> = observed.split('|').collect();
        assert_eq!(fields.len(), 3, "子进程环境落盘失败: {observed:?}");
        assert_eq!(fields[0], "<unset>", "传输标识不得外传");
        assert_eq!(fields[1], "<unset>", "宿主令牌不得外传");
        assert_eq!(fields[2], "/tmp/kept-root", "存储根必须保留");
    }
}
