//! Subagent 插件 sidecar 服务：统一交互总线。
//!
//! 职责：扫描管理持久 Agent（~/.tiangong/agents/）、维护会话激活与 Workspace
//! 写互斥、路由消息与任务到运行后端（首版 CLI Adapter）、归一化状态、把重要
//! 反馈经 Hook 投回激活会话、退出时清理 managed 运行实例。
//!
//! 会话归属纪律：AI 工具操作的会话与 Workspace 以宿主注入的 invocation
//! context 为准；UI 操作显式携带 session_id / workspace。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;

use tiangong_plugin_runtime::protocol::{HANDSHAKE_OPERATION, PROTOCOL_VERSION, Request, Response};
use tiangong_plugin_sidecar::SidecarService;
use tiangong_plugin_sidecar::invocation_context;
use tiangong_plugin_sidecar::server::emit_notification;
use tiangong_plugin_subagent_protocol::config::{AgentConfig, BackendKind, WorkspacePolicy};
use tiangong_plugin_subagent_protocol::hooks::{HookEvent, HookEventType};
use tiangong_plugin_subagent_protocol::ops::*;
use tiangong_plugin_subagent_protocol::state::{
    ActivationRecord, AgentEventRecord, RunKind, RunRecord, RunStatus, TaskRecord, TaskStatus,
};
use tiangong_plugin_subagent_protocol::{NOTIFICATION_CHANNEL, PLUGIN_ID, PLUGIN_VERSION};

use crate::agent_store::AgentStore;
use crate::delivery::DeliveryWorker;
use crate::paths::{new_id, now_string};
use crate::runner::{BeginFrame, CliEvent, ExitInfo, McpGuide, RunHooks, RunnerHub};
use crate::runtime_store::RuntimeStore;

/// 工具响应统一形状（对齐宿主 ToolOutcome 映射）。
fn tool_ok(summary: String) -> serde_json::Value {
    json!({ "ok": true, "summary": summary, "exit_code": 0 })
}

fn tool_fail(summary: String) -> serde_json::Value {
    json!({ "ok": false, "summary": summary, "exit_code": 1 })
}

fn tool_detail(summary: String, detail: serde_json::Value) -> serde_json::Value {
    let mut payload = tool_ok(summary);
    payload["stdout"] =
        serde_json::Value::String(serde_json::to_string(&detail).unwrap_or_default());
    payload
}

/// runner 回调进入串行事件流水线（保证同一 run 的事件顺序）。
enum RunnerEvent {
    Cli { run_id: String, event: CliEvent },
    Log { run_id: String, line: String },
    Exit { run_id: String, info: ExitInfo },
}

/// MCP 模式的回退会话上下文：外部 agent 工具经 MCP server 调用总线时
/// 没有宿主线程本地上下文，以此固定身份执行（启动时一次性设置）。
pub static MCP_CONTEXT: std::sync::OnceLock<std::sync::RwLock<Option<(String, String)>>> =
    std::sync::OnceLock::new();

/// 初始化 MCP 回退上下文（--mcp 模式启动时调用一次）。
pub fn init_mcp_context(session: &str, workspace: &str) {
    let _ = MCP_CONTEXT.set(std::sync::RwLock::new(Some((
        session.to_string(),
        workspace.to_string(),
    ))));
}

/// 集群协作发起方信息：发起会话（回报投回目标）与展示名（投递正文用）。
struct CollabOrigin<'a> {
    session: &'a str,
    label: String,
}

pub struct SubagentService {
    agents: AgentStore,
    store: Arc<RuntimeStore>,
    runner: RunnerHub,
    /// 会话后端投递与源会话通知共用的 HTTP 客户端。
    http: reqwest::Client,
    /// 变更操作互斥（激活、任务、运行控制、事件状态机）。
    ops: Arc<Mutex<()>>,
    shutting_down: Arc<AtomicBool>,
}

impl SubagentService {
    pub fn new() -> Result<Self> {
        Self::new_with_restore(true)
    }

    /// 外部接入（MCP）构造：跳过启动恢复——多实例共享存储时，恢复是
    /// 执行管理者（宿主实例）的职责，外部接入层不得把其他实例正在
    /// 执行的运行误标为遗留中断。
    pub fn new_without_restore() -> Result<Self> {
        Self::new_with_restore(false)
    }

    fn new_with_restore(restore: bool) -> Result<Self> {
        let agents = AgentStore::open()?;
        let store = Arc::new(RuntimeStore::open()?);
        if restore {
            Self::restore_orphan_runs(&store);
        }
        let runner = RunnerHub::new();
        let service = Self {
            agents,
            store,
            runner,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .context("构建 HTTP 客户端失败")?,
            ops: Arc::new(Mutex::new(())),
            shutting_down: Arc::new(AtomicBool::new(false)),
        };
        service.start_event_pipeline();
        // Hook 投递 worker：补投重启前未送达事件。
        DeliveryWorker::start(Arc::clone(&service.store));
        Ok(service)
    }

    /// 启动时恢复：上次 sidecar 退出后残留的非终态运行标记为已中断。
    fn restore_orphan_runs(store: &RuntimeStore) {
        let now = now_string();
        for mut run in store.list_runs() {
            if !run.status.is_alive() {
                continue;
            }
            if run.pid.is_none() {
                // 会话型孤儿：执行在宿主会话内，不因 sidecar 重启而停止——
                // 保留等待 turn 归因（运行标记仍可精确对号）。
                tracing::info!(run_id = %run.run_id, "会话型孤儿运行保留，等待归因");
                continue;
            }
            // CLI 孤儿：本 sidecar 刚启动，不存在属于本代次的活跃子进程；
            // 残留 pid 即使存活也不能接管（可能是无关进程）。
            run.status = RunStatus::Interrupted;
            run.finished_at = Some(now.clone());
            run.updated_at = now.clone();
            run.summary = Some("上次运行未正常收尾（sidecar 重启）".to_string());
            if let Err(error) = store.save_run(&run) {
                tracing::warn!(run_id = %run.run_id, %error, "恢复孤儿运行状态失败");
            }
            release_collab_activation(store, &run.activation_id);
            if let Some(task_id) = run.task_id.clone()
                && let Ok(mut task) = store.load_task(&task_id)
                && task.status == TaskStatus::Running
            {
                task.status = TaskStatus::Interrupted;
                task.updated_at = now.clone();
                let _ = store.save_task(&task);
            }
        }
    }

    /// runner 回调 → 串行事件流水线（单消费者，保证顺序与状态一致）。
    fn start_event_pipeline(&self) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RunnerEvent>();
        let store = Arc::clone(&self.store);
        let agents = AgentStore::open().ok();
        let runner = self.runner.clone();
        let ops = Arc::clone(&self.ops);

        // 注册回调（由 runner 侧触发，仅做转发，不在回调内做重活）。
        let cli_tx = tx.clone();
        let log_tx = tx.clone();
        let exit_tx = tx.clone();
        let hooks = RunHooks {
            on_event: Arc::new(move |run_id, event| {
                let _ = cli_tx.send(RunnerEvent::Cli {
                    run_id: run_id.to_string(),
                    event,
                });
            }),
            on_log: Arc::new(move |run_id, line| {
                let _ = log_tx.send(RunnerEvent::Log {
                    run_id: run_id.to_string(),
                    line: line.to_string(),
                });
            }),
            on_exit: Arc::new(move |run_id, info| {
                let _ = exit_tx.send(RunnerEvent::Exit {
                    run_id: run_id.to_string(),
                    info,
                });
            }),
        };
        if RUN_HOOKS.set(hooks).is_err() {
            panic!("RunnerHooks 只能初始化一次");
        }

        // 消费循环：串行处理，last_message 跟踪每次运行的最后一条输出。
        tokio::spawn(async move {
            let mut last_message: HashMap<String, String> = HashMap::new();
            while let Some(event) = rx.recv().await {
                let _guard = ops.lock().await;
                match event {
                    RunnerEvent::Log { run_id, line } => {
                        store.append_run_log(&run_id, &line);
                    }
                    RunnerEvent::Cli { run_id, event } => {
                        handle_cli_event(
                            &store,
                            &agents,
                            &runner,
                            &mut last_message,
                            &run_id,
                            event,
                        );
                    }
                    RunnerEvent::Exit { run_id, info } => {
                        handle_exit(&store, &agents, &runner, &last_message, &run_id, info).await;
                        last_message.remove(&run_id);
                    }
                }
            }
        });
    }

    // ── dispatch ──────────────────────────────────────────────

    /// MCP 模式公开入口：外部工具调用与宿主走同一分发。
    pub async fn dispatch_inner_public(&self, request: &Request) -> serde_json::Value {
        self.dispatch_inner(request).await
    }

    async fn dispatch_inner(&self, request: &Request) -> serde_json::Value {
        let operation = request.operation.as_str();
        let payload = request.payload.clone();
        let result: Result<serde_json::Value> = match operation {
            HANDSHAKE_OPERATION => Ok(handshake_payload()),
            SHUTDOWN_OPERATION => {
                self.begin_shutdown().await;
                Ok(tool_ok("Subagent 总线已优雅关闭".to_string()))
            }
            // ── AI 工具操作（会话归属来自宿主注入的 invocation context）──
            TOOL_CREATE_AGENT => self.tool_create_agent(&payload).await,
            TOOL_LIST_AGENTS => self.tool_list_agents().await,
            TOOL_GET_AGENT => self.tool_get_agent(&payload).await,
            TOOL_ACTIVATE_AGENT => self.tool_activate(&payload).await,
            TOOL_DEACTIVATE_AGENT => self.tool_deactivate(&payload).await,
            TOOL_LIST_ACTIVE_AGENTS => self.tool_list_active().await,
            TOOL_SEND_AGENT_MESSAGE => self.tool_send_message(&payload).await,
            TOOL_SUBMIT_AGENT_TASK => self.tool_submit_task(&payload).await,
            TOOL_GET_AGENT_TASK => self.tool_get_task(&payload).await,
            TOOL_LIST_AGENT_TASKS => self.tool_list_tasks(&payload).await,
            TOOL_GET_AGENT_RUN => self.tool_get_run(&payload).await,
            TOOL_INTERRUPT_AGENT_RUN => self.tool_interrupt_run(&payload).await,
            TOOL_CANCEL_AGENT_RUN => self.tool_cancel_run(&payload).await,
            TOOL_LIST_AGENT_EVENTS => self.tool_list_events(&payload).await,
            TOOL_GET_AGENT_ARTIFACTS => self.tool_get_artifacts(&payload).await,
            TOOL_GET_AGENT_MEMORY => self.tool_get_memory(&payload).await,
            TOOL_APPEND_AGENT_MEMORY => self.tool_append_memory(&payload).await,
            TOOL_APPEND_AGENT_INSTRUCTIONS => self.tool_append_instructions(&payload).await,
            TOOL_REPORT_AGENT_RESULT => self.tool_report_result(&payload).await,
            TOOL_LIST_PENDING_WORK => self.tool_list_pending_work(&payload).await,
            TOOL_LOAD_WORKSPACE_STATE => self.tool_load_workspace_state(&payload).await,
            TOOL_UPDATE_WORKSPACE_STATE => self.tool_update_workspace_state(&payload).await,
            // ── UI 操作（显式携带会话） ──
            UI_STATE_SNAPSHOT => self.ui_state_snapshot(&payload).await,
            UI_AGENT_CREATE => self.ui_agent_create(&payload).await,
            UI_AGENT_UPDATE => self.ui_agent_update(&payload).await,
            UI_AGENT_DELETE => self.ui_agent_delete(&payload).await,
            UI_ACTIVATE => self.ui_activate(&payload).await,
            UI_DEACTIVATE => self.ui_deactivate(&payload).await,
            UI_SEND_MESSAGE => self.ui_send_message(&payload).await,
            UI_SUBMIT_TASK => self.ui_submit_task(&payload).await,
            UI_INTERRUPT_RUN => self.ui_interrupt_run(&payload).await,
            UI_CANCEL_RUN => self.ui_cancel_run(&payload).await,
            UI_LIST_SESSIONS => serde_json::to_value(crate::sessions::list_sessions())
                .map_err(|error| anyhow::anyhow!("序列化会话列表失败: {error}")),
            UI_LIST_MEMORY => self.ui_list_memory(&payload).await,
            UI_LIST_WORKSPACE_STATES => self.ui_list_workspace_states(&payload).await,
            UI_READ_MEMORY => self.ui_read_memory(&payload).await,
            UI_WRITE_MEMORY => self.ui_write_memory(&payload).await,
            UI_DELETE_MEMORY => self.ui_delete_memory(&payload).await,
            UI_COMPILE_MEMORY => self.ui_compile_memory(&payload).await,
            // ── WASM 转发：@ 提及候选（启用且后端已实现的 Agent） ──
            MENTION_CANDIDATES => {
                let candidates: Vec<serde_json::Value> = self
                    .agents
                    .list()
                    .into_iter()
                    .filter(|config| config.enabled && config.backend.implemented())
                    .map(|config| {
                        json!({
                            "value": format!("@{}", config.name),
                            "label": config.name,
                            "kind": "agent",
                            "hint": if config.description.is_empty() {
                                config.backend.label().to_string()
                            } else {
                                format!("{} · {}", config.backend.label(), config.description)
                            },
                            "mark": "@",
                        })
                    })
                    .collect();
                serde_json::to_value(candidates)
                    .map_err(|error| anyhow::anyhow!("序列化提及候选失败: {error}"))
            }
            // ── WASM 生命周期钩子转发 ──
            SESSION_TURN_FINISHED => match parse_request::<SessionTurnFinishedRequest>(&payload) {
                Ok(request) => self
                    .handle_session_turn_finished(&request)
                    .await
                    .map(tool_ok),
                Err(error) => Err(error),
            },
            other => Err(anyhow::anyhow!("未知操作: {other}")),
        };
        match result {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(operation, %error, "Subagent 操作失败");
                tool_fail(error.to_string())
            }
        }
    }

    // ── 会话上下文 ────────────────────────────────────────────

    /// 工具调用的会话上下文（宿主权威注入；缺失即拒绝执行）。
    fn require_context() -> Result<(String, String)> {
        if let Some(context) = invocation_context() {
            return Ok((context.session_id, context.workspace));
        }
        if let Some(cell) = MCP_CONTEXT.get()
            && let Some((session, workspace)) = cell.read().ok().and_then(|guard| guard.clone())
        {
            return Ok((session, workspace));
        }
        bail!("缺少宿主注入的会话上下文，拒绝执行（不允许从参数推断会话归属）")
    }
}

