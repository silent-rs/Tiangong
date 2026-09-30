//! 工具执行流水线（ALR-301/302，任务 17）。
//!
//! 模型响应中的工具调用整批交给 [`execute_tool_batch`]：规范化与参数校验
//! （无效调用上下文）→ 有界并行执行 → 顺序提交结果并更新
//! [`TaskContract`](super::contract::TaskContract) → 闭合 Provider 工具协议。
//! 取消/引导到达时批次收敛闭合后把命令交还执行驱动。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc as tokio_mpsc;
use tokio::task::{Id as TaskId, JoinSet};

use crate::core::command::Command;
use crate::core::plugin::Plugin;
use crate::permission::TrustMode;
use crate::turn_context::TurnContext;
use tiangong_types::StreamEvent;

use super::command::Deferred;
use super::compression::observed_total_tokens;
use super::execute::{
    AgentLoopState, CompletedToolCall, ToolInjectionBuffer, ToolPreflightOutcome,
    append_failure_recovery_prompt, append_invalid_tool_calls_context, prepare_tool_call,
    record_completed_tool_call, set_runtime_trust_mode,
};
use super::helpers::record_plugin_usage;
use super::message::append_tool_result_message;
use super::phase::PreparedToolCall;
use super::phase::ToolBatchState;
use super::tool_call::start_tool_call;

/// 单批工具的最大并发数（有界并行，ALR-301）。
const MAX_PARALLEL_TOOLS: usize = 8;

/// 工具批次执行结果。
pub(super) enum ToolBatchOutcome {
    /// 批次全部闭合（成功/失败/拒绝/跳过均为合法结果），回到 `NeedModel`：
    /// 拒绝结果已写入会话，模型下一轮可见，可解释结束或换路径；是否允许
    /// 完成仍由完成门控判定（必需义务被拒时不会虚假成功，ALR-307）。
    Completed,
    /// 中断类命令（取消/关闭/引导）到达：批次已收敛闭合，命令交还执行驱动。
    Interrupted(Deferred),
}

/// 已就绪待执行的单个工具调用（自 phase.rs 迁入，随流水线归属 tools.rs）。
pub(super) struct RunningToolCall {
    pub(super) tool: PreparedToolCall,
    pub(super) started_at: std::time::Instant,
}

/// 工具任务输出。
pub(super) struct ToolTaskOutput {
    pub(super) result: crate::tools::result::ToolResult,
    pub(super) duration_ms: u64,
}

