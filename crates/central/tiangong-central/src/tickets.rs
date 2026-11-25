//! 授权票据（R3，方案 §5.6）：用户确认必须是宿主事实，不是模型文字。
//!
//! 三类高风险动作的放行凭证：
//!
//! | 票据 | 守护什么 | 签发者 |
//! | --- | --- | --- |
//! | `WorkspaceApprovalTicket` | 向 Workspace 派发后台工作 | 宿主 UI（真实用户操作） |
//! | `DecisionTicket` | 恢复等待用户决策的执行 | 宿主 UI（用户选择选项） |
//! | `ModelSwitchTicket` | 切换 Worker 模型 | 已有实现（tools.rs，R3 补入注册表） |
//!
//! 硬规则：
//! - **模型只能请求票据，不能生成票据**：`TicketRegistry` 只暴露
//!   `issue_*`（宿主 UI 调用）与 `consume_*`（执行路径调用）；
//! - **一次性**：消费即失效（`used_at` 回填），重放拒绝；
//! - **有效期**：过期拒绝（不得默认视为同意）；
//! - **绑定**：错绑 workspace / execution 的票据拒绝；
//! - **fail-closed**：票据不存在、字段残缺、过期、已用，一律拒绝。
//!
//! POC 的 `confirm_workspace` 工具是模型可自调的软约束（留痕+事后曝光），
//! R3 起由宿主签发的本票据取代（工具移除在接线片完成）。

use serde::{Deserialize, Serialize};

use crate::ids::{new_id, now_text};

/// 风险等级复用域模型。
pub use crate::risk::RiskLevel;

/// 时间文本解析比较（本地时间 "%Y-%m-%d %H:%M:%S" 字典序即时间序）。
fn is_expired(expires_at: &str, now: &str) -> bool {
    !expires_at.is_empty() && now > expires_at
}

/// Workspace 派发授权票据（方案 §5.6）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceApprovalTicket {
    pub approval_id: String,
    /// 授权的 Workspace（canonical path 文本）。
    pub workspace_id: String,
    /// 签发时用户见到的任务指纹（防「确认 A、执行 B」偷换）。
    pub task_fingerprint: String,
    pub risk_level: RiskLevel,
    /// 用户决定（true = 同意）。
    pub user_decision: bool,
    pub created_at: String,
    pub expires_at: String,
    /// 消费时刻（Some = 已用，一次性）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_at: Option<String>,
}

/// 等待用户决策的恢复票据（方案 §5.6 / §3.6）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionTicket {
    pub ticket_id: String,
    /// 绑定的执行（恢复必须回到同一 execution）。
    pub execution_id: String,
    /// 阻塞问题版本（问题变化后旧票据作废）。
    pub question_version: u32,
    /// 用户选定的选项（必须属于签发时的选项集合）。
    pub chosen_option: String,
    pub issued_at: String,
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_at: Option<String>,
}

/// 一次性票据校验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketVerdict {
    /// 放行（消费并标记已用）。
    Granted,
    /// 拒绝：不存在或已消费。
    DeniedUsedOrMissing,
    /// 拒绝：过期。
    DeniedExpired,
    /// 拒绝：绑定不符（错 workspace / execution / 选项）。
    DeniedMismatch,
}

impl TicketVerdict {
    pub fn is_granted(self) -> bool {
        matches!(self, TicketVerdict::Granted)
    }
}

/// 票据注册表（进程内；持久化与 UI 签发接线在宿主侧完成）。
///
/// `issue_*` 仅供宿主 UI 在**真实用户操作**（点击授权卡片、选择选项）
/// 后调用；执行路径只经 `consume_*`——没有票据就无法放行。
#[derive(Default)]
pub struct TicketRegistry {
    workspace_approvals: std::sync::Mutex<Vec<WorkspaceApprovalTicket>>,
    decisions: std::sync::Mutex<Vec<DecisionTicket>>,
}

