//! 统一命令处理（任务 11，自 execute.rs 机械拆分）。
//!
//! 所有命令（含 PendingFinish 阶段收到的）经 [`handle_command`]：处理器不直接
//! 驱动循环控制流，返回 [`CommandEffect`] 由执行驱动统一解释。

use std::sync::Arc;
use tokio::sync::mpsc as tokio_mpsc;

use crate::core::command::Command;
use crate::core::plugin::Plugin;
use crate::permission::TrustMode;
use crate::react::outcome::TurnExecutionResult;
use crate::react::phase::ExecutionPhase;
use crate::turn_context::TurnContext;
use tiangong_types::StreamEvent;

use super::execute::{AgentLoopState, ToolInjectionBuffer, set_runtime_trust_mode};
use super::helpers::record_plugin_usage;
use super::interrupt::interrupt_active_work;

/// 校验并事务性保存运行中注入的用户消息；成功才向界面确认接收，并重置为新用户
/// 意图（ALR-102：重置阶段预算与工具去重记录，保留物理 turn 累计用量）。
/// 调用前须先 interrupt_active_work；成功后由调用方安装 `NeedModel`。
pub(super) fn save_user_message_and_restart(
    ctx: &mut TurnContext,
    state: &mut AgentLoopState,
    stream_tx: &std::sync::mpsc::Sender<StreamEvent>,
    message_id: String,
    content: Vec<tiangong_types::ContentBlock>,
) -> Result<(), String> {
    let content_text = tiangong_types::content_blocks_text(&content);
    let content_blocks = tiangong_types::stable_content_blocks(&content);
    let event_message_id = message_id.clone();
    ctx.session
        .try_append_prepared_user_message_with_id(message_id, content)?;
    let _ = stream_tx.send(StreamEvent::UserMessage {
        message_id: event_message_id,
        content: content_text,
        content_blocks,
        media: Vec::new(),
    });
    tracing::info!(
        session_id = %ctx.session.id,
        "运行中注入用户消息：中断当前执行并追加新消息"
    );
    state.reset_tool_history();
    Ok(())
}

/// Waiting 阶段 select 出或 drain 到的命令载体（`Closed` 表示通道关闭，等同取消）。
pub(super) enum Deferred {
    Command(Command),
    Closed,
}

/// 命令处理效果：处理器不直接驱动循环控制流，统一由驱动解释（任务 07）。
#[allow(clippy::large_enum_variant)]
pub(super) enum CommandEffect {
    /// 副作用已应用，阶段保持（处理器未触碰阶段）。
    KeepCurrent,
    /// 产出新阶段（重启/暂定结果撤销等），由驱动统一安装。
    ToPhase(ExecutionPhase),
    /// 终止本轮（取消/关闭/保存失败）。
    Terminate(TurnExecutionResult),
}