/// 执行一个完整的工具批次（准备 → 并行执行 → 收尾）。
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_tool_batch(
    ctx: &mut TurnContext,
    state: &mut AgentLoopState,
    injections: &mut ToolInjectionBuffer,
    trust_mode: &mut TrustMode,
    plugins: &[Arc<dyn Plugin>],
    stream_tx: &std::sync::mpsc::Sender<StreamEvent>,
    context_limit: usize,
    cmd_rx: &mut tokio_mpsc::UnboundedReceiver<Command>,
    mut batch: ToolBatchState,
) -> ToolBatchOutcome {
    let mut tasks: JoinSet<ToolTaskOutput> = JoinSet::new();
    let mut running: HashMap<TaskId, RunningToolCall> = HashMap::new();
    // 已就绪待启动（受并发上限约束）与已完成待按序提交的结果。
    let mut pending_ready: std::collections::VecDeque<PreparedToolCall> =
        std::collections::VecDeque::new();
    let mut completed_buffer: Vec<(usize, RunningToolCall, ToolTaskOutput)> = Vec::new();
    // 本批工具产物中声明的待注入图片（RFC 0017）：批次闭合时统一落成
    // 宿主注入的 User 消息，保证同批工具结果在消息序列中保持连续。
    let mut pending_images: Vec<super::message::PendingImageInjection> = Vec::new();

    'pipeline: loop {
        // ── 准备：逐个弹出待处理调用（参数校验）──
        while let Some((index, call)) = batch.calls.pop_front() {
            match prepare_tool_call(ctx, &call, &mut state.tool_history) {
                ToolPreflightOutcome::Skip { needs_recovery } => {
                    batch.needs_failure_recovery |= needs_recovery;
                    ctx.session.persist_to_disk();
                }
                ToolPreflightOutcome::Execute { args_summary } => {
                    let tool = PreparedToolCall {
                        index,
                        call,
                        args_summary,
                    };
                    batch.ready_tools.push(tool);
                }
            }
        }

        // ── 执行：有界并行启动（≤MAX_PARALLEL_TOOLS），结果按模型声明顺序提交 ──
        pending_ready.extend(std::mem::take(&mut batch.ready_tools));
        launch_ready_tools(ctx, &mut pending_ready, &mut tasks, &mut running);
        if tasks.is_empty() {
            flush_ordered_results(
                ctx,
                state,
                &mut batch,
                &mut completed_buffer,
                usize::MAX,
                &mut pending_images,
            );
            break 'pipeline;
        }
        let joined = tokio::select! {
            biased;
            command = cmd_rx.recv() => {
                let command = match command {
                    Some(command) => command,
                    None => {
                        flush_ordered_results(
                            ctx,
                            state,
                            &mut batch,
                            &mut completed_buffer,
                            usize::MAX,
                            &mut pending_images,
                        );
                        abort_running_tools(ctx, &mut tasks, &mut running, stream_tx).await;
                        close_pending_calls(ctx, stream_tx, "工具调用因用户发送新消息而中断。");
                        commit_image_injections(ctx, &mut pending_images);
                        return ToolBatchOutcome::Interrupted(Deferred::Closed);
                    }
                };
                if is_interrupting(&command) {
                    flush_ordered_results(
                        ctx,
                        state,
                        &mut batch,
                        &mut completed_buffer,
                        usize::MAX,
                        &mut pending_images,
                    );
                    abort_running_tools(ctx, &mut tasks, &mut running, stream_tx).await;
                    close_pending_calls(ctx, stream_tx, "工具调用因用户发送新消息而中断。");
                    commit_image_injections(ctx, &mut pending_images);
                    return ToolBatchOutcome::Interrupted(Deferred::Command(command));
                }
                handle_ambient_command(
                    ctx,
                    state,
                    injections,
                    trust_mode,
                    plugins,
                    stream_tx,
                    context_limit,
                    command,
                );
                continue;
            }
            joined = tasks.join_next_with_id() => joined,
        };
        let joined = joined.expect("工具任务集合非空时必须返回结果");
        let (task_id, task_output) = match joined {
            Ok((task_id, output)) => (task_id, output),
            Err(error) => {
                let task_id = error.id();
                let running_record = running.get(&task_id).expect("异常工具任务必须存在运行记录");
                let message = format!("工具任务异常结束：{error}");
                (
                    task_id,
                    ToolTaskOutput {
                        result: crate::tools::result::ToolResult {
                            ok: false,
                            summary: message.clone(),
                            stdout: String::new(),
                            stderr: message,
                            exit_code: 1,
                            execution: None,
                        },
                        duration_ms: running_record.started_at.elapsed().as_millis() as u64,
                    },
                )
            }
        };
        let running_record = running
            .remove(&task_id)
            .expect("完成的工具任务必须存在运行记录");
        completed_buffer.push((running_record.tool.index, running_record, task_output));
        // 提交边界：index 小于仍在途（运行或待启动）的最小 index 的结果
        // 按 model 声明顺序落库（ALR-301）。
        let inflight_min = running
            .values()
            .map(|record| record.tool.index)
            .chain(pending_ready.iter().map(|tool| tool.index))
            .min()
            .unwrap_or(usize::MAX);
        flush_ordered_results(
            ctx,
            state,
            &mut batch,
            &mut completed_buffer,
            inflight_min,
            &mut pending_images,
        );
        launch_ready_tools(ctx, &mut pending_ready, &mut tasks, &mut running);
    }

    // ── 收尾：无效调用上下文、失败恢复提示、观测压力记录 ──
    append_invalid_tool_calls_context(ctx, &batch.invalid_tool_calls);
    if batch.needs_failure_recovery {
        let request_tools = ctx.tools.clone();
        append_failure_recovery_prompt(ctx, &state.tool_history, &request_tools);
    } else {
        ctx.session.persist_to_disk();
    }
    // 图片注入消息位于失败恢复提示之前还是之后无协议影响；放在收尾最末，
    // 保证它出现在本批全部工具结果与恢复提示之后、下一次模型请求之前。
    commit_image_injections(ctx, &mut pending_images);
    let observed_tokens = observed_total_tokens(&batch.response_usage);
    state.last_observed_tokens = observed_tokens;
    ctx.session.current_tokens = observed_tokens;
    ToolBatchOutcome::Completed
}

