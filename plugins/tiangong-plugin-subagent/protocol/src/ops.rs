//! 操作名与请求/响应类型。
//!
//! 两组入口：
//! - AI 工具操作（operation 等于工具名）：会话归属以宿主注入的 invocation
//!   context 为准，忽略请求参数中的会话字段；
//! - UI 操作（`ui_*`）：由管理页显式携带 session_id / workspace。

use serde::{Deserialize, Serialize};

use crate::config::{AdapterCapabilities, AgentConfig, BackendKind, WorkspacePolicy};
use crate::state::{ActivationRecord, AgentEventRecord, RunRecord, RunStatus, TaskRecord};

// ── AI 工具操作名 ──────────────────────────────────────────────

pub const TOOL_CREATE_AGENT: &str = "create_agent";
pub const TOOL_LIST_AGENTS: &str = "list_agents";
pub const TOOL_GET_AGENT: &str = "get_agent";
pub const TOOL_ACTIVATE_AGENT: &str = "activate_agent";
pub const TOOL_DEACTIVATE_AGENT: &str = "deactivate_agent";
pub const TOOL_LIST_ACTIVE_AGENTS: &str = "list_active_agents";
pub const TOOL_SEND_AGENT_MESSAGE: &str = "send_agent_message";
pub const TOOL_SUBMIT_AGENT_TASK: &str = "submit_agent_task";
pub const TOOL_GET_AGENT_TASK: &str = "get_agent_task";
pub const TOOL_LIST_AGENT_TASKS: &str = "list_agent_tasks";
pub const TOOL_GET_AGENT_RUN: &str = "get_agent_run";
pub const TOOL_INTERRUPT_AGENT_RUN: &str = "interrupt_agent_run";
pub const TOOL_CANCEL_AGENT_RUN: &str = "cancel_agent_run";
pub const TOOL_LIST_AGENT_EVENTS: &str = "list_agent_events";
pub const TOOL_GET_AGENT_ARTIFACTS: &str = "get_agent_artifacts";
pub const TOOL_GET_AGENT_MEMORY: &str = "get_agent_memory";
pub const TOOL_APPEND_AGENT_MEMORY: &str = "append_agent_memory";
pub const TOOL_APPEND_AGENT_INSTRUCTIONS: &str = "append_agent_instructions";
pub const TOOL_REPORT_AGENT_RESULT: &str = "report_agent_result";
pub const TOOL_LIST_PENDING_WORK: &str = "list_pending_work";
pub const TOOL_LOAD_WORKSPACE_STATE: &str = "load_workspace_state";
pub const TOOL_UPDATE_WORKSPACE_STATE: &str = "update_workspace_state";

/// 全部工具操作名（与 WASM tool-specs 一一对应）。
pub const TOOL_OPERATIONS: &[&str] = &[
    TOOL_CREATE_AGENT,
    TOOL_LIST_AGENTS,
    TOOL_GET_AGENT,
    TOOL_ACTIVATE_AGENT,
    TOOL_DEACTIVATE_AGENT,
    TOOL_LIST_ACTIVE_AGENTS,
    TOOL_SEND_AGENT_MESSAGE,
    TOOL_SUBMIT_AGENT_TASK,
    TOOL_GET_AGENT_TASK,
    TOOL_LIST_AGENT_TASKS,
    TOOL_GET_AGENT_RUN,
    TOOL_INTERRUPT_AGENT_RUN,
    TOOL_CANCEL_AGENT_RUN,
    TOOL_LIST_AGENT_EVENTS,
    TOOL_GET_AGENT_ARTIFACTS,
    TOOL_GET_AGENT_MEMORY,
    TOOL_APPEND_AGENT_MEMORY,
    TOOL_APPEND_AGENT_INSTRUCTIONS,
    TOOL_REPORT_AGENT_RESULT,
    TOOL_LIST_PENDING_WORK,
    TOOL_LOAD_WORKSPACE_STATE,
    TOOL_UPDATE_WORKSPACE_STATE,
];

// ── UI 操作名 ──────────────────────────────────────────────────

