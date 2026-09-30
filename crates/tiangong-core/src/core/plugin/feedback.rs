//! 插件到 Core 的反馈通道。
//!
//! 会话注入、用量上报和流事件都通过当前 turn 的命令通道投递，由 Agent Loop
//! 按命令顺序处理。插件不持有 Session、用量收集器或前端事件发送端。

use crate::core::command::Command;
use tokio::sync::mpsc::UnboundedSender;

/// 插件向 Core 投递的会话注入反馈。
#[derive(Debug, Clone)]
pub struct PluginFeedback {
    pub tool_name: String,
    pub payload: serde_json::Value,
}

impl PluginFeedback {
    pub fn new(tool_name: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            tool_name: tool_name.into(),
            payload,
        }
    }
}

/// 插件状态反馈通道的发送端。
///
/// 该发送端只属于当前 turn；turn 结束后接收端随任务释放，迟到反馈会发送失败，
/// 不会进入后续 turn。
#[derive(Clone)]
pub struct PluginFeedbackTx {
    sender: UnboundedSender<Command>,
}

impl PluginFeedbackTx {
    pub(crate) fn new(sender: UnboundedSender<Command>) -> Self {
        Self { sender }
    }

    /// 将插件产生的结构化内容注入当前会话。
    ///
    /// 返回 `true` 表示已成功投递到当前 turn 的命令队列；`false` 表示通道已关闭
    ///（turn 已结束消费 / Core 已退出）。调用方可据此决定是否回退到缓冲重试。
    ///
    /// 注意：`true` 仅保证命令已入队，不保证 turn 内一定会处理它——若 turn 在
    /// 入队后、消费前结束，命令仍可能丢失。彻底消除该竞态需配合 turn 侧在
    /// Agent Loop 结束后立即 drop 接收端（见 `run_turn`）。
    pub fn inject_tool(&self, tool_name: impl Into<String>, payload: serde_json::Value) -> bool {
        self.sender
            .send(Command::InjectTool {
                tool_name: tool_name.into(),
                payload,
            })
            .is_ok()
    }

    /// 上报插件内部模型调用产生的用量，并向前端发送用量事件。
    /// 返回 `true` 表示已成功投递到当前 turn 的命令队列；`false` 表示封口或
    /// 通道关闭导致投递被拒。调用方应据此记录丢失，不能静默忽略。
    pub fn report_token_usage(
        &self,
        usage: tiangong_types::TokenUsage,
        source: impl Into<String>,
    ) -> bool {
        self.sender
            .send(Command::ReportUsage {
                usage,
                source: source.into(),
                emit_event: true,
            })
            .is_ok()
    }

    /// 将已经逐笔通知过前端的嵌套执行用量并入当前 turn，不重复发送用量事件。
    /// 返回 `true` 表示已成功投递到当前 turn 的命令队列；`false` 表示封口或
    /// 通道关闭导致投递被拒。调用方应据此记录丢失，不能静默忽略。
    pub fn accumulate_token_usage(
        &self,
        usage: tiangong_types::TokenUsage,
        source: impl Into<String>,
    ) -> bool {
        self.sender
            .send(Command::ReportUsage {
                usage,
                source: source.into(),
                emit_event: false,
            })
            .is_ok()
    }

    /// 向当前 turn 投递一个前端流事件。
    pub fn send_stream_event(&self, event: tiangong_types::StreamEvent) {
        let _ = self.sender.send(Command::EmitStreamEvent(Box::new(event)));
    }

    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    /// 当前通道是否仍可接收命令。turn 结束后返回 `false`。
    pub fn is_accepting(&self) -> bool {
        !self.sender.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage() -> tiangong_types::TokenUsage {
        tiangong_types::TokenUsage {
            prompt_tokens: 3,
            completion_tokens: 2,
            total_tokens: 5,
            prompt_cache_hit_tokens: None,
            prompt_cache_miss_tokens: None,
        }
    }

    #[test]
    fn usage_reports_use_the_turn_command_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let feedback = PluginFeedbackTx::new(tx);

        feedback.report_token_usage(usage(), "attachment");
        feedback.accumulate_token_usage(usage(), "child");

        let Command::ReportUsage {
            source, emit_event, ..
        } = rx.try_recv().unwrap()
        else {
            panic!("expected usage report");
        };
        assert_eq!(source, "attachment");
        assert!(emit_event);

        let Command::ReportUsage {
            source, emit_event, ..
        } = rx.try_recv().unwrap()
        else {
            panic!("expected accumulated usage report");
        };
        assert_eq!(source, "child");
        assert!(!emit_event);
    }

    #[test]
    fn inject_tool_returns_true_when_consumer_alive() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let feedback = PluginFeedbackTx::new(tx);

        assert!(feedback.inject_tool("browser_data", serde_json::json!({"url": "x"})));
        match rx.try_recv() {
            Ok(Command::InjectTool { tool_name, .. }) => assert_eq!(tool_name, "browser_data"),
            _ => panic!("expected InjectTool"),
        }
    }

    #[test]
    fn inject_tool_returns_false_after_receiver_dropped() {
        // 模拟 turn 收尾窗口：Agent Loop 已退出，接收端被显式 drop。
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let feedback = PluginFeedbackTx::new(tx);
        drop(rx);

        // drop 接收端后，is_closed() 应为 true，inject_tool 也必须返回 false。
        assert!(feedback.is_closed());
        assert!(!feedback.inject_tool("terminal_user_input", serde_json::json!({})));
    }
}