/// 统一命令处理器：所有命令（含 PendingFinish 阶段收到的）都经此入口。
///
/// 命令按到达顺序处理（ALR-203）：决定性取消/关闭立即终止；引导消息中断后从
/// 新意图重启；PendingFinish 收到 InjectTool 撤销暂定结果重新分析（不重置用户
/// 意图预算）。
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_command(
    cmd_rx: &mut tokio_mpsc::UnboundedReceiver<Command>,
    deferred_command: Deferred,
    ctx: &mut TurnContext,
    state: &mut AgentLoopState,
    injections: &mut ToolInjectionBuffer,
    trust_mode: &mut TrustMode,
    plugins: &[Arc<dyn Plugin>],
    stream_tx: &std::sync::mpsc::Sender<StreamEvent>,
    context_limit: usize,
) -> CommandEffect {
    let is_cancel = matches!(
        &deferred_command,
        Deferred::Closed
            | Deferred::Command(Command::Cancel)
            | Deferred::Command(Command::Shutdown)
    );
    if is_cancel {
        cmd_rx.close();
        // 取消路径：Summary 部分输出保持取消语义（不降级）；插件 on_cancel
        // 由 run_turn 在终态判定后调用。
        interrupt_active_work(ctx, state, injections, stream_tx, context_limit, false).await;
        return CommandEffect::Terminate(TurnExecutionResult::cancelled(
            state.accumulated_usage.clone(),
        ));
    }
    match deferred_command {
        // 交互闭合在声明式插件的工具 Future 内完成（注册表唤醒），
        // 命令通道不再承载闭合结果。
        Deferred::Command(Command::InjectUserMessage {
            message_id,
            content,
        }) => {
            // 引导消息：中断主循环直接拥有的活动（Summary 降级 ALR-104），
            // 校验并保存，成功才确认，然后从新意图重启（ALR-101/102）。
            interrupt_active_work(ctx, state, injections, stream_tx, context_limit, true).await;
            match save_user_message_and_restart(ctx, state, stream_tx, message_id, content) {
                Ok(()) => CommandEffect::ToPhase(ExecutionPhase::NeedModel),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        session_id = %ctx.session.id,
                        "运行中注入用户消息保存失败"
                    );
                    CommandEffect::Terminate(TurnExecutionResult::failed(
                        state.accumulated_usage.clone(),
                        format!("用户消息保存失败：{error}"),
                    ))
                }
            }
        }
        Deferred::Command(Command::SetTrustMode(mode)) => {
            // 工具执行中的信任模式变化由流水线就地处理；
            // 其余阶段只更新运行时值，下一次权限判断生效。
            set_runtime_trust_mode(trust_mode, plugins, mode);
            CommandEffect::KeepCurrent
        }
        Deferred::Command(Command::SetReasoningEffort(effort)) => {
            ctx.agent_config.reasoning_effort = effort;
            ctx.session.reasoning_effort = Some(ctx.agent_config.reasoning_effort);
            CommandEffect::KeepCurrent
        }
        Deferred::Command(Command::InjectTool { tool_name, payload }) => {
            injections.receive(stream_tx, tool_name, payload);
            // PendingFinish 收到工具注入：撤销暂定结果、重新分析（不重置用户
            // 意图预算——这是当前任务的新信息，不是新意图）。
            let phase = state.take_phase();
            match phase {
                ExecutionPhase::PendingFinish(_) => {
                    tracing::debug!(
                        session_id = %ctx.session.id,
                        "PendingFinish 收到工具注入：撤销暂定结果并重新分析"
                    );
                    CommandEffect::ToPhase(ExecutionPhase::NeedModel)
                }
                other => CommandEffect::ToPhase(other),
            }
        }
        Deferred::Command(Command::SetTitle {
            title,
            only_if_default,
        }) => {
            if !only_if_default || crate::core::is_default_title(&ctx.session.title) {
                ctx.session.title = title.clone();
                ctx.session.updated_at = tiangong_types::now_text();
                // 通知消费线程转发 sessions_updated（core 层不碰 tauri，走自有 StreamEvent 通道）。
                let _ = stream_tx.send(tiangong_types::StreamEvent::TitleChanged { title });
            }
            // 不立即 persist：turn 结束 run_turn 统一落盘。
            CommandEffect::KeepCurrent
        }
        Deferred::Command(Command::EmitStreamEvent(event)) => {
            let _ = stream_tx.send(*event);
            CommandEffect::KeepCurrent
        }
        Deferred::Command(Command::ReportUsage {
            usage,
            source,
            emit_event,
        }) => {
            record_plugin_usage(
                stream_tx,
                context_limit,
                &mut state.accumulated_usage,
                usage,
                source,
                emit_event,
            );
            // 晚到用量不丢失：累计进 accumulated_usage；若当前处于
            // PendingFinish，提交时统一刷新为最新用量（ALR-111）。
            CommandEffect::KeepCurrent
        }
        Deferred::Command(Command::CompressContext | Command::ResetContext) => {
            // 维护命令由常驻 Driver 在 turn 结束后的空闲边界执行。
            CommandEffect::KeepCurrent
        }
        Deferred::Command(Command::Cancel | Command::Shutdown) => {
            unreachable!("Cancel/Shutdown 已在上方取消分支处理")
        }
        Deferred::Closed => unreachable!("Closed 已在上方取消分支处理"),
    }
}