pub const UI_STATE_SNAPSHOT: &str = "ui_state_snapshot";
pub const UI_AGENT_CREATE: &str = "ui_agent_create";
pub const UI_AGENT_UPDATE: &str = "ui_agent_update";
pub const UI_AGENT_DELETE: &str = "ui_agent_delete";
pub const UI_ACTIVATE: &str = "ui_activate";
pub const UI_DEACTIVATE: &str = "ui_deactivate";
pub const UI_SEND_MESSAGE: &str = "ui_send_message";
pub const UI_SUBMIT_TASK: &str = "ui_submit_task";
pub const UI_INTERRUPT_RUN: &str = "ui_interrupt_run";
pub const UI_CANCEL_RUN: &str = "ui_cancel_run";
pub const UI_LIST_SESSIONS: &str = "ui_list_sessions";
pub const UI_LIST_MEMORY: &str = "ui_list_memory";
pub const UI_LIST_WORKSPACE_STATES: &str = "ui_list_workspace_states";
pub const UI_READ_MEMORY: &str = "ui_read_memory";
pub const UI_WRITE_MEMORY: &str = "ui_write_memory";
pub const UI_DELETE_MEMORY: &str = "ui_delete_memory";
pub const UI_COMPILE_MEMORY: &str = "ui_compile_memory";

/// 优雅关闭操作（宿主退出流程在终止 sidecar 前调用）。
pub const SHUTDOWN_OPERATION: &str = "subagent_shutdown";

/// WASM 生命周期钩子转发操作：关联会话本轮完成（on_turn_finished → sidecar）。
pub const SESSION_TURN_FINISHED: &str = "session_turn_finished";

/// @ 提及候选查询（manifest mention.candidates_op → sidecar）：
/// 返回启用 Agent 的候选列表。操作名含插件前缀（与清单声明一致）。
pub const MENTION_CANDIDATES: &str = "subagent.mention_candidates";

// ── 请求类型 ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AgentIdRequest {
    #[serde(default)]
    pub agent_id: String,
}

/// 招募缺省后端：原生 Subagent（无需会话或命令线索）。
fn default_recruit_backend() -> BackendKind {
    BackendKind::AgentTeam
}

/// AI 招募请求：创建（或复用同名）持久 Subagent 并在当前会话激活。
#[derive(Debug, Deserialize)]
pub struct CreateAgentRequest {
    /// 成员名称；同名 Agent 已存在时直接复用（延续其指令与记忆）。
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// 运行后端：agent_team（原生，默认，无需额外参数）、cli（需 command）
    /// 或 tiangong_session（需 session_id / session_query 之一）。
    #[serde(default = "default_recruit_backend")]
    pub backend: BackendKind,
    #[serde(default)]
    pub command: Option<String>,
    /// 关联会话 ID（tiangong_session）。
    #[serde(default)]
    pub session_id: Option<String>,
    /// 按标题关键词搜索会话并取最近匹配（tiangong_session 的替代写法）。
    #[serde(default)]
    pub session_query: Option<String>,
    #[serde(default)]
    pub workspace_policy: Option<WorkspacePolicy>,
    #[serde(default)]
    pub instructions: Option<String>,
    /// 创建后是否立即在当前会话激活，默认 true。
    #[serde(default)]
    pub activate: Option<bool>,
}

/// 随消息携带给成员的附件（本地路径）。
///
/// 由 WASM 侧自动提取发起会话本轮用户消息的附件注入，模型无需也无法
/// 手工填写路径；sidecar 透传给 Server 消息接口的 media 字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentPayload {
    /// 附件本地路径。
    pub path: String,
    /// 媒体类型：image / video / audio / file。
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub agent_id: String,
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<AttachmentPayload>,
}

#[derive(Debug, Deserialize)]
pub struct SubmitTaskRequest {
    pub agent_id: String,
    pub goal: String,
    #[serde(default)]
    pub completion_criteria: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AttachmentPayload>,
}

#[derive(Debug, Deserialize)]
pub struct TaskIdRequest {
    pub task_id: String,
}