impl TicketRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 宿主签发 Workspace 授权（用户真实点击后调用）。
    pub fn issue_workspace_approval(
        &self,
        workspace_id: impl Into<String>,
        task_fingerprint: impl Into<String>,
        risk_level: RiskLevel,
        ttl_seconds: i64,
    ) -> WorkspaceApprovalTicket {
        let now = now_text();
        let expires = chrono::Local::now() + chrono::Duration::seconds(ttl_seconds.max(1));
        let ticket = WorkspaceApprovalTicket {
            approval_id: new_id(),
            workspace_id: workspace_id.into(),
            task_fingerprint: task_fingerprint.into(),
            risk_level,
            user_decision: true,
            created_at: now,
            expires_at: expires.format("%Y-%m-%d %H:%M:%S").to_string(),
            used_at: None,
        };
        self.workspace_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(ticket.clone());
        ticket
    }

    /// 消费 Workspace 授权：一次性、有效期、绑定三重校验。
    pub fn consume_workspace_approval(
        &self,
        approval_id: &str,
        workspace_id: &str,
        task_fingerprint: &str,
    ) -> TicketVerdict {
        let now = now_text();
        let mut tickets = self
            .workspace_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(ticket) = tickets
            .iter_mut()
            .rev()
            .find(|t| t.approval_id == approval_id)
        else {
            return TicketVerdict::DeniedUsedOrMissing;
        };
        if ticket.used_at.is_some() {
            return TicketVerdict::DeniedUsedOrMissing;
        }
        if is_expired(&ticket.expires_at, &now) {
            return TicketVerdict::DeniedExpired;
        }
        if ticket.workspace_id != workspace_id || ticket.task_fingerprint != task_fingerprint {
            return TicketVerdict::DeniedMismatch;
        }
        ticket.used_at = Some(now);
        TicketVerdict::Granted
    }

    /// 按对消费（派发路径）：取最新的匹配 (workspace, 任务指纹) 的未用
    /// 票据并消费；无匹配/已用/过期拒绝。任务指纹不一致 = 偷换，拒绝。
    pub fn consume_workspace_approval_for(
        &self,
        workspace_id: &str,
        task_fingerprint: &str,
    ) -> TicketVerdict {
        let now = now_text();
        let mut tickets = self
            .workspace_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        fn matches(
            t: &WorkspaceApprovalTicket,
            workspace_id: &str,
            task_fingerprint: &str,
        ) -> bool {
            t.workspace_id == workspace_id && t.task_fingerprint == task_fingerprint
        }
        // 取最新的**未用且未过期**票据（与 peek 口径一致）：多票并存时不得
        // 因为最新一张恰好已用 / 已过期就拒绝用户仍然有效的授权
        fn usable(
            t: &WorkspaceApprovalTicket,
            now: &str,
            workspace_id: &str,
            task_fingerprint: &str,
        ) -> bool {
            t.used_at.is_none()
                && !is_expired(&t.expires_at, now)
                && matches(t, workspace_id, task_fingerprint)
        }
        let Some(latest) = tickets
            .iter()
            .rposition(|t| matches(t, workspace_id, task_fingerprint))
        else {
            return TicketVerdict::DeniedUsedOrMissing;
        };
        if let Some(position) = tickets
            .iter()
            .rposition(|t| usable(t, &now, workspace_id, task_fingerprint))
        {
            tickets[position].used_at = Some(now);
            return TicketVerdict::Granted;
        }
        if is_expired(&tickets[latest].expires_at, &now) {
            TicketVerdict::DeniedExpired
        } else {
            TicketVerdict::DeniedUsedOrMissing
        }
    }

    /// 按对查验（不消费）：是否存在匹配 (workspace, 任务指纹) 的未用、未过期
    /// 票据。供派发规划在「排队」决策前判断是否已授权——排队项要等依赖终态
    /// 后才真正投递，若在入队时就把一次性票据消费掉，投递那一刻反而无票
    /// 可用（2026-11-25 修复）。
    pub fn peek_workspace_approval_for(&self, workspace_id: &str, task_fingerprint: &str) -> bool {
        let now = now_text();
        self.workspace_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .rev()
            .any(|t| {
                t.workspace_id == workspace_id
                    && t.task_fingerprint == task_fingerprint
                    && t.used_at.is_none()
                    && !is_expired(&t.expires_at, &now)
            })
    }

    /// 宿主签发决策票据（用户真实选择选项后调用）。
    pub fn issue_decision(
        &self,
        execution_id: impl Into<String>,
        question_version: u32,
        chosen_option: impl Into<String>,
        ttl_seconds: i64,
    ) -> DecisionTicket {
        let issued = now_text();
        let expires = chrono::Local::now() + chrono::Duration::seconds(ttl_seconds.max(1));
        let ticket = DecisionTicket {
            ticket_id: new_id(),
            execution_id: execution_id.into(),
            question_version,
            chosen_option: chosen_option.into(),
            issued_at: issued,
            expires_at: expires.format("%Y-%m-%d %H:%M:%S").to_string(),
            used_at: None,
        };
        self.decisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(ticket.clone());
        ticket
    }

    /// 按执行消费（恢复链路，Z6）：取最新的匹配 `execution_id` 的未用
    /// 票据并消费——无票据/已用/过期返回非 Granted（渐进收紧：调用方
    /// 决定是否放行；协议结构化后转为硬校验）。
    pub fn consume_decision_for_execution(&self, execution_id: &str) -> TicketVerdict {
        let now = now_text();
        let mut tickets = self
            .decisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(position) = tickets.iter().rposition(|t| t.execution_id == execution_id) else {
            return TicketVerdict::DeniedUsedOrMissing;
        };
        let ticket = &mut tickets[position];
        if ticket.used_at.is_some() {
            return TicketVerdict::DeniedUsedOrMissing;
        }
        if is_expired(&ticket.expires_at, &now) {
            return TicketVerdict::DeniedExpired;
        }
        ticket.used_at = Some(now);
        TicketVerdict::Granted
    }

    /// 消费决策票据：绑定 execution 与问题版本，一次性。
    pub fn consume_decision(
        &self,
        ticket_id: &str,
        execution_id: &str,
        question_version: u32,
    ) -> TicketVerdict {
        let now = now_text();
        let mut tickets = self
            .decisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(ticket) = tickets.iter_mut().rev().find(|t| t.ticket_id == ticket_id) else {
            return TicketVerdict::DeniedUsedOrMissing;
        };
        if ticket.used_at.is_some() {
            return TicketVerdict::DeniedUsedOrMissing;
        }
        if is_expired(&ticket.expires_at, &now) {
            return TicketVerdict::DeniedExpired;
        }
        if ticket.execution_id != execution_id || ticket.question_version != question_version {
            return TicketVerdict::DeniedMismatch;
        }
        ticket.used_at = Some(now);
        TicketVerdict::Granted
    }
}