#[async_trait]
impl SidecarService for SubagentService {
    async fn dispatch(&self, request: Request) -> Response {
        let payload = self.dispatch_inner(&request).await;
        // 对齐官方惯例（scheduler 等）：传输成功即 success=true，业务成败
        // 一律经 payload（{ok, summary}）表达。success=false 会被宿主当作
        // 传输级错误抛给 WASM，最终在 Core 层被误报为「未注册的工具」。
        Response {
            protocol_version: PROTOCOL_VERSION.to_string(),
            request_id: request.request_id,
            success: true,
            payload: Some(payload),
            error_code: None,
            error_message: None,
            retryable: false,
        }
    }

    async fn cancel(&self, request: &Request) -> Result<()> {
        tracing::info!(operation = %request.operation, "请求被取消（长任务在后台继续，经 Hook 回报）");
        Ok(())
    }
}

// ── 全局回调句柄（RunnerHub spawn 时读取） ─────────────────────

static RUN_HOOKS: std::sync::OnceLock<RunHooks> = std::sync::OnceLock::new();

// ── 事件流水线处理 ────────────────────────────────────────────

/// CLI 事件 → 状态机更新 + 事件历史 + Hook 投递。
fn handle_cli_event(
    store: &RuntimeStore,
    agents: &Option<AgentStore>,
    runner: &RunnerHub,
    last_message: &mut HashMap<String, String>,
    run_id: &str,
    event: CliEvent,
) {
    let Ok(mut run) = store.load_run(run_id) else {
        return;
    };
    let timestamp = now_string();
    let mut hook: Option<(HookEventType, serde_json::Value)> = None;
    let mut new_status: Option<RunStatus> = None;
    let mut summary: Option<String> = None;
    let event_type = match &event {
        CliEvent::Message { .. } => "message",
        CliEvent::Status { .. } => "status",
        CliEvent::Blocked { .. } => "blocked",
        CliEvent::ApprovalRequired { .. } => "approval_required",
        CliEvent::Completed { .. } => "completed",
        CliEvent::Failed { .. } => "failed",
    };
    match event {
        CliEvent::Message { text } => {
            // 普通输出只进事件历史与管理页，不唤醒会话（issue #480 可靠性约定）；
            // 最终回复经 Exit 时的 completed Hook 携带。
            last_message.insert(run_id.to_string(), text.clone());
            append_event(
                store,
                &run,
                event_type,
                &json!({ "text": text }),
                &timestamp,
            );
            notify(json!({ "kind": "agent_message", "run_id": run_id, "agent_id": run.agent_id }));
            return;
        }
        CliEvent::Status { status, text } => {
            let mapped = match status.as_str() {
                "working" => Some(RunStatus::Working),
                "ready" | "idle" => Some(RunStatus::Ready),
                _ => None,
            };
            if let Some(status) = mapped {
                new_status = Some(status);
            }
            append_event(
                store,
                &run,
                event_type,
                &json!({ "status": status, "text": text }),
                &timestamp,
            );
        }
        CliEvent::Blocked { text } => {
            new_status = Some(RunStatus::Blocked);
            hook = Some((HookEventType::Blocked, json!({ "text": text })));
        }
        CliEvent::ApprovalRequired { text } => {
            new_status = Some(RunStatus::ApprovalRequired);
            hook = Some((HookEventType::ApprovalRequired, json!({ "text": text })));
        }
        CliEvent::Completed { result } => {
            new_status = Some(RunStatus::Completed);
            summary = result.clone().or_else(|| {
                last_message
                    .get(run_id)
                    .map(|text| format!("（最后输出）{text}"))
            });
            hook = Some((
                HookEventType::Completed,
                json!({ "text": summary.clone().unwrap_or_default() }),
            ));
        }
        CliEvent::Failed { error } => {
            new_status = Some(RunStatus::Failed);
            summary = Some(error.clone());
            hook = Some((HookEventType::Failed, json!({ "text": error })));
        }
    }
    if let Some(status) = new_status {
        run.status = status;
    }
    if let Some(summary) = summary {
        run.summary = Some(summary);
    }
    run.updated_at = timestamp.clone();
    if run.status.is_terminal() {
        run.finished_at = Some(timestamp.clone());
    }
    let _ = store.save_run(&run);
    sync_task_status(store, &run);
    if run.status.is_terminal() {
        release_collab_activation(store, &run.activation_id);
    }
    if run.status == RunStatus::Completed
        && let Some(summary) = run.summary.as_deref()
    {
        archive_completion(store, agents.as_ref(), &run, summary);
    }
    if let Some((event_type, payload)) = hook {
        append_event(store, &run, event_type.as_str(), &payload, &timestamp);
        enqueue_hook(
            store,
            agents.as_ref(),
            &run,
            event_type,
            payload,
            &timestamp,
        );
        // 终态后子进程应自行退出；5s 未退则回收，防止注册表残留。
        if run.status.is_terminal() {
            let runner = runner.clone();
            let run_id_owned = run_id.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let _ = runner
                    .terminate(&run_id_owned, Duration::from_secs(2))
                    .await;
            });
        }
    }
    notify_run_status(&run);
}

/// 子进程退出 → 终态判定（未收到显式 completed/failed 时兜底）。
async fn handle_exit(
    store: &RuntimeStore,
    agents: &Option<AgentStore>,
    runner: &RunnerHub,
    last_message: &HashMap<String, String>,
    run_id: &str,
    info: ExitInfo,
) {
    let Ok(mut run) = store.load_run(run_id) else {
        return;
    };
    if run.status.is_terminal() {
        return;
    }
    let timestamp = now_string();
    run.finished_at = Some(timestamp.clone());
    run.updated_at = timestamp.clone();
    let interrupt_sent = runner.interrupt_sent(run_id).await;
    if run.status == RunStatus::Stopping {
        // 停止请求（停用/取消）后的退出通知：以停止意图收尾，正常退出
        // 码不改写为完成。
        run.status = RunStatus::Interrupted;
        run.summary = Some("停止请求后进程退出，运行已收尾".to_string());
        append_event(
            store,
            &run,
            "interrupted",
            &json!({ "text": run.summary.clone() }),
            &timestamp,
        );
        let _ = store.save_run(&run);
        sync_task_status(store, &run);
        release_collab_activation(store, &run.activation_id);
        notify_run_status(&run);
        return;
    }
    if info.code == Some(0) {
        run.status = RunStatus::Completed;
        let text = last_message
            .get(run_id)
            .cloned()
            .unwrap_or_else(|| "运行结束（无输出）".to_string());
        run.summary = Some(text.clone());
        archive_completion(store, agents.as_ref(), &run, &text);
        append_event(
            store,
            &run,
            "completed",
            &json!({ "text": text }),
            &timestamp,
        );
        enqueue_hook(
            store,
            agents.as_ref(),
            &run,
            HookEventType::Completed,
            json!({ "text": run.summary }),
            &timestamp,
        );
    } else if interrupt_sent {
        // 中断信号导致的退出记为已中断，不作为失败唤醒会话。
        run.status = RunStatus::Interrupted;
        run.summary = Some("运行被中断".to_string());
        append_event(store, &run, "interrupted", &json!({}), &timestamp);
    } else {
        run.status = RunStatus::Failed;
        let text = format!(
            "运行异常退出（exit={}）",
            info.code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "未知".to_string())
        );
        run.summary = Some(text.clone());
        append_event(store, &run, "failed", &json!({ "text": text }), &timestamp);
        enqueue_hook(
            store,
            agents.as_ref(),
            &run,
            HookEventType::Failed,
            json!({ "text": text }),
            &timestamp,
        );
    }
    let _ = store.save_run(&run);
    sync_task_status(store, &run);
    release_collab_activation(store, &run.activation_id);
    notify_run_status(&run);
}