/// 中断类命令：取消/关闭/引导消息——批次收敛闭合后交还执行驱动。
fn is_interrupting(command: &Command) -> bool {
    matches!(
        command,
        Command::Cancel | Command::Shutdown | Command::InjectUserMessage { .. }
    )
}

/// 落地本批图片注入（RFC 0017 安全边界：工具批次闭合之后、下一次模型
/// 请求组装之前）。空批次零开销；落地后立即持久化并向前端同步消息
/// 快照（前端按 `MessagePhase::HostInjected` 归入助手轮次作过程片段展示）。
fn commit_image_injections(
    ctx: &mut TurnContext,
    pending_images: &mut Vec<super::message::PendingImageInjection>,
) {
    if pending_images.is_empty() {
        return;
    }
    super::message::append_host_injected_images(&mut ctx.session, pending_images);
    pending_images.clear();
    ctx.session.persist_to_disk();
    if let Some(mut snapshot) = ctx.session.messages.last().cloned() {
        snapshot.clear_transient_data();
        let _ = ctx.stream_tx.send(StreamEvent::SessionMessageUpsert {
            message: snapshot,
            deferred_tool_injections: None,
        });
    }
}

/// 就地消化非中断命令的运行时副作用（与统一命令处理的 KeepCurrent 类一致）。
#[allow(clippy::too_many_arguments)]
fn handle_ambient_command(
    ctx: &mut TurnContext,
    state: &mut AgentLoopState,
    injections: &mut ToolInjectionBuffer,
    trust_mode: &mut TrustMode,
    plugins: &[Arc<dyn Plugin>],
    stream_tx: &std::sync::mpsc::Sender<StreamEvent>,
    context_limit: usize,
    command: Command,
) {
    match command {
        Command::SetTrustMode(mode) => {
            set_runtime_trust_mode(trust_mode, plugins, mode);
        }
        Command::SetReasoningEffort(effort) => {
            ctx.agent_config.reasoning_effort = effort;
            ctx.session.reasoning_effort = Some(ctx.agent_config.reasoning_effort);
        }
        Command::SetTitle {
            title,
            only_if_default,
        } => {
            if !only_if_default || crate::core::is_default_title(&ctx.session.title) {
                ctx.session.title = title.clone();
                ctx.session.updated_at = tiangong_types::now_text();
                let _ = stream_tx.send(StreamEvent::TitleChanged { title });
            }
        }
        Command::InjectTool { tool_name, payload } => {
            injections.receive(stream_tx, tool_name, payload);
        }
        Command::EmitStreamEvent(event) => {
            let _ = stream_tx.send(*event);
        }
        Command::ReportUsage {
            usage,
            source,
            emit_event,
        } => {
            record_plugin_usage(
                stream_tx,
                context_limit,
                &mut state.accumulated_usage,
                usage,
                source,
                emit_event,
            );
        }
        Command::CompressContext | Command::ResetContext => {
            // Driver 在 turn 结束后的空闲边界执行维护命令。
        }
        Command::Cancel | Command::Shutdown => {}
        Command::InjectUserMessage { .. } => unreachable!("中断类命令已在上方分流"),
    }
}

/// 在并发上限内启动就绪工具。
fn launch_ready_tools(
    ctx: &mut TurnContext,
    pending_ready: &mut std::collections::VecDeque<PreparedToolCall>,
    tasks: &mut JoinSet<ToolTaskOutput>,
    running: &mut HashMap<TaskId, RunningToolCall>,
) {
    while running.len() < MAX_PARALLEL_TOOLS
        && let Some(tool) = pending_ready.pop_front()
    {
        start_tool_execution(ctx, tool, tasks, running);
    }
}

