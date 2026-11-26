//! 后台工作活动状态采集（Issue #778 的保留部分：可见性）。
//!
//! Worker 会话的流事件经 [`classify_signal`] 映射为活动状态值
//! [`WorkActivity`] 与最近动作文案，由消费循环的统一入口 [`note_stream_signal`]
//! 刷新到当前活跃 session 对应的 Worker 登记（状态条 / 抽屉的数据源）。
//! Worker 与 workspace 一一对应；没有活跃 session 时活动与最近动作清空，
//! Worker 回到待机。`execution_id` 只用于轮次审计/终态路由，不参与活动投影。
//!
//! 采集点在 `start_stream_consumer` 循环体最前（任何 `continue` 之前）——压缩 /
//! 切换事件走免锁快速通道，也要被采集；hook 只取 registry 短锁，不触碰
//! `session_send_lock` / `app_state` 锁，快速通道的免锁前提不变；普通会话
//! 在分类和分配文案前按 session 排除，不触碰 Core 或广播节流锁。

use central_state::{WorkActivity, WorkState};
use tiangong_types::StreamEvent;

use crate::CentralHostState;

/// 流事件 → 活动信号：`(活动状态值, 最近动作文案)`。
///
/// 覆盖 Worker 会话的全部「正在做事」证据：输出（回复 / 过程文本 / 思考）、
/// 请求轮次、工具、上下文压缩、模型切换、重试、子代理。`TurnElapsed`
/// （每秒计时）、`TokenUsage`、`UserMessage` 等不计——它们不证明模型或工具
/// 在工作；`Done / Error` 是终态，走回报路径。
pub fn classify_signal(event: &StreamEvent) -> Option<(WorkActivity, String)> {
    let (activity, detail) = match event {
        StreamEvent::Delta { .. } | StreamEvent::SummaryText { .. } => {
            (WorkActivity::Responding, "回复中".to_string())
        }
        StreamEvent::ReactText { .. } => (WorkActivity::Responding, "输出中".to_string()),
        StreamEvent::Reasoning { .. } => (WorkActivity::Reasoning, "思考中".to_string()),
        StreamEvent::PhaseChanged { phase, iteration } => (
            WorkActivity::Requesting,
            format!("阶段 {phase}（第 {iteration} 轮）"),
        ),
        StreamEvent::ToolCalls { names, .. } => (
            WorkActivity::ToolRunning,
            if names.is_empty() {
                "调用工具".to_string()
            } else {
                format!("调用 {}", names.join("、"))
            },
        ),
        StreamEvent::ToolStart { name, .. } => (WorkActivity::ToolRunning, format!("执行 {name}")),
        StreamEvent::ToolFinished { name, ok, .. } | StreamEvent::ToolResult { name, ok, .. } => (
            WorkActivity::ToolRunning,
            format!("{name} {}", if *ok { "完成" } else { "失败" }),
        ),
        StreamEvent::ContextCompressing { .. } => {
            (WorkActivity::Compressing, "压缩上下文".to_string())
        }
        StreamEvent::ContextCompressed { action, .. } => {
            let detail = match action {
                tiangong_types::stream::ContextCompressAction::Failed => "上下文压缩失败",
                tiangong_types::stream::ContextCompressAction::Cancelled => "上下文压缩已取消",
                tiangong_types::stream::ContextCompressAction::Noop => "无需压缩上下文",
                tiangong_types::stream::ContextCompressAction::Clear => "上下文清理完成",
                tiangong_types::stream::ContextCompressAction::Compress
                | tiangong_types::stream::ContextCompressAction::Auto => "上下文压缩完成",
            };
            (WorkActivity::Requesting, detail.to_string())
        }
        StreamEvent::ModelSwitchStarted { model_name } => (
            WorkActivity::SwitchingModel,
            format!("切换模型（{model_name}）"),
        ),
        StreamEvent::ModelSwitched { model_name } => (
            WorkActivity::Requesting,
            format!("模型已切换（{model_name}）"),
        ),
        StreamEvent::Retry {
            attempt,
            max_attempts,
            ..
        } => (
            WorkActivity::Retrying,
            format!("重试模型请求（{attempt}/{max_attempts}）"),
        ),
        StreamEvent::AgentCreated { label, .. } | StreamEvent::AgentStatusChanged { label, .. } => {
            (WorkActivity::Subagent, format!("子代理 {label}"))
        }
        StreamEvent::AgentMessage {
            from_agent_label, ..
        } => (WorkActivity::Subagent, format!("子代理 {from_agent_label}")),
        StreamEvent::AgentNotification {
            agent_id,
            agent_label,
            ..
        } => {
            // 系统通知（如手动压缩失败）不是子代理活动证据。
            if agent_id == "system" {
                return None;
            }
            (WorkActivity::Subagent, format!("子代理 {agent_label}"))
        }
        _ => return None,
    };
    Some((activity, detail))
}

