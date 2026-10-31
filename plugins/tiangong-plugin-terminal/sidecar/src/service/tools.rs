//! Agent 工具入口（宿主直连；每会话唯一调度者在本进程内）。
//!
//! 工具编排（选终端、新建、开标签、执行、收结果）全部在 sidecar 内完成：
//! 会话归属与权威工作目录来自请求帧携带的宿主上下文（Session.cwd 真相源），
//! 不接受插件页面上下文；terminal_id 归属 scope 校验在操作内完成。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tiangong_plugin_sidecar::protocol::RequestInvocationContext;

use super::advice;
use super::jobs::{BackgroundJob, session_processes};
use super::types::{
    AcquireRequest, CloseRequest, ExecRequest, SendRequest, SessionIdRequest, SpawnRequest,
    default_cols, default_rows,
};
use super::{ActiveTool, PtySession, SessionPhase, ShellNotReady, TerminalService};

/// 工具调用的宿主权威会话上下文（请求帧 context，缺省拒绝执行）。
pub(super) struct ToolScope {
    pub(super) session_id: String,
    pub(super) workspace: String,
    pub(super) request_id: String,
}

/// 活动工具登记。正常执行结束时由 `complete` 清除；dispatch future 先被
/// 丢弃时有意保留映射，随后到达的 `cancel()` 仍能定位并中断 PTY。
pub(super) struct ActiveToolRegistration {
    pub(super) active_tools: Arc<Mutex<HashMap<String, ActiveTool>>>,
    pub(super) request_id: String,
}

impl ActiveToolRegistration {
    fn complete(self) {
        if let Ok(mut active) = self.active_tools.lock() {
            active.remove(&self.request_id);
        }
    }
}

pub(super) fn tool_scope(
    request_id: &str,
    context: Option<&RequestInvocationContext>,
) -> Result<ToolScope> {
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
pub(super) struct ToolOutcome {
    pub(super) ok: bool,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) stderr: Option<String>,
    pub(super) exit_code: i32,
}

