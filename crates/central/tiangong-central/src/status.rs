//! 后台工作状态机（设计稿 §8.1）。
//!
//! ```text
//! draft → planned → queued → running
//! running ├ waiting_user / waiting_resource / blocked / paused / failed / completed
//! ```
//!
//! 转移规则（POC 实现，依据设计稿语义最小化）：
//! - 标准链 `draft → planned → queued → running`；
//! - `running` 可进入等待用户、等待资源、阻塞、暂停、失败、完成；
//! - 等待与阻塞状态可回到 `running`，也可转 `failed` / `cancelled`；
//! - `blocked` 解除后可重新排队（`queued`，由中枢重新派发）；
//! - `paused` 恢复后可回到 `running` 或重新排队；
//! - `failed` 是「本次执行」的终态，但工作项可重试（→ `queued`）或取消；
//! - `completed` / `cancelled` 为绝对终态，不可再转移；
//! - 任何非终态均可 `cancelled`（对应事件 `work_item.cancelled`）。
//!
//! 设计稿 §8.1 的状态表未显式列出 `cancelled` 状态，本框架依据 §8.2 的
//! `work_item.cancelled` 事件与 §5.1 的「取消后台工作」能力补全该终态。

use serde::{Deserialize, Serialize};

/// 后台工作状态（设计稿 §8.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStatus {
    /// 中枢助手已理解目标，尚未正式安排
    Draft,
    /// 已形成执行计划，等待调度
    Planned,
    /// 已提交，等待 Workspace 或 Agent 可用
    Queued,
    /// Worker 正在执行
    Running,
    /// 需要用户做决定、审批或补充信息（含等待用户选定接力模型，§8.1）
    WaitingUser,
    /// 等待外部资源、依赖、登录态或其他工作释放
    WaitingResource,
    /// 已无法继续，需要中枢助手判断或汇报
    Blocked,
    /// 用户或系统暂停
    Paused,
    /// 执行失败且自动恢复未解决；可重试（→ queued）或取消
    Failed,
    /// 完成条件满足且有证据（绝对终态）
    Completed,
    /// 已取消（绝对终态）
    Cancelled,
}

impl WorkItemStatus {
    /// 全部状态，按生命周期顺序排列。
    pub const ALL: [WorkItemStatus; 11] = [
        WorkItemStatus::Draft,
        WorkItemStatus::Planned,
        WorkItemStatus::Queued,
        WorkItemStatus::Running,
        WorkItemStatus::WaitingUser,
        WorkItemStatus::WaitingResource,
        WorkItemStatus::Blocked,
        WorkItemStatus::Paused,
        WorkItemStatus::Failed,
        WorkItemStatus::Completed,
        WorkItemStatus::Cancelled,
    ];

    /// 是否为绝对终态（不可再发生任何状态转移）。
    ///
    /// 注意 `failed` 不属于绝对终态：重试（`failed → queued`）是设计稿
    /// 阶段 2 的「失败分类归并」与开放问题 18 的既有语义。
    pub fn is_terminal(self) -> bool {
        matches!(self, WorkItemStatus::Completed | WorkItemStatus::Cancelled)
    }

    /// 是否处于「本次执行已结束」的状态（completed / cancelled / failed）。
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            WorkItemStatus::Completed | WorkItemStatus::Cancelled | WorkItemStatus::Failed
        )
    }

    /// 是否处于等待类状态（waiting_user / waiting_resource / blocked / paused）。
    pub fn is_waiting(self) -> bool {
        matches!(
            self,
            WorkItemStatus::WaitingUser
                | WorkItemStatus::WaitingResource
                | WorkItemStatus::Blocked
                | WorkItemStatus::Paused
        )
    }

    /// 状态转移是否合法（见模块级转移规则说明）。
    pub fn can_transition_to(self, target: WorkItemStatus) -> bool {
        use WorkItemStatus::*;
        match self {
            Draft => matches!(target, Planned | Cancelled),
            Planned => matches!(target, Queued | Cancelled),
            Queued => matches!(target, Running | Cancelled),
            Running => matches!(
                target,
                WaitingUser | WaitingResource | Blocked | Paused | Failed | Completed
            ),
            WaitingUser => matches!(target, Running | Blocked | Failed | Cancelled),
            WaitingResource => matches!(target, Running | Blocked | Failed | Cancelled),
            Blocked => matches!(target, Running | Queued | Failed | Cancelled),
            Paused => matches!(target, Running | Queued | Cancelled),
            Failed => matches!(target, Queued | Cancelled),
            Completed | Cancelled => false,
        }
    }

    pub fn as_str(self) -> &'static str {
        use WorkItemStatus::*;
        match self {
            Draft => "draft",
            Planned => "planned",
            Queued => "queued",
            Running => "running",
            WaitingUser => "waiting_user",
            WaitingResource => "waiting_resource",
            Blocked => "blocked",
            Paused => "paused",
            Failed => "failed",
            Completed => "completed",
            Cancelled => "cancelled",
        }
    }
}
