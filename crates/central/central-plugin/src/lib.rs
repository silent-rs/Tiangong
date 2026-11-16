//! 中枢模式（central mode）宿主支持：中枢对话的独立执行域。
//!
//! **中枢对话直接由 TiangongCore 承载，不经 CoreManager**：中枢需要单独
//! 内置且唯一的中枢插件（[`CentralPlugin`]，承载中枢助手人设与 §5.3 白
//! 名单工具集），CoreManager 的插件构造（runtime 聚合桥）与编排不适用；
//! 中枢 Core 由 `TiangongApp` 直接构造、持有与投递（见 `app.rs` 的
//! `ensure_central_core` 等方法）。
//!
//! 其余边界不变：
//! - **单独存储**：路径规则内建于 `tiangong_core::session::Session::
//!   session_file_path`（`{storage_root}/central/sessions/`，读 / 写 /
//!   元数据共用）；会话列表与按组删除只扫 `sessions/`，天然不触达；
//! - **固定 Session ID**：常量位于 `tiangong_types::central_session`
//!   （前端 `frontend/src/lib/centralSession.ts` 同名常量）。

use std::future::Future;
use std::pin::Pin;

use tiangong_core::core::Plugin;
use tiangong_core::session::Session;
use tiangong_core::tools::extension::ToolOverrideHandler;
use tiangong_core::tools::result::ToolResult;
use tiangong_llm::tool::ToolCall;

pub use tiangong_types::central_session::{is_central_session, CENTRAL_SESSION_ID};

// 持久化与状态重建已迁移至 central-state crate（重构批次②）；
// 此处 re-export 保持 crate::central::xxx 调用路径兼容。
mod persona;
mod tool_specs;
mod tools_context;
mod tools_control;
mod tools_dispatch;
mod tools_goal_report;
mod workspace_summary;

use workspace_summary::{list_worktrees, read_head_branch, workspace_git_summary};

pub use central_state::{
    add_confirmed_workspace, canonical_path_string, interrupted_work_items,
    load_confirmed_workspaces, load_goals, load_reports, load_work_items, mark_work_item_finished,
    mark_work_item_finished_with_session, recover_registry_entries, save_goals, save_reports,
    save_work_items, set_work_item_outcome, upsert_work_item, workspace_confirmed, GoalRecord,
    ReportRecord, WorkItemEvent, WorkItemRecord,
};

/// 后台工作状态登记条目（前端「后台工作」状态条数据源，命令
pub use central_state::WorkStatus as CentralWorkStatus;
/// 中枢插件 → app 执行器的消息（unbounded channel）。
#[derive(Debug, Clone)]
pub enum CentralPluginMessage {
    /// 派发后台工作（见 [`CentralDispatchRequest`]）。
    Dispatch(CentralDispatchRequest),
    /// 向已有 Worker 会话发送引导消息（见 [`CentralDispatchRequest`]，
    /// `task` 为引导内容；执行中作为引导消息并入当前 turn，空闲时开启
    /// 新 turn——完成后与派发同样走回报时机规则）。
    Steer(CentralDispatchRequest),
    /// `collect_work_reports` 已取走全部暂存回报：同步清理状态登记。
    ReportsCollected,
    /// 工作控制（取消/暂停/恢复）——由执行器操作真实运行时。
    Control(CentralControlRequest),
    /// 应用内通知（send_notification 工具 → 执行器 emit 前端事件）。
    Notification {
        title: String,
        message: String,
        level: String,
    },
    /// 请求用户授权 Workspace（R3 票据化的「模型只能请求」侧）：
    /// 触发前端授权卡片；真正的票据只能由用户操作签发。
    RequestWorkspaceApproval {
        path: String,
        /// 任务指纹（与后续派发的 task 文本一致才放行，防偷换）。
        task_fingerprint: String,
    },
}

/// 中枢工作控制请求（cancel_work / pause_work / resume_work）。
#[derive(Debug, Clone)]
pub struct CentralControlRequest {
    pub work_id: String,
    pub session_id: String,
    /// cancelled / paused / resume
    pub action: String,
}