/// 消费循环对单个流事件的处理结果：是否广播状态条刷新。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emit {
    /// 非活动信号，或该会话无 running 登记 / 文案未变：无事可做。
    None,
    /// 活动状态或最近动作发生变化，立即广播最新状态。
    Now,
}

/// 信号 hook 的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalOutcome {
    /// 该会话没有 `running` 登记条目（普通会话 / 已不在途）：零语义变化。
    NotRunning,
    /// 已刷新；`changed` 表示活动状态值或最近动作文案发生变化。
    Updated { changed: bool },
}

/// 消费循环的统一入口：流事件 → 活动信号 → 刷新当前活跃 session 的登记。
/// 仅在活动值或最近动作实际变化时广播，重复增量不会造成前端刷新风暴。
pub fn note_stream_signal(state: &CentralHostState, session_id: &str, event: &StreamEvent) -> Emit {
    let Some((activity, detail)) = classify_signal(event) else {
        return Emit::None;
    };
    match state.note_work_signal(session_id, activity, &detail) {
        SignalOutcome::NotRunning | SignalOutcome::Updated { changed: false } => Emit::None,
        SignalOutcome::Updated { changed: true } => Emit::Now,
    }
}

impl CentralHostState {
    /// 当前 session 是否有活跃 Worker 登记。消费者在分类、分配文案和访问
    /// Core 之前调用，普通会话只为一次 registry 短锁扫描，不承担活动采集成本。
    pub fn has_running_work_session(&self, session_id: &str) -> bool {
        let registry = self
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        registry
            .iter()
            .rev()
            .any(|entry| entry.session_id == session_id && entry.state == WorkState::Running)
    }

    /// 刷新该会话 `running` 条目的活动状态值与最近动作文案。
    ///
    /// 这里按当前活跃 session 定位 Worker，不按 `execution_id` 过滤；轮次
    /// 归属由终态路由器负责，活动展示只回答“这个 Worker 现在在做什么”。
    pub fn note_work_signal(
        &self,
        session_id: &str,
        activity: WorkActivity,
        detail: &str,
    ) -> SignalOutcome {
        let mut registry = self
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(entry) = registry
            .iter_mut()
            .rev()
            .find(|entry| entry.session_id == session_id && entry.state == WorkState::Running)
        else {
            return SignalOutcome::NotRunning;
        };
        let mut changed = entry.activity != activity;
        entry.activity = activity;
        if entry.last_activity.as_deref() != Some(detail) {
            entry.last_activity = Some(detail.to_string());
            changed = true;
        }
        SignalOutcome::Updated { changed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_covers_activity_and_ignores_timer_noise() {
        let delta = StreamEvent::Delta {
            message_id: "m1".to_string(),
            content: "x".to_string(),
            tokens: None,
        };
        assert_eq!(
            classify_signal(&delta),
            Some((WorkActivity::Responding, "回复中".to_string()))
        );
        assert_eq!(
            classify_signal(&StreamEvent::TurnElapsed { seconds: 9 }),
            None
        );
        assert_eq!(
            classify_signal(&StreamEvent::TitleChanged {
                title: "t".to_string()
            }),
            None
        );
        // 免锁快速通道的事件也必须被采集
        assert_eq!(
            classify_signal(&StreamEvent::ContextCompressing {
                summary_up_to: 0,
                total_messages: 1
            })
            .unwrap()
            .0,
            WorkActivity::Compressing
        );
        let retry = classify_signal(&StreamEvent::Retry {
            message: "timeout".to_string(),
            attempt: 3,
            max_attempts: 3,
        })
        .unwrap();
        assert_eq!(
            retry,
            (WorkActivity::Retrying, "重试模型请求（3/3）".to_string())
        );
        let tool = classify_signal(&StreamEvent::ToolStart {
            name: "cargo".to_string(),
            args_summary: String::new(),
        })
        .unwrap();
        assert_eq!(tool, (WorkActivity::ToolRunning, "执行 cargo".to_string()));
        assert_eq!(
            classify_signal(&StreamEvent::ContextCompressed {
                action: tiangong_types::stream::ContextCompressAction::Failed,
                summary_up_to: 2,
                remaining_messages: 3,
            }),
            Some((WorkActivity::Requesting, "上下文压缩失败".to_string()))
        );
        assert_eq!(
            classify_signal(&StreamEvent::AgentNotification {
                agent_id: "system".into(),
                agent_label: "系统".into(),
                content: "压缩失败".into(),
                level: "warning".into(),
            }),
            None
        );
        assert_eq!(
            classify_signal(&StreamEvent::ModelSwitchStarted {
                model_name: "k3-pro".into(),
            }),
            Some((
                WorkActivity::SwitchingModel,
                "切换模型（k3-pro）".to_string()
            ))
        );
    }
}
