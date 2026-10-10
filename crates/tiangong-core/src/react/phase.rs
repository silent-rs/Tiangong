//! 执行阶段数据与执行预算（任务 03）。
//!
//! 阶段数据类型集中于此，供 execute.rs 及后续拆分的兄弟模块统一使用。
//! 所有权模式（take/install）与取消方式已由任务 02 原型验证，
//! 结论见 design.md 3.1；任务 04 起在此数据模型上接入正式 `ExecutionPhase` 驱动。

use std::collections::VecDeque;

use tokio::sync::mpsc::UnboundedReceiver;

use super::outcome::TurnExecutionResult;
use crate::stream_throttle::ThrottledStreamSink;
use tiangong_llm::tool::ToolCall;
use tiangong_llm::{InvalidToolCall, ModelResponse, ModelStreamChunk};
use tiangong_types::TokenUsage;

/// 执行阶段：任意时刻当前阶段唯一（ALR-001）。
///
/// Ready 阶段（NeedModel / PendingFinish）由驱动同步推进；
/// Waiting 阶段持有活动资源并进入事件等待。
///
/// 任务 15 起不再有独立完成度检查（Summary）或强制最终回复阶段：模型无工具
/// 调用的响应只形成候选完成，由 `contract::TaskContract` 同步门控（ALR-003）。
pub(super) enum ExecutionPhase {
    /// Ready：需要发起下一次 ReAct 模型请求。
    NeedModel,
    /// Waiting：ReAct 模型请求进行中。
    WaitingModel(ActiveLlm),
    /// Ready：大循环已产出暂定结果，待提交（任务 07 接入命令仲裁）。
    PendingFinish(TurnExecutionResult),
}

impl ExecutionPhase {
    /// 阶段名（迁移日志用，不含敏感内容）。
    pub(super) fn name(&self) -> &'static str {
        match self {
            ExecutionPhase::NeedModel => "NeedModel",
            ExecutionPhase::WaitingModel(_) => "WaitingModel",
            ExecutionPhase::PendingFinish(_) => "PendingFinish",
        }
    }
}

/// 执行预算：物理 turn 内的各阶段计数。
///
/// `reset_for_new_intent`（引导消息注入时使用）代表新的用户意图，清阶段级计数。
/// `request_round` 是物理 turn 内的日志/事件序号，任何重置都保留它；
/// `accumulated_usage` 不在此结构中（永远不重置）。
///
/// 任务 15 起不再有 `continuation_count` / `max_outer_iterations`：工具协议
/// 修复计数在 `contract::TaskContract` 中，独立且很小（ALR-305）。
#[derive(Default)]
pub(super) struct ExecutionBudget {
    /// 物理 turn 内的全局请求编号（仅日志/流事件，不参与重置决策）。
    pub(super) request_round: usize,
}

/// 已就绪待执行的单个工具调用。
pub(super) struct PreparedToolCall {
    pub(super) index: usize,
    pub(super) call: ToolCall,
    pub(super) args_summary: String,
}

/// 一批工具调用的执行状态。
pub(super) struct ToolBatchState {
    pub(super) calls: VecDeque<(usize, ToolCall)>,
    pub(super) ready_tools: Vec<PreparedToolCall>,
    pub(super) invalid_tool_calls: Vec<InvalidToolCall>,
    pub(super) response_usage: TokenUsage,
}

/// 模型请求的用途。
pub(super) enum LlmPurpose {
    React { request_injection_generation: u64 },
}

/// 单个流式阶段的耗时测量：记录首个与最后一个增量的到达时刻。
#[derive(Default)]
pub(super) struct StreamElapsed {
    first: Option<std::time::Instant>,
    last: Option<std::time::Instant>,
}

impl StreamElapsed {
    /// 收到一个非空增量时调用。
    pub(super) fn record(&mut self) {
        let now = std::time::Instant::now();
        if self.first.is_none() {
            self.first = Some(now);
        }
        self.last = Some(now);
    }

    /// 首末增量间的时长（毫秒）；从未收到增量时为 `None`。
    pub(super) fn elapsed_ms(&self) -> Option<u64> {
        Some(self.last?.duration_since(self.first?).as_millis() as u64)
    }
}

/// 一次模型流式输出中思考与正文两个阶段的耗时测量。
#[derive(Default)]
pub(super) struct StreamTiming {
    pub(super) reasoning: StreamElapsed,
    pub(super) text: StreamElapsed,
}

/// 活跃模型请求（阶段持有）。
pub(super) struct ActiveLlm {
    pub(super) purpose: LlmPurpose,
    pub(super) pending_msg_id: String,
    pub(super) sink: ThrottledStreamSink,
    pub(super) chunk_rx: UnboundedReceiver<ModelStreamChunk>,
    pub(super) task: tokio::task::JoinHandle<anyhow::Result<ModelResponse>>,
    pub(super) streamed_text: String,
    pub(super) streamed_reasoning: String,
    pub(super) streaming_usage: TokenUsage,
    pub(super) timing: StreamTiming,
}

impl ActiveLlm {
    /// 诊断摘要（不含流式正文）。
    #[allow(dead_code)]
    pub(super) fn debug_summary(&self) -> String {
        let purpose = match &self.purpose {
            LlmPurpose::React { .. } => "react".to_string(),
        };
        format!(
            "llm: purpose={purpose} streamed_chars={} usage_total={}",
            self.streamed_text.chars().count(),
            self.streaming_usage.total_tokens,
        )
    }
}