/// 中枢派发后台工作的请求。
///
/// 插件侧只做校验与登记（同步返回工具结果），实际创建/复用 Worker 会话
/// 与投递由 app 侧异步执行（`TiangungApp` 的中枢派发执行器，见 app.rs）；
/// 完成或失败按时机规则回到中枢对话（§9：失败必达、成功汇总）。
#[derive(Debug, Clone)]
pub struct CentralDispatchRequest {
    /// 派发标识（中枢对话内引用用）。
    pub work_id: String,
    /// 本次执行的唯一归属（R0 契约，§5.4）：`exec-<scru128>`。派发/恢复/
    /// 重试/重放各生成新值；终态回调与日志据此唯一定位，不靠会话猜测。
    pub execution_id: String,
    /// Worker 会话 ID：新建时为插件生成的 scru128；复用已有会话时为
    /// `reuse_session_id` 校验通过后的同一 ID（应用尽用，保持上下文连续）。
    pub session_id: String,
    /// 任务描述（原文，作为 Worker 的执行指令）。
    pub task: String,
    /// 指定模型 key（已通过解析校验；None 用路由默认）。
    pub model: Option<String>,
    /// 工作目录（None：复用会话时用该会话自身 cwd，新建用全局默认）。
    pub cwd: Option<String>,
    /// 排队依赖：等待该 work 终态后再投递本工作（None 立即派发）。
    pub after_work_id: Option<String>,
    /// Workspace 写入锁豁免：true 时不因同目录冲突自动排队（只读并行）。
    pub parallel: bool,
    /// 是否复用已有会话（插件侧决定并携带；app 据此跳过新会话标题等）。
    pub reused_session: bool,
}

/// 中枢助手内置插件：中枢 Core 唯一挂载的插件。
///
/// 工具集（§5.3 白名单的实现态，10 项——与 tool_specs 一一对应；二十轮
/// 合并后形态：21 工具收敛为 10，复用会话的引导并入 dispatch_work、
/// 换模型经 dispatch_work 带 model 参数复用原会话上下文）：
/// - 只读上下文：`get_context` / `inspect_context`（工作区·工作区详情·
///   会话三合一）/ `list_models` / `search_history`；
/// - 工作操作：`dispatch_work`（新建/复用/引导/换模型统一派发）、
///   `control`（cancel/pause/resume）、`work`（list/status/timeline/
///   reports）、`goal`（save_goal/set_goal_status/save_report/list_reports）；
/// - 授权与通知：`request_workspace_approval` / `send_notification`。
///
pub struct CentralPlugin {
    /// 全局默认工作区（构造时注入的快照；中枢不绑定具体工作区，§6）。
    #[doc(hidden)]
    pub workspace_dir: String,
    /// 只读元数据源（工作区聚合）：与普通会话管理器共享同一实例，仅使用
    /// `list_session_metadata`（带缓存），不触碰会话内容或 Core 生命周期。
    #[doc(hidden)]
    pub sessions: tiangong_core_manager::CoreManager,
    /// 派发通道：把后台工作请求交给 app 执行器（见 app.rs）。
    #[doc(hidden)]
    pub dispatch_tx: tokio::sync::mpsc::UnboundedSender<CentralPluginMessage>,
    /// 已到达、尚未进入对话的后台工作回报（与 app 共享同一实例；
    /// 暂存批量模式：随用户下一条消息一并注入，或经 collect_work_reports
    /// 由中枢在当前 turn 主动收集）。
    #[doc(hidden)]
    pub work_reports: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// 派发登记表（work_id → 派发事实；状态汇报经注入消息回到对话）。
    /// 持久化于 `{storage_root}/central/work-items.json`：应用重启后
    /// `work`（action=list）仍可查到既往派发（应用尽用的判定依据之一）。
    #[doc(hidden)]
    pub work_items: std::sync::Mutex<Vec<WorkItemRecord>>,
}

/// 工作区是否已被用户确认可用。
///
/// 天然已确认：默认工作区**及其子目录**（应用级选择的根，其子树均属
/// 既定授权范围，按前缀判定而非精确相等——review P1：精确比较会把
/// 默认工作区内的子目录误拦为「新工作区」）。
/// 按 session 归属写中枢事件日志（测试辅助：生产写路径在宿主
/// central_events；仅插件测试构造事件用）。
#[doc(hidden)]
pub fn record_central_event_by_session(
    storage_root: &std::path::Path,
    session_id: &str,
    event_type: tiangong_central::event::WorkEventType,
    note: &str,
) {
    let record = load_work_items(storage_root)
        .into_iter()
        .rev()
        .find(|item| item.session_id == session_id);
    let Some(record) = record else {
        return;
    };
    let log = match tiangong_central::EventLog::open(
        &storage_root.join("central").join("events.jsonl"),
    ) {
        Ok(log) => log,
        Err(_) => return,
    };
    let mut event = tiangong_central::WorkEvent::new(&record.work_id, event_type)
        .with_worker_session(session_id);
    if let Some(execution_id) = record.execution_id.as_deref() {
        event = event.with_execution(execution_id);
    }
    event.payload = serde_json::json!({ "note": note });
    let _ = log.append(event);
}