/// workspace 短显示（末两段路径），视图标注用。
/// 从投递正文提取本轮运行标记（`（运行标记 r-xxxx）` 的短码）。
///
/// 取**最后一次**出现：投递正文把标记排在最后，而正文中注入的长期记忆与
/// 成员自维护工作区状态（task/plan/context）由成员自己书写，可能包含早先
/// 工作的标记字样——按首次出现取会把本轮归因到旧运行。
fn extract_run_tag(user_text: &str) -> Option<&str> {
    user_text
        .rsplit_once("（运行标记 r-")
        .and_then(|(_, rest)| rest.split('）').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// 运行是否属于指定会话（面板调度记录隔离用）：发起会话（session_id）
/// 或协作发起会话（origin_session）命中皆算；session_id 为空（无会话
/// 上下文）时不过滤，保持全局视图兜底。
fn run_in_session(run: &RunRecord, session_id: &str) -> bool {
    session_id.is_empty()
        || run.session_id == session_id
        || run.origin_session.as_deref() == Some(session_id)
}

fn short_workspace(workspace: &str) -> String {
    let mut tail: Vec<&str> = workspace.split('/').filter(|p| !p.is_empty()).collect();
    if tail.len() > 2 {
        tail = tail.split_off(tail.len() - 2);
    }
    if tail.is_empty() {
        return workspace.to_string();
    }
    format!("…{}", tail.join("/"))
}

/// 协作激活释放（自由函数版，终态出口与启动失败回滚共用）：协作激活
/// （collab- 前缀）下已无活跃运行时解除登记，写占用随终态消失。
fn release_collab_activation(store: &RuntimeStore, activation_id: &str) {
    if !activation_id.starts_with("collab-") {
        return;
    }
    let still_busy = store
        .list_runs()
        .into_iter()
        .any(|other| other.activation_id == activation_id && other.status.is_alive());
    if still_busy {
        return;
    }
    let Some(mut activation) = store
        .activations()
        .into_iter()
        .find(|activation| activation.activation_id == activation_id)
    else {
        return;
    };
    if activation.active() {
        activation.deactivated_at = Some(now_string());
        if let Err(error) = store.replace_activation(activation) {
            tracing::warn!(%activation_id, %error, "释放协作激活登记失败");
        }
    }
}

/// Run 完成后的结论归档（事件流水线与 turn 回报共用）。
fn archive_completion(
    store: &RuntimeStore,
    agents: Option<&AgentStore>,
    run: &RunRecord,
    result: &str,
) {
    let Some(agents) = agents else {
        return;
    };
    let goal = run
        .task_id
        .as_deref()
        .and_then(|task_id| store.load_task(task_id).ok())
        .map(|task| task.goal)
        .unwrap_or_else(|| match run.kind {
            RunKind::Task => "（任务）".to_string(),
            RunKind::Message => "（消息往返）".to_string(),
        });
    crate::memory::archive_run_result(agents, &run.agent_id, &goal, result);
}

/// 任务状态跟随其最新运行终态。
fn sync_task_status(store: &RuntimeStore, run: &RunRecord) {
    let Some(task_id) = run.task_id.as_deref() else {
        return;
    };
    let Ok(mut task) = store.load_task(task_id) else {
        return;
    };
    let mapped = match run.status {
        RunStatus::Completed => Some(TaskStatus::Completed),
        RunStatus::Failed => Some(TaskStatus::Failed),
        RunStatus::Cancelled => Some(TaskStatus::Cancelled),
        RunStatus::Interrupted => Some(TaskStatus::Interrupted),
        RunStatus::Working
        | RunStatus::Stopping
        | RunStatus::Blocked
        | RunStatus::ApprovalRequired => Some(TaskStatus::Running),
        RunStatus::Ready => None,
    };
    // 逻辑取消是独立裁定：运行状态变化（如停用把 Stopping 映射为执行中）
    // 不得把已取消的任务改回执行中；终态间的正常流转不受影响。
    if mapped == Some(TaskStatus::Running) && task.status == TaskStatus::Cancelled {
        return;
    }
    if let Some(status) = mapped
        && task.status != status
    {
        task.status = status;
        if task.status == TaskStatus::Completed {
            task.result_summary = run.summary.clone();
        }
        task.updated_at = now_string();
        let _ = store.save_task(&task);
    }
}

/// 追加事件历史。
fn append_event(
    store: &RuntimeStore,
    run: &RunRecord,
    event_type: &str,
    payload: &serde_json::Value,
    timestamp: &str,
) {
    // 集群协作运行自动标注发起会话：事件流即可还原「谁委托、结果回谁」。
    let mut payload = payload.clone();
    if let Some(origin) = run.origin_session.as_deref() {
        payload["origin_session"] = serde_json::json!(origin);
    }
    let record = AgentEventRecord {
        event_id: new_id(),
        agent_id: run.agent_id.clone(),
        activation_id: Some(run.activation_id.clone()),
        session_id: run.session_id.clone(),
        task_id: run.task_id.clone(),
        run_id: Some(run.run_id.clone()),
        workspace: run.workspace.clone(),
        event_type: event_type.to_string(),
        created_at: timestamp.to_string(),
        payload,
    };
    if let Err(error) = store.append_event(&record) {
        tracing::warn!(%error, "追加事件历史失败");
    }
}

/// 重要反馈：先落盘再入投递队列。
fn enqueue_hook(
    store: &RuntimeStore,
    agents: Option<&AgentStore>,
    run: &RunRecord,
    event_type: HookEventType,
    payload: serde_json::Value,
    timestamp: &str,
) {
    let agent_name = agents
        .and_then(|store| store.load(&run.agent_id).ok())
        .map(|config| config.name)
        .unwrap_or_else(|| run.agent_id.clone());
    let event = HookEvent {
        event_id: new_id(),
        agent_id: run.agent_id.clone(),
        agent_name,
        activation_id: Some(run.activation_id.clone()),
        // 集群协作运行回报投回发起方会话；主会话发起保持投回激活会话。
        conversation_id: run
            .origin_session
            .clone()
            .unwrap_or_else(|| run.session_id.clone()),
        task_id: run.task_id.clone(),
        run_id: Some(run.run_id.clone()),
        workspace: run.workspace.clone(),
        event_type,
        created_at: timestamp.to_string(),
        payload,
        attempts: 0,
        last_error: None,
    };
    if let Err(error) = store.enqueue_hook(&event) {
        tracing::warn!(%error, "Hook 事件落盘失败");
    }
}

fn notify(payload: serde_json::Value) {
    if let Ok(body) = serde_json::to_string(&payload) {
        emit_notification(NOTIFICATION_CHANNEL, body);
    }
}

fn notify_run_status(run: &RunRecord) {
    notify(json!({
        "kind": "run_status",
        "run_id": run.run_id,
        "agent_id": run.agent_id,
        "session_id": run.session_id,
        "status": run.status,
    }));
}

// ── 激活与 Workspace 写互斥 ───────────────────────────────────

impl SubagentService {
    /// 激活前置校验。
    /// 后端能力校验：策略与后端组合不支持时显式拒绝（不假装支持）。
    /// 会话型后端在专属/关联会话内执行，目录不受插件管理——隔离 worktree
    /// 无法落实；只读/独占写暂为提示级约束（需求文档已知限制）。
    fn ensure_policy_supported(backend: &BackendKind, policy: WorkspacePolicy) -> Result<()> {
        if matches!(
            backend,
            BackendKind::TiangongSession | BackendKind::AgentTeam
        ) && policy == WorkspacePolicy::IsolatedWorktree
        {
            bail!(
                "会话型后端不支持隔离工作区策略（成员在专属会话内执行，目录不受插件管理）；请改用只读或独占写策略"
            );
        }
        Ok(())
    }

    fn validate_activation(&self, config: &AgentConfig) -> Result<()> {
        if !config.enabled {
            bail!("Agent 已禁用: {}（{}）", config.name, config.id);
        }
        if !config.backend.implemented() {
            bail!(
                "运行后端「{}」将在后续阶段提供，当前版本仅支持 CLI 命令与天工会话后端",
                config.backend.label()
            );
        }
        Self::ensure_policy_supported(&config.backend, config.workspace_policy)?;
        if config.backend == BackendKind::Cli
            && config.command.as_deref().unwrap_or("").trim().is_empty()
        {
            bail!("CLI 后端缺少启动命令");
        }
        // 天工会话后端不再要求预绑定全局会话：成员×工作区专属会话在首次
        // 投递时按映射创建（见 workspace_state::session_for_workspace）。
        // 原生后端的会话存续由成员×工作区映射负责（丢失重建），
        // 老全局绑定仅作迁移源保留。
        Ok(())
    }

    /// Workspace 写互斥：读可并行，独占写只允许一个写者。
    /// 占用两处来源：激活表（显式/协作登记）与活跃运行（含停止请求中——
    /// 激活可能已解除绑定，运行结束前写占用不释放）。
    fn check_workspace_exclusive(&self, agent_id: &str, source_workspace: &str) -> Result<()> {
        self.check_workspace_exclusive_for(&[agent_id.to_string()], source_workspace)
    }

    /// 写互斥（排除名单版）：协作场景排除发起方与目标——发起方已持有
    /// 写权时，目标成员以发起方名义代执行（写入者数不增加，权限不扩大），
    /// 不与发起方自己的占用冲突。
    fn check_workspace_exclusive_for(
        &self,
        exempt: &[String],
        source_workspace: &str,
    ) -> Result<()> {
        for activation in self.store.activations() {
            if !activation.active() || exempt.contains(&activation.agent_id) {
                continue;
            }
            if activation.source_workspace == source_workspace
                && activation.workspace_policy.allows_write()
            {
                bail!(
                    "Workspace 已被 Agent「{}」以{}策略占用（{}）；同一工作区同时只允许一个写入者，可改用只读或隔离 worktree 策略",
                    activation.agent_id,
                    activation.workspace_policy.label(),
                    activation.workspace_policy
                );
            }
        }
        for run in self.store.list_runs() {
            if !run.status.is_alive() || exempt.contains(&run.agent_id) {
                continue;
            }
            let holds_write = self
                .agents
                .load(&run.agent_id)
                .map(|config| config.workspace_policy.allows_write())
                .unwrap_or(false);
            if holds_write && run.workspace == source_workspace {
                bail!(
                    "Workspace 正被 Agent「{}」的运行占用（{}，{}）；运行结束前不释放写入权",
                    run.agent_id,
                    run.run_id,
                    run.status.label()
                );
            }
        }
        Ok(())
    }

    /// isolated-worktree：为本次激活创建独立 git worktree。
    fn create_worktree(
        &self,
        activation_id: &str,
        agent_id: &str,
        source: &Path,
    ) -> Result<PathBuf> {
        let parent = self.store.root().join("worktrees").join(activation_id);
        if parent.exists() {
            std::fs::remove_dir_all(&parent).ok();
        }
        let branch = format!("subagent/{agent_id}-{activation_id}");
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(source)
            .args(["worktree", "add"])
            .arg(&parent)
            .arg("-b")
            .arg(&branch)
            .output()
            .context("启动 git 创建 worktree 失败（isolated 策略要求 Workspace 是 git 仓库）")?;
        if !output.status.success() {
            bail!(
                "创建隔离 worktree 失败: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(parent)
    }

    /// 停用时清理 worktree：无未提交修改即移除；有修改则保留并记录。
    fn remove_worktree(&self, activation: &ActivationRecord) -> Result<bool> {
        let source = Path::new(&activation.source_workspace);
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(source)
            .args(["worktree", "remove"])
            .arg(&activation.workspace)
            .output()
            .context("启动 git 移除 worktree 失败")?;
        if output.status.success() {
            return Ok(true);
        }
        tracing::warn!(
            activation_id = %activation.activation_id,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "worktree 存在未提交修改或移除失败，保留目录"
        );
        Ok(false)
    }

    /// 激活核心（工具与 UI 入口共用）。
    async fn activate_agent_in_session(
        &self,
        agent_id: &str,
        session_id: &str,
        workspace: &str,
    ) -> Result<ActivationRecord> {
        let _guard = self.ops.lock().await;
        let config = self.agents.load(agent_id)?;
        self.validate_activation(&config)?;
        let source = canonical_workspace(workspace)?;
        // 幂等：同一 (agent, session) 的活跃激活直接返回。
        for mut activation in self.store.activations() {
            if activation.active()
                && activation.agent_id == agent_id
                && activation.session_id == session_id
            {
                if activation.workspace_policy != config.workspace_policy {
                    // 策略变更：先释放旧激活再按新策略重建。
                    self.release_activation_locked(&mut activation).await?;
                } else {
                    return Ok(activation);
                }
            }
        }
        if config.workspace_policy.allows_write() {
            self.check_workspace_exclusive(agent_id, &source)?;
        }
        let activation_id = new_id();
        let (workspace, worktree_retained) = match config.workspace_policy {
            WorkspacePolicy::IsolatedWorktree => {
                let path = self.create_worktree(&activation_id, agent_id, Path::new(&source))?;
                (path.to_string_lossy().into_owned(), false)
            }
            _ => (source.clone(), false),
        };
        let record = ActivationRecord {
            activation_id,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            workspace,
            workspace_policy: config.workspace_policy,
            source_workspace: source,
            activated_at: now_string(),
            deactivated_at: None,
            worktree_retained,
        };
        self.store.upsert_activation(record.clone())?;
        notify(
            json!({ "kind": "activation", "agent_id": agent_id, "session_id": session_id, "active": true }),
        );
        Ok(record)
    }

    /// 停用核心：中断该激活上的活跃运行 → 释放 Workspace/worktree → 记录。
    async fn deactivate_agent_in_session(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<Vec<String>> {
        let _guard = self.ops.lock().await;
        let mut notes = Vec::new();
        for mut activation in self.store.activations() {
            if activation.active()
                && activation.agent_id == agent_id
                && activation.session_id == session_id
            {
                notes.extend(self.release_activation_locked(&mut activation).await?);
            }
        }
        Ok(notes)
    }

    /// 释放单个激活（持 ops 锁调用）。返回各运行的停止状态摘要（供停用
    /// 入口如实反馈：已发送/发送失败/已随退出通知结束）。
    async fn release_activation_locked(
        &self,
        activation: &mut ActivationRecord,
    ) -> Result<Vec<String>> {
        let timestamp = now_string();
        let mut stop_notes = Vec::new();
        for run in self.store.list_runs() {
            if run.activation_id != activation.activation_id || !run.status.is_alive() {
                continue;
            }
            if run.pid.is_none() {
                // 会话后端停止为通知语义（宿主内 turn 无法硬取消）：
                // 投递失败如实上抛、状态与占用不动；成功则置 Stopping，
                // 占用由运行维持到本轮收尾归因，写互斥不会失守。
                if let Err(error) = self.notify_source_session(&run, "停用中断请求").await {
                    bail!(
                        "停止请求投递失败，运行仍在继续（{}）；未确认停止，激活与占用保留",
                        error
                    );
                }
                let mut run = run;
                run.status = RunStatus::Stopping;
                run.summary = Some("停止请求已投递，等待成员会话本轮收尾确认".to_string());
                run.updated_at = timestamp.clone();
                let _ = self.store.save_run(&run);
                notify_run_status(&run);
                stop_notes.push("停止请求已投递，后台收尾尚未确认".to_string());
                continue;
            }
            // CLI 后端：温和停止——最多 3 次中断（协议帧 + SIGINT，间隔约
            // 1 秒），期间收到退出通知立即停止重试；未确认退出前运行保持
            // Stopping（占用保留），由既有退出通知路径收尾。强杀与退出确认
            // 保留给取消路径的终态裁定。
            let note = self.graceful_stop_cli_run(&run).await;
            stop_notes.push(note);
        }
        if activation.workspace_policy == WorkspacePolicy::IsolatedWorktree {
            activation.worktree_retained = !self.remove_worktree(activation)?;
        }
        activation.deactivated_at = Some(timestamp);
        self.store.replace_activation(activation.clone())?;
        notify(json!({
            "kind": "activation",
            "agent_id": activation.agent_id,
            "session_id": activation.session_id,
            "active": false,
            "worktree_retained": activation.worktree_retained,
        }));
        Ok(stop_notes)
    }

    /// CLI 运行的温和停止：最多三次中断尝试（协议帧尽力 + SIGINT 信号），
    /// 间隔约 1 秒；每轮先查运行是否已随退出通知结束。返回如实摘要——
    /// 已发送次数、失败原因或已确认结束，绝不把「已发送」说成「已退出」。
    async fn graceful_stop_cli_run(&self, run: &RunRecord) -> String {
        const MAX_ATTEMPTS: usize = 3;
        let mut sent = 0usize;
        let mut last_error: Option<String> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            if attempt > 1 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            // 退出通知（独立事件管线）可能在间隔内已收尾：先查再发。
            match self.store.load_run(&run.run_id) {
                Ok(fresh) if !fresh.status.is_alive() => {
                    return format!(
                        "运行已随退出通知结束（{}）",
                        fresh.summary.as_deref().unwrap_or("进程退出")
                    );
                }
                _ => {}
            }
            // 协议帧为约定通道尽力送达；SIGINT 为真中断信号，发送失败如实记录。
            self.runner
                .write_line(&run.run_id, &json!({ "type": "interrupt" }))
                .await
                .ok();
            match self.runner.interrupt(&run.run_id).await {
                Ok(()) => {
                    sent += 1;
                    last_error = None;
                }
                Err(error) => {
                    last_error = Some(format!("第 {attempt} 次发送失败：{error}"));
                }
            }
        }
        let summary = match (&last_error, sent) {
            (Some(error), 0) => format!("停用中断全部发送失败（{error}）"),
            (Some(error), n) => format!("中断已发送 {n} 次，{error}"),
            (None, n) => format!("中断已发送 {n} 次，等待后台退出通知"),
        };
        if let Ok(mut fresh) = self.store.load_run(&run.run_id)
            && fresh.status.is_alive()
        {
            fresh.status = RunStatus::Stopping;
            fresh.summary = Some(summary.clone());
            fresh.updated_at = now_string();
            let _ = self.store.save_run(&fresh);
            notify_run_status(&fresh);
        }
        format!("停用后运行停止请求中：{summary}")
    }

    /// 查找 (agent, session) 的活跃激活。
    /// 自动激活：成员在当前会话未激活时自动建立激活（默认只读策略）。
    /// Subagent 创建后所有会话自动可用——@成员或派活即被调度，
    /// 无需手动激活步骤。
    fn auto_activate(
        &self,
        config: &AgentConfig,
        session_id: &str,
        workspace: &str,
    ) -> Result<ActivationRecord> {
        if let Ok(activation) = self.active_activation(&config.id, session_id) {
            return Ok(activation);
        }
        self.validate_activation(config)?;
        let activation = ActivationRecord {
            activation_id: format!("auto-{}", new_id()),
            agent_id: config.id.clone(),
            session_id: session_id.to_string(),
            workspace: workspace.to_string(),
            workspace_policy: WorkspacePolicy::ReadOnly,
            source_workspace: workspace.to_string(),
            activated_at: now_string(),
            deactivated_at: None,
            worktree_retained: false,
        };
        self.store.replace_activation(activation.clone())?;
        tracing::info!(
            agent_id = %config.id,
            session_id,
            "成员自动激活到当前会话（默认只读）"
        );
        Ok(activation)
    }

    fn active_activation(&self, agent_id: &str, session_id: &str) -> Result<ActivationRecord> {
        self.store
            .activations()
            .into_iter()
            .find(|activation| {
                activation.active()
                    && activation.agent_id == agent_id
                    && activation.session_id == session_id
            })
            .ok_or_else(|| {
                anyhow::anyhow!("Agent 未在当前会话激活，请先调用 activate_agent / 在管理页激活")
            })
    }

    /// 发送消息：有活跃运行则注入，否则新建轻量消息运行。
    /// 解析成员：优先按 ID，其次按名称唯一匹配（集群协作常用名字指人）。
    fn resolve_agent(&self, agent_id: &str) -> Result<AgentConfig> {
        let key = agent_id.trim();
        if let Ok(config) = self.agents.load(key) {
            return Ok(config);
        }
        let matches: Vec<AgentConfig> = self
            .agents
            .list()
            .into_iter()
            .filter(|config| config.name == key)
            .collect();
        match matches.len() {
            1 => Ok(matches[0].clone()),
            0 => bail!("Agent 不存在: {agent_id}"),
            _ => bail!("名称「{agent_id}」匹配多个成员，请改用 agent_id 指定"),
        }
    }

    /// 发起会话是否为某成员的后端会话（专属/关联）——集群协作识别。
    fn collaboration_origin(&self, session_id: &str) -> Option<AgentConfig> {
        // 会话归属经成员×工作区映射反查（老全局绑定在映射内兜底）。
        let session_owner = crate::workspace_state::agent_for_session(&self.agents, session_id);
        self.agents.list().into_iter().find(|config| {
            matches!(
                config.backend,
                BackendKind::TiangongSession | BackendKind::AgentTeam
            ) && session_owner.as_deref() == Some(config.id.as_str())
        })
    }

    /// 成员最近的激活 Workspace（活跃优先，其次最新历史记录）。
    fn latest_activation_workspace(&self, agent_id: &str) -> Option<String> {
        let mut history: Vec<ActivationRecord> = self
            .store
            .activations()
            .into_iter()
            .filter(|activation| activation.agent_id == agent_id)
            .collect();
        history.sort_by(|a, b| b.activated_at.cmp(&a.activated_at));
        history
            .iter()
            .find(|activation| activation.active())
            .or_else(|| history.first())
            .map(|activation| activation.workspace.clone())
    }

    /// 合成集群协作激活：目标成员未在发起会话激活时的运行载体。
    /// 与普通派活同一套规则：策略能力校验、写互斥检查（激活表 + 活跃运行
    /// 占用），并登记进统一激活表——后续申请能看到本协作的占用，随运行
    /// 终态自动释放（release_collab_activation_if_idle）。
    /// Workspace 解析：目标最近激活 → 发起方最近激活 → 用户 home。
    fn synthetic_collab_activation(
        &self,
        target: &AgentConfig,
        origin: &AgentConfig,
        origin_session: &str,
    ) -> Result<ActivationRecord> {
        Self::ensure_policy_supported(&target.backend, target.workspace_policy)?;
        // 协作 workspace 域绑定发起会话的工作区（发起成员专属会话的 cwd）：
        // 不同 workspace 的协作链互不漂移；发起会话无有效 cwd 才回退
        // 目标/发起方最近激活，最终回退用户 home。
        let workspace = crate::sessions::session_workspace(origin_session)
            .or_else(|| self.latest_activation_workspace(&target.id))
            .or_else(|| self.latest_activation_workspace(&origin.id))
            .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| ".".to_string()));
        if !Path::new(&workspace).is_dir() {
            bail!("协作 Workspace 不存在: {workspace}");
        }
        if target.workspace_policy.allows_write() {
            self.check_workspace_exclusive_for(&[target.id.clone(), origin.id.clone()], &workspace)
                .context("协作运行无法取得工作区写入权")?;
        }
        let record = ActivationRecord {
            activation_id: format!("collab-{}", new_id()),
            agent_id: target.id.clone(),
            session_id: origin_session.to_string(),
            workspace: workspace.clone(),
            workspace_policy: target.workspace_policy,
            source_workspace: workspace,
            activated_at: now_string(),
            deactivated_at: None,
            worktree_retained: false,
        };
        // 登记进统一激活表：后续派活/协作的写互斥检查可见本占用。
        self.store.replace_activation(record.clone())?;
        Ok(record)
    }

    /// 协作激活释放：该协作激活下已无活跃运行时解除登记（终态统一出口
    /// 调用；普通激活由停用路径管理，不经过此处）。
    fn release_collab_activation_if_idle(&self, run: &RunRecord) {
        release_collab_activation(&self.store, &run.activation_id);
    }

    async fn send_message_core(
        &self,
        agent_id: &str,
        session_id: &str,
        workspace: &str,
        content: &str,
        attachments: &[AttachmentPayload],
    ) -> Result<SendOutcome> {
        if content.trim().is_empty() {
            bail!("消息内容不能为空");
        }
        let _guard = self.ops.lock().await;
        let config = self.resolve_agent(agent_id)?;
        // 集群协作识别：发起会话是某成员的后端会话（专属/关联）时，视为
        // 该成员向目标的定向协作——目标无需在发起会话激活，运行以合成
        // 协作激活执行，完成回报投回发起会话（origin_session）。
        let origin_agent = self.collaboration_origin(session_id);
        let origin = origin_agent.as_ref().map(|agent| CollabOrigin {
            session: session_id,
            label: format!("成员「{}」（{}）", agent.name, agent.id),
        });
        // 消息意图关联（有状态成员语义）：追问、补充与纠偏是「对进行中
        // 工作的追加输入」，不是新执行——发起方在该成员上若有等待中的
        // 运行（本会话派活或协作委托），消息注入该运行，不新建。
        // 判定先于任何资源申请：复用工作时不得额外创建激活记录。
        let injectable = self
            .store
            .list_runs()
            .into_iter()
            .filter(|run| {
                run.agent_id == config.id
                    && run.status.is_alive()
                    && run.status != RunStatus::Stopping
                    // 归属判定：本会话发起（协作 origin 或主会话激活）的运行。
                    && match run.origin_session.as_deref() {
                        Some(origin) => origin == session_id,
                        None => run.session_id == session_id,
                    }
                    // 后端匹配：会话型注入走投递，CLI 注入走进程通道。
                    && if config.backend == BackendKind::Cli {
                        run.pid.is_some()
                    } else {
                        run.pid.is_none()
                    }
            })
            .max_by_key(|run| run.run_id.clone());
        if let Some(run) = injectable {
            if config.backend == BackendKind::Cli {
                // 补充帧附件约定与启动帧一致（无附件不带字段）。
                let mut frame = json!({ "type": "user_message", "content": content });
                if !attachments.is_empty() {
                    frame["attachments"] =
                        serde_json::to_value(attachments).context("序列化补充帧附件失败")?;
                }
                self.runner.write_line(&run.run_id, &frame).await?;
                append_event(
                    &self.store,
                    &run,
                    "user_message",
                    &json!({ "text": content }),
                    &now_string(),
                );
                return Ok(SendOutcome {
                    run_id: run.run_id,
                    injected_into_run: true,
                });
            }
            // 补充消息沿实际执行会话（成员×工作区映射会话）路由；
            // 老运行数据无该字段时回退旧全局绑定（旧模型语义即全局会话）。
            let source_session = run
                .executor_session
                .as_deref()
                .or(config.session_id.as_deref())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("运行缺少执行会话，无法注入补充消息"))?
                .to_string();
            // 轻量载体仅供投递正文提示（workspace 来自该成员最近激活），
            // 不落盘——复用工作进行中，不新增占用记录。
            let workspace = crate::sessions::session_workspace(session_id)
                .or_else(|| self.latest_activation_workspace(&config.id))
                .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| ".".to_string()));
            let carrier = ActivationRecord {
                activation_id: run.activation_id.clone(),
                agent_id: config.id.clone(),
                session_id: session_id.to_string(),
                workspace: workspace.clone(),
                workspace_policy: config.workspace_policy,
                source_workspace: workspace,
                activated_at: now_string(),
                deactivated_at: None,
                worktree_retained: false,
            };
            // 携带原运行的标记投递：整轮（含补充）按标记归因回原运行；
            // 正文标注「补充」，归因侧据此识别修订链。
            let run_tag: String = run.run_id.chars().rev().take(8).collect();
            let run_tag: String = run_tag.chars().rev().collect();
            let supplement_content = format!("（补充消息）{content}");
            self.deliver_session_message(
                &config,
                &carrier,
                &source_session,
                None,
                Some(&supplement_content),
                origin.as_ref(),
                Some(&run_tag),
                None,
                attachments,
            )
            .await?;
            append_event(
                &self.store,
                &run,
                "supplement",
                &json!({ "text": content }),
                &now_string(),
            );
            notify_run_status(&run);
            return Ok(SendOutcome {
                run_id: run.run_id,
                injected_into_run: true,
            });
        }
        let activation = match &origin_agent {
            Some(origin_agent) => {
                self.synthetic_collab_activation(&config, origin_agent, session_id)?
            }
            None => self.auto_activate(&config, session_id, workspace)?,
        };
        let run = self
            .spawn_run(
                &config,
                &activation,
                RunKind::Message,
                None,
                Some(content),
                origin.as_ref(),
                attachments,
            )
            .await?;
        Ok(SendOutcome {
            run_id: run.run_id,
            injected_into_run: false,
        })
    }

    /// 提交正式任务。
    async fn submit_task_core(
        &self,
        agent_id: &str,
        session_id: &str,
        workspace: &str,
        goal: &str,
        completion_criteria: Option<&str>,
        attachments: &[AttachmentPayload],
    ) -> Result<SubmitOutcome> {
        if goal.trim().is_empty() {
            bail!("任务目标不能为空");
        }
        let _guard = self.ops.lock().await;
        let config = self.resolve_agent(agent_id)?;
        let origin_agent = self.collaboration_origin(session_id);
        let origin = origin_agent.as_ref().map(|agent| CollabOrigin {
            session: session_id,
            label: format!("成员「{}」（{}）", agent.name, agent.id),
        });
        let activation = match &origin_agent {
            Some(origin_agent) => {
                self.synthetic_collab_activation(&config, origin_agent, session_id)?
            }
            None => self.auto_activate(&config, session_id, workspace)?,
        };
        let timestamp = now_string();
        let task = TaskRecord {
            task_id: new_id(),
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            goal: goal.trim().to_string(),
            completion_criteria: completion_criteria
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            status: TaskStatus::Running,
            runs: Vec::new(),
            result_summary: None,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        self.store.save_task(&task)?;
        let run = self
            .spawn_run(
                &config,
                &activation,
                RunKind::Task,
                Some(&task),
                None,
                origin.as_ref(),
                attachments,
            )
            .await?;
        Ok(SubmitOutcome {
            task_id: task.task_id,
            run_id: run.run_id,
        })
    }

    /// 启动运行（持 ops 锁调用）：CLI 后端启动子进程，会话后端投递关联会话。
    /// 启动失败时回滚协作登记（无活跃运行即释放），不留残留占用。
    #[allow(clippy::too_many_arguments)]
    async fn spawn_run(
        &self,
        config: &AgentConfig,
        activation: &ActivationRecord,
        kind: RunKind,
        task: Option<&TaskRecord>,
        message: Option<&str>,
        origin: Option<&CollabOrigin<'_>>,
        attachments: &[AttachmentPayload],
    ) -> Result<RunRecord> {
        let collab_activation = activation.activation_id.clone();
        let result = self
            .spawn_run_inner(config, activation, kind, task, message, origin, attachments)
            .await;
        if result.is_err() {
            release_collab_activation(&self.store, &collab_activation);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_run_inner(
        &self,
        config: &AgentConfig,
        activation: &ActivationRecord,
        kind: RunKind,
        task: Option<&TaskRecord>,
        message: Option<&str>,
        origin: Option<&CollabOrigin<'_>>,
        attachments: &[AttachmentPayload],
    ) -> Result<RunRecord> {
        if self.shutting_down.load(Ordering::Acquire) {
            bail!("Subagent 总线正在关闭，不再接受新任务");
        }
        let timestamp = now_string();
        let mut run = RunRecord {
            run_id: new_id(),
            task_id: task.map(|task| task.task_id.clone()),
            agent_id: config.id.clone(),
            activation_id: activation.activation_id.clone(),
            session_id: activation.session_id.clone(),
            executor_session: None,
            kind,
            status: RunStatus::Working,
            pid: None,
            workspace: activation.workspace.clone(),
            origin_session: origin.map(|origin| origin.session.to_string()),
            created_at: timestamp.clone(),
            updated_at: timestamp,
            finished_at: None,
            summary: None,
        };
        match config.backend {
            BackendKind::TiangongSession | BackendKind::AgentTeam => {
                // 成员×工作区固定长期会话：(agent, workspace) → session 映射。
                // 首次生成会话号并随消息携带 workspace 由宿主创建（cwd 即
                // 工作区）；复用映射会话不再携带（会话创建后工作区不修改，
                // 上下文过长走 Core 压缩）。老成员的全局绑定一次性迁移。
                let (source_session, session_is_new) =
                    crate::workspace_state::session_for_workspace(
                        &self.agents,
                        &config.id,
                        &activation.workspace,
                        config.session_id.as_deref(),
                    )?;
                // 运行标记：投递正文携带 run 短码，turn 完成回报按标记精确
                // 归因（同一成员并发多运行时不串任务）。
                let run_tag: String = run.run_id.chars().rev().take(8).collect();
                let run_tag: String = run_tag.chars().rev().collect();
                let outcome = self
                    .deliver_session_message(
                        config,
                        activation,
                        &source_session,
                        task,
                        message,
                        origin,
                        Some(&run_tag),
                        if session_is_new {
                            Some(activation.workspace.as_str())
                        } else {
                            None
                        },
                        attachments,
                    )
                    .await?;
                run.executor_session = Some(source_session.clone());
                run.summary = Some(outcome);
                self.store.save_run(&run)?;
                if let Some(task) = task {
                    let mut task = task.clone();
                    task.runs.push(run.run_id.clone());
                    task.updated_at = now_string();
                    let _ = self.store.save_task(&task);
                }
                append_event(
                    &self.store,
                    &run,
                    "run_started",
                    &json!({
                        "backend": if config.backend == BackendKind::AgentTeam { "agent_team" } else { "tiangong_session" },
                        "source_session": source_session,
                    }),
                    &now_string(),
                );
                notify_run_status(&run);
                Ok(run)
            }
            _ => {
                self.spawn_cli_run(config, activation, run, task, message, attachments)
                    .await
            }
        }
    }

    /// 天工会话后端：把消息/任务投递给关联会话（携带长期指令与记忆摘要）。
    #[allow(clippy::too_many_arguments)]
    async fn deliver_session_message(
        &self,
        config: &AgentConfig,
        activation: &ActivationRecord,
        source_session: &str,
        task: Option<&TaskRecord>,
        message: Option<&str>,
        origin: Option<&CollabOrigin<'_>>,
        run_tag: Option<&str>,
        session_workspace: Option<&str>,
        attachments: &[AttachmentPayload],
    ) -> Result<String> {
        let instructions = self.agents.instructions(&config.id).unwrap_or_default();
        let memory = crate::memory::injection_snapshot(&self.agents, &config.id);
        let origin_label = origin
            .map(|origin| origin.label.as_str())
            .unwrap_or("主会话");
        let mut body = String::new();
        match (task, message) {
            (Some(task), _) => {
                body.push_str(&format!(
                    "【Subagent 任务】来自{origin_label}：\n目标：{}\n",
                    task.goal
                ));
                if let Some(criteria) = task.completion_criteria.as_deref() {
                    body.push_str(&format!("完成条件：{criteria}\n"));
                }
            }
            (None, Some(message)) => {
                body.push_str(&format!(
                    "【Subagent 消息】来自{origin_label}：\n{message}\n"
                ));
            }
            (None, None) => {
                body.push_str(&format!("【Subagent 消息】来自{origin_label}。\n"));
            }
        }
        body.push_str(&format!(
            "\n【任务工作区】{}\n本次任务的所有文件操作（读/写/搜索/执行命令）都必须在此目录下进行——这是发起会话的工作区，你的会话默认目录可能与它不同，请始终使用基于此目录的绝对路径。如需其他成员协助，可用 send_agent_message 向其发送协作消息。",
            activation.workspace
        ));
        // 回复地址 = 发起方会话（协作时为协作发起成员的会话，主会话
        // 发起时为激活源会话）——source_session 是本条消息的接收会话
        // （成员执行会话），不得误作回复地址。
        let reply_to = origin
            .map(|origin| origin.session)
            .unwrap_or(activation.session_id.as_str());
        body.push_str(&format!(
            "\n\n【回复地址】会话 {reply_to}\n完成、阻塞或失败时，用 report_agent_result 向该地址回报结果（to_session 填此地址）；同一工作区有多项工作时在 task.md 中区分各自进展。"
        ));
        if !instructions.trim().is_empty() {
            body.push_str(&format!("\n\n【长期指令】\n{}", instructions.trim()));
        }
        if !memory.is_empty() {
            body.push_str(&format!("\n\n【长期记忆】\n{memory}"));
        }
        // 成员自维护工作区状态注入：工作含义由成员维护，sidecar 只透传。
        let state_section = crate::workspace_state::injection_summary(
            &self.agents,
            &config.id,
            &activation.workspace,
        );
        body.push_str(&state_section);
        body.push_str("\n\n【成长约定】用 update_workspace_state 在 task.md 维护每项工作的发起者（其回复地址）、目标、进展、依赖与待反馈事项——同一工作区的多个发起方靠它区分，总线不替你猜测消息归属。完成本次工作后：把可复用经验（成功做法、踩坑、用户偏好等，一行一条、结论式）用 append_agent_memory 追加到 lessons.md；确实学到稳定的新规则时，用 append_agent_instructions 并入你的长期指令（追加式，勿重复已有内容）。");
        if let Some(run_tag) = run_tag {
            body.push_str(&format!("\n\n（运行标记 r-{run_tag}）"));
        }
        // 工作区仅随新会话的首条消息传递（宿主创建时落为 cwd）；复用已有
        // 会话不携带——会话创建后工作区固定，不随消息修改。附件随消息
        // 携带（本地路径经 Server 归档进成员会话），正文标注数量供成员感知。
        if !attachments.is_empty() {
            body.push_str(&format!(
                "\n\n【随消息附件】本条消息附带 {} 个附件（已随消息送达，按内容中的资源引用处理）。",
                attachments.len()
            ));
        }
        crate::delivery::deliver_message(
            &self.http,
            source_session,
            &body,
            session_workspace,
            attachments,
            tiangong_plugin_subagent_protocol::render::delivery(
                task.is_some(),
                origin_label,
                run_tag,
            ),
        )
        .await?;
        Ok(format!("已投递到关联会话 {source_session}，等待其完成回复"))
    }

    /// CLI 成员的 MCP 接入引导：sidecar 二进制外部直起即 stdio MCP server
    ///（标准 JSON-RPC），透传绝对路径与协作说明——注册与否、注册到用户级
    /// 还是项目级由 CLI 工具与其用户决定，总线不代写任何外部配置文件。
    /// 去重：该 workspace 已完成过 MCP 握手（mcp_access 登记）即视为已
    /// 接入，不再携带引导。
    fn mcp_guide(workspace: &str) -> Option<McpGuide> {
        if crate::mcp_access::is_connected(workspace) {
            return None;
        }
        let command = std::env::current_exe().ok()?.to_string_lossy().into_owned();
        Some(McpGuide {
            server: "tiangong-subagent",
            command,
            workspace: workspace.to_string(),
            hint: "天工协作接入（可选）：将 command 字段指向的可执行文件注册为本工具的 MCP server（stdio JSON-RPC，工作目录建议使用 workspace 字段），即可调用 list_agents / send_agent_message / load_workspace_state / list_pending_work 与天工成员协作（查看同伴、传递消息、汇报进展）。已注册过时忽略本段。",
        })
    }

    /// CLI 后端：启动子进程（持 ops 锁调用）。
    #[allow(clippy::too_many_arguments)]
    async fn spawn_cli_run(
        &self,
        config: &AgentConfig,
        activation: &ActivationRecord,
        mut run: RunRecord,
        task: Option<&TaskRecord>,
        message: Option<&str>,
        attachments: &[AttachmentPayload],
    ) -> Result<RunRecord> {
        let command = config.command.as_deref().unwrap_or_default();
        let instructions = self.agents.instructions(&config.id).unwrap_or_default();
        let memory = crate::memory::injection_snapshot(&self.agents, &config.id);
        let cwd = PathBuf::from(&activation.workspace);
        if !cwd.is_dir() {
            bail!("运行 Workspace 不存在: {}", cwd.display());
        }
        let begin = BeginFrame {
            r#type: "begin",
            agent_id: &config.id,
            agent_name: &config.name,
            instructions: &instructions,
            memory: &memory,
            activation_id: &activation.activation_id,
            session_id: &activation.session_id,
            workspace: &activation.workspace,
            task_id: task.map(|task| task.task_id.as_str()),
            goal: task.map(|task| task.goal.as_str()),
            completion_criteria: task.and_then(|task| task.completion_criteria.as_deref()),
            message,
            input: message.or_else(|| task.map(|task| task.goal.as_str())),
            // 附件本地路径随启动帧透传（无附件省略字段，老协议实现兼容）。
            attachments: (!attachments.is_empty()).then_some(attachments),
            // MCP 接入引导：sidecar 自身即 stdio MCP server（外部直起形态），
            // 透传路径与说明由 CLI 工具自行注册——不代写任何外部配置文件。
            mcp: Self::mcp_guide(&activation.workspace),
        };
        let hooks = RUN_HOOKS
            .get()
            .cloned()
            .unwrap_or_else(|| panic!("RunnerHooks 未初始化"));
        let pid = self
            .runner
            .spawn(&run.run_id, command, &cwd, &[], &begin, hooks)
            .await?;
        run.pid = Some(pid);
        self.store.save_run(&run)?;
        if let Some(task) = task {
            let mut task = task.clone();
            task.runs.push(run.run_id.clone());
            task.updated_at = now_string();
            let _ = self.store.save_task(&task);
        }
        append_event(
            &self.store,
            &run,
            "run_started",
            &json!({ "pid": pid, "kind": run.kind }),
            &now_string(),
        );
        notify_run_status(&run);
        Ok(run)
    }

    /// 中断运行（CLI：协议帧+信号；会话后端：投递停止通知）。
    async fn interrupt_run_core(&self, run_id: &str) -> Result<()> {
        let _guard = self.ops.lock().await;
        let run = self.store.load_run(run_id)?;
        if !run.status.is_alive() {
            bail!("运行已结束（{}），无需中断", run.status.label());
        }
        if run.pid.is_none() {
            // 天工会话后端：通知式中断（宿主内 turn 无法硬取消）。
            if let Err(error) = self.notify_source_session(&run, "中断请求").await {
                tracing::warn!(run_id = %run.run_id, %error, "中断通知投递失败（中断为尽力语义）");
            }
        } else {
            self.runner
                .write_line(run_id, &json!({ "type": "interrupt" }))
                .await
                .ok();
            // 信号在 Seatbelt 沙箱内会被拒绝（process-signal 默认不放行）：
            // 协议帧已送达即视为中断发起，信号尽力而为。
            if let Err(error) = self.runner.interrupt(run_id).await {
                tracing::debug!(run_id, %error, "中断信号未送达（沙箱内预期），已依赖协议帧");
            }
        }
        append_event(&self.store, &run, "interrupted", &json!({}), &now_string());
        notify_run_status(&run);
        Ok(())
    }

    /// 取消运行（终态 cancelled）：CLI 走协议帧 → stdin EOF → 信号宽限；
    /// 会话后端为调度标记——立即落终态并尽力通知，不等待执行侧真实取消。
    async fn cancel_run_core(&self, run_id: &str) -> Result<()> {
        let _guard = self.ops.lock().await;
        let mut run = self.store.load_run(run_id)?;
        if !run.status.is_alive() {
            bail!("运行已结束（{}），无需取消", run.status.label());
        }
        let timestamp = now_string();
        if run.pid.is_none() {
            // 会话后端：取消是发起方的调度裁定，仅作消息标记——立即把运行
            // 置 Cancelled 终态并释放占用，不等执行侧真实取消。成员执行
            // 会话按 workspace 固定共享（当前轮次可能正执行其他发起方的
            // 运行），硬取消会话轮次会跨运行误伤；标记后执行侧轮次自然
            // 收尾，迟到结果只记事件（run 已终态，归因链不再改写结论）。
            run.status = RunStatus::Cancelled;
            run.summary = Some("任务已取消（调度标记，不再需要结果）".to_string());
            run.finished_at = Some(timestamp.clone());
            run.updated_at = timestamp.clone();
            self.store.save_run(&run)?;
            if let Some(task_id) = run.task_id.clone()
                && let Ok(mut task) = self.store.load_task(&task_id)
                && task.status != TaskStatus::Completed
            {
                task.status = TaskStatus::Cancelled;
                task.updated_at = now_string();
                let _ = self.store.save_task(&task);
            }
            self.release_collab_activation_if_idle(&run);
            append_event(
                &self.store,
                &run,
                "cancelled",
                &json!({ "kind": "session-marker" }),
                &timestamp,
            );
            notify_run_status(&run);
            // 尽力投递取消通知（纯消息标记）：让成员知道结果不再需要，
            // 按需尽快收尾；投递失败不影响取消成立。
            if let Err(error) = self.notify_source_session(&run, "取消请求").await {
                tracing::warn!(run_id = %run.run_id, %error, "取消通知投递失败（标记已生效，不影响取消）");
            }
            return Ok(());
        }
        self.runner
            .write_line(run_id, &json!({ "type": "cancel" }))
            .await
            .ok();
        // 先断 stdin（协议约定 EOF 即退出），再等信号宽限；
        // 沙箱内信号被拒时靠 EOF 与宿主退出级联兜底。terminate 确认进程
        // 退出后才落终态——CLI 的取消即实际停止。
        self.runner.close_stdin(run_id).await.ok();
        self.runner
            .terminate(run_id, Duration::from_secs(3))
            .await?;
        run.status = RunStatus::Cancelled;
        run.finished_at = Some(timestamp.clone());
        run.updated_at = timestamp.clone();
        run.summary = Some("已取消".to_string());
        let _ = self.store.save_run(&run);
        sync_task_status(&self.store, &run);
        self.release_collab_activation_if_idle(&run);
        append_event(&self.store, &run, "cancelled", &json!({}), &now_string());
        notify_run_status(&run);
        Ok(())
    }

    /// 会话后端运行控制通知（中断/取消尽力语义）。
    async fn notify_source_session(&self, run: &RunRecord, action: &str) -> Result<()> {
        let config = self.agents.load(&run.agent_id)?;
        // 控制通知沿实际执行会话路由；老运行数据回退旧全局绑定。
        let source_session = run
            .executor_session
            .as_deref()
            .or(config.session_id.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("运行缺少执行会话，无法投递{action}通知"))?;
        let body = format!(
            "【Subagent 控制】会话「{}」（{}）对刚才提交的请求发起{}：无需继续处理，若已在处理请尽快收尾并说明未完成的部分。",
            config.name, config.id, action
        );
        crate::delivery::deliver_message(
            &self.http,
            source_session,
            &body,
            None,
            &[],
            tiangong_plugin_subagent_protocol::render::control(&config.name, &config.id),
        )
        .await
    }

    /// 关联会话本轮完成（WASM on_turn_finished 转发）：
    /// 本轮用户消息带 Subagent 标记时，把完成归因到该源会话上最新的活跃运行。
    async fn handle_session_turn_finished(
        // 占位注释（实际插入在拿到 request 之后，见下）
        &self,
        request: &SessionTurnFinishedRequest,
    ) -> Result<String> {
        let _guard = self.ops.lock().await;
        if !request.user_text.contains("【Subagent") {
            return Ok("本轮非 Subagent 投递触发，忽略".to_string());
        }
        let request_session_owner =
            crate::workspace_state::agent_for_session(&self.agents, &request.session_id);
        // 找到以该会话为源的后端 Agent（关联会话后端与原生后端的专属会话）。
        let agents: Vec<AgentConfig> = self
            .agents
            .list()
            .into_iter()
            .filter(|config| {
                matches!(
                    config.backend,
                    BackendKind::TiangongSession | BackendKind::AgentTeam
                ) && request_session_owner.as_deref() == Some(config.id.as_str())
            })
            .collect();
        if agents.is_empty() {
            return Ok(format!("没有以会话 {} 为源的 Subagent", request.session_id));
        }
        let agent_ids: Vec<&str> = agents.iter().map(|config| config.id.as_str()).collect();
        let alive_runs: Vec<RunRecord> = self
            .store
            .list_runs()
            .into_iter()
            .filter(|run| {
                run.pid.is_none()
                    && run.status.is_alive()
                    && agent_ids.contains(&run.agent_id.as_str())
            })
            .collect();
        // 控制通知（停止/取消请求）触发的回复不是任务结果，忽略——
        // 只认正文以【Subagent 控制】开头的控制通知本身；任务正文任何
        // 位置引用这段字样不受影响（仍按运行标记正常归因）。
        if request
            .user_text
            .trim_start()
            .starts_with("【Subagent 控制】")
        {
            return Ok("本轮为控制通知的回复，不归因任何运行".to_string());
        }
        // 归因只认运行标记：无标记的轮次（用户在成员会话直接对话等）
        // 不属于任何任务投递，忽略；绝不泛化为「最新活跃运行」。
        let tag = extract_run_tag(&request.user_text);
        let Some(tag) = tag else {
            return Ok("本轮回报缺少运行标记（非任务投递触发），忽略".to_string());
        };
        let Some(mut run) = alive_runs
            .iter()
            .find(|run| run.run_id.ends_with(&tag))
            .cloned()
        else {
            // 补充轮次对应的原运行已终结：只把本轮执行事实（系统抓取的
            // 最终文本）追加为事件，供观测与诊断——不自动改写原运行结论、
            // 不同步任务结论、不代发修订回报；修订成果由成员经
            // report_agent_result 主动反馈（与普通收尾同一约定）。
            if request.user_text.contains("（补充消息）")
                && let Some(finished) = self.store.list_runs().into_iter().find(|run| {
                    // 归属校验：只匹配本会话所属成员的工作。
                    run.run_id.ends_with(&tag)
                        && run.status == RunStatus::Completed
                        && agents.iter().any(|config| {
                            config.id == run.agent_id
                                && request_session_owner.as_deref() == Some(config.id.as_str())
                        })
                })
            {
                let text = request.assistant_text.trim();
                if text.is_empty() {
                    return Ok("补充轮无回复文本，忽略".to_string());
                }
                let timestamp = now_string();
                append_event(
                    &self.store,
                    &finished,
                    "supplement_turn",
                    &json!({ "text": text }),
                    &timestamp,
                );
                return Ok("补充轮次已记录执行事件（业务修订由成员主动回报）".to_string());
            }
            return Ok(format!(
                "运行标记 r-{tag} 无匹配的活跃运行（迟到或重复回报），忽略"
            ));
        };
        let timestamp = now_string();
        // 轮次结束只表示本轮处理结束，不替成员决定任务状态：成员已明确
        // 表态（blocked 等待补充 / 待审批）的运行，轮次结束仅记录事件、
        // 状态保持——只有未表态（working/ready）的运行才由轮次结果收尾；
        // 停止请求中（Stopping）例外：其轮次结束正是「停止确认」路径，
        // 落取消终态。
        if matches!(run.status, RunStatus::Blocked | RunStatus::ApprovalRequired) {
            append_event(
                &self.store,
                &run,
                "turn_ended",
                &json!({ "held": run.status.label() }),
                &timestamp,
            );
            return Ok(format!(
                "本轮处理结束；运行状态保持「{}」（成员已明确表态，不由轮次收尾改写）",
                run.status.label()
            ));
        }
        // Stopping（停止/取消请求已投递）的收尾确认：终态一律 Cancelled，
        // 本轮回复内容不改变已作出的取消裁定。
        let status = if run.status == RunStatus::Stopping {
            RunStatus::Cancelled
        } else {
            match request.turn_status.as_deref() {
                Some("cancelled") => RunStatus::Cancelled,
                Some("failed") => RunStatus::Failed,
                _ => RunStatus::Completed,
            }
        };
        let text = if run.status == RunStatus::Stopping {
            "执行侧已收尾，取消确认".to_string()
        } else if request.assistant_text.trim().is_empty() {
            match status {
                RunStatus::Cancelled => "运行被取消".to_string(),
                RunStatus::Failed => "运行失败（无回复文本）".to_string(),
                _ => "运行完成（无回复文本）".to_string(),
            }
        } else {
            request.assistant_text.trim().to_string()
        };
        run.status = status;
        // 轮次结束只记录执行事实：本轮执行完毕与最终文本（来源标记为
        // 系统抓取，供观测区分「成员主动投递」）。业务回报由成员经
        // report_agent_result 主动发送——这里不代替成员生成完成回报，
        // 也不代为归档成果（工作含义与反馈内容由成员维护）。
        run.summary = Some(format!("[轮次收尾] {text}"));
        run.finished_at = Some(timestamp.clone());
        run.updated_at = timestamp.clone();
        self.store.save_run(&run)?;
        sync_task_status(&self.store, &run);
        self.release_collab_activation_if_idle(&run);
        let (event_type, payload) = match status {
            RunStatus::Completed => ("completed", json!({ "text": text })),
            RunStatus::Failed => ("failed", json!({ "text": text })),
            _ => ("cancelled", json!({ "text": text })),
        };
        append_event(&self.store, &run, event_type, &payload, &timestamp);
        notify_run_status(&run);
        Ok(format!(
            "运行 {} 已随源会话本轮结束归因（{}）",
            run.run_id,
            run.status.label()
        ))
    }

    /// 优雅关闭：中断全部 managed 运行并落盘（宿主退出流程 / 终止信号调用）。
    pub async fn begin_shutdown(&self) {
        if self.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        let _guard = self.ops.lock().await;
        let timestamp = now_string();
        let runs = self.store.list_runs();
        let live: Vec<RunRecord> = runs
            .into_iter()
            .filter(|run| run.status.is_alive())
            .collect();
        if !live.is_empty() {
            // 优雅中断 → 宽限 → 强杀，与 issue 退出策略一致。
            self.runner.shutdown_all(Duration::from_secs(3)).await;
        }
        for mut run in live {
            run.status = RunStatus::Interrupted;
            run.finished_at = Some(timestamp.clone());
            run.updated_at = timestamp.clone();
            run.summary = Some("天工退出，运行已中断".to_string());
            let _ = self.store.save_run(&run);
            sync_task_status(&self.store, &run);
            release_collab_activation(&self.store, &run.activation_id);
        }
        tracing::info!("Subagent 总线已停止全部 managed 运行实例");
    }

    // ── 工具实现 ──────────────────────────────────────────────

    /// AI 招募：创建（或复用同名）持久 Subagent，并默认立即在当前会话激活。
    async fn tool_create_agent(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let (session_id, workspace) = Self::require_context()?;
        let request: CreateAgentRequest = parse_request(payload)?;
        let name = request.name.trim();
        if name.is_empty() {
            bail!("成员名称不能为空");
        }
        let instructions = request
            .instructions
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // 同名复用：延续已有身份的指令与记忆（招募熟手语义）。
        let existing = self
            .agents
            .list()
            .into_iter()
            .find(|config| config.name == name);
        let (config, reused) = if let Some(config) = existing {
            // 允许本次招募补充长期指令与描述（不覆盖后端与关联）。
            if instructions.is_some() || request.description.is_some() {
                let updated = self.agents.update(
                    &config.id,
                    crate::agent_store::AgentChanges {
                        description: request.description.as_deref(),
                        instructions,
                        ..Default::default()
                    },
                )?;
                (updated, true)
            } else {
                (config, true)
            }
        } else {
            let session_binding = self.resolve_recruit_session(&request)?;
            let config = self.agents.create(
                name,
                request.description.as_deref().unwrap_or(""),
                request.backend,
                request.command.as_deref(),
                session_binding.as_deref(),
                request
                    .workspace_policy
                    .unwrap_or(WorkspacePolicy::ReadOnly),
                instructions,
            )?;
            (config, false)
        };
        self.validate_activation(&config)?;
        let mut summary = if reused {
            format!(
                "已复用现有 Subagent「{}」（{}），延续其长期指令与记忆",
                config.name, config.id
            )
        } else {
            format!(
                "已创建 Subagent「{}」（{}，后端：{}）",
                config.name,
                config.id,
                config.backend.label()
            )
        };
        if request.activate.unwrap_or(true) {
            let activation = self
                .activate_agent_in_session(&config.id, &session_id, &workspace)
                .await?;
            summary.push_str(&format!(
                "，并已在当前会话激活（Workspace 策略：{}）；现在可用 send_agent_message 交流或 submit_agent_task 派活",
                activation.workspace_policy.label()
            ));
        } else {
            summary.push_str("；尚未激活，需要时先用 activate_agent 激活");
        }
        notify(json!({ "kind": "agent_created", "agent_id": config.id }));
        Ok(tool_detail(
            summary,
            json!({ "agent_id": config.id, "reused": reused }),
        ))
    }

    /// 解析天工会话后端的关联会话：显式 ID 优先，其次按标题关键词取最近匹配。
    fn resolve_recruit_session(&self, request: &CreateAgentRequest) -> Result<Option<String>> {
        if request.backend != BackendKind::TiangongSession {
            return Ok(None);
        }
        let explicit = request
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(session_id) = explicit {
            if !crate::sessions::session_exists(session_id) {
                bail!("关联会话不存在: {session_id}");
            }
            return Ok(Some(session_id.to_string()));
        }
        let query = request
            .session_query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                // 附最近会话候选，模型可直接据此补 session_query 重试。
                let candidates = crate::sessions::list_sessions()
                    .into_iter()
                    .take(5)
                    .map(|session| {
                        format!(
                            "「{}」（{} 条消息，{}）",
                            session.title, session.message_count, session.updated_at
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("、");
                anyhow::anyhow!(
                    "天工会话后端需要提供 session_id 或 session_query（按标题搜索）之一{}",
                    if candidates.is_empty() {
                        String::new()
                    } else {
                        format!("；最近的会话：{candidates}")
                    }
                )
            })?;
        let keyword = query.to_lowercase();
        let matched = crate::sessions::list_sessions()
            .into_iter()
            .find(|session| {
                session.title.to_lowercase().contains(&keyword)
                    || session.id.to_lowercase().contains(&keyword)
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "没有找到标题包含「{query}」的会话；可换用更准确的关键词或显式 session_id"
                )
            })?;
        Ok(Some(matched.id))
    }

    async fn tool_list_agents(&self) -> Result<serde_json::Value> {
        let (session_id, _) = Self::require_context()?;
        let summaries = self.build_summaries(Some(&session_id));
        let mut lines = Vec::new();
        for summary in &summaries {
            let activation = if summary.activated_in_session {
                "已激活"
            } else {
                "未激活"
            };
            let runtime = summary
                .runtime_status
                .map(|status| status.label())
                .unwrap_or("空闲");
            lines.push(format!(
                "- {}（{}）[{}|{}|{}] {}",
                summary.config.name,
                summary.config.id,
                summary.config.backend.label(),
                activation,
                runtime,
                summary.config.description
            ));
        }
        let summary_text = if lines.is_empty() {
            "暂无持久 Subagent，可在扩展区管理页创建".to_string()
        } else {
            lines.join("\n")
        };
        Ok(tool_detail(summary_text, json!({ "agents": summaries })))
    }

    async fn tool_get_agent(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let (session_id, _) = Self::require_context()?;
        let request: AgentIdRequest = parse_request(payload)?;
        let summaries = self.build_summaries(Some(&session_id));
        let summary = summaries
            .into_iter()
            .find(|summary| summary.config.id == request.agent_id)
            .ok_or_else(|| anyhow::anyhow!("Agent 不存在: {}", request.agent_id))?;
        let instructions = self.agents.instructions(&request.agent_id)?;
        let recent_tasks = self
            .store
            .list_tasks()
            .into_iter()
            .filter(|task| task.agent_id == request.agent_id)
            .take(10)
            .collect::<Vec<_>>();
        let recent_events = self.store.list_events(Some(&request.agent_id), None, 10);
        let artifact_count = self.list_artifacts(&request.agent_id)?.len();
        let detail = AgentDetail {
            summary,
            instructions,
            recent_tasks,
            recent_events,
            artifact_count,
        };
        let mut text = format!(
            "{}（{}）\n后端：{}｜Workspace 策略：{}\n状态：{}\n说明：{}",
            detail.summary.config.name,
            detail.summary.config.id,
            detail.summary.config.backend.label(),
            detail.summary.config.workspace_policy.label(),
            detail
                .summary
                .runtime_status
                .map(|status| status.label())
                .unwrap_or("空闲"),
            detail.summary.config.description
        );
        if !detail.instructions.is_empty() {
            text.push_str(&format!("\n长期指令：{}", detail.instructions));
        }
        if let Some(task) = detail.recent_tasks.first() {
            text.push_str(&format!(
                "\n最近任务：{}（{}）",
                task.goal,
                task.status.label()
            ));
        }
        Ok(tool_detail(text, serde_json::to_value(detail)?))
    }

    async fn tool_activate(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let (session_id, workspace) = Self::require_context()?;
        let request: AgentIdRequest = parse_request(payload)?;
        let activation = self
            .activate_agent_in_session(&request.agent_id, &session_id, &workspace)
            .await?;
        Ok(tool_ok(format!(
            "已在当前会话激活（Workspace：{}，策略：{}）",
            activation.workspace,
            activation.workspace_policy.label()
        )))
    }

    async fn tool_deactivate(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let (session_id, _) = Self::require_context()?;
        let request: AgentIdRequest = parse_request(payload)?;
        let notes = self
            .deactivate_agent_in_session(&request.agent_id, &session_id)
            .await?;
        let mut summary = "已在当前会话停用该 Subagent，不再接收新任务".to_string();
        if !notes.is_empty() {
            summary.push_str(&format!("；{}", notes.join("；")));
        }
        Ok(tool_ok(summary))
    }

    async fn tool_list_active(&self) -> Result<serde_json::Value> {
        let (session_id, _) = Self::require_context()?;
        let activations: Vec<_> = self
            .store
            .activations()
            .into_iter()
            .filter(|activation| activation.active() && activation.session_id == session_id)
            .collect();
        let runs = self.store.list_runs();
        let lines: Vec<String> = activations
            .iter()
            .map(|activation| {
                let runtime = runs
                    .iter()
                    .filter(|run| {
                        run.activation_id == activation.activation_id && run.status.is_alive()
                    })
                    .map(|run| run.status.label())
                    .next_back()
                    .unwrap_or("空闲");
                format!(
                    "- {}（{}）[{}] Workspace：{}",
                    activation.agent_id, activation.activation_id, runtime, activation.workspace
                )
            })
            .collect();
        let summary_text = if lines.is_empty() {
            "当前会话没有已激活的 Subagent".to_string()
        } else {
            lines.join("\n")
        };
        Ok(tool_detail(
            summary_text,
            json!({ "activations": activations }),
        ))
    }

    async fn tool_send_message(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let (session_id, workspace) = Self::require_context()?;
        let request: SendMessageRequest = parse_request(payload)?;
        let outcome = self
            .send_message_core(
                &request.agent_id,
                &session_id,
                &workspace,
                &request.content,
                &request.attachments,
            )
            .await?;
        Ok(tool_ok(if outcome.injected_into_run {
            format!("消息已注入运行中实例（run {}）", outcome.run_id)
        } else {
            format!(
                "已向 Subagent 发送消息并启动处理（run {}），回复将经 Hook 返回本会话",
                outcome.run_id
            )
        }))
    }

    async fn tool_submit_task(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let (session_id, workspace) = Self::require_context()?;
        let request: SubmitTaskRequest = parse_request(payload)?;
        let outcome = self
            .submit_task_core(
                &request.agent_id,
                &session_id,
                &workspace,
                &request.goal,
                request.completion_criteria.as_deref(),
                &request.attachments,
            )
            .await?;
        Ok(tool_ok(format!(
            "任务已提交（task {}，run {}）；完成、阻塞或失败将反馈到本会话",
            outcome.task_id, outcome.run_id
        )))
    }

    async fn tool_get_task(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: TaskIdRequest = parse_request(payload)?;
        let task = self.store.load_task(&request.task_id)?;
        let runs: Vec<RunRecord> = task
            .runs
            .iter()
            .filter_map(|run_id| self.store.load_run(run_id).ok())
            .collect();
        let summary_text = format!(
            "任务 {}：{}（状态 {}，运行 {} 次）",
            task.task_id,
            task.goal,
            task.status,
            runs.len()
        );
        Ok(tool_detail(
            summary_text,
            json!({ "task": task, "runs": runs }),
        ))
    }

    async fn tool_list_tasks(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        // 有会话上下文时默认按当前会话过滤；显式指定 agent 或无上下文（管理页
        // 诊断路径）则列出该 Agent 全部任务。
        let session_filter = invocation_context().map(|context| context.session_id);
        let request: ListAgentTasksRequest = parse_request(payload)?;
        let tasks: Vec<TaskRecord> = self
            .store
            .list_tasks()
            .into_iter()
            .filter(|task| {
                request
                    .agent_id
                    .as_deref()
                    .is_none_or(|id| task.agent_id == id)
            })
            .filter(|task| {
                request.agent_id.is_some()
                    || session_filter
                        .as_deref()
                        .is_none_or(|session| task.session_id == session)
            })
            .take(20)
            .collect();
        let lines: Vec<String> = tasks
            .iter()
            .map(|task| {
                format!(
                    "- {}（{}）[{}] {}",
                    task.task_id,
                    task.agent_id,
                    task.status.label(),
                    task.goal
                )
            })
            .collect();
        let summary_text = if lines.is_empty() {
            "暂无任务记录".to_string()
        } else {
            lines.join("\n")
        };
        Ok(tool_detail(summary_text, json!({ "tasks": tasks })))
    }

    async fn tool_get_run(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: RunIdRequest = parse_request(payload)?;
        let run = self.store.load_run(&request.run_id)?;
        let events = self.store.list_events(None, Some(&request.run_id), 20);
        let summary_text = format!(
            "运行 {}（{}）：{}{}",
            run.run_id,
            run.kind,
            run.status.label(),
            run.summary
                .as_deref()
                .map(|summary| format!("——{summary}"))
                .unwrap_or_default()
        );
        Ok(tool_detail(
            summary_text,
            json!({ "run": run, "events": events }),
        ))
    }

    async fn tool_interrupt_run(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: RunIdRequest = parse_request(payload)?;
        self.interrupt_run_core(&request.run_id).await?;
        Ok(tool_ok(
            "已发送中断信号（现场保留，可继续输出）".to_string(),
        ))
    }

    async fn tool_cancel_run(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: RunIdRequest = parse_request(payload)?;
        self.cancel_run_core(&request.run_id).await?;
        Ok(tool_ok("运行已取消".to_string()))
    }

    async fn tool_list_events(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: ListAgentEventsRequest = parse_request(payload)?;
        let events = self.store.list_events(
            request.agent_id.as_deref(),
            request.run_id.as_deref(),
            request.limit.unwrap_or(20),
        );
        let lines: Vec<String> = events
            .iter()
            .map(|event| {
                format!(
                    "- {} [{}] {}",
                    event.created_at,
                    event.event_type,
                    event
                        .payload
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| {
                            event
                                .payload
                                .get("status")
                                .and_then(serde_json::Value::as_str)
                        })
                        .unwrap_or("")
                )
            })
            .collect();
        let summary_text = if lines.is_empty() {
            "暂无事件".to_string()
        } else {
            lines.join("\n")
        };
        Ok(tool_detail(summary_text, json!({ "events": events })))
    }

    async fn tool_get_artifacts(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: AgentIdRequest = parse_request(payload)?;
        let artifacts = self.list_artifacts(&request.agent_id)?;
        let summary_text = if artifacts.is_empty() {
            "该 Subagent 暂无产物".to_string()
        } else {
            artifacts
                .iter()
                .map(|entry| {
                    format!(
                        "- {}（{}）",
                        entry.name,
                        entry.modified_at.clone().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(tool_detail(summary_text, json!({ "artifacts": artifacts })))
    }

    async fn tool_get_memory(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: AgentMemoryRequest = parse_request(payload)?;
        let files = crate::memory::list(&self.agents, &request.agent_id)?;
        if files.is_empty() {
            return Ok(tool_ok("该 Subagent 暂无长期记忆".to_string()));
        }
        // 默认返回全部记忆内容（注入形态），供模型读取参考。
        let snapshot = crate::memory::injection_snapshot(&self.agents, &request.agent_id);
        let summary_text = if snapshot.is_empty() {
            "长期记忆文件存在但内容为空".to_string()
        } else {
            snapshot
        };
        Ok(tool_detail(summary_text, json!({ "files": files })))
    }

    async fn tool_append_memory(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: AppendAgentMemoryRequest = parse_request(payload)?;
        let name = crate::memory::append_note(
            &self.agents,
            &request.agent_id,
            &request.content,
            request.note.as_deref(),
            request.memory_name.as_deref(),
        )?;
        notify(json!({ "kind": "memory_updated", "agent_id": request.agent_id }));
        Ok(tool_ok(format!("已追加到长期记忆（memory/{name}）")))
    }

    /// 成员自维护工作区状态（读）：按当前会话上下文的工作区解析稳定
    /// 身份后读取 plan/context/task——sidecar 只存取不理解。
    /// 解析目标成员：显式编号优先；缺省=当前会话归属的成员（自己）；
    /// 非成员会话且未提供编号时明确失败。
    fn resolve_state_target(&self, agent_id: &str, session_id: &str) -> Result<String> {
        if !agent_id.is_empty() {
            return Ok(agent_id.to_string());
        }
        self.collaboration_origin(session_id)
            .map(|config| config.id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "当前会话不是任何 Subagent 的后端会话；查询其他成员请显式指定 agent_id"
                )
            })
    }

    async fn tool_load_workspace_state(
        &self,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let request: AgentIdRequest = parse_request(payload)?;
        let (session_id, workspace) = Self::require_context()?;
        let target = self.resolve_state_target(request.agent_id.as_str(), &session_id)?;
        let state = crate::workspace_state::load(&self.agents, &target, &workspace)?;
        // WASM 工具桥接只透传 summary/stdout 文本字段；状态内容必须装入
        // stdout，否则成员读到"成功但空结果"。state 字段保留供结构化消费。
        let field = |name: &str| {
            state
                .get(name)
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty())
                .unwrap_or("（未填写）")
        };
        let stdout = format!(
            "## 长期计划\n{}\n\n## 工作背景\n{}\n\n## 当前任务\n{}",
            field("plan"),
            field("context"),
            field("task"),
        );
        Ok(json!({
            "ok": true,
            "summary": format!("已读取 {} 的工作区状态", target),
            "stdout": stdout,
            "state": state,
        }))
    }

    /// 成员写工作状态（plan/context）：写入方为该成员自己的会话
    /// （其他成员/主会话不得代写——工作含义由成员自己维护）。
    async fn tool_update_workspace_state(
        &self,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let (session_id, workspace) = Self::require_context()?;
        let request: UpdateWorkspaceStateRequest = parse_request(payload)?;
        let agent_id = request.agent_id.as_deref().unwrap_or_default();
        let target = if agent_id.is_empty() {
            // 缺省=成员自己：当前会话须是其后端会话。
            self.collaboration_origin(&session_id)
                .map(|config| config.id)
                .ok_or_else(|| anyhow::anyhow!("当前会话不是任何 Subagent 的后端会话；主 Agent 侧请显式指定 agent_id 仅在查询场景"))?
        } else {
            agent_id.to_string()
        };
        // 写入仅限成员自己（主会话/他成员不代写业务状态）。
        if !agent_id.is_empty() {
            let session_owner =
                crate::workspace_state::agent_for_session(&self.agents, &session_id);
            let is_self = session_owner.is_some_and(|owner| owner == target);
            if !is_self {
                bail!("工作区状态由成员自己维护：只有成员自己的执行会话可写入 plan/context");
            }
        }
        let id = crate::workspace_state::write(
            &self.agents,
            &target,
            &workspace,
            &request.file,
            &request.content,
        )?;
        Ok(json!({ "ok": true, "workspace_id": id }))
    }

    /// 协作关系视图：谁在为谁执行、每个发起方在等谁的结果——从运行
    /// 记录派生（含停止请求中，那是占用事实），供主 Agent 调度参考。
    async fn tool_list_pending_work(
        &self,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let request: ListPendingWorkRequest = parse_request(payload)?;
        let agents = self.agents.list();
        let name_of = |agent_id: &str| {
            agents
                .iter()
                .find(|config| config.id == agent_id)
                .map(|config| config.name.clone())
                .unwrap_or_else(|| agent_id.to_string())
        };
        // 发起方标定：发起会话是某成员的后端会话 → 该成员；否则主会话。
        // 归属按成员×工作区映射逐会话反查（老全局绑定兜底）。
        let origin_label = |session_id: &str| {
            let owner = crate::workspace_state::agent_for_session(&self.agents, session_id);
            agents
                .iter()
                .find(|config| owner.as_deref() == Some(config.id.as_str()))
                .map(|config| format!("成员「{}」", config.name))
                .unwrap_or_else(|| "主会话".to_string())
        };
        let pending: Vec<RunRecord> = self
            .store
            .list_runs()
            .into_iter()
            .filter(|run| {
                run.status.is_alive()
                    && run
                        .agent_id
                        .as_str()
                        .contains(request.agent_id.as_deref().unwrap_or(""))
            })
            .collect();
        let mut lines_by_member: Vec<String> = Vec::new();
        let mut lines_by_origin: Vec<String> = Vec::new();
        let mut rows = Vec::new();
        for run in &pending {
            let goal = run
                .task_id
                .as_deref()
                .and_then(|task_id| self.store.load_task(task_id).ok())
                .map(|task| task.goal)
                .unwrap_or_else(|| "（消息往返）".to_string());
            let origin = run
                .origin_session
                .as_deref()
                .map(origin_label)
                .unwrap_or_else(|| "主会话".to_string());
            let short_run: String = run
                .run_id
                .chars()
                .rev()
                .take(6)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            lines_by_member.push(format!(
                "- 为 {origin}：{goal}（{}，workspace {}，run …{short_run}）",
                run.status.label(),
                short_workspace(&run.workspace),
            ));
            lines_by_origin.push(format!(
                "- {} → 成员「{}」：{goal}（{}，workspace {}）",
                origin,
                name_of(&run.agent_id),
                run.status.label(),
                short_workspace(&run.workspace)
            ));
            rows.push(json!({
                "run_id": run.run_id,
                "agent_id": run.agent_id,
                "agent_name": name_of(&run.agent_id),
                "origin": origin,
                "origin_session": run.origin_session,
                "goal": goal,
                "status": run.status.label(),
                "task_id": run.task_id,
                "workspace": run.workspace,
            }));
        }
        let mut text = String::new();
        if rows.is_empty() {
            text.push_str("当前没有等待中的 Subagent 工作。");
        } else {
            text.push_str(&format!(
                "按执行成员：\n{}\n\n按发起方（谁在等结果）：\n{}",
                lines_by_member.join("\n"),
                lines_by_origin.join("\n")
            ));
        }
        Ok(tool_detail(text, json!({ "pending": rows })))
    }

    /// 成员主动回报：成员在自己的后端会话内调用，把当前工作的结果
    /// 主动投递给发起方（主会话或协作发起成员），并终结对应的运行记录。
    /// 回报是成员的主动投递动作，带明确任务归属——轮次收尾抓取仅为
    /// 未回报时的兜底路径。
    async fn tool_report_result(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let (session_id, _) = Self::require_context()?;
        let session_owner = crate::workspace_state::agent_for_session(&self.agents, &session_id);
        let request: ReportAgentResultRequest = parse_request(payload)?;
        let result = request.result.trim();
        if result.is_empty() {
            bail!("回报结果不能为空");
        }
        let to_session = request.to_session.trim();
        if to_session.is_empty() {
            bail!("回报必须指定接收方（to_session，任务消息中的【回复地址】）");
        }
        // 消息来自成员：以该会话为后端会话的成员署名。
        let config = self
            .agents
            .list()
            .into_iter()
            .find(|config| {
                matches!(
                    config.backend,
                    BackendKind::TiangongSession | BackendKind::AgentTeam
                ) && session_owner.as_deref() == Some(config.id.as_str())
            })
            .ok_or_else(|| anyhow::anyhow!("当前会话不是任何 Subagent 的后端会话，无法回报"))?;
        // 纯消息投递：回报只负责传达——不选择任务、不终结运行、不释放
        // 执行资源（终结由执行系统按真实事件记录，工作含义由成员与主
        // Agent 各自判断）。
        let mut body = format!(
            "【Subagent 回报】来自成员「{}」（{}）：\n{}",
            config.name, config.id, result
        );
        if let Some(note) = request
            .note
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            body.push_str(&format!("\n（{note}）"));
        }
        let marker = request
            .run_marker
            .as_deref()
            .map(str::trim)
            .map(|s| s.trim_start_matches("r-"))
            .filter(|s| !s.is_empty());
        if let Some(marker) = marker {
            body.push_str(&format!("\n（运行标记 r-{marker}）"));
        }
        crate::delivery::deliver_message(
            &self.http,
            to_session,
            &body,
            None,
            &[],
            tiangong_plugin_subagent_protocol::render::report(&config.name, &config.id, marker),
        )
        .await?;
        // 诊断追踪：标记匹配到等待中的运行时追加一条投递事件（不动状态）。
        if let Some(marker) = marker
            && let Some(run) = self
                .store
                .list_runs()
                .into_iter()
                .find(|run| run.run_id.ends_with(marker) && run.status.is_alive())
        {
            let text: String = result.chars().take(200).collect();
            append_event(
                &self.store,
                &run,
                "report_delivered",
                &json!({ "text": text, "to_session": to_session }),
                &now_string(),
            );
        }
        Ok(tool_ok(format!("回报已投递到会话 {to_session}")))
    }

    /// 指令受控成长：向成员长期指令追加稳定规则（不覆盖既有内容）。
    /// 权限——主会话可操作任意成员；成员后端会话只能操作自己。
    async fn tool_append_instructions(
        &self,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let (session_id, _) = Self::require_context()?;
        let request: AppendAgentInstructionsRequest = parse_request(payload)?;
        let addition = request.addition.trim();
        if addition.is_empty() {
            bail!("追加内容不能为空");
        }
        if addition.chars().count() > 2000 {
            bail!("单次追加过长（上限 2000 字符）；长内容请改用 append_agent_memory 存入记忆");
        }
        let config = self.agents.load(&request.agent_id)?;
        if let Some(origin) = self.collaboration_origin(&session_id)
            && origin.id != config.id
        {
            bail!("成员只能追加自己的长期指令；如需调整其他成员，请在主会话发起");
        }
        let existing = self.agents.instructions(&config.id)?;
        if existing.chars().count() + addition.chars().count() > 16_000 {
            bail!("长期指令总量接近上限，请先整理既有内容再追加");
        }
        let timestamp = now_string();
        let merged = if existing.trim().is_empty() {
            format!("{addition}\n")
        } else {
            format!(
                "{}\n\n<!-- 追加于 {timestamp} -->\n{addition}\n",
                existing.trim_end()
            )
        };
        self.agents.update(
            &config.id,
            crate::agent_store::AgentChanges {
                instructions: Some(merged.as_str()),
                ..Default::default()
            },
        )?;
        notify(json!({ "kind": "agent_updated", "agent_id": config.id }));
        Ok(tool_ok(format!(
            "已追加到「{}」的长期指令（当前共 {} 字符）",
            config.name,
            merged.chars().count()
        )))
    }

    fn list_artifacts(&self, agent_id: &str) -> Result<Vec<ArtifactEntry>> {
        let dir = self.agents.root().join(agent_id).join("artifacts");
        let mut entries = Vec::new();
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            return Ok(entries);
        };
        for entry in read_dir.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let modified_at = metadata.modified().ok().map(|time| {
                let datetime: chrono::DateTime<chrono::Local> = time.into();
                datetime.naive_local().to_string()
            });
            entries.push(ArtifactEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: metadata.is_dir(),
                size_bytes: metadata.len(),
                modified_at,
            });
        }
        Ok(entries)
    }

    // ── UI 实现 ───────────────────────────────────────────────

    async fn ui_state_snapshot(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: UiSessionRequest = parse_request(payload)?;
        // 调度记录按会话隔离：任务/运行/事件只显示本会话发起的工作——
        // 运行的 origin_session 为协作发起成员会话，一并归入；成员列表保持
        // 全局（身份与激活管理不分会话）。session_id 为空（无会话上下文）
        // 时保持全局视图兜底。
        let session_id = request.session_id.trim().to_string();
        let mut runs = self.store.list_runs();
        runs.retain(|run| run_in_session(run, &session_id));
        runs.sort_by(|a, b| b.run_id.cmp(&a.run_id));
        runs.truncate(30);
        let snapshot = StateSnapshot {
            agents: self.build_summaries(Some(&request.session_id)),
            session_id: request.session_id,
            active_tasks: self
                .store
                .list_tasks()
                .into_iter()
                .filter(|task| session_id.is_empty() || task.session_id == session_id)
                .filter(|task| {
                    task.status == TaskStatus::Running || task.status == TaskStatus::Pending
                })
                .take(50)
                .collect(),
            recent_runs: runs,
            recent_events: self
                .store
                .list_events(None, None, 200)
                .into_iter()
                .filter(|event| session_id.is_empty() || event.session_id == session_id)
                .collect(),
        };
        Ok(serde_json::to_value(snapshot)?)
    }

    async fn ui_agent_create(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: UiAgentCreateRequest = parse_request(payload)?;
        if request.backend == BackendKind::TiangongSession {
            let session_id = request.session_id.as_deref().unwrap_or("").trim();
            if session_id.is_empty() {
                bail!("天工会话后端必须选择一个关联会话");
            }
            if !crate::sessions::session_exists(session_id) {
                bail!("关联会话不存在: {session_id}");
            }
        }
        let config = self.agents.create(
            &request.name,
            request.description.as_deref().unwrap_or(""),
            request.backend,
            request.command.as_deref(),
            request.session_id.as_deref(),
            request
                .workspace_policy
                .unwrap_or(WorkspacePolicy::ReadOnly),
            request.instructions.as_deref(),
        )?;
        notify(json!({ "kind": "agent_created", "agent_id": config.id }));
        Ok(serde_json::to_value(config)?)
    }

    async fn ui_agent_update(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: UiAgentUpdateRequest = parse_request(payload)?;
        if let Some(session_id) = request.session_id.as_deref() {
            let session_id = session_id.trim();
            if !session_id.is_empty() && !crate::sessions::session_exists(session_id) {
                bail!("关联会话不存在: {session_id}");
            }
        }
        let config = self.agents.update(
            &request.agent_id,
            crate::agent_store::AgentChanges {
                name: request.name.as_deref(),
                description: request.description.as_deref(),
                command: request.command.as_deref(),
                session_id: request.session_id.as_deref(),
                workspace_policy: request.workspace_policy,
                enabled: request.enabled,
                instructions: request.instructions.as_deref(),
            },
        )?;
        notify(json!({ "kind": "agent_updated", "agent_id": config.id }));
        Ok(serde_json::to_value(config)?)
    }

    async fn ui_list_workspace_states(
        &self,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let request: AgentIdRequest = parse_request(payload)?;
        crate::workspace_state::list_all(&self.agents, &request.agent_id)
    }

    async fn ui_list_memory(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: AgentMemoryRequest = parse_request(payload)?;
        let files = crate::memory::list(&self.agents, &request.agent_id)?;
        Ok(json!({ "files": files }))
    }

    async fn ui_read_memory(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: UiReadMemoryRequest = parse_request(payload)?;
        let content = crate::memory::read(&self.agents, &request.agent_id, &request.name)?;
        Ok(json!({ "name": request.name, "content": content }))
    }

    async fn ui_write_memory(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: UiWriteMemoryRequest = parse_request(payload)?;
        crate::memory::write(
            &self.agents,
            &request.agent_id,
            &request.name,
            &request.content,
        )?;
        notify(json!({ "kind": "memory_updated", "agent_id": request.agent_id }));
        Ok(json!({ "written": request.name }))
    }

    async fn ui_delete_memory(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: UiDeleteMemoryRequest = parse_request(payload)?;
        crate::memory::delete(&self.agents, &request.agent_id, &request.name)?;
        notify(json!({ "kind": "memory_updated", "agent_id": request.agent_id }));
        Ok(json!({ "deleted": request.name }))
    }

    /// 整理记忆 = 向成员发送整理请求（成员按自身规则整理保存并经通用
    /// 消息机制反馈）。目标会话按发起方（管理页所在会话）工作区的映射
    /// 路由；不再由 sidecar 抓取会话历史代替成员整理。
    async fn ui_compile_memory(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: AgentMemoryRequest = parse_request(payload)?;
        let config = self.agents.load(&request.agent_id)?;
        if !matches!(
            config.backend,
            BackendKind::TiangongSession | BackendKind::AgentTeam
        ) {
            bail!("整理请求仅适用于天工会话后端的成员（其他后端的记忆来源是任务归档与手动记录）");
        }
        // UI 桥接没有工具调用上下文：发起方会话与工作区由管理页从宿主
        // 上下文显式传递（与其他 ui_* 请求同一模式）。
        let session_id = request
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少当前会话上下文（session_id），无法发送整理请求"))?;
        let workspace = request
            .workspace
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少当前工作区（workspace），无法发送整理请求"))?;
        let content = format!(
            "请整理当前工作区的重要结论：用 update_workspace_state 更新当前工作（task.md）、背景约定（context.md）与计划（plan.md），把可复用经验用 append_agent_memory 追加到 lessons.md，完成后用 report_agent_result 向我反馈整理结果（to_session 填会话 {session_id}）。"
        );
        self.send_message_core(&request.agent_id, session_id, workspace, &content, &[])
            .await?;
        notify(json!({ "kind": "memory_request_sent", "agent_id": request.agent_id }));
        Ok(json!({ "sent": true }))
    }

    async fn ui_agent_delete(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: UiAgentDeleteRequest = parse_request(payload)?;
        // 删除前释放该 Agent 的全部激活。
        for mut activation in self.store.activations() {
            if activation.active() && activation.agent_id == request.agent_id {
                self.release_activation_locked(&mut activation).await?;
            }
        }
        self.agents.delete(&request.agent_id)?;
        notify(json!({ "kind": "agent_deleted", "agent_id": request.agent_id }));
        Ok(json!({ "deleted": true }))
    }

    async fn ui_activate(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: UiActivateRequest = parse_request(payload)?;
        let activation = self
            .activate_agent_in_session(&request.agent_id, &request.session_id, &request.workspace)
            .await?;
        Ok(serde_json::to_value(activation)?)
    }

    async fn ui_deactivate(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: UiDeactivateRequest = parse_request(payload)?;
        self.deactivate_agent_in_session(&request.agent_id, &request.session_id)
            .await?;
        Ok(json!({ "deactivated": true }))
    }

    async fn ui_send_message(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: UiSendMessageRequest = parse_request(payload)?;
        let outcome = self
            .send_message_core(
                &request.agent_id,
                &request.session_id,
                &request.workspace,
                &request.content,
                &[],
            )
            .await?;
        Ok(serde_json::to_value(outcome)?)
    }

    async fn ui_submit_task(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: UiSubmitTaskRequest = parse_request(payload)?;
        let outcome = self
            .submit_task_core(
                &request.agent_id,
                &request.session_id,
                &request.workspace,
                &request.goal,
                request.completion_criteria.as_deref(),
                &[],
            )
            .await?;
        Ok(serde_json::to_value(outcome)?)
    }

    async fn ui_interrupt_run(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: RunIdRequest = parse_request(payload)?;
        self.interrupt_run_core(&request.run_id).await?;
        Ok(json!({ "interrupted": true }))
    }

    async fn ui_cancel_run(&self, payload: &serde_json::Value) -> Result<serde_json::Value> {
        let request: RunIdRequest = parse_request(payload)?;
        self.cancel_run_core(&request.run_id).await?;
        Ok(json!({ "cancelled": true }))
    }

    // ── 概览构建 ──────────────────────────────────────────────

    fn build_summaries(&self, session_id: Option<&str>) -> Vec<AgentSummary> {
        let activations = self.store.activations();
        let runs = self.store.list_runs();
        self.agents
            .list()
            .into_iter()
            .map(|config| {
                let agent_activations: Vec<ActivationRecord> = activations
                    .iter()
                    .filter(|activation| activation.agent_id == config.id && activation.active())
                    .cloned()
                    .collect();
                let activated_in_session = session_id.is_some_and(|session| {
                    agent_activations
                        .iter()
                        .any(|activation| activation.session_id == session)
                });
                let active_run = runs
                    .iter()
                    .filter(|run| {
                        run.agent_id == config.id
                            && run.status.is_alive()
                            && agent_activations
                                .iter()
                                .any(|activation| activation.activation_id == run.activation_id)
                    })
                    .max_by_key(|run| run.run_id.as_str());
                let instructions = self.agents.instructions(&config.id).unwrap_or_default();
                AgentSummary {
                    capabilities: config.backend.capabilities(),
                    config,
                    activations: agent_activations,
                    activated_in_session,
                    runtime_status: active_run.map(|run| run.status),
                    active_run_id: active_run.map(|run| run.run_id.clone()),
                    instructions,
                }
            })
            .collect()
    }
}

