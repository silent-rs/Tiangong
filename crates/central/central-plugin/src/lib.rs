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
use tiangong_core::tools::extension::{ToolOverrideHandler, ToolSpecProvider};
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
/// 工具集（§5.3 白名单的实现态，21 项——与 tool_specs 一一对应；R3 移除
/// confirm_workspace：Workspace 授权票据化，模型不再持有登记通道）：
/// - 只读上下文：`get_context` / `list_workspaces` / `inspect_workspace` /
///   `list_models` / `list_sessions` / `search_history`；
/// - 后台工作：`dispatch_work`（新建/复用派发）、`send_to_worker`（引导）、
///   `collect_work_reports`（收集回报）、`list_work_items`（派发登记）。
///
/// 其余白名单工具（暂停/恢复/取消、征询、通知等）随中枢编排后续片扩展。
pub struct CentralPlugin {
    /// 全局默认工作区（构造时注入的快照；中枢不绑定具体工作区，§6）。
    workspace_dir: String,
    /// 只读元数据源（工作区聚合）：与普通会话管理器共享同一实例，仅使用
    /// `list_session_metadata`（带缓存），不触碰会话内容或 Core 生命周期。
    sessions: tiangong_core_manager::CoreManager,
    /// 派发通道：把后台工作请求交给 app 执行器（见 app.rs）。
    dispatch_tx: tokio::sync::mpsc::UnboundedSender<CentralPluginMessage>,
    /// 已到达、尚未进入对话的后台工作回报（与 app 共享同一实例；
    /// 暂存批量模式：随用户下一条消息一并注入，或经 collect_work_reports
    /// 由中枢在当前 turn 主动收集）。
    work_reports: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// 派发登记表（work_id → 派发事实；状态汇报经注入消息回到对话）。
    /// 持久化于 `{storage_root}/central/work-items.json`：应用重启后
    /// `list_work_items` 仍可查到既往派发（应用尽用的判定依据之一）。
    work_items: std::sync::Mutex<Vec<WorkItemRecord>>,
}

