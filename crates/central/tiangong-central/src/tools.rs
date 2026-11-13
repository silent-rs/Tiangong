//! 中枢助手工具白名单与模型切换票据（设计稿 §5.3 / §15.10 / §15.11）。
//!
//! 权限隔离不靠提示词，由宿主在工具路由层建立独立白名单（§19 原则 12：
//! 「权限由系统强制」）。本模块提供白名单常量与判定函数；工具路由的
//! 强制接入点随阶段 1「中枢助手工具白名单隔离」开发片接线。
//!
//! 判断原则（§5.3）：中枢助手可以「安排一次测试」，但不能「自己运行测试」；
//! 可以「读取测试已经通过的事件」，但不能「替 Worker 说测试通过」。

use serde::{Deserialize, Serialize};

use crate::ids::now_text;
use crate::model::{HandoverDecision, ModelHandoverConsultation};

/// 中枢助手工具白名单（设计稿 §5.3，共 17 项协调类工具）。
///
/// 工作会话（Worker）工具集是开放集合（fs / terminal / command / browser /
/// computer-use / database / coding / test_runner / plugin-specific），
/// 不做穷举定义；权限网关的强制点是「中枢只能调用白名单内工具」。
pub const CENTRAL_TOOL_ALLOWLIST: &[&str] = &[
    "get_context",
    "list_workspaces",
    "inspect_workspace",
    "create_goal",
    "create_work_item",
    "dispatch_work_item",
    "pause_work_item",
    "resume_work_item",
    "cancel_work_item",
    "get_work_item_status",
    "get_work_item_events",
    "list_model_profiles",
    "get_model_status",
    "switch_worker_model",
    "request_user",
    "send_notification",
    "create_report",
];

/// 判定工具是否允许中枢助手调用。
///
/// 返回 `false` 的工具一律被权限网关拒绝——包括但不限于 fs / terminal /
/// command / browser / computer-use / database / coding 等工作型工具。
pub fn is_central_tool_allowed(tool_name: &str) -> bool {
    CENTRAL_TOOL_ALLOWLIST.contains(&tool_name)
}

/// `switch_worker_model` 的用户确认票据（§5.3 / §15.10 / §15.11）。
///
/// 设计稿硬约束：工作会话换模型每次都要和用户商量，`switch_worker_model`
/// 只是对现有 `TiangongCore::switch_model` 的一次**带用户确认票据**的调用；
/// 权限网关校验票据，防止绕过商量直接换模型。票据只能从一次已完成
/// （用户明确选定换模型）的协商记录中签发。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSwitchTicket {
    /// 签发票据的协商记录 id
    pub consultation_id: String,
    /// 目标工作项
    pub work_item_id: String,
    /// 用户选定的模型
    pub target_model: String,
    /// 用户作出决定的时间
    pub decided_at: String,
}

impl ModelSwitchTicket {
    /// 从协商记录签发票据。
    ///
    /// 只有当协商记录已有用户决定、且决定为「换用指定模型继续」、
    /// 且协商确实归属该工作项时才签发；其余情况一律返回 `None`
    /// （用户未回复 / 挂起 / 缩小范围 / 取消 / 记录缺失都不得换模型）。
    pub fn issue(consultation: &ModelHandoverConsultation) -> Option<Self> {
        let work_item_id = consultation.work_item_id.as_deref()?;
        let decision = consultation.user_decision.as_ref()?;
        let HandoverDecision::SwitchModel { model } = decision else {
            return None;
        };
        let decided_at = consultation.decided_at.as_deref()?;
        Some(Self {
            consultation_id: consultation.id.clone(),
            work_item_id: work_item_id.to_string(),
            target_model: model.clone(),
            decided_at: decided_at.to_string(),
        })
    }

    /// 票据是否匹配目标工作项与目标模型（权限网关校验入口）。
    pub fn matches(&self, work_item_id: &str, target_model: &str) -> bool {
        !self.consultation_id.is_empty()
            && self.work_item_id == work_item_id
            && self.target_model == target_model
            && !self.decided_at.is_empty()
    }

    /// 票据是否仍在有效期内语义上的提示（POC：仅拒绝未填决定时间的票据）。
    pub fn is_well_formed(&self) -> bool {
        !self.consultation_id.is_empty()
            && !self.work_item_id.is_empty()
            && !self.target_model.is_empty()
            && !self.decided_at.is_empty()
    }
}

/// 生成当前时间（供上层在用户确认后回填 `decided_at` 时使用）。
pub fn decision_time() -> String {
    now_text()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FailureClass;

    #[test]
    fn allowlist_matches_design() {
        assert_eq!(CENTRAL_TOOL_ALLOWLIST.len(), 17);
        // 中枢专属协调工具在白名单内
        for tool in [
            "get_context",
            "create_goal",
            "dispatch_work_item",
            "switch_worker_model",
            "request_user",
            "create_report",
        ] {
            assert!(is_central_tool_allowed(tool), "{tool} 应在白名单内");
        }
    }

    #[test]
    fn worker_tools_are_denied_for_central() {
        for tool in [
            "fs",
            "terminal",
            "command",
            "browser",
            "computer-use",
            "database",
            "coding",
            "test_runner",
        ] {
            assert!(!is_central_tool_allowed(tool), "{tool} 不得进入中枢白名单");
        }
    }

    #[test]
    fn ticket_requires_confirmed_switch_decision() {
        let mut consultation = ModelHandoverConsultation::new("conv_1", FailureClass::AuthExpired);
        // 未决定：不签发
        assert!(ModelSwitchTicket::issue(&consultation).is_none());

        // 决定为挂起：不签发（不得把沉默或挂起当作同意换模型）
        consultation.user_decision = Some(HandoverDecision::Suspend);
        assert!(ModelSwitchTicket::issue(&consultation).is_none());

        // 决定换模型但缺工作项归属：不签发
        consultation.work_item_id = None;
        consultation.user_decision = Some(HandoverDecision::SwitchModel {
            model: "model-b".to_string(),
        });
        assert!(ModelSwitchTicket::issue(&consultation).is_none());

        // 补齐归属与决定时间：签发并可校验
        consultation.work_item_id = Some("work_1".to_string());
        consultation.decided_at = Some(decision_time());
        let ticket = ModelSwitchTicket::issue(&consultation).expect("完整协商应签发票据");
        assert!(ticket.is_well_formed());
        assert!(ticket.matches("work_1", "model-b"));
        assert!(!ticket.matches("work_2", "model-b"));
        assert!(!ticket.matches("work_1", "model-c"));
    }
}