/// canonicalize Workspace（必须是存在的绝对目录）。
fn canonical_workspace(workspace: &str) -> Result<String> {
    let path = Path::new(workspace);
    if !path.is_absolute() {
        bail!("Workspace 必须是绝对路径: {workspace}");
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| anyhow::anyhow!("解析 Workspace 失败（{workspace}）: {error}"))?;
    if !canonical.is_dir() {
        bail!("Workspace 不是目录: {}", canonical.display());
    }
    Ok(canonical.to_string_lossy().into_owned())
}

fn parse_request<T: serde::de::DeserializeOwned>(payload: &serde_json::Value) -> Result<T> {
    serde_json::from_value(payload.clone())
        .map_err(|error| anyhow::anyhow!("请求参数无效: {error}"))
}

/// 握手响应（宿主据此将工具直连本 sidecar）。
pub fn handshake_payload() -> serde_json::Value {
    let mut capabilities: Vec<String> = TOOL_OPERATIONS
        .iter()
        .map(|tool| format!("tool:{tool}"))
        .collect();
    capabilities.push("subagent".to_string());
    json!({
        "plugin_id": PLUGIN_ID,
        "plugin_version": PLUGIN_VERSION,
        "sidecar_version": PLUGIN_VERSION,
        "protocol_version": PROTOCOL_VERSION,
        "business_protocol": tiangong_plugin_subagent_protocol::SUBAGENT_PROTOCOL_VERSION,
        "capabilities": capabilities,
        "instance_id": format!("subagent-sidecar-{}", std::process::id()),
        "status": "ready",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 正文尾部的标记才是本轮标记：注入段落（长期记忆、成员自维护的
    /// task/plan/context）里的历史标记不得抢先命中。
    #[test]
    fn 运行标记取最后一次出现() {
        let body = "【Subagent 任务】来自主会话：\n目标：做一件事\n\n【长期记忆】\n上次工作（运行标记 r-AAAAAAAA）已交付。\n\n【当前任务】\n继续（运行标记 r-BBBBBBBB）的后续。\n\n（运行标记 r-CCCCCCCC）";
        assert_eq!(extract_run_tag(body), Some("CCCCCCCC"));
    }

    #[test]
    fn 无标记的正文不归因() {
        assert_eq!(extract_run_tag("【Subagent 消息】来自主会话：\n你好"), None);
        assert_eq!(extract_run_tag(""), None);
    }

    #[test]
    fn 标记为空或未闭合时不归因() {
        assert_eq!(extract_run_tag("（运行标记 r-）"), None);
        assert_eq!(extract_run_tag("（运行标记 r-   ）"), None);
        assert_eq!(extract_run_tag("（运行标记 r-"), None);
    }

    #[test]
    fn 标记两侧空白被容忍() {
        assert_eq!(
            extract_run_tag("（运行标记 r- DDDDDDDD ）"),
            Some("DDDDDDDD")
        );
    }

    /// 面板调度记录按会话隔离：运行按发起会话或协作发起会话命中；
    /// 空会话（无上下文）不过滤兜底。
    #[test]
    fn 运行按会话隔离判定() {
        let base = || RunRecord {
            run_id: "r-1".to_string(),
            task_id: None,
            agent_id: "agent-1".to_string(),
            activation_id: "act-1".to_string(),
            session_id: "sess-dev".to_string(),
            executor_session: None,
            kind: RunKind::Message,
            status: RunStatus::Working,
            pid: None,
            workspace: "/ws".to_string(),
            origin_session: None,
            created_at: String::new(),
            updated_at: String::new(),
            finished_at: None,
            summary: None,
        };

        // 发起会话命中。
        assert!(run_in_session(&base(), "sess-dev"));
        // 其他会话（如 gaming life）看不到开发会话的运行。
        assert!(!run_in_session(&base(), "sess-gaming"));
        // 协作发起会话也命中。
        let collab = RunRecord {
            session_id: "sess-member-exec".to_string(),
            origin_session: Some("sess-dev".to_string()),
            ..base()
        };
        assert!(run_in_session(&collab, "sess-dev"));
        assert!(!run_in_session(&collab, "sess-gaming"));
        // 无会话上下文：全局兜底不过滤。
        assert!(run_in_session(&base(), ""));
    }
}
