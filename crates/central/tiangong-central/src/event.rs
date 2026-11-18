//! 后台工作事件（设计稿 §8.2 / §8.3 / §13.4）。
//!
//! 事件是中枢模式的可靠性基础：Worker 产生事件 → 事件总线持久化 →
//! 中枢订阅 → 判定是否需要用户可见反馈（§8.4）。本模块定义事件类型与
//! 结构；事件总线的订阅分发由后续片实现。
//!
//! 事件名采用设计稿的带点形式（如 `work_item.completed`），serde 序列化
//! 与 `as_str()` 均使用该形式。模型类事件只记录协商与兜底过程，不改变
//! 工作终态；终态仍以 `work_item.failed` / `work_item.completed` 为准。

use serde::{Deserialize, Serialize};

use crate::ids::{new_id, now_text};

/// 工作事件类型（设计稿 §8.2，共 22 种）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkEventType {
    #[serde(rename = "work_item.created")]
    WorkItemCreated,
    #[serde(rename = "work_item.started")]
    WorkItemStarted,
    #[serde(rename = "work_item.progress")]
    WorkItemProgress,
    #[serde(rename = "work_item.phase_completed")]
    WorkItemPhaseCompleted,
    #[serde(rename = "work_item.waiting_user")]
    WorkItemWaitingUser,
    #[serde(rename = "work_item.waiting_resource")]
    WorkItemWaitingResource,
    #[serde(rename = "work_item.blocked")]
    WorkItemBlocked,
    #[serde(rename = "work_item.failed")]
    WorkItemFailed,
    #[serde(rename = "work_item.completed")]
    WorkItemCompleted,
    #[serde(rename = "work_item.cancelled")]
    WorkItemCancelled,
    #[serde(rename = "workspace.locked")]
    WorkspaceLocked,
    #[serde(rename = "workspace.unlocked")]
    WorkspaceUnlocked,
    #[serde(rename = "approval.required")]
    ApprovalRequired,
    #[serde(rename = "artifact.created")]
    ArtifactCreated,
    #[serde(rename = "verification.completed")]
    VerificationCompleted,
    #[serde(rename = "report.ready")]
    ReportReady,
    #[serde(rename = "model.unavailable")]
    ModelUnavailable,
    #[serde(rename = "model.handover.proposed")]
    ModelHandoverProposed,
    #[serde(rename = "model.handover.confirmed")]
    ModelHandoverConfirmed,
    #[serde(rename = "model.handover.failed")]
    ModelHandoverFailed,
    #[serde(rename = "central.model.fallback")]
    CentralModelFallback,
    #[serde(rename = "central.degraded")]
    CentralDegraded,
}

impl WorkEventType {
    /// 全部事件类型。
    pub const ALL: [WorkEventType; 22] = [
        WorkEventType::WorkItemCreated,
        WorkEventType::WorkItemStarted,
        WorkEventType::WorkItemProgress,
        WorkEventType::WorkItemPhaseCompleted,
        WorkEventType::WorkItemWaitingUser,
        WorkEventType::WorkItemWaitingResource,
        WorkEventType::WorkItemBlocked,
        WorkEventType::WorkItemFailed,
        WorkEventType::WorkItemCompleted,
        WorkEventType::WorkItemCancelled,
        WorkEventType::WorkspaceLocked,
        WorkEventType::WorkspaceUnlocked,
        WorkEventType::ApprovalRequired,
        WorkEventType::ArtifactCreated,
        WorkEventType::VerificationCompleted,
        WorkEventType::ReportReady,
        WorkEventType::ModelUnavailable,
        WorkEventType::ModelHandoverProposed,
        WorkEventType::ModelHandoverConfirmed,
        WorkEventType::ModelHandoverFailed,
        WorkEventType::CentralModelFallback,
        WorkEventType::CentralDegraded,
    ];

