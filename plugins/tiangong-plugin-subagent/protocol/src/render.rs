//! 投递消息的插件渲染声明（`POST /api/v1/messages` 的 `render` 字段）。
//!
//! 投递正文保持完整：它是模型读到的内容，也是旧版宿主（不认识 render）
//! 的显示兜底。渲染声明只给界面用——宿主据此把消息交给本插件的
//! `session.message-item` 贡献 [`MESSAGE_VIEW`]（`render: "replace"`）渲染为卡片，
//! `data` 携带卡片所需的结构化字段，视图不再从正文里猜格式。
//!
//! 分段边界由视图按正文中的段落标记（【任务工作区】等）定位；正文本身由
//! 视图从消息上下文的 `text` 读取，避免重复携带大段文本（宿主限制 render
//! 序列化后不超过 16KB）。

use serde::Serialize;
use serde_json::Value;

use crate::hooks::{HookEvent, HookEventType};

/// 本插件渲染投递消息的 `session.message-item` 贡献 ID（见 plugin.json）。
pub const MESSAGE_VIEW: &str = "agent-message";

/// 卡片类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CardKind {
    /// 发给成员执行会话的正式任务。
    Task,
    /// 发给成员执行会话的协作 / 补充消息。
    Message,
    /// 成员经 report_agent_result 回报给发起方。
    Report,
    /// 运行状态 Hook（完成、失败、阻塞、审批、消息）。
    Hook,
    /// 运行控制通知（中断 / 取消）。
    Control,
}

/// 渲染声明的 `data`。
#[derive(Debug, Clone, Serialize)]
pub struct CardData {
    pub kind: CardKind,
    /// 发起方展示名（任务 / 消息：如「主会话」「成员「A」（id）」）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// 成员名（回报 / Hook / 控制）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Hook 状态：message / completed / failed / blocked / approval_required。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    /// 运行标记短码（任务 / 消息 / 回报正文尾部的「（运行标记 r-…）」），
    /// 卡片以元信息展示，正文核心内容不再重复显示该行。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_tag: Option<String>,
}

/// 组装渲染声明（`{plugin, view, data}`）。
pub fn render(data: &CardData) -> Value {
    serde_json::json!({
        "plugin": crate::PLUGIN_ID,
        "view": MESSAGE_VIEW,
        "data": data,
    })
}

/// 任务 / 消息投递（成员执行会话收到）。
pub fn delivery(task: bool, source: &str, run_tag: Option<&str>) -> Value {
    render(&CardData {
        kind: if task {
            CardKind::Task
        } else {
            CardKind::Message
        },
        source: Some(source.to_string()),
        agent_name: None,
        agent_id: None,
        status: None,
        run_tag: run_tag.map(str::to_string),
    })
}

/// 成员回报（发起方会话收到）。
pub fn report(agent_name: &str, agent_id: &str, run_tag: Option<&str>) -> Value {
    render(&CardData {
        kind: CardKind::Report,
        source: None,
        agent_name: Some(agent_name.to_string()),
        agent_id: Some(agent_id.to_string()),
        status: None,
        run_tag: run_tag.map(str::to_string),
    })
}

/// 运行控制通知（成员执行会话收到）。
pub fn control(agent_name: &str, agent_id: &str) -> Value {
    render(&CardData {
        kind: CardKind::Control,
        source: None,
        agent_name: Some(agent_name.to_string()),
        agent_id: Some(agent_id.to_string()),
        status: None,
        run_tag: None,
    })
}

impl HookEvent {
    /// Hook 投递的渲染声明。
    pub fn render(&self) -> Value {
        render(&CardData {
            kind: CardKind::Hook,
            source: None,
            agent_name: Some(self.agent_name.clone()),
            agent_id: Some(self.agent_id.clone()),
            status: Some(match self.event_type {
                HookEventType::Message => "message",
                HookEventType::Blocked => "blocked",
                HookEventType::ApprovalRequired => "approval_required",
                HookEventType::Completed => "completed",
                HookEventType::Failed => "failed",
            }),
            run_tag: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 渲染声明指向本插件视图且省略空字段() {
        let value = delivery(true, "主会话", Some("ABCDEFGH"));
        assert_eq!(value["plugin"], crate::PLUGIN_ID);
        assert_eq!(value["view"], MESSAGE_VIEW);
        assert_eq!(value["data"]["kind"], "task");
        assert_eq!(value["data"]["source"], "主会话");
        assert_eq!(value["data"]["run_tag"], "ABCDEFGH");
        assert!(value["data"].get("agent_name").is_none());

        let value = report("评审", "agent-1", None);
        assert_eq!(value["data"]["kind"], "report");
        assert_eq!(value["data"]["agent_name"], "评审");
        assert!(value["data"].get("run_tag").is_none());
    }

    #[test]
    fn hook_渲染声明携带状态() {
        let event = HookEvent {
            event_id: "e".into(),
            agent_id: "agent-1".into(),
            agent_name: "评审".into(),
            activation_id: None,
            conversation_id: "s".into(),
            task_id: None,
            run_id: None,
            workspace: "/w".into(),
            event_type: HookEventType::Completed,
            created_at: String::new(),
            payload: serde_json::json!({ "text": "完成" }),
            attempts: 0,
            last_error: None,
        };
        let value = event.render();
        assert_eq!(value["data"]["kind"], "hook");
        assert_eq!(value["data"]["status"], "completed");
        assert_eq!(value["data"]["agent_name"], "评审");
        // 正文不变（模型与旧版宿主读取）。
        assert_eq!(event.render_message(), "[Subagent·评审] 任务完成：完成");
    }
}
