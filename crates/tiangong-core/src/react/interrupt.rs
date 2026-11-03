//! 引导/取消的结构化中断与协议闭合（任务 11，自 execute.rs 机械拆分）。

use crate::react::phase::{ActiveLlm, ExecutionPhase, LlmPurpose};
use crate::turn_context::TurnContext;
use tiangong_types::StreamEvent;

use super::cancel::{abort_and_join, emit_cancel_usage};
use super::execute::{AgentLoopState, ToolInjectionBuffer, persist_streamed_react_message};

/// 持久化被中断的 LLM 请求已流式收到的部分输出（降级为 React 过程消息）。
pub(super) fn persist_interrupted_llm_output(
    ctx: &mut TurnContext,
    purpose: &LlmPurpose,
    pending_msg_id: &str,
    streamed_text: &str,
    streamed_reasoning: &str,
    reasoning_elapsed_ms: Option<u64>,
    text_elapsed_ms: Option<u64>,
) {
    match purpose {
        LlmPurpose::React { .. } => {
            persist_streamed_react_message(
                ctx,
                pending_msg_id,
                streamed_text,
                streamed_reasoning,
                reasoning_elapsed_ms,
                text_elapsed_ms,
            );
        }
    }
}

/// 中断主循环直接拥有的活动（阶段感知）：消费当前阶段持有的资源并完成收尾。
///
/// `reason` 区分两种中断语义：
/// - 引导消息（ALR-101）：Summary 部分输出**降级**为 React 过程消息（ALR-104）；
/// - 取消/关闭：Summary 部分输出按取消路径持久化（保持 Summary 身份，与旧行为
///   一致），工具/压缩处理相同。
///
/// 两者都**不取消插件独立持有的后台任务**（ALR-103）。中断后安装 `NeedModel`
/// 之外的目标阶段由调用方决定；本函数保证阶段资源全部转移、取消或完成（ALR-205）。
pub(super) async fn interrupt_active_work(
    ctx: &mut TurnContext,
    state: &mut AgentLoopState,
    injections: &mut ToolInjectionBuffer,
    stream_tx: &std::sync::mpsc::Sender<StreamEvent>,
    context_limit: usize,
    downgrade_summary: bool,
) {
    let phase = state.take_phase();
    tracing::debug!(
        session_id = %ctx.session.id,
        from_phase = phase.name(),
        downgrade_summary,
        "中断主循环直接拥有的活动"
    );
    match phase {
        ExecutionPhase::WaitingModel(active) => {
            let ActiveLlm {
                purpose,
                pending_msg_id,
                sink,
                task,
                mut streamed_text,
                mut streamed_reasoning,
                mut streaming_usage,
                mut chunk_rx,
                timing,
                ..
            } = active;
            sink.finish();
            let completed_response = abort_and_join(task).await.ok().and_then(Result::ok);
            // 命令优先可能抢在最后一帧之前，取消后排空已接收的累计用量。
            while let Ok(chunk) = chunk_rx.try_recv() {
                if let Some(usage) = chunk.usage {
                    streaming_usage.merge_snapshot(&usage.into());
                }
                streamed_text.push_str(&chunk.content);
                streamed_reasoning.push_str(&chunk.reasoning_content);
            }
            if let Some(response) = completed_response {
                streaming_usage.merge_snapshot(&response.usage);
            }
            let _ = downgrade_summary;
            // 输出耗时为 decode 口径（首末输出增量间隔，见 StreamTiming）；
            // 未收到任何增量的中断请求无输出耗时，速度留空。
            persist_interrupted_llm_output(
                ctx,
                &purpose,
                &pending_msg_id,
                &streamed_text,
                &streamed_reasoning,
                timing.reasoning.elapsed_ms(),
                timing.output.elapsed_ms(),
            );
            emit_cancel_usage(stream_tx, &streaming_usage, context_limit);
            state.accumulated_usage.accumulate(&streaming_usage);
            super::context::record_call_usage(
                ctx,
                &pending_msg_id,
                &streaming_usage,
                "react",
                tiangong_types::TurnStatus::Cancelled,
            );
            ctx.session.persist_to_disk();
        }
        ExecutionPhase::PendingFinish(_) => {
            // 无在途活动；悬空调用由下方统一闭合。
        }
        ExecutionPhase::NeedModel => {}
    }

    injections.commit(ctx);

    // 闭合残留的未完成 tool calls（模型已返回但工具未开始执行的）。
    let closed = ctx
        .session
        .close_unfinished_tool_calls_with_reason("工具调用因用户发送新消息而中断。");
    for (tool_call_id, tool_name, output) in closed {
        let _ = stream_tx.send(StreamEvent::ToolResult {
            name: tool_name,
            tool_call_id: Some(tool_call_id),
            ok: false,
            output,
            full_output: None,
            duration_ms: None,
        });
    }
}