/// 工作区是否已被用户确认可用。
///
/// 天然已确认：默认工作区**及其子目录**（应用级选择的根，其子树均属
/// 既定授权范围，按前缀判定而非精确相等——review P1：精确比较会把
/// 默认工作区内的子目录误拦为「新工作区」）。
/// 按 session 归属写中枢事件日志（R5 单写，取代旧内嵌时间线）。
fn record_central_event_by_session(
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
        tracing::warn!(session_id, "事件归属会话无登记，事实未入日志");
        return;
    };
    let log = match tiangong_central::EventLog::open(
        &storage_root.join("central").join("events.jsonl"),
    ) {
        Ok(log) => log,
        Err(error) => {
            tracing::warn!(%error, "中枢事件日志打开失败（事实未入日志）");
            return;
        }
    };
    let mut event = tiangong_central::WorkEvent::new(&record.work_id, event_type)
        .with_worker_session(session_id);
    if let Some(execution_id) = record.execution_id.as_deref() {
        event = event.with_execution(execution_id);
    }
    event.payload = serde_json::json!({ "note": note });
    if let Err(error) = log.append(event) {
        tracing::warn!(%error, "中枢事件日志追加失败（事实未入日志）");
    }
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
            "get_context" => self.handle_get_context(),
            "list_workspaces" => self.handle_list_workspaces(),
            "inspect_workspace" => {
                let path = call
                    .arguments
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_inspect_workspace(path)
            }
            "list_models" => self.handle_list_models(),
            "list_sessions" => self.handle_list_sessions(),
            "cancel_work" | "pause_work" | "resume_work" => {
                let work_id = call
                    .arguments
                    .get("work_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let action = match call.name.as_str() {
                    "cancel_work" => "cancelled",
                    "pause_work" => "paused",
                    _ => "resume",
                };
                self.handle_work_control(work_id, action)
            }
            "create_goal" => {
                let title = call
                    .arguments
                    .get("title")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let description = call
                    .arguments
                    .get("description")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let workspace = call
                    .arguments
                    .get("workspace")
                    .and_then(|value| value.as_str());
                self.handle_create_goal(title, description, workspace)
            }
            "update_goal_status" => {
                let goal_id = call
                    .arguments
                    .get("goal_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let status = call
                    .arguments
                    .get("status")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_update_goal_status(goal_id, status)
            }
            "create_report" => {
                let work_id = call
                    .arguments
                    .get("work_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let summary = call
                    .arguments
                    .get("summary")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let artifacts = call
                    .arguments
                    .get("artifacts")
                    .and_then(|value| value.as_array())
                    .map(|list| {
                        list.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                let risks = call
                    .arguments
                    .get("risks")
                    .and_then(|value| value.as_array())
                    .map(|list| {
                        list.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                self.handle_create_report(work_id, summary, &artifacts, &risks)
            }
            "list_reports" => {
                let work_id = call
                    .arguments
                    .get("work_id")
                    .and_then(|value| value.as_str());
                self.handle_list_reports(work_id)
            }
            "switch_worker_model" => {
                let session_id = call
                    .arguments
                    .get("session_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let model = call
                    .arguments
                    .get("model")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let consultation = call
                    .arguments
                    .get("consultation_summary")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_switch_worker_model(session_id, model, consultation)
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
            "get_work_item_status" => {
                let work_id = call
                    .arguments
                    .get("work_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_work_item_status(work_id)
            }
            "get_work_timeline" => {
                let work_id = call
                    .arguments
                    .get("work_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_work_timeline(work_id)
            }
            "send_to_worker" => {
                let session_id = call
                    .arguments
                    .get("session_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let message = call
                    .arguments
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                self.handle_send_to_worker(session_id, message)
            }
            "dispatch_work" => {
                let after_work_id = call
                    .arguments
                    .get("after_work_id")
                    .and_then(|value| value.as_str());
                let parallel = call
                    .arguments
                    .get("parallel")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
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
                self.handle_dispatch_work(task, model, cwd, session_id, after_work_id, parallel)
            }
            "collect_work_reports" => self.handle_collect_work_reports(),
            "list_work_items" => self.handle_list_work_items(),
            _ => None,
        };
        Box::pin(async move { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn central_session_id_matches_frontend_constant() {
        // 与前端 frontend/src/lib/centralSession.ts 的 CENTRAL_SESSION_ID 一致；
        // 变更需两侧同步（历史文件按 ID 命名，改值会丢弃既有中枢对话）。
        assert_eq!(CENTRAL_SESSION_ID, "central-hub");
        assert!(is_central_session(CENTRAL_SESSION_ID));
        assert!(!is_central_session("0198abcd"));
        assert!(!is_central_session(""));
    }

    /// 中枢会话文件单独存储，且路径规则单点维护（读/写/元数据共用）。
    #[test]
    fn central_session_file_is_isolated_but_single_rule() {
        use tiangong_core::session::Session;
        let root = std::path::Path::new("/data/tiangong");
        assert_eq!(
            Session::session_file_path(root, CENTRAL_SESSION_ID).unwrap(),
            std::path::PathBuf::from("/data/tiangong/central/sessions/central-hub.json")
        );
        assert_eq!(
            Session::session_file_path(root, "0198abcd").unwrap(),
            std::path::PathBuf::from("/data/tiangong/sessions/0198abcd.json")
        );
        assert!(Session::session_file_path(root, "../evil").is_err());
        assert!(Session::session_file_path(root, "a/b").is_err());
    }

    /// 端到端读写一致性：中枢 Session 持久化到独立目录并可按同一路径加载。
    #[test]
    fn central_session_roundtrip_in_isolated_dir() {
        use tiangong_core::session::Session;
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new("中枢对话");
        session.id = CENTRAL_SESSION_ID.to_string();
        session.bind_storage_root(dir.path());
        session.try_persist_to_disk().unwrap();

        assert!(dir
            .path()
            .join("central/sessions/central-hub.json")
            .exists());
        assert!(!dir.path().join("sessions/central-hub.json").exists());

        let loaded = Session::load_from_storage(dir.path(), CENTRAL_SESSION_ID).unwrap();
        assert_eq!(loaded.id, CENTRAL_SESSION_ID);
    }

    /// 人设注入 + 白名单工具声明（§5.3 前三项）。
    /// 测试辅助：构造 plugin（派发请求收集到返回的接收端，供断言）。
    fn plugin_with_channel(
        workspace_dir: String,
        manager: tiangong_core_manager::CoreManager,
    ) -> (
        CentralPlugin,
        tokio::sync::mpsc::UnboundedReceiver<CentralPluginMessage>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let reports: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            CentralPlugin::new(workspace_dir, manager, tx, std::sync::Arc::clone(&reports)),
            rx,
            reports,
        )
    }

    /// 测试辅助：构造带独立 sessions 目录的 manager，并写入指定 cwd 的会话元数据。
    fn manager_with_session_cwds(
        root: &std::path::Path,
        cwds: &[&str],
    ) -> tiangong_core_manager::CoreManager {
        use tiangong_core::config::core::{CoreConfig, CoreConfigProvider};
        let sessions_dir = root.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        for (index, cwd) in cwds.iter().enumerate() {
            let id = format!("s{index}");
            let session = serde_json::json!({
                "id": id,
                "title": format!("会话 {index}"),
                "created_at": "2026-11-08 10:00:00",
                "updated_at": "2026-11-08 10:00:00",
                "trust_mode": "Supervised",
                "cwd": cwd,
                "messages": []
            });
            std::fs::write(sessions_dir.join(format!("{id}.json")), session.to_string()).unwrap();
        }
        tiangong_core_manager::CoreManager::new(
            CoreConfigProvider::new(CoreConfig::default()),
            root.to_path_buf(),
        )
    }

    #[test]
    fn central_plugin_persona_and_tool_specs() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        assert_eq!(plugin.id(), "central");
        let sections =
            <CentralPlugin as tiangong_core::tools::extension::PromptSectionProvider>::prompt_sections(
                &plugin,
            );
        assert_eq!(sections.len(), 1);
        assert!(sections[0].contains("天工中枢"));
        assert!(sections[0].contains("先调用工具了解情况再回答"));
        let specs = plugin.tool_specs();
        assert!(
            specs.iter().any(|spec| spec.name == "list_sessions"),
            "list_sessions 必须有声明（review o：有 handler 无 spec 则模型不可见）"
        );
        assert!(
            specs
                .iter()
                .find(|spec| spec.name == "dispatch_work")
                .is_some_and(|spec| spec
                    .input_schema
                    .get("properties")
                    .and_then(|props| props.get("session_id"))
                    .is_some()),
            "dispatch_work 声明必须包含 session_id（复用入口）"
        );
    }

    /// 工具声明与 handler 一致性：spec 集合 == 可处理集合（防再脱节）。
    #[test]
    fn central_tool_specs_equal_handlers() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let spec_names: std::collections::HashSet<String> = plugin
            .tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        // handler 侧从源码 match arm 提取（无副作用、无遗漏——review 硬断言：
        // spec 与 handler 双向相等，曾以 10 名清单 + let _ = result 假绿）
        let source = include_str!("lib.rs");
        let handle_pos = source
            .find("let result = match call.name.as_str() {")
            .expect("handle match");
        let match_body = &source[handle_pos..source.len().min(handle_pos + 16000)];
        // 截断到 match 闭合（首个顶格 '}'）
        let body_end = match_body
            .find("\n        let result")
            .unwrap_or(match_body.len());
        let body = &match_body[..body_end];
        // 按 => 分段：每段末尾的引号词（可能跨行以 | 连接）是 arm 名
        let mut handler_names: std::collections::HashSet<String> = Default::default();
        for segment in body.split("=>") {
            // 取段尾部连续的 arm 名行（以引号词结尾的行，向上直到非 arm 行）
            let tail: Vec<&str> = segment
                .lines()
                .rev()
                .take_while(|line| {
                    let t = line.trim();
                    (t.starts_with('"') || t.starts_with('|')) && !t.contains("match")
                })
                .collect();
            for line in tail {
                for token in line.split('|') {
                    let token = token.trim().trim_matches('"');
                    if !token.is_empty()
                        && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    {
                        handler_names.insert(token.to_string());
                    }
                }
            }
        }
        // match 头部（call.name.as_str() {）不会出现在 tail（无引号词）；
        // 兜底过滤显然非工具名的内容
        handler_names.retain(|name| name != "as_str");
        assert_eq!(
            spec_names, handler_names,
            "spec 与 handler 集合必须相等（spec 缺失模型不可见；handler 缺失为死声明）"
        );
        assert_eq!(
            spec_names.len(),
            21,
            "工具数应为 21（confirm_workspace 已移除；新增工具时同步更新）"
        );
    }

    /// 复用校验：中枢对话自身不可作为工作会话（review q）。
    #[test]
    fn dispatch_rejects_reusing_central_hub_itself() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, mut dispatch_rx, _reports) =
            plugin_with_channel("/tmp/ws".to_string(), manager);
        let result = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "自查", "session_id": CENTRAL_SESSION_ID}),
        );
        assert!(!result.ok);
        assert!(result.stderr.contains("中枢对话自身"));
        assert!(dispatch_rx.try_recv().is_err(), "守卫拒绝不得派发");
    }

    fn call_tool(plugin: &CentralPlugin, name: &str, args: serde_json::Value) -> ToolResult {
        let call = ToolCall {
            id: "t1".to_string(),
            name: name.to_string(),
            arguments: args,
        };
        let mut session = Session::new("测试");
        let future = ToolOverrideHandler::handle(plugin, &call, &mut session, "test");
        futures_block_on(future).expect("中枢工具必须被本插件处理")
    }

    /// 极简 block_on（测试内同步等待一次性 future，不引入运行时依赖）。
    fn futures_block_on<F: Future>(future: F) -> F::Output {
        // 简单轮询：当前 future 全部同步完成（Box::pin(async move { result })），
        // poll 一次即 Ready。
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn noop(_: *const ()) {}
        fn clone(ptr: *const ()) -> RawWaker {
            RawWaker::new(ptr, &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("中枢插件工具不应挂起"),
        }
    }

    #[test]
    fn get_context_reports_central_state() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let result = call_tool(&plugin, "get_context", serde_json::json!({}));
        assert!(result.ok);
        assert!(result.stdout.contains("后台工作派发: 已接入"));
        assert!(result.stdout.contains("/tmp/ws"));
    }

    /// 派发：模型 key 校验通过后经通道发出并登记。
    #[test]
    fn dispatch_work_sends_request_and_registers() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        // 提供一个可用模型，供派发校验
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {
                    "k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}
                },
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, mut dispatch_rx, _reports) =
            plugin_with_channel("/tmp/ws".to_string(), manager);
        // /tmp 非默认工作区：预先落盘宿主授权产物（Workspace 授权）
        add_confirmed_workspace(plugin.sessions.storage_root(), std::path::Path::new("/tmp"));
        let result = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "审查仓库提交", "model": "k3", "cwd": "/tmp"}),
        );
        assert!(result.ok, "stderr: {}", result.stderr);
        assert!(result.stdout.contains("已派发后台工作"));
        let message = dispatch_rx.try_recv().expect("派发请求应经通道发出");
        let request = match message {
            CentralPluginMessage::Dispatch(request) => request,
            other => panic!("应收到 Dispatch 消息，实际 {other:?}"),
        };
        assert_eq!(request.model.as_deref(), Some("k3"));
        assert_eq!(request.task, "审查仓库提交");
        assert_eq!(request.cwd.as_deref(), Some("/tmp"));
        assert!(!request.work_id.is_empty());
        // 登记表可见
        let listing = call_tool(&plugin, "list_work_items", serde_json::json!({}));
        assert!(listing.stdout.contains(&request.work_id));
        assert!(listing.stdout.contains("审查仓库提交"));
    }

    /// 派发：模型 key 不存在时拒绝并附可用清单。
    #[test]
    fn dispatch_work_rejects_unknown_model() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {
                    "k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}
                },
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, mut dispatch_rx, _reports) =
            plugin_with_channel("/tmp/ws".to_string(), manager);
        let result = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "审查", "model": "not-exist"}),
        );
        assert!(!result.ok);
        assert!(result.stderr.contains("可用模型"));
        assert!(result.stderr.contains("k3"));
        assert!(dispatch_rx.try_recv().is_err(), "校验失败不得派发");
    }

    /// 模型清单：列出 key 并标记路由默认。
    #[test]
    fn list_models_reports_registry() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {
                    "k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]},
                    "glm": {"provider": "p", "model": "glm-5.3", "capabilities": ["chat"]}
                },
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let result = call_tool(&plugin, "list_models", serde_json::json!({}));
        assert!(result.ok);
        assert!(result.stdout.contains("k3"));
        assert!(result.stdout.contains("k3-pro"));
        assert!(result.stdout.contains("（默认）"));
    }

    #[test]
    fn inspect_workspace_reads_git_metadata() {
        let main = tempfile::tempdir().unwrap();
        let git = main.path().join(".git");
        std::fs::create_dir_all(git.join("worktrees/central-poc")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/poc/central-mode\n").unwrap();
        let worktree_dir = main.path().join("wt/central-poc");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        // worktree 的 .git 是指向主仓库元目录的指针文件
        std::fs::write(
            worktree_dir.join(".git"),
            format!("gitdir: {}\n", git.join("worktrees/central-poc").display()),
        )
        .unwrap();
        std::fs::write(
            git.join("worktrees/central-poc/HEAD"),
            "ref: refs/heads/poc/central-mode\n",
        )
        .unwrap();
        std::fs::write(
            git.join("worktrees/central-poc/gitdir"),
            format!("{}/.git\n", worktree_dir.display()),
        )
        .unwrap();

        let (plugin, _dispatch_rx, _reports) = plugin_with_channel(
            main.path().display().to_string(),
            manager_with_session_cwds(main.path(), &[]),
        );
        let result = call_tool(
            &plugin,
            "inspect_workspace",
            serde_json::json!({"path": worktree_dir.display().to_string()}),
        );
        assert!(result.ok);
        assert!(result.stdout.contains("主仓库分支: poc/central-mode"));
        assert!(result.stdout.contains("worktree central-poc"));
    }

    #[test]
    fn inspect_workspace_rejects_missing_path() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let result = call_tool(
            &plugin,
            "inspect_workspace",
            serde_json::json!({"path": "/definitely/not/exist"}),
        );
        assert!(!result.ok);
        assert_eq!(result.error_kind.as_deref(), Some("argument_error"));
    }
    #[test]
    fn list_workspaces_aggregates_session_cwds_with_git_branch() {
        let root = tempfile::tempdir().unwrap();
        // 两个会话 cwd，其中一个重复
        let repo = root.path().join("dev/tiangong");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/poc/central-mode\n").unwrap();
        let manager = manager_with_session_cwds(
            root.path(),
            &[repo.to_str().unwrap(), repo.to_str().unwrap()],
        );
        let (plugin, _dispatch_rx, _reports) =
            plugin_with_channel(root.path().display().to_string(), manager);
        let result = call_tool(&plugin, "list_workspaces", serde_json::json!({}));
        assert!(result.ok);
        // 默认工作区在列（去重后仅一次 dev/tiangong）
        assert!(result.stdout.contains("（默认）"));
        assert_eq!(result.stdout.matches("dev/tiangong").count(), 1);
        assert!(result.stdout.contains("git 分支 poc/central-mode"));
    }
    /// 应用尽用兜底：无 session_id 的后续派发，返回应附既往派发提示。
    #[test]
    fn dispatch_without_reuse_hints_prior_works() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {
                    "k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}
                },
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        // 第一次派发（新建）：无既往，无提示
        let first = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "第一轮 review"}),
        );
        assert!(first.ok);
        assert!(!first.stdout.contains("既往派发"));
        // 第二次派发仍未指定 session_id：应提示既往派发供自查
        let second = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "继续 review 新增内容"}),
        );
        assert!(second.ok);
        assert!(second.stdout.contains("既往派发"));
        assert!(second.stdout.contains(
            "继续 review 新增内容"
                .chars()
                .take(30)
                .collect::<String>()
                .as_str()
        ));
        assert!(second.stdout.contains("向对应会话重派"));
    }

    /// 回报批量模式：暂存 → collect 取走 → 再 collect 为空。
    #[test]
    fn collect_work_reports_takes_all_then_empty() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx, reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        reports
            .lock()
            .unwrap()
            .push("[后台工作完成] work-a：结果一".to_string());
        reports
            .lock()
            .unwrap()
            .push("[后台工作完成] work-b：结果二".to_string());

        let first = call_tool(&plugin, "collect_work_reports", serde_json::json!({}));
        assert!(first.ok);
        assert!(first.stdout.contains("共 2 条"));
        assert!(first.stdout.contains("work-a"));
        assert!(first.stdout.contains("work-b"));

        let second = call_tool(&plugin, "collect_work_reports", serde_json::json!({}));
        assert!(second.ok);
        assert!(second.stdout.contains("没有待汇报"));
    }
    /// 引导消息：校验 + 登记 + 通道消息类型。
    #[test]
    fn send_to_worker_validates_and_sends_steer() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, mut dispatch_rx, _reports) =
            plugin_with_channel("/tmp/ws".to_string(), manager);
        // 目标不存在 → 拒绝
        let missing = call_tool(
            &plugin,
            "send_to_worker",
            serde_json::json!({"session_id": "nope", "message": "调整"}),
        );
        assert!(!missing.ok);
        assert!(missing.stderr.contains("会话不存在"));
        // 建一个会话文件后 → 通过并发送 Steer
        let target = root.path().join("sessions/worker-1.json");
        std::fs::write(
            &target,
            serde_json::json!({
                "id": "worker-1", "title": "w", "created_at": "2026-11-10 10:00:00",
                "updated_at": "2026-11-10 10:00:00", "trust_mode": "Supervised",
                "cwd": "/tmp", "messages": []
            })
            .to_string(),
        )
        .unwrap();
        let ok = call_tool(
            &plugin,
            "send_to_worker",
            serde_json::json!({"session_id": "worker-1", "message": "重点看事件流部分"}),
        );
        assert!(ok.ok, "stderr: {}", ok.stderr);
        match dispatch_rx.try_recv().expect("应有通道消息") {
            CentralPluginMessage::Steer(request) => {
                assert_eq!(request.session_id, "worker-1");
                assert_eq!(request.task, "重点看事件流部分");
                assert!(request.work_id.starts_with("steer-"));
            }
            other => panic!("应收到 Steer，实际 {other:?}"),
        }
        // 中枢自身 → 拒绝
        let self_send = call_tool(
            &plugin,
            "send_to_worker",
            serde_json::json!({"session_id": CENTRAL_SESSION_ID, "message": "x"}),
        );
        assert!(!self_send.ok);
    }
    /// 跨会话历史搜索：标题与正文命中、片段返回。
    #[tokio::test]
    async fn search_history_finds_title_and_body() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        // 两个会话：一个标题命中，一个正文命中
        std::fs::write(
            root.path().join("sessions/s-title.json"),
            serde_json::json!({
                "id": "s-title", "title": "中枢模式开发准备", "created_at": "2026-11-10 10:00:00",
                "updated_at": "2026-11-10 11:00:00", "trust_mode": "Supervised",
                "cwd": "/tmp", "messages": []
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.path().join("sessions/s-body.json"),
            serde_json::json!({
                "id": "s-body", "title": "其他", "created_at": "2026-11-10 10:00:00",
                "updated_at": "2026-11-10 12:00:00", "trust_mode": "Supervised",
                "cwd": "/tmp",
                "messages": [
                    {"id": "m1", "role": "assistant", "created_at": "2026-11-10 12:00:01",
                     "content": [{"type": "text", "text": "中枢模式的设计模式调整已经完成"}]}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let call = ToolCall {
            id: "t1".to_string(),
            name: "search_history".to_string(),
            arguments: serde_json::json!({"query": "中枢模式"}),
        };
        let mut session = Session::new("测试");
        let result = ToolOverrideHandler::handle(&plugin, &call, &mut session, "test")
            .await
            .expect("应有结果");
        assert!(result.ok);
        assert!(
            result.stdout.contains("s-title"),
            "标题命中：{}",
            result.stdout
        );
        assert!(
            result.stdout.contains("设计模式调整"),
            "正文片段：{}",
            result.stdout
        );
        // 无命中
        let call2 = ToolCall {
            id: "t2".to_string(),
            name: "search_history".to_string(),
            arguments: serde_json::json!({"query": "绝不存在的关键词xyz"}),
        };
        let result2 = ToolOverrideHandler::handle(&plugin, &call2, &mut session, "test")
            .await
            .expect("应有结果");
        assert!(result2.ok);
        assert!(result2.stdout.contains("未找到"));
    }
    /// 派发登记会话维度 upsert：同会话再派发更新而非堆积；上限 20 条。
    #[test]
    fn work_items_upsert_by_session_and_cap_20() {
        let root = tempfile::tempdir().unwrap();
        // 预置 25 条不同会话的记录
        for i in 0..25 {
            let record = WorkItemRecord {
                work_id: format!("work-old-{i}"),
                reused_session: false,
                session_id: format!("sess-{i}"),
                task: format!("旧任务 {i}"),
                model: None,
                dispatched_at: format!("2026-11-10 1{:02}:00:00", i % 10),
                finished_at: Some("2026-11-10 12:00:00".to_string()),
                outcome: None,
                events: Vec::new(),
                queued_at: None,
                after_work_id: None,
                cwd: None,
                execution_id: None,
            };
            upsert_work_item(root.path(), &record);
        }
        // 同一会话 sess-24 再派发 → 更新（不新增）
        let again = WorkItemRecord {
            work_id: "work-new-1".to_string(),
            reused_session: true,
            session_id: "sess-24".to_string(),
            task: "继续上次的任务".to_string(),
            model: Some("k3".to_string()),
            dispatched_at: "2026-11-10 15:00:00".to_string(),
            finished_at: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: None,
            outcome: None,
            execution_id: None,
        };
        upsert_work_item(root.path(), &again);
        let items = load_work_items(root.path());
        assert_eq!(items.len(), 20, "上限 20：{}", items.len());
        // sess-24 只有一条且是更新后的
        let hits: Vec<_> = items
            .iter()
            .filter(|item| item.session_id == "sess-24")
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].work_id, "work-new-1");
        assert_eq!(hits[0].task, "继续上次的任务");
        assert!(hits[0].reused_session);
        // 最旧的 sess-0 已被裁剪
        assert!(!items.iter().any(|item| item.session_id == "sess-0"));
    }
    /// 终态登记兜底：work_id 未命中时按 session_id 找未终态条目——
    /// 用户实测「执行完毕显示中断」的修复路径；双键都未命中时不落盘。
    #[test]
    fn mark_finished_falls_back_to_session() {
        let root = tempfile::tempdir().unwrap();
        let record = WorkItemRecord {
            work_id: "work-real".to_string(),
            reused_session: false,
            session_id: "sess-1".to_string(),
            task: "执行任务".to_string(),
            model: None,
            dispatched_at: "2026-11-11 12:00:00".to_string(),
            finished_at: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: None,
            outcome: None,
            execution_id: None,
        };
        upsert_work_item(root.path(), &record);
        // 错误 work_id + 正确 session_id：兜底命中并写终态
        mark_work_item_finished_with_session(root.path(), "work-wrong", Some("sess-1"), false);
        let items = load_work_items(root.path());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].work_id, "work-real");
        assert!(
            items[0].finished_at.is_some(),
            "session 兜底应写入终态：{items:?}"
        );
        // 双键都未命中：不新增不改动
        mark_work_item_finished_with_session(root.path(), "work-none", Some("sess-none"), true);
        let after = load_work_items(root.path());
        assert_eq!(after.len(), 1, "未命中不得新增条目：{after:?}");
        // 成败落盘：成功 → completed；失败 → failed
        let finished = &after[0];
        assert_eq!(finished.outcome.as_deref(), Some("completed"));
        mark_work_item_finished_with_session(root.path(), "work-x", Some("sess-9"), true);
        // sess-9 无未终态条目（sess-1 已终态）→ 不改动
        assert_eq!(load_work_items(root.path()).len(), 1);
    }

    /// legacy 终态从中枢会话回报推断成败：回报含「[后台工作失败] work-X」
    /// → failed；其余终态 → completed。
    #[test]
    fn recover_infers_legacy_outcome_from_central_session() {
        let root = tempfile::tempdir().unwrap();
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        // 构造中枢会话（含一条失败回报与一条完成回报）
        let central_dir = root.path().join("central").join("sessions");
        std::fs::create_dir_all(&central_dir).unwrap();
        std::fs::write(
            central_dir.join("central-hub.json"),
            serde_json::json!({
                "messages": [
                    {"role": "user", "content": "[后台工作完成] work-legacy-ok（模型 k3）\n任务：正常\n执行结果：完成"},
                    {"role": "user", "content": "[后台工作失败] work-legacy-bad（模型 k3）\n任务：出错\n失败原因：超时"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        for (id, task) in [
            ("work-legacy-ok", "正常任务"),
            ("work-legacy-bad", "出错任务"),
        ] {
            upsert_work_item(
                root.path(),
                &WorkItemRecord {
                    work_id: id.to_string(),
                    reused_session: false,
                    session_id: format!("s-{id}"),
                    task: task.to_string(),
                    model: None,
                    dispatched_at: now.clone(),
                    finished_at: Some(now.clone()),
                    outcome: None, // legacy：字段上线前的终态记录
                    events: Vec::new(),
                    queued_at: None,
                    after_work_id: None,
                    cwd: None,
                    execution_id: None,
                },
            );
        }
        let entries = recover_registry_entries(root.path());
        let ok = entries.iter().find(|e| e.work_id == "work-legacy-ok");
        let bad = entries.iter().find(|e| e.work_id == "work-legacy-bad");
        assert_eq!(ok.map(|e| e.state.as_str()), Some("completed"));
        assert_eq!(bad.map(|e| e.state.as_str()), Some("failed"));
    }

    /// 重启重建：outcome 分态、旧终态近似 completed、未终态近期
    /// interrupted、过期未终态跳过、completed/failed 各截 10。
    #[test]
    fn recover_registry_rebuilds_all_states() {
        let root = tempfile::tempdir().unwrap();
        let now = chrono::Local::now();
        let recent = now.format("%Y-%m-%d %H:%M:%S").to_string();
        let stale = (now - chrono::Duration::hours(12))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let mut records = Vec::new();
        for i in 0..12 {
            records.push(WorkItemRecord {
                work_id: format!("work-c{i}"),
                reused_session: false,
                session_id: format!("sc{i}"),
                task: format!("完成 {i}"),
                model: None,
                dispatched_at: recent.clone(),
                finished_at: Some(recent.clone()),
                outcome: Some("completed".to_string()),
                events: Vec::new(),
                queued_at: None,
                after_work_id: None,
                cwd: None,
                execution_id: None,
            });
        }
        records.push(WorkItemRecord {
            work_id: "work-old-done".to_string(),
            reused_session: false,
            session_id: "s-old".to_string(),
            task: "旧终态（无 outcome）".to_string(),
            model: None,
            dispatched_at: stale.clone(),
            finished_at: Some(stale.clone()),
            outcome: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: None,
            execution_id: None,
        });
        records.push(WorkItemRecord {
            work_id: "work-fail".to_string(),
            reused_session: false,
            session_id: "s-f".to_string(),
            task: "失败任务".to_string(),
            model: None,
            dispatched_at: recent.clone(),
            finished_at: Some(recent.clone()),
            outcome: Some("failed".to_string()),
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: None,
            execution_id: None,
        });
        records.push(WorkItemRecord {
            work_id: "work-recent-open".to_string(),
            reused_session: false,
            session_id: "s-ro".to_string(),
            task: "近期未终态".to_string(),
            model: None,
            dispatched_at: recent.clone(),
            finished_at: None,
            outcome: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: None,
            execution_id: None,
        });
        records.push(WorkItemRecord {
            work_id: "work-stale-open".to_string(),
            reused_session: false,
            session_id: "s-so".to_string(),
            task: "过期未终态".to_string(),
            model: None,
            dispatched_at: stale.clone(),
            finished_at: None,
            outcome: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: None,
            execution_id: None,
        });
        for record in &records {
            upsert_work_item(root.path(), record);
        }
        let entries = recover_registry_entries(root.path());
        let count = |state: &str| entries.iter().filter(|e| e.state.as_str() == state).count();
        // completed = 12 条 outcome + 1 条旧终态近似 → 截断为 10
        assert_eq!(count("completed"), 10, "completed 截断 10：{entries:?}");
        // 截断保留最新的（work-c11 在，work-c0 被移除）
        assert!(entries.iter().any(|e| e.work_id == "work-c11"));
        assert!(!entries.iter().any(|e| e.work_id == "work-c0"));
        assert_eq!(count("failed"), 1);
        assert!(entries
            .iter()
            .any(|e| e.work_id == "work-fail" && e.state.as_str() == "failed"));
        // 近期未终态 → interrupted；过期未终态跳过
        assert_eq!(count("interrupted"), 1);
        assert!(entries
            .iter()
            .any(|e| e.work_id == "work-recent-open" && e.state.as_str() == "interrupted"));
        assert!(!entries.iter().any(|e| e.work_id == "work-stale-open"));
    }

    /// 登记文件损坏（坏 JSON）时按空表恢复，后续 upsert 修复文件。
    #[test]
    fn work_items_recover_from_corruption() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("central")).unwrap();
        std::fs::write(
            central_state::persistence::work_items_path(root.path()),
            "{ not valid json !!",
        )
        .unwrap();
        // 损坏 → 空表（不 panic）
        assert!(load_work_items(root.path()).is_empty());
        // upsert 以空表为基础写入 → 文件被合法内容覆盖
        let record = WorkItemRecord {
            work_id: "work-1".to_string(),
            reused_session: false,
            session_id: "sess-1".to_string(),
            task: "任务".to_string(),
            model: None,
            dispatched_at: "2026-11-11 12:00:00".to_string(),
            finished_at: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: None,
            outcome: None,
            execution_id: None,
        };
        upsert_work_item(root.path(), &record);
        let items = load_work_items(root.path());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].work_id, "work-1");
    }

    /// 工作区确认判定：默认工作区子树按前缀放行（含子目录）；同级近名
    /// 目录不误放行；登记目录 canonicalize 精确匹配。
    #[test]
    fn workspace_confirmation_prefix_and_canonical() {
        let root = tempfile::tempdir().unwrap();
        let default_ws = tempfile::tempdir().unwrap(); // 默认工作区（真实存在，canonicalize 生效）
        let default = default_ws.path().to_string_lossy().to_string();
        let inside = default_ws.path().join("sub").join("proj");
        std::fs::create_dir_all(&inside).unwrap();
        // 默认工作区与其子目录：天然已确认
        assert!(workspace_confirmed(root.path(), &default, &default));
        assert!(workspace_confirmed(
            root.path(),
            &default,
            &inside.to_string_lossy()
        ));
        // 同级近名目录（/x-pro 不是 /x 子树）：不放行
        let near = default_ws.path().parent().unwrap().join(format!(
            "{}-near",
            default_ws.path().file_name().unwrap().to_string_lossy()
        ));
        assert!(!workspace_confirmed(
            root.path(),
            &default,
            &near.to_string_lossy()
        ));
        // 未登记的无关目录：不放行
        let outside = tempfile::tempdir().unwrap();
        let outside_path = outside.path().canonicalize().unwrap();
        assert!(!workspace_confirmed(
            root.path(),
            &default,
            &outside_path.to_string_lossy()
        ));
        // 登记（canonicalize 落盘）后：原样路径与带尾斜杠变体均放行
        std::fs::create_dir_all(root.path().join("central")).unwrap();
        std::fs::write(
            central_state::persistence::confirmed_workspaces_path(root.path()),
            serde_json::to_string(&vec![outside_path.to_string_lossy().to_string()]).unwrap(),
        )
        .unwrap();
        assert!(workspace_confirmed(
            root.path(),
            &default,
            &outside_path.to_string_lossy()
        ));
    }

    /// 目标登记与状态更新：同名幂等、非法状态拒绝、get_context 注入。
    #[test]
    fn goal_lifecycle_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, mut dispatch_rx, _reports) =
            plugin_with_channel("/tmp/ws".to_string(), manager);
        let _ = &mut dispatch_rx; // 本测试无派发，防 unused
                                  // 登记
        let created = call_tool(
            &plugin,
            "create_goal",
            serde_json::json!({"title": "完成中枢模式 POC", "description": "按设计稿实现完整功能"}),
        );
        assert!(created.ok, "stderr: {}", created.stderr);
        assert!(created.stdout.contains("完成中枢模式 POC"));
        // 同名幂等
        let again = call_tool(
            &plugin,
            "create_goal",
            serde_json::json!({"title": "完成中枢模式 POC"}),
        );
        assert!(again.ok);
        assert!(again.stdout.contains("已存在"));
        // 空标题拒绝
        let empty = call_tool(&plugin, "create_goal", serde_json::json!({"title": " "}));
        assert!(!empty.ok);
        // 状态更新（取出 goal_id）
        let goal_id = load_goals(plugin.sessions.storage_root())[0]
            .goal_id
            .clone();
        let updated = call_tool(
            &plugin,
            "update_goal_status",
            serde_json::json!({"goal_id": goal_id, "status": "achieved"}),
        );
        assert!(updated.ok, "stderr: {}", updated.stderr);
        assert_eq!(
            load_goals(plugin.sessions.storage_root())[0].status,
            "achieved"
        );
        // 非法状态拒绝
        let bad = call_tool(
            &plugin,
            "update_goal_status",
            serde_json::json!({"goal_id": goal_id, "status": "done"}),
        );
        assert!(!bad.ok);
        // get_context 注入目标（新的 active 目标）
        let _ = call_tool(
            &plugin,
            "create_goal",
            serde_json::json!({"title": "重构状态机"}),
        );
        let context = call_tool(&plugin, "get_context", serde_json::json!({}));
        assert!(context.ok);
        assert!(context.stdout.contains("重构状态机"), "{}", context.stdout);
    }

    /// 报告存档与查询往返。
    #[test]
    fn report_archive_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let created = call_tool(
            &plugin,
            "create_report",
            serde_json::json!({
                "work_id": "work-x1",
                "summary": "第十轮 review 修复全部完成",
                "artifacts": ["/tmp/report.md"],
                "risks": ["旧记录无 outcome"]
            }),
        );
        assert!(created.ok, "stderr: {}", created.stderr);
        // 空摘要拒绝
        let empty = call_tool(
            &plugin,
            "create_report",
            serde_json::json!({"work_id": "w", "summary": " "}),
        );
        assert!(!empty.ok);
        // 查询
        let listed = call_tool(&plugin, "list_reports", serde_json::json!({}));
        assert!(listed.ok);
        assert!(listed.stdout.contains("第十轮 review"), "{}", listed.stdout);
        assert!(listed.stdout.contains("/tmp/report.md"));
        // 按工作过滤
        let filtered = call_tool(
            &plugin,
            "list_reports",
            serde_json::json!({"work_id": "work-x1"}),
        );
        assert!(filtered.ok);
        assert!(filtered.stdout.contains("第十轮 review"));
        let none = call_tool(
            &plugin,
            "list_reports",
            serde_json::json!({"work_id": "work-none"}),
        );
        assert!(none.ok);
        assert!(none.stdout.contains("没有"));
    }

    /// 换模型：商量结论缺失拒绝；有效请求通过校验并留痕。
    #[test]
    fn switch_model_requires_consultation() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &["/tmp/ws"]);
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        // 无商量结论：拒绝（§15.11 先商量硬约束）
        let no_consult = call_tool(
            &plugin,
            "switch_worker_model",
            serde_json::json!({"session_id": "s0", "model": "k3"}),
        );
        assert!(!no_consult.ok, "缺少商量结论必须拒绝");
        assert!(no_consult.stderr.contains("商量"));
        // 中枢会话拒绝
        let central = call_tool(
            &plugin,
            "switch_worker_model",
            serde_json::json!({"session_id": "central-hub", "model": "k3", "consultation_summary": "用户同意换成 k3 处理超时问题"}),
        );
        assert!(!central.ok);
        // 未知模型拒绝（附可用清单）
        let unknown = call_tool(
            &plugin,
            "switch_worker_model",
            serde_json::json!({"session_id": "s0", "model": "nope", "consultation_summary": "用户同意换模型"}),
        );
        assert!(!unknown.ok);
        assert!(unknown.stderr.contains("可用模型"));
        // 有效请求：离线会话 → 待生效（不报错，留痕完成）
        let valid = call_tool(
            &plugin,
            "switch_worker_model",
            serde_json::json!({"session_id": "s0", "model": "k3", "consultation_summary": "模型超时，用户同意换 k3"}),
        );
        assert!(valid.ok, "stderr: {}", valid.stderr);
        assert!(valid.stdout.contains("留痕"));
    }

    /// get_context 工具数与 tool_specs 一致（review 硬断言：漂移即失真）。
    #[test]
    fn get_context_tool_count_matches_specs() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let context = call_tool(&plugin, "get_context", serde_json::json!({}));
        assert!(context.ok, "stderr: {}", context.stderr);
        let count: usize = context
            .stdout
            .lines()
            .find_map(|line| line.trim().strip_prefix("中枢工具数: "))
            .and_then(|value| value.trim().parse().ok())
            .expect("get_context 必须输出「中枢工具数: N」");
        let specs = plugin.tool_specs().len();
        assert_eq!(count, specs, "get_context 工具数与 tool_specs 漂移");
    }

    /// dispatch_work schema：after_work_id/parallel 必须在 properties 内
    /// （review P1：曾落在 properties 外——模型看不到参数，写入锁豁免
    /// 不可达且同目录派发被无条件串行）。
    #[test]
    fn dispatch_work_schema_exposes_queue_and_parallel_params() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &[]);
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let specs = plugin.tool_specs();
        let spec = specs
            .iter()
            .find(|spec| spec.name == "dispatch_work")
            .expect("dispatch_work spec");
        let properties = spec
            .input_schema
            .get("properties")
            .and_then(|value| value.as_object())
            .expect("properties 必须是对象");
        for key in [
            "task",
            "model",
            "cwd",
            "session_id",
            "after_work_id",
            "parallel",
        ] {
            assert!(
                properties.contains_key(key),
                "dispatch_work.properties 缺少 {key}（模型将看不到该参数）"
            );
        }
    }

    /// 时间线：事件追加持久化 + get_work_timeline 工具查询往返。
    #[test]
    fn work_timeline_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &["/tmp/ws"]);
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        let dispatch = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "时间线任务", "session_id": "s0"}),
        );
        assert!(dispatch.ok, "stderr: {}", dispatch.stderr);
        // 追加事件（模拟宿主侧写入——R5：事件日志是唯一事实源）
        record_central_event_by_session(
            root.path(),
            "s0",
            tiangong_central::event::WorkEventType::WorkItemWaitingUser,
            "提出问题，工作中挂起",
        );
        record_central_event_by_session(
            root.path(),
            "s0",
            tiangong_central::event::WorkEventType::WorkItemStarted,
            "收到答复恢复",
        );
        // 查询往返
        let work_id = {
            let items = plugin.work_items.lock().unwrap_or_else(|e| e.into_inner());
            items
                .iter()
                .rev()
                .find(|item| item.session_id == "s0")
                .map(|item| item.work_id.clone())
                .unwrap()
        };
        let timeline = call_tool(
            &plugin,
            "get_work_timeline",
            serde_json::json!({"work_id": work_id}),
        );
        assert!(timeline.ok, "stderr: {}", timeline.stderr);
        assert!(
            timeline.stdout.contains("work_item.waiting_user"),
            "{}",
            timeline.stdout
        );
        assert!(
            timeline.stdout.contains("提出问题，工作中挂起"),
            "{}",
            timeline.stdout
        );
        assert!(
            timeline.stdout.contains("work_item.started"),
            "{}",
            timeline.stdout
        );
        // 未知的 work_id 拒绝
        let ghost = call_tool(
            &plugin,
            "get_work_timeline",
            serde_json::json!({"work_id": "work-none"}),
        );
        assert!(!ghost.ok);
    }

    /// 工作控制工具：work_id 校验 + Control 消息送达（通道层验证）。
    #[test]
    fn work_control_validates_and_sends() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager_with_session_cwds(root.path(), &["/tmp/ws"]);
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, mut dispatch_rx, _reports) =
            plugin_with_channel("/tmp/ws".to_string(), manager);
        // 派发一个工作（登记存在）
        let dispatch = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "长任务", "session_id": "s0"}),
        );
        assert!(dispatch.ok, "stderr: {}", dispatch.stderr);
        let work_id = {
            let items = plugin.work_items.lock().unwrap_or_else(|e| e.into_inner());
            items
                .iter()
                .rev()
                .find(|item| item.session_id == "s0")
                .map(|item| item.work_id.clone())
                .unwrap()
        };
        // 排空派发消息（dispatch 先于 control 到达）
        let _ = dispatch_rx.try_recv();
        // 取消：消息送达
        let cancel = call_tool(
            &plugin,
            "cancel_work",
            serde_json::json!({"work_id": work_id}),
        );
        assert!(cancel.ok, "stderr: {}", cancel.stderr);
        match dispatch_rx.try_recv() {
            Ok(CentralPluginMessage::Control(control)) => {
                assert_eq!(control.action, "cancelled");
                assert_eq!(control.work_id, work_id);
                assert_eq!(control.session_id, "s0");
            }
            other => panic!("应收到 Control 消息，实际 {other:?}"),
        }
        // 不存在的 work_id 拒绝
        let ghost = call_tool(
            &plugin,
            "pause_work",
            serde_json::json!({"work_id": "work-none"}),
        );
        assert!(!ghost.ok, "不存在的 work 必须拒绝");
        // 空 work_id 拒绝
        let empty = call_tool(&plugin, "resume_work", serde_json::json!({"work_id": "  "}));
        assert!(!empty.ok, "空 work_id 必须拒绝");
    }

    /// Workspace 授权（R3）：未经授权的 cwd 拒绝且指引等待用户授权；
    /// 授权是宿主事实（此处以落盘模拟宿主授权产物）→ 放行；
    /// 默认工作区与已授权目录无需授权；复用会话的 cwd 同样校验。
    #[test]
    fn dispatch_requires_workspace_confirmation() {
        let root = tempfile::tempdir().unwrap();
        // 预置会话 s0（cwd 指向未确认目录，供复用路径校验测试）
        let pending = tempfile::tempdir().unwrap();
        let pending_path = pending.path().to_string_lossy().to_string();
        let manager = manager_with_session_cwds(root.path(), &[&pending_path]);
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let extra = tempfile::tempdir().unwrap();
        let extra_path = extra.path().to_string_lossy().to_string();
        // 默认工作区设为另一目录：extra 才是「未确认的新工作区」
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        // 未确认：拒绝并给出征询指引
        let denied = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "扫描依赖并升级", "cwd": extra_path}),
        );
        assert!(!denied.ok, "未确认工作区必须拒绝：{}", denied.stderr);
        assert!(
            denied.stderr.contains("尚未经用户授权"),
            "拒绝信息须说明授权流程：{}",
            denied.stderr
        );
        assert!(
            denied.stderr.contains("授权"),
            "拒绝信息须指向用户授权（非助手自证）：{}",
            denied.stderr
        );
        // 宿主授权产物（模拟：授权通道落盘——助手侧无登记工具）
        add_confirmed_workspace(
            plugin.sessions.storage_root(),
            std::path::Path::new(&extra_path),
        );
        // 确认后放行
        let allowed = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "扫描依赖并升级", "cwd": extra_path}),
        );
        assert!(allowed.ok, "stderr: {}", allowed.stderr);
        assert!(!allowed.stderr.contains("尚未经用户授权"));
        // 复用路径同样校验（review P1：强制复用把模型赶向复用，该路径
        // 不得成为绕过通道）——会话 cwd 未确认时复用派发被拒
        let reuse = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "扫描依赖并升级（复用）", "session_id": "s0"}),
        );
        assert!(
            !reuse.ok,
            "复用未确认 cwd 的会话必须被拒绝：{}",
            reuse.stderr
        );
        assert!(
            reuse.stderr.contains("未经用户授权"),
            "复用拒绝须说明授权流程：{}",
            reuse.stderr
        );
    }

    /// 相似任务强制复用：同前缀第二次（不带 session_id/【全新】）拒绝，
    /// 带 session_id 与【全新】标记放行。
    #[test]
    fn dispatch_similarity_hint_excludes_self() {
        let root = tempfile::tempdir().unwrap();
        // 预置会话 s0（cwd=默认工作区）：复用链路的真实目标
        let manager = manager_with_session_cwds(root.path(), &["/tmp/ws"]);
        std::fs::write(
            root.path().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let (plugin, _dispatch_rx, _reports) = plugin_with_channel("/tmp/ws".to_string(), manager);
        // 首次派发（复用 s0）：无相似历史，不得自匹配拒绝
        let first = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "Review 天工仓库的分支开发情况", "session_id": "s0"}),
        );
        assert!(first.ok, "stderr: {}", first.stderr);
        assert!(
            !first.stderr.contains("高度相似"),
            "首次派发不得自匹配：{}",
            first.stderr
        );
        // 强制复用：同前缀任务且未带 session_id、未标【全新】时必须拒绝，
        // 且拒绝信息必须指向 s0（从登记取真相源，不做纯文本解析）
        let second = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "Review 天工仓库的分支开发情况（增量）"}),
        );
        assert!(
            !second.ok,
            "同前缀且未带 session_id/【全新】的派发必须被拒绝：{}",
            second.stderr
        );
        assert!(
            second.stderr.contains("高度相似"),
            "拒绝信息必须说明相似来源：{}",
            second.stderr
        );
        assert!(
            second.stderr.contains("session_id=s0"),
            "拒绝信息必须指明带 session_id=s0 重发：{}",
            second.stderr
        );
        // 带 session_id 重发同前缀任务：放行（无条件执行，不可静默跳过）
        let third = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "Review 天工仓库的分支开发情况（增量）", "session_id": "s0"}),
        );
        assert!(third.ok, "stderr: {}", third.stderr);
        assert!(
            !third.stderr.contains("高度相似"),
            "显式指定 session_id 不得被拒绝：{}",
            third.stderr
        );
        // 【全新】标记：跳过相似拒绝，允许无关的全新工作
        let fourth = call_tool(
            &plugin,
            "dispatch_work",
            serde_json::json!({"task": "【全新】Review 天工仓库的分支开发情况"}),
        );
        assert!(fourth.ok, "stderr: {}", fourth.stderr);
        assert!(
            !fourth.stderr.contains("高度相似"),
            "【全新】标记必须放行：{}",
            fourth.stderr
        );
    }
}
