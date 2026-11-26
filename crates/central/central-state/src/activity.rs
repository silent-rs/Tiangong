//! 后台工作的活动状态值（Issue #778 的保留部分：可见性）。
//!
//! `WorkState::Running` 是粗粒度生命周期状态，不表达「正在做什么」。本模块给
//! 登记条目 [`crate::WorkStatus`] 提供一个正交的活动状态值 [`WorkActivity`]，
//! 由 Worker 会话的流事件刷新，驱动状态条 / 抽屉的实时展示（「回复中」
//! 「压缩上下文」「重试 n/3」……），取代只能转圈的「执行中」。
//!
//! 这里**不做**无进展判定：模型请求、插件工具调用、终端命令在各自层面都有
//! 超时，静默会被兜成失败走既有回报通道；可见性负责让用户知道此刻在做什么。
//! Worker 与 workspace 一一对应，活动来自当前活跃 session；没有活跃 session
//! 时活动与最近动作清空，Worker 待机。`execution_id` 不参与活动投影。

use serde::{Deserialize, Serialize};

/// 工作活动状态值（`Running` 的细粒度状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkActivity {
    /// 已派发，尚无任何活动信号。
    #[default]
    Pending,
    /// 回复中（最终回复或过程文本增量）。
    Responding,
    /// 思考中（推理增量）。
    Reasoning,
    /// 已发出模型请求、等待响应（新一轮请求、压缩完成、模型切换完成）。
    Requesting,
    /// 工具执行中。
    ToolRunning,
    /// 上下文压缩中（压缩模型调用期间静默）。
    Compressing,
    /// 切换模型中。
    SwitchingModel,
    /// 模型请求重试中。
    Retrying,
    /// 子代理活动中。
    Subagent,
}

impl WorkActivity {
    /// 全部活动状态（顺序即声明顺序）。
    pub const ALL: [WorkActivity; 9] = [
        WorkActivity::Pending,
        WorkActivity::Responding,
        WorkActivity::Reasoning,
        WorkActivity::Requesting,
        WorkActivity::ToolRunning,
        WorkActivity::Compressing,
        WorkActivity::SwitchingModel,
        WorkActivity::Retrying,
        WorkActivity::Subagent,
    ];

    /// 线上形式（与 serde 一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            WorkActivity::Pending => "pending",
            WorkActivity::Responding => "responding",
            WorkActivity::Reasoning => "reasoning",
            WorkActivity::Requesting => "requesting",
            WorkActivity::ToolRunning => "tool_running",
            WorkActivity::Compressing => "compressing",
            WorkActivity::SwitchingModel => "switching_model",
            WorkActivity::Retrying => "retrying",
            WorkActivity::Subagent => "subagent",
        }
    }

    /// 中文短标签（日志与兜底展示）。
    pub fn label(self) -> &'static str {
        match self {
            WorkActivity::Pending => "等待输出",
            WorkActivity::Responding => "回复中",
            WorkActivity::Reasoning => "思考中",
            WorkActivity::Requesting => "等待响应",
            WorkActivity::ToolRunning => "执行中",
            WorkActivity::Compressing => "压缩中",
            WorkActivity::SwitchingModel => "切换模型",
            WorkActivity::Retrying => "重试中",
            WorkActivity::Subagent => "子代理中",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_as_str_matches_serde_snake_case() {
        for activity in WorkActivity::ALL {
            let json = serde_json::to_string(&activity).unwrap();
            assert_eq!(json, format!("\"{}\"", activity.as_str()));
            assert_eq!(
                serde_json::from_str::<WorkActivity>(&json).unwrap(),
                activity
            );
            assert!(!activity.label().is_empty());
        }
        assert_eq!(WorkActivity::default(), WorkActivity::Pending);
    }
}