/// 读工作项时间线：事件日志优先，旧内嵌时间线兼容读（R5）。
fn work_timeline_from_log(
    storage_root: &std::path::Path,
    work_id: &str,
) -> Vec<(String, String, String)> {
    let mut timeline = Vec::new();
    if let Ok(log) =
        tiangong_central::EventLog::open(&storage_root.join("central").join("events.jsonl"))
    {
        if let Ok(entries) = log.load_all() {
            for logged in entries {
                if logged.event.work_item_id == work_id {
                    let note = logged
                        .event
                        .payload
                        .get("note")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    timeline.push((
                        logged.event.created_at.clone(),
                        logged.event.event_type.as_str().to_string(),
                        note.to_string(),
                    ));
                }
            }
        }
    }
    if !timeline.is_empty() {
        return timeline;
    }
    load_work_items(storage_root)
        .into_iter()
        .rev()
        .find(|item| item.work_id == work_id)
        .map(|item| {
            item.events
                .iter()
                .map(|e| (e.at.clone(), e.kind.clone(), e.note.clone()))
                .collect()
        })
        .unwrap_or_default()
}

impl CentralPlugin {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace_dir: String,
        sessions: tiangong_core_manager::CoreManager,
        dispatch_tx: tokio::sync::mpsc::UnboundedSender<CentralPluginMessage>,
        work_reports: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Self {
        let stored_work_items = load_work_items(sessions.storage_root());
        Self {
            workspace_dir,
            sessions,
            dispatch_tx,
            work_reports,
            work_items: std::sync::Mutex::new(stored_work_items),
        }
    }

    pub(crate) fn text_result(&self, summary: &str, body: String) -> Option<ToolResult> {
        Some(ToolResult {
            ok: true,
            summary: summary.to_string(),
            stdout: body,
            stderr: String::new(),
            exit_code: 0,
            error_kind: None,
            retryable: None,
            execution: None,
        })
    }

    pub(crate) fn error_result(&self, message: String) -> Option<ToolResult> {
        Some(ToolResult {
            ok: false,
            summary: message.clone(),
            stdout: String::new(),
            stderr: message,
            exit_code: 1,
            error_kind: Some("argument_error".to_string()),
            retryable: Some(false),
            execution: None,
        })
    }
}

impl Plugin for CentralPlugin {
    fn id(&self) -> &str {
        "central"
    }
}

impl ToolOverrideHandler for CentralPlugin {
    fn handle(
        &self,
        call: &ToolCall,
        _session: &mut Session,
        _actor_id: &str,
    ) -> Pin<Box<dyn Future<Output = Option<ToolResult>> + Send>> {
        let result = match call.name.as_str() {
            // search_history 返回异步 future（spawn_blocking 扫描）
            "search_history" => {
                let query = call
                    .arguments
                    .get("query")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let limit = call
                    .arguments
                    .get("limit")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(10) as usize;
                let scope_all = call
                    .arguments
                    .get("scope")
                    .and_then(|value| value.as_str())
                    .is_some_and(|scope| scope == "all");
                return self.handle_search_history(query, limit, scope_all);
            }
            // ── 合并入口（二十轮：21 → 10）──────────────────────────
            "get_context" => self.handle_get_context(),
            "inspect_context" => {
                let focus = call
                    .arguments
                    .get("focus")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let path = call
                    .arguments
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_inspect_context(focus, path)
            }
            "list_models" => self.handle_list_models(),
            "dispatch_work" => {
                let task = call
                    .arguments
                    .get("task")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let model = call.arguments.get("model").and_then(|value| value.as_str());
                let cwd = call.arguments.get("cwd").and_then(|value| value.as_str());
                let session_id = call
                    .arguments
                    .get("session_id")
                    .and_then(|value| value.as_str());
                let after_work_id = call
                    .arguments
                    .get("after_work_id")
                    .and_then(|value| value.as_str());
                let parallel = call
                    .arguments
                    .get("parallel")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                self.handle_dispatch_work(task, model, cwd, session_id, after_work_id, parallel)
            }
            "control" => {
                let work_id = call
                    .arguments
                    .get("work_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let action = call
                    .arguments
                    .get("action")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_control(work_id, action)
            }
            "work" => {
                let action = call
                    .arguments
                    .get("action")
                    .and_then(|value| value.as_str())
                    .unwrap_or("list");
                let work_id = call
                    .arguments
                    .get("work_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_work(action, work_id)
            }
            "goal" => {
                let action = call
                    .arguments
                    .get("action")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_goal(call, action)
            }
            "request_workspace_approval" => {
                let path = call
                    .arguments
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let task_fingerprint = call
                    .arguments
                    .get("task_fingerprint")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_request_workspace_approval(path, task_fingerprint)
            }
            "send_notification" => {
                let title = call
                    .arguments
                    .get("title")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let message = call
                    .arguments
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let level = call
                    .arguments
                    .get("level")
                    .and_then(|value| value.as_str())
                    .unwrap_or("info");
                self.handle_send_notification(title, message, level)
            }
            _ => None,
        };
        Box::pin(async move { result })
    }
}
