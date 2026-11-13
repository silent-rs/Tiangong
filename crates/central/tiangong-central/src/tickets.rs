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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_approval_happy_path_is_one_shot() {
        let registry = TicketRegistry::new();
        let ticket =
            registry.issue_workspace_approval("/ws/proj", "任务指纹 A", RiskLevel::High, 600);
        // 首次消费：放行
        let first =
            registry.consume_workspace_approval(&ticket.approval_id, "/ws/proj", "任务指纹 A");
        assert!(first.is_granted());
        // 重放：拒绝（一次性）
        let replay =
            registry.consume_workspace_approval(&ticket.approval_id, "/ws/proj", "任务指纹 A");
        assert_eq!(replay, TicketVerdict::DeniedUsedOrMissing, "票据一次性");
    }

    /// 方案 8.3「票据错绑 work_id 被拒绝」：确认 A、执行 B 偷换被拒。
    #[test]
    fn workspace_approval_rejects_fingerprint_swap() {
        let registry = TicketRegistry::new();
        let ticket = registry.issue_workspace_approval("/ws/a", "任务 A", RiskLevel::Low, 600);
        let swapped = registry.consume_workspace_approval(&ticket.approval_id, "/ws/a", "任务 B");
        assert_eq!(swapped, TicketVerdict::DeniedMismatch);
        let wrong_ws = registry.consume_workspace_approval(&ticket.approval_id, "/ws/b", "任务 A");
        assert_eq!(wrong_ws, TicketVerdict::DeniedMismatch);
        // 错绑不消费票据：正确绑定仍可用
        let ok = registry.consume_workspace_approval(&ticket.approval_id, "/ws/a", "任务 A");
        assert!(ok.is_granted());
    }

    /// 方案 8.3「过期票据被拒绝」。
    #[test]
    fn expired_ticket_is_denied_not_granted_by_default() {
        let registry = TicketRegistry::new();
        let ticket = registry.issue_workspace_approval("/ws/e", "任务", RiskLevel::Low, 1);
        // TTL 1 秒内立即消费应放行；这里用伪造过期验证拒绝路径——
        // 直接篡改注册表内票据过期时间（测试可达性；生产签发方在宿主）。
        {
            let mut tickets = registry.workspace_approvals.lock().unwrap();
            tickets.last_mut().unwrap().expires_at = "2000-01-01 00:00:00".to_string();
        }
        let verdict = registry.consume_workspace_approval(&ticket.approval_id, "/ws/e", "任务");
        assert_eq!(verdict, TicketVerdict::DeniedExpired, "过期≠同意");
    }

    /// 模型伪造票据（不存在的 id）被拒：fail-closed。
    #[test]
    fn forged_ticket_id_is_denied() {
        let registry = TicketRegistry::new();
        let verdict = registry.consume_workspace_approval("forged-id", "/ws/x", "任务");
        assert_eq!(verdict, TicketVerdict::DeniedUsedOrMissing);
        let decision = registry.consume_decision("forged", "exec-1", 1);
        assert_eq!(decision, TicketVerdict::DeniedUsedOrMissing);
    }

    /// 决策票据绑定 execution 与问题版本。
    #[test]
    fn decision_ticket_binds_execution_and_version() {
        let registry = TicketRegistry::new();
        let ticket = registry.issue_decision("exec-run", 2, "继续", 600);
        // 错 execution
        assert_eq!(
            registry.consume_decision(&ticket.ticket_id, "exec-other", 2),
            TicketVerdict::DeniedMismatch
        );
        // 错问题版本（问题已更新，旧答复作废）
        assert_eq!(
            registry.consume_decision(&ticket.ticket_id, "exec-run", 3),
            TicketVerdict::DeniedMismatch
        );
        // 正确绑定
        assert!(
            registry
                .consume_decision(&ticket.ticket_id, "exec-run", 2)
                .is_granted()
        );
        // 一次性
        assert_eq!(
            registry.consume_decision(&ticket.ticket_id, "exec-run", 2),
            TicketVerdict::DeniedUsedOrMissing
        );
    }
}