#[derive(Debug, Deserialize)]
pub struct RunIdRequest {
    pub run_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ListAgentTasksRequest {
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListAgentEventsRequest {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct UiActivateRequest {
    pub agent_id: String,
    pub session_id: String,
    pub workspace: String,
}

#[derive(Debug, Deserialize)]
pub struct UiDeactivateRequest {
    pub agent_id: String,
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
pub struct UiSessionRequest {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
pub struct UiSendMessageRequest {
    pub agent_id: String,
    pub session_id: String,
    pub workspace: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct UiSubmitTaskRequest {
    pub agent_id: String,
    pub session_id: String,
    pub workspace: String,
    pub goal: String,
    #[serde(default)]
    pub completion_criteria: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UiAgentCreateRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub backend: BackendKind,
    #[serde(default)]
    pub command: Option<String>,
    /// 天工会话后端：关联的源会话 ID。
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub workspace_policy: Option<WorkspacePolicy>,
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UiAgentUpdateRequest {
    pub agent_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub workspace_policy: Option<WorkspacePolicy>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UiAgentDeleteRequest {
    pub agent_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SessionTurnFinishedRequest {
    /// 完成本轮的会话（Agent 关联的源会话）。
    pub session_id: String,
    /// 本轮用户消息文本（用于确认该轮由 Subagent 投递触发，防止误归因）。
    #[serde(default)]
    pub user_text: String,
    /// 本轮最终回复文本（assistant 消息 text 块拼接）。
    #[serde(default)]
    pub assistant_text: String,
    /// 本轮用户消息锚点的 turn 终态：success / failed / cancelled。
    #[serde(default)]
    pub turn_status: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionBrief {
    pub id: String,
    pub title: String,
    pub updated_at: String,
    pub message_count: usize,
}

#[derive(Debug, Deserialize)]
pub struct UiReadMemoryRequest {
    pub agent_id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct UiWriteMemoryRequest {
    pub agent_id: String,
    pub name: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct UiDeleteMemoryRequest {
    pub agent_id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct AgentMemoryRequest {
    pub agent_id: String,
    /// 管理页整理请求用：发起方会话与工作区（UI 桥接无工具调用上下文，
    /// 由页面从宿主上下文显式传递）。
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AppendAgentMemoryRequest {
    pub agent_id: String,
    pub content: String,
    #[serde(default)]
    pub note: Option<String>,
    /// 目标记忆文件（默认 notes.md；可复用经验写 lessons.md）。
    #[serde(default)]
    pub memory_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AppendAgentInstructionsRequest {
    pub agent_id: String,
    /// 追加的稳定规则（带日期分段写入，不覆盖既有内容）。
    pub addition: String,
}

/// 写入成员自维护的工作区状态（plan / context 整文件覆盖，成员有序写入）。
#[derive(Debug, Deserialize)]
pub struct UpdateWorkspaceStateRequest {
    /// 目标文件：plan（工作规划）或 context（工作背景与约定）。
    pub file: String,
    pub content: String,
    /// 可选：操作其他成员的工作区状态（默认当前归属成员自己）。
    #[serde(default)]
    pub agent_id: Option<String>,
}

/// 待处理工作查询（协作关系视图）：全部或指定成员的等待中运行。
#[derive(Debug, Deserialize)]
pub struct ListPendingWorkRequest {
    /// 可选：只看某个成员；缺省返回全部。
    #[serde(default)]
    pub agent_id: Option<String>,
}

/// 成员主动回报：向明确接收方投递消息（纯消息投递，不附带任务选择
/// 或运行终结——工作是否完成由成员与主 Agent 各自判断，执行实例的
/// 终结由执行系统按真实事件记录）。
#[derive(Debug, Deserialize)]
pub struct ReportAgentResultRequest {
    /// 回报结果正文。
    pub result: String,
    /// 接收方会话 ID（任务消息中的【回复地址】）。
    pub to_session: String,
    /// 可选备注（产物位置、后续建议等）。
    #[serde(default)]
    pub note: Option<String>,
    /// 可选运行标记（消息尾部的 r-短码）：仅用于诊断追踪，不用于
    /// 选择或终结运行。
    #[serde(default)]
    pub run_marker: Option<String>,
}

// ── 响应类型 ───────────────────────────────────────────────────

/// Agent 概览：身份 + 当前会话激活 + 运行实例状态。
#[derive(Debug, Clone, Serialize)]
pub struct AgentSummary {
    pub config: AgentConfig,
    pub capabilities: AdapterCapabilities,
    /// 该 Agent 当前所有活跃激活。
    pub activations: Vec<ActivationRecord>,
    /// 本会话是否已激活（无会话上下文时为 false）。
    #[serde(default)]
    pub activated_in_session: bool,
    /// 成员在当前会话工作区的专属会话 ID（在该工作区发起过即有记录；
    /// 无工作区上下文或从未在该工作区发起时为空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_session_id: Option<String>,
    /// 当前运行实例状态（任一激活上的活跃 run；无则空闲）。
    pub runtime_status: Option<RunStatus>,
    /// 活跃 run id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_id: Option<String>,
    /// 长期指令（instructions.md 全文；管理页查看与编辑用）。
    #[serde(default)]
    pub instructions: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentDetail {
    pub summary: AgentSummary,
    /// 长期指令（instructions.md）。
    pub instructions: String,
    pub recent_tasks: Vec<TaskRecord>,
    pub recent_events: Vec<AgentEventRecord>,
    pub artifact_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendOutcome {
    pub run_id: String,
    /// 消息注入了既有运行还是新建了运行。
    pub injected_into_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubmitOutcome {
    pub task_id: String,
    pub run_id: String,
}

/// 管理页一次性快照。
#[derive(Debug, Clone, Serialize)]
pub struct StateSnapshot {
    pub agents: Vec<AgentSummary>,
    pub session_id: String,
    pub active_tasks: Vec<TaskRecord>,
    pub recent_runs: Vec<RunRecord>,
    pub recent_events: Vec<AgentEventRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactEntry {
    pub name: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub modified_at: Option<String>,
}

/// Agent 长期记忆文件条目（memory/ 下 markdown）。
#[derive(Debug, Clone, Serialize)]
pub struct MemoryFileEntry {
    pub name: String,
    pub size_bytes: u64,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryReadOutcome {
    pub name: String,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 附件载荷的 wire 形状须与 Server 端 ConnectorMessageRequest.media
    /// （MediaAsset：kind/url/mime_type/title/capability，除 kind、url 外
    /// 均可缺省）保持兼容——sidecar 投递时按该形状转换。
    #[test]
    fn 附件载荷缺省字段可省略且类型可反序列化() {
        let payload = AttachmentPayload {
            path: "/tmp/a.png".to_string(),
            kind: "image".to_string(),
            mime_type: Some("image/png".to_string()),
            name: Some("a.png".to_string()),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["path"], "/tmp/a.png");
        assert_eq!(json["kind"], "image");
        assert_eq!(json["mime_type"], "image/png");
        assert_eq!(json["name"], "a.png");

        // 省略可选字段（mime_type/name 缺失）仍可反序列化——WASM 注入侧
        // 附件缺失元信息时按 null 序列化。
        let minimal = serde_json::json!({
            "path": "/tmp/b.pdf",
            "kind": "file",
            "mime_type": null,
            "name": null,
        });
        let parsed: AttachmentPayload = serde_json::from_value(minimal).unwrap();
        assert_eq!(parsed.path, "/tmp/b.pdf");
        assert_eq!(parsed.kind, "file");
        assert!(parsed.mime_type.is_none() && parsed.name.is_none());

        // 工具请求缺省 attachments 字段时兼容（旧调用方/管理页 UI 不带）。
        let request: SendMessageRequest =
            serde_json::from_value(serde_json::json!({ "agent_id": "a", "content": "hi" }))
                .unwrap();
        assert!(request.attachments.is_empty());
    }
}