    /// 带点事件名（设计稿 §8.2 的线上形式）。
    pub fn as_str(self) -> &'static str {
        match self {
            WorkEventType::WorkItemCreated => "work_item.created",
            WorkEventType::WorkItemStarted => "work_item.started",
            WorkEventType::WorkItemProgress => "work_item.progress",
            WorkEventType::WorkItemPhaseCompleted => "work_item.phase_completed",
            WorkEventType::WorkItemWaitingUser => "work_item.waiting_user",
            WorkEventType::WorkItemWaitingResource => "work_item.waiting_resource",
            WorkEventType::WorkItemBlocked => "work_item.blocked",
            WorkEventType::WorkItemFailed => "work_item.failed",
            WorkEventType::WorkItemCompleted => "work_item.completed",
            WorkEventType::WorkItemCancelled => "work_item.cancelled",
            WorkEventType::WorkspaceLocked => "workspace.locked",
            WorkEventType::WorkspaceUnlocked => "workspace.unlocked",
            WorkEventType::ApprovalRequired => "approval.required",
            WorkEventType::ArtifactCreated => "artifact.created",
            WorkEventType::VerificationCompleted => "verification.completed",
            WorkEventType::ReportReady => "report.ready",
            WorkEventType::ModelUnavailable => "model.unavailable",
            WorkEventType::ModelHandoverProposed => "model.handover.proposed",
            WorkEventType::ModelHandoverConfirmed => "model.handover.confirmed",
            WorkEventType::ModelHandoverFailed => "model.handover.failed",
            WorkEventType::CentralModelFallback => "central.model.fallback",
            WorkEventType::CentralDegraded => "central.degraded",
        }
    }

    /// 按带点事件名解析。
    pub fn from_str_name(name: &str) -> Option<Self> {
        WorkEventType::ALL
            .iter()
            .find(|event_type| event_type.as_str() == name)
            .copied()
    }

    /// 是否为工作项终态事件（§8.2：终态以 failed / completed 为准）。
    pub fn is_work_item_final(self) -> bool {
        matches!(
            self,
            WorkEventType::WorkItemFailed | WorkEventType::WorkItemCompleted
        )
    }
}

/// 事件严重级别。
///
/// 设计稿 §8.3 示例只出现 `normal`；此处按反馈语义细分为四级
/// （POC 决策，阶段 1 使用，后续可随开放问题 11 的事件架构决议调整）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSeverity {
    Info,
    Normal,
    Warning,
    Critical,
}

impl EventSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            EventSeverity::Info => "info",
            EventSeverity::Normal => "normal",
            EventSeverity::Warning => "warning",
            EventSeverity::Critical => "critical",
        }
    }
}

/// 工作事件结构（设计稿 §8.3 示例与 §13.4 字段合并）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkEvent {
    pub id: String,
    pub work_item_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_session_id: Option<String>,
    /// 归属执行（R0 契约，§5.4）：终态与过程事件唯一挂到一次执行上，
    /// 不再依赖「当前 session 最近一条 registry」猜归属。旧数据缺失时
    /// 为 None（兼容读取）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    pub event_type: WorkEventType,
    pub severity: EventSeverity,
    pub requires_user_action: bool,
    #[serde(default)]
    pub payload: serde_json::Value,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<String>,
}

impl WorkEvent {
    /// 创建事件：自动生成 id 与时间戳，默认 `normal` 级别、不要求用户动作。
    pub fn new(work_item_id: impl Into<String>, event_type: WorkEventType) -> Self {
        Self {
            id: new_id(),
            work_item_id: work_item_id.into(),
            workspace_id: None,
            worker_session_id: None,
            execution_id: None,
            event_type,
            severity: EventSeverity::Normal,
            requires_user_action: false,
            payload: serde_json::Value::Null,
            created_at: now_text(),
            acknowledged_at: None,
        }
    }

    /// 标记归属执行（§5.4：所有 Started / Progress / 终态事件必须带
    /// `execution_id`）。
    pub fn with_execution(mut self, execution_id: impl Into<String>) -> Self {
        self.execution_id = Some(execution_id.into());
        self
    }

    /// 标记该事件需要用户动作（如 `approval.required`，§8.2）。
    ///
    /// 注意：`requires_user_action` 描述事件本身是否携带待决事项；
    /// 待决事项默认由中枢代决（§5.4），是否真的询问用户由中枢决定。
    pub fn with_user_action(mut self) -> Self {
        self.requires_user_action = true;
        self
    }

    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }

    /// 标记归属 Worker 会话（§13.4 字段）。
    pub fn with_worker_session(mut self, session_id: impl Into<String>) -> Self {
        self.worker_session_id = Some(session_id.into());
        self
    }

    pub fn with_severity(mut self, severity: EventSeverity) -> Self {
        self.severity = severity;
        self
    }
}