impl ToolOutcome {
    pub(super) fn ok(summary: String) -> Self {
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

/// 控制键的可读名（延续声明的 round_label 用）。
fn control_key_name(ch: char) -> Option<&'static str> {
    match ch {
        '\x03' => Some("Ctrl+C"),
        '\x04' => Some("Ctrl+D"),
        '\x1a' => Some("Ctrl+Z"),
        '\x15' => Some("Ctrl+U"),
        '\x17' => Some("Ctrl+W"),
        '\x12' => Some("Ctrl+R"),
        '\x0c' => Some("Ctrl+L"),
        '\x01' => Some("Ctrl+A"),
        '\x05' => Some("Ctrl+E"),
        '\x02' => Some("Ctrl+B"),
        '\x06' => Some("Ctrl+F"),
        '\x0e' => Some("Ctrl+N"),
        '\x10' => Some("Ctrl+P"),
        _ => None,
    }
}

/// 终端输入的可读轮次标签：纯控制键 →「发送 Ctrl+C」，带回车的作答 →
/// 「输入：y」，普通文本原样；空输入返回空串（界面按续等展示）。
fn send_round_label(input: &str) -> String {
    let decoded = super::input::decode_terminal_escapes(input);
    let body = decoded.trim_end_matches(['\r', '\n']);
    if body.is_empty() {
        return String::new();
    }
    if super::input::is_control_keys(body) {
        let names = body
            .chars()
            .filter_map(control_key_name)
            .collect::<Vec<_>>()
            .join(" + ");
        return format!("发送 {names}");
    }
    if decoded.ends_with('\r') || decoded.ends_with('\n') {
        return format!("输入：{body}");
    }
    body.to_string()
}

#[derive(Debug, Deserialize)]
struct RunCommandArgs {
    pub(super) cmd: String,
    #[serde(default)]
    pub(super) args: Vec<String>,
    #[serde(default)]
    pub(super) cwd: Option<String>,
    #[serde(default)]
    pub(super) timeout: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RunShellArgs {
    pub(super) script: String,
    #[serde(default)]
    pub(super) cwd: Option<String>,
    #[serde(default)]
    pub(super) timeout: Option<u64>,
    #[serde(default)]
    pub(super) interactive: bool,
}

#[derive(Debug, Deserialize)]
struct TerminalSendArgs {
    pub(super) input: String,
    pub(super) terminal_id: String,
    #[serde(default)]
    pub(super) wait: Option<u64>,
    #[serde(default)]
    pub(super) enter: bool,
}

#[derive(Debug, Deserialize)]
struct TerminalCloseArgs {
    pub(super) terminal_id: String,
}

#[derive(Debug, Deserialize)]
struct TerminalStatusArgs {
    /// 缺省查询当前会话全部终端；指定则返回该终端详情。
    #[serde(default)]
    pub(super) terminal_id: String,
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
///
/// sidecar 不感知宿主形态：无前端入口（CLI / Server）的宿主未注入 App
/// 处理器，反馈在宿主侧静默降级（见 `handle_runtime_feedback`）。
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
    pub(super) async fn tool_run_command(
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
        // 用法提示附在摘要末尾（只提示不拦截）：现写多行脚本、nohup / &
        // 后台启动会让终端逐步执行与多终端并行的能力失效。
        let hints = advice::command_hints(&command, is_shell);
        let summary = if hints.is_empty() {
            summary
        } else {
            format!("{summary}。提示：{}", hints.join("；"))
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
    pub(super) async fn tool_terminal_open(&self, scope: &ToolScope) -> ToolOutcome {
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
        ToolOutcome::ok(format!("已打开终端 {instance_id}"))
    }

    /// terminal：终端管理统一入口（action 模式：open/send/close/status）。
    ///
    /// 参数结构体不使用 deny_unknown_fields，`action` 字段可随负载透传给
    /// 各子处理（与 fs 插件的 file_read action 模式一致）。
    pub(super) async fn tool_terminal(
        &self,
        scope: &ToolScope,
        payload: serde_json::Value,
    ) -> ToolOutcome {
        let action = payload
            .get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim();
        match action {
            "open" => self.tool_terminal_open(scope).await,
            "send" => self.tool_terminal_send(scope, payload).await,
            "close" => self.tool_terminal_close(scope, payload).await,
            "status" => self.tool_terminal_status(scope, payload),
            other => ToolOutcome::fail(format!(
                "terminal 缺少或未知 action（{other:?}）：应为 open / send / close / status"
            )),
        }
    }

    /// terminal_send：向指定终端发送输入；目标必须属于当前会话。
    pub(super) async fn tool_terminal_send(
        &self,
        scope: &ToolScope,
        payload: serde_json::Value,
    ) -> ToolOutcome {
        let parsed = match serde_json::from_value::<TerminalSendArgs>(payload) {
            Ok(parsed) => parsed,
            Err(error) => {
                return ToolOutcome::fail(format!("terminal(action=send) 参数无效：{error}"));
            }
        };
        let target = parsed.terminal_id.trim().to_string();
        if target.is_empty() {
            return ToolOutcome::fail("terminal(action=send) 缺少 terminal_id".to_string());
        }
        let round_label = send_round_label(&parsed.input);
        let sent = self
            .send_to_session(SendRequest {
                scope_id: scope.session_id.clone(),
                session_id: target,
                input: parsed.input,
                wait: parsed.wait,
                enter: parsed.enter,
            })
            .await;
        match sent {
            Ok(sent) => {
                let target = &sent.session_id;
                let elapsed = sent.elapsed_secs.unwrap_or_default();
                let command = sent.command.as_deref().unwrap_or("");
                // 摘要直接给出命令真实状态与下一步做法：Agent 不再需要
                // 从输出猜测命令是否跑完（issue #673）。
                let summary = if sent.rejected {
                    format!(
                        "未发送：终端 {target} 上一条命令 `{command}` 仍在执行（已 {elapsed} 秒），\
本次输入未写入，避免被排进输入缓冲延后执行造成输出错位。\
可 input 留空且不设 enter 继续等待并读取新增输出；发送 \\x03 中断（控制键无论是否设 enter 都直接写入）；\
若程序在等待输入（如 [y/N]），不设 enter 并在 input 末尾带 \\r 作答"
                    )
                } else {
                    match sent.status {
                        Some("completed") => format!("终端 {target} 命令已执行完成，输出见 stdout"),
                        Some("interactive") => format!(
                            "终端 {target} 已进入交互程序（如 ssh 登录、vi、REPL），正在等待输入；\
后续命令继续用 terminal（action=send）发送"
                        ),
                        Some(_) => format!(
                            "终端 {target} 命令 `{command}` 仍在执行（已 {elapsed} 秒），stdout 为截至目前的新增输出；\
可 input 留空且不设 enter 继续等待（wait 最长 110 秒），完成前不要发送新命令"
                        ),
                        None => format!("终端 {target} 已更新，当前内容见 stdout"),
                    }
                };
                // stdout 首行为通用延续标记行（约定：`@continuation ` + 单行 JSON）：
                // 声明本结果是同一逻辑执行的延续轮次（如终端命令续等），宿主
                // 界面按 key 合并展示，模型也能从 status_label 看到命令状态
                //（issue #673/#707）。字段全为展示语义，任何插件的长任务轮次
                //（构建进度、生成轮次等）都可复用该约定。
                let command = sent.command.clone().unwrap_or_default();
                let trimmed_command = command.trim().to_string();
                let stdout = if sent.rejected {
                    sent.stdout
                } else {
                    let (kind, label) = match sent.status {
                        Some("completed") => {
                            ("completed", format!("已完成 · 退出码 {}", sent.exit_code))
                        }
                        Some("interactive") => ("interactive", "交互中 · 等待输入".to_string()),
                        Some(_) => (
                            "running",
                            format!("仍在执行 · 已 {} 秒", sent.elapsed_secs.unwrap_or_default()),
                        ),
                        None => ("updated", "已更新".to_string()),
                    };
                    let marker = serde_json::json!({
                        "key": format!("terminal-send:{target}"),
                        "title": trimmed_command,
                        "status": kind,
                        "status_label": label,
                        "elapsed_ms": sent.elapsed_secs.map(|secs| secs * 1000),
                        "round_label": round_label,
                    });
                    let marker_line = format!("@continuation {marker}");
                    if sent.stdout.is_empty() {
                        marker_line
                    } else {
                        format!("{marker_line}\n{}", sent.stdout)
                    }
                };
                ToolOutcome {
                    ok: !sent.rejected,
                    summary,
                    stdout: Some(stdout),
                    stderr: None,
                    exit_code: sent.exit_code,
                }
            }
            Err(error) => ToolOutcome::fail(format!("{error:#}")),
        }
    }

    /// terminal_close：精确关闭指定终端及其面板（归属校验在关闭操作内）。
    pub(super) async fn tool_terminal_close(
        &self,
        scope: &ToolScope,
        payload: serde_json::Value,
    ) -> ToolOutcome {
        let parsed = match serde_json::from_value::<TerminalCloseArgs>(payload) {
            Ok(parsed) => parsed,
            Err(error) => {
                return ToolOutcome::fail(format!("terminal(action=close) 参数无效：{error}"));
            }
        };
        let target = parsed.terminal_id.trim().to_string();
        if target.is_empty() {
            return ToolOutcome::fail(
                "terminal(action=close) 缺少 terminal_id，未关闭任何终端".to_string(),
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

    /// terminal_status 概要：当前会话全部终端与遗留的后台进程。
    fn terminal_overview(
        &self,
        scope: &ToolScope,
        background_jobs: Vec<BackgroundJob>,
    ) -> ToolOutcome {
        let sessions = self.sessions.lock().expect("会话表锁损坏");
        // 概要含已退出待回收的终端：Agent 排查「为什么复用失败」时
        // 需要看到完整在表集合，与前端恢复列表（只列存活）目的不同。
        let mut entries: Vec<(u64, &String, &PtySession)> = sessions
            .iter()
            .filter(|(_, session)| session.scope_id.as_deref() == Some(scope.session_id.as_str()))
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
        let mut summary = if terminals.is_empty() {
            "当前会话没有终端".to_string()
        } else {
            let breakdown = status_counts
                .iter()
                .map(|(label, count)| format!("{}×{}", phase_label_cn(label), count))
                .collect::<Vec<_>>()
                .join("、");
            format!("当前会话有 {} 个终端：{}", terminals.len(), breakdown)
        };
        // 遗留后台进程（`&` / `nohup` 起的服务等）：沙箱内 ps/pgrep 不可用，
        // 这里是 Agent 定位它们的唯一途径，用完应 kill <pid> 结束。
        if !background_jobs.is_empty() {
            let listed = background_jobs
                .iter()
                .map(|job| format!("{}({})", job.pid, job.name))
                .collect::<Vec<_>>()
                .join("、");
            summary.push_str(&format!(
                "；遗留后台进程 {} 个：{listed}，不再需要时用 kill <pid> 结束",
                background_jobs.len()
            ));
        }
        ToolOutcome {
            ok: true,
            summary,
            stdout: Some(
                serde_json::to_string(&serde_json::json!({
                    "terminals": terminals,
                    "background_jobs": background_jobs,
                }))
                .unwrap_or_default(),
            ),
            stderr: None,
            exit_code: 0,
        }
    }

    /// terminal_status：查询当前会话的终端状态（只读，不触碰任何 PTY）。
    /// 不带 terminal_id 返回全部终端概要（按创建序）；带 terminal_id 返回
    /// 该终端详情（含最近可见输出尾部），归属校验与 terminal_send 一致。
    pub(super) fn tool_terminal_status(
        &self,
        scope: &ToolScope,
        payload: serde_json::Value,
    ) -> ToolOutcome {
        let parsed = match serde_json::from_value::<TerminalStatusArgs>(payload) {
            Ok(parsed) => parsed,
            Err(error) => return ToolOutcome::fail(format!("terminal_status 参数无效：{error}")),
        };
        let target = parsed.terminal_id.trim().to_string();
        if target.is_empty() {
            // 枚举进程在会话表锁外完成：终端存活集合先取快照。
            let alive: Vec<String> = self
                .sessions
                .lock()
                .expect("会话表锁损坏")
                .iter()
                .filter(|(_, session)| session.exited_code.is_none())
                .map(|(id, _)| id.clone())
                .collect();
            let background_jobs = self.jobs.live_jobs(Some(scope.session_id.as_str()), |id| {
                alive.iter().any(|alive| alive == id)
            });
            return self.terminal_overview(scope, background_jobs);
        }
        let sessions = self.sessions.lock().expect("会话表锁损坏");
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
        let exited_code = session.exited_code;
        let shell_pid = session.shell_pid.filter(|_| exited_code.is_none());
        // 枚举进程在会话表锁外完成，避免阻塞其他终端操作。
        drop(sessions);
        // 终端会话内的进程树（含登录 shell）：沙箱内 ps/top 不可用
        // （issue #677），Agent 据此查看终端里跑着什么、按 pid 结束。
        let processes = shell_pid.map(session_processes).unwrap_or_default();
        let mut summary = match &running_command {
            Some(command) => format!("终端 {target} 状态：{}（{command}）", phase_label_cn(label)),
            None => format!("终端 {target} 状态：{}", phase_label_cn(label)),
        };
        if !processes.is_empty() {
            summary.push_str(&format!("；会话内进程 {} 个", processes.len()));
        }
        ToolOutcome {
            ok: true,
            summary,
            stdout: Some(
                serde_json::to_string(&serde_json::json!({
                    "terminal_id": target,
                    "status": label,
                    "command": running_command,
                    "exited_code": exited_code,
                    "recent_output": recent_output,
                    "processes": processes,
                }))
                .unwrap_or_default(),
            ),
            stderr: None,
            exit_code: 0,
        }
    }
}
