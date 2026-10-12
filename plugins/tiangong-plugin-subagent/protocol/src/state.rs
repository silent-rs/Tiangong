//! 运行态记录：激活关系、任务、运行与事件历史。
//!
//! 由 sidecar 持久化在 `~/.tiangong/data/subagent/runtime/`，与 agent.toml 身份目录分离。

use serde::{Deserialize, Serialize};

use crate::config::WorkspacePolicy;

/// 会话激活关系。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationRecord {
    pub activation_id: String,
    pub agent_id: String,
    /// 激活发生的会话（conversation）。
    pub session_id: String,
    /// 本次激活绑定的工作上下文（会话 Workspace 原样，或独立 worktree 路径）。
    pub workspace: String,
    /// 本次激活生效的 Workspace 策略。
    pub workspace_policy: WorkspacePolicy,
    /// 会话原始 Workspace（isolated-worktree 时与 workspace 不同）。
    pub source_workspace: String,
    pub activated_at: String,
    pub deactivated_at: Option<String>,
    /// 停用时 worktree 因存在未提交修改被保留。
    #[serde(default)]
    pub worktree_retained: bool,
}

impl ActivationRecord {
    pub fn active(&self) -> bool {
        self.deactivated_at.is_none()
    }
}

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl TaskStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pending => "待执行",
            Self::Running => "进行中",
            Self::Completed => "已完成",
            Self::Failed => "失败",
            Self::Cancelled => "已取消",
            Self::Interrupted => "已中断",
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// 运行实例状态（归一化后的统一状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// 空闲/就绪（子进程已启动，等待或处理中）。
    Ready,
    /// 工作中。
    Working,
    /// 已投递停止请求，等待执行侧确认收尾（占用保留）。
    Stopping,
    /// 阻塞，等待外部输入。
    Blocked,
    /// 等待审批。
    ApprovalRequired,
    /// 完成。
    Completed,
    /// 失败。
    Failed,
    /// 已取消。
    Cancelled,
    /// 被中断（进程仍在或已中断待恢复）。
    Interrupted,
}

impl RunStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }

    pub fn is_alive(&self) -> bool {
        !self.is_terminal()
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Ready => "空闲",
            Self::Working => "工作中",
            Self::Stopping => "停止请求中",
            Self::Blocked => "阻塞",
            Self::ApprovalRequired => "待审批",
            Self::Completed => "已完成",
            Self::Failed => "失败",
            Self::Cancelled => "已取消",
            Self::Interrupted => "已中断",
        }
    }
}

/// 运行种类：正式任务执行或普通消息往返。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Task,
    Message,
}

impl RunKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Task => "任务",
            Self::Message => "消息",
        }
    }
}

impl std::fmt::Display for RunKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Task：有目标、范围和完成条件的正式工作。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: String,
    pub agent_id: String,
    /// 提交任务的会话。
    pub session_id: String,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_criteria: Option<String>,
    pub status: TaskStatus,
    /// 关联的运行（按创建顺序）。
    pub runs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Run：Task（或消息往返）的一次具体执行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub agent_id: String,
    pub activation_id: String,
    pub session_id: String,
    pub kind: RunKind,
    pub status: RunStatus,
    /// managed 子进程 PID（attached 后端为空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 本次运行使用的 Workspace。
    pub workspace: String,
    /// 实际执行会话（成员×工作区映射的专属会话），会话型后端投递后回填。
    /// session_id 保持发起会话语义（回报回投与发起方判定的既有依据），
    /// 两者不得混用——补充消息与控制通知必须沿执行会话路由。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_session: Option<String>,
    /// 集群协作发起方会话：成员互发消息时记录，完成/失败回报投回该会话；
    /// 缺省（主会话发起）回投激活会话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// 最近一次输出摘要或终态说明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// 事件历史条目（append-only，供查询与展示）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEventRecord {
    pub event_id: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_id: Option<String>,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub workspace: String,
    pub event_type: String,
    pub created_at: String,
    pub payload: serde_json::Value,
}