/// 按 model 声明顺序提交已完成结果：提交所有 index < `boundary` 的缓冲项。
/// `pending_images` 收集工具产物中声明的待注入图片，由批次闭合处统一落地。
fn flush_ordered_results(
    ctx: &mut TurnContext,
    state: &mut AgentLoopState,
    batch: &mut ToolBatchState,
    completed_buffer: &mut Vec<(usize, RunningToolCall, ToolTaskOutput)>,
    boundary: usize,
    pending_images: &mut Vec<super::message::PendingImageInjection>,
) {
    completed_buffer.sort_by_key(|(index, _, _)| *index);
    let mut needs_recovery = false;
    while completed_buffer
        .first()
        .is_some_and(|(index, _, _)| *index < boundary)
    {
        let (_, running_record, task_output) = completed_buffer.remove(0);
        needs_recovery |= record_completed_tool_call(
            ctx,
            CompletedToolCall {
                call: &running_record.tool.call,
                args_summary: &running_record.tool.args_summary,
                result: &task_output.result,
                duration_ms: task_output.duration_ms,
            },
            &mut state.tool_history,
            pending_images,
        );
    }
    batch.needs_failure_recovery |= needs_recovery;
}

/// 启动一个已就绪的工具任务（ToolStart 事件 + JoinSet 注册）。
pub(super) fn start_tool_execution(
    ctx: &mut TurnContext,
    tool: PreparedToolCall,
    tasks: &mut JoinSet<ToolTaskOutput>,
    running: &mut HashMap<TaskId, RunningToolCall>,
) {
    let _ = ctx.stream_tx.send(StreamEvent::ToolStart {
        name: tool.call.name.clone(),
        args_summary: tool.args_summary.clone(),
    });
    let actor_id = ctx.session.id.clone();
    let future = start_tool_call(ctx, &tool.call, &actor_id);
    let started_at = std::time::Instant::now();
    let task = tasks.spawn(async move {
        let result = future.await;
        ToolTaskOutput {
            result,
            duration_ms: started_at.elapsed().as_millis() as u64,
        }
    });
    running.insert(task.id(), RunningToolCall { tool, started_at });
}

/// 中断时收敛运行中的工具任务：全部中止并按声明顺序闭合协议。
async fn abort_running_tools(
    ctx: &mut TurnContext,
    tasks: &mut JoinSet<ToolTaskOutput>,
    running: &mut HashMap<TaskId, RunningToolCall>,
    stream_tx: &std::sync::mpsc::Sender<StreamEvent>,
) {
    tasks.shutdown().await;
    let mut interrupted = running.drain().map(|(_, tool)| tool).collect::<Vec<_>>();
    interrupted.sort_by_key(|running_call| running_call.tool.index);
    let mut interrupted_events = Vec::with_capacity(interrupted.len());
    for running_call in interrupted {
        let duration_ms = running_call.started_at.elapsed().as_millis() as u64;
        let output = "工具调用因用户发送新消息而中断。".to_string();
        append_tool_result_message(
            &mut ctx.session,
            &running_call.tool.call.id,
            &running_call.tool.call.name,
            output.clone(),
            true,
        );
        interrupted_events.push(StreamEvent::ToolResult {
            name: running_call.tool.call.name.clone(),
            tool_call_id: Some(running_call.tool.call.id.clone()),
            ok: false,
            output,
            full_output: None,
            duration_ms: Some(duration_ms),
        });
    }
    ctx.session.persist_to_disk();
    for event in interrupted_events {
        let _ = stream_tx.send(event);
    }
}

/// 闭合批次中尚未执行的悬空调用（模型已返回但工具未启动）。
fn close_pending_calls(
    ctx: &mut TurnContext,
    stream_tx: &std::sync::mpsc::Sender<StreamEvent>,
    reason: &str,
) {
    let closed = ctx.session.close_unfinished_tool_calls_with_reason(reason);
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
