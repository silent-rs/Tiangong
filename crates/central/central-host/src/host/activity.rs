//! 后台工作活动状态采集（Issue #778 的保留部分：可见性）。
//!
//! Worker 会话的流事件经 [`classify_signal`] 映射为活动状态值
//! [`WorkActivity`] 与最近动作文案，由消费循环的统一入口 [`note_stream_signal`]
//! 刷新到登记（状态条 / 抽屉的数据源）。不做无进展判定：模型请求、插件工具
//! 调用、终端命令在各自层面都有超时，静默会被兜成失败走既有回报通道。
//!
//! 采集点在 `start_stream_consumer` 循环体最前（任何 `continue` 之前）——压缩 /
//! 切换事件走免锁快速通道，也要被采集；hook 只取 registry 短锁，不触碰
//! `session_send_lock` / `app_state` 锁，快速通道的免锁前提不变；普通会话
//!（registry 无该会话 `running` 条目）直接返回。

use std::time::{Duration, Instant};

use central_state::{WorkActivity, WorkState};
use tiangong_types::StreamEvent;

use crate::CentralHostState;

/// 进度广播节流：高频文本增量 2 秒至多广播一次，防前端风暴（深检 E）。
pub const ACTIVITY_EMIT_THROTTLE: Duration = Duration::from_secs(2);

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
        StreamEvent::ContextCompressed { .. } => {
            (WorkActivity::Requesting, "上下文压缩完成".to_string())
        }
        StreamEvent::ModelSwitchStarted { .. } => {
            (WorkActivity::SwitchingModel, "切换模型".to_string())
        }
        StreamEvent::ModelSwitched { .. } => (WorkActivity::Requesting, "模型已切换".to_string()),
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
        StreamEvent::AgentNotification { agent_label, .. } => {
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
    /// 已刷新但仍在 2 秒广播节流窗口内。
    Throttled,
    /// 立即广播（文案变化且已过节流窗口）。
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

/// 消费循环的统一入口：流事件 → 活动信号 → 刷新登记，并给出是否广播状态条
/// 的决策（按 2 秒节流）。独立成函数是为了能在不启动消费者线程 / Tauri
/// 句柄的情况下按真实事件驱动测试。
pub fn note_stream_signal(
    state: &CentralHostState,
    session_id: &str,
    event: &StreamEvent,
    last_emit: &mut Instant,
) -> Emit {
    let Some((activity, detail)) = classify_signal(event) else {
        return Emit::None;
    };
    match state.note_work_signal(session_id, activity, &detail) {
        SignalOutcome::NotRunning | SignalOutcome::Updated { changed: false } => Emit::None,
        SignalOutcome::Updated { changed: true } => {
            let now = Instant::now();
            if now.duration_since(*last_emit) >= ACTIVITY_EMIT_THROTTLE {
                *last_emit = now;
                Emit::Now
            } else {
                Emit::Throttled
            }
        }
    }
}

impl CentralHostState {
    /// 刷新该会话 `running` 条目的活动状态值与最近动作文案。
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
    }
}
