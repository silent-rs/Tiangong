//! 单个 turn 的生命周期。
//!
//! [`TurnContext`] 定义在 `crate::turn_context`,是 turn 级能力容器。本文件负责
//! turn 的启动、插件回调、状态提交与最终持久化。

use tokio::sync::mpsc as tokio_mpsc;

use crate::core::command::Command;
use crate::session::{Message, MessageRole};
use crate::turn_context::TurnContext;
use tiangong_types::StreamEvent;

use super::execute::execute_turn;
use super::outcome::TurnExecutionOutcome;
use super::timer::TurnElapsedTimer;

/// 执行并收尾一个完整的 turn task。
///
/// `deliver` 已完成用户消息接收并构建 [`TurnContext`]；本函数依次负责插件生命周期、
/// Agent Loop、消息协议收尾、轮次状态提交和最终持久化。
pub(crate) async fn run_turn(
    mut ctx: TurnContext,
    cmd_rx: &mut tokio_mpsc::UnboundedReceiver<Command>,
) -> StreamEvent {
    // ── 本轮锚点 ──
    // 起轮时已知本轮用户消息（`turn_id`）；最终 turn_status 也写在它上面。
    // 插件生命周期仍按位置定位本轮（turn_start_idx），在 turn 开始时由
    // turn_id 换算并固定（ALR-108：一个物理 turn 只触发一次）。
    let stream_tx = ctx.stream_tx.clone();
    let turn_started = std::time::Instant::now();
    let Some(turn_start_idx) = ctx.turn_id.as_ref().and_then(|turn_id| {
        ctx.session
            .messages
            .iter()
            .position(|message| &message.id == turn_id)
    }) else {
        let event = StreamEvent::Error {
            message: "本轮缺少用户消息".to_string(),
        };
        let _ = stream_tx.send(event.clone());
        return event;
    };
    let elapsed_timer = TurnElapsedTimer::start(turn_started, stream_tx.clone());

    // ── 启动插件生命周期 ──
    // 插件看到的是已包含本轮用户消息的完整 Session。
    for plugin in &ctx.plugins {
        plugin.on_turn_started(&mut ctx.session, turn_start_idx);
    }

    // ── 执行 Agent Loop ──
    // execute_turn 返回明确的执行结果和累计用量，不在内部发送终态事件。
    let execution = execute_turn(&mut ctx, cmd_rx).await;
    let usage = execution.usage;
    let mut outcome = execution.outcome;
    let mut finalized_candidate_id = execution.finalized_candidate_id;
    ctx.session.token_usage.accumulate(&usage);

    // ── 修复消息协议 ──
    // 先为悬空的 tool_call 补齐失败结果，保证 Provider 历史满足
    // Assistant(tool_call) -> Tool(result) 的配对要求。
    let interrupted_tools = ctx
        .session
        .close_unfinished_tool_calls_with_reason("工具调用因本轮结束而中断，未执行。");
    let had_interrupted_tools = !interrupted_tools.is_empty();
    for (tool_call_id, tool_name, output) in interrupted_tools {
        let _ = stream_tx.send(StreamEvent::ToolResult {
            name: tool_name,
            tool_call_id: Some(tool_call_id),
            ok: false,
            output,
            full_output: None,
            duration_ms: None,
        });
    }
    if had_interrupted_tools && matches!(outcome, TurnExecutionOutcome::Success) {
        outcome = TurnExecutionOutcome::Failed("本轮仍有未完成的工具调用，已安全中断".to_string());
        demote_finalized_candidate(&mut ctx.session, &stream_tx, finalized_candidate_id.take());
    }

    // ── 提交轮次状态 ──
    // 测试同步点：Agent Loop 已提交结果，turn 尚未执行最终收尾。
    #[cfg(test)]
    crate::core::test_support::turn_finish_barrier(&ctx.session.id).await;
    // 结果写入本轮当前要回应的用户消息（ALR-107）。
    elapsed_timer.stop().await;
    let elapsed_ms = turn_started.elapsed().as_millis() as u64;
    let status = outcome.status();
    if let Some(message) = current_user_message(&mut ctx) {
        message.set_turn_result(elapsed_ms, status);
    }

    // ── 失败轮次追加用户可见的错误消息 ──
    // Notice 是"系统发给用户的通知"通道，角色本身保证排除出模型上下文与
    // 压缩摘要（context 构建、压缩、provider 转换三处过滤），无需再加
    // model_excluded。前端按 "[错误]" 前缀渲染红色错误框。消息先入 session
    // 随下方最终落盘持久化，终态发布前再补发 upsert 事件，实时会话与重载
    // 会话都能看到失败原因。给模型的失败痕迹由 persist_error 注入的
    // react_loop_error 消息对负责。
    let user_error_snapshot = match &outcome {
        TurnExecutionOutcome::Failed(message) => {
            let error_message = Message::new(MessageRole::Notice, format!("[错误] {message}"));
            ctx.session.messages.push(error_message.clone());
            Some(error_message)
        }
        _ => None,
    };

    // ── 清理运行态并最终持久化（不等待插件收尾）──
    // base64 等瞬态内容只用于本轮模型请求，不能进入磁盘会话合同。
    // 关键路径顺序：落盘 → 快照 → 终态。插件收尾（含 on_cancel）移到终态之后，
    // 任何插件或 Sidecar 阻塞都不得吞掉本轮终态——用户看到结束事件不再取决于
    // 插件收尾速度。
    ctx.session.clear_transient_content();

    if let Err(error) = ctx.session.try_persist_to_disk() {
        // 最终落盘失败必须把本轮降级为 Failed，并带着失败状态再尝试保存一次。
        // 候选回收只在原本成功时执行：原本 Failed/Cancelled 的轮次没有本轮
        // 候选，按 ID 回收不会误伤**上一轮**或插件追加的最终答复。
        let was_success = matches!(outcome, TurnExecutionOutcome::Success);
        outcome = TurnExecutionOutcome::Failed(format!("最终会话持久化失败：{error}"));
        if was_success {
            demote_finalized_candidate(&mut ctx.session, &stream_tx, finalized_candidate_id.take());
        }
        if let Some(message) = current_user_message(&mut ctx) {
            message.set_turn_result(elapsed_ms, tiangong_types::TurnStatus::Failed);
        }
        let _ = ctx.session.try_persist_to_disk();
    }

    // ── 成功终态发布最终答复快照 ──
    // 放在最终落盘之后：落盘失败降级路径已回收相位并发布 React 快照，
    // 此处只剩成功路径——失败终态不得发布 Summary 快照。
    // 按**本轮候选 ID** 查找（与失败回收同一 ID）：插件在 on_turn_finished
    // 追加 Summary 相位消息时，发布的仍是模型候选而非插件消息。
    if matches!(outcome, TurnExecutionOutcome::Success)
        && let Some(message_id) = finalized_candidate_id.as_ref()
        && let Some(message) = ctx
            .session
            .messages
            .iter()
            .find(|message| &message.id == message_id)
        && message.phase == crate::session::MessagePhase::Summary
    {
        let mut snapshot = message.clone();
        snapshot.clear_transient_data();
        let _ = stream_tx.send(StreamEvent::SessionMessageUpsert {
            message: snapshot,
            deferred_tool_injections: None,
        });
    }

    // ── 生成终态 ──
    // 每轮独立终态：turn 收尾完成即发布（连续轮次各自拥有自己的终态事件）。
    // 失败错误消息先于终态发布：前端先插入红框消息，再收到 error 终态更新
    // 轮次状态，避免终态把运行状态归位后错误才姗姗来迟。
    if let Some(message) = user_error_snapshot {
        let _ = stream_tx.send(StreamEvent::SessionMessageUpsert {
            message,
            deferred_tool_injections: None,
        });
    }
    // ── 终态前发布用户消息快照 ──
    // set_turn_result 只更新后端 Session；前端轮次总时长依赖秒级 TurnElapsed
    // 事件累计，事件链路波动时会缺失。终态前补发含最终 elapsed_ms/turn_status
    // 的用户消息快照，回复底部与用户消息旁的「执行总时长」始终有精确值。
    if let Some(message) = current_user_message(&mut ctx) {
        let mut snapshot = message.clone();
        snapshot.clear_transient_data();
        let _ = stream_tx.send(StreamEvent::SessionMessageUpsert {
            message: snapshot,
            deferred_tool_injections: None,
        });
    }
    let terminal = outcome.terminal_event(usage);
    let _ = stream_tx.send(terminal.clone());

    // ── 插件收尾（终态已发布，通知即返回）──
    // on_cancel 处理本轮取消回滚（限时等待，超时丢弃剩余回滚，终态不受影响）；
    // on_turn_finished 是通知型钩子：后台线程投递、不等待完成，收尾成败与产出
    // 由插件自行负责（issue #404），turn 任务与注册表槽位立即释放。
    let session_id = ctx.session.id.clone();
    if status == tiangong_types::TurnStatus::Cancelled {
        let cancelled = tokio::time::timeout(PLUGIN_FINISH_TIMEOUT, async {
            for plugin in &ctx.plugins {
                plugin.on_cancel(&mut ctx.session).await;
            }
        })
        .await;
        if cancelled.is_err() {
            tracing::warn!(
                session_id = %session_id,
                "取消收尾（on_cancel）超时：剩余取消回滚被丢弃，终态不受影响"
            );
        }
    }
    crate::core::plugin::notify_turn_finished(&ctx.plugins, &ctx.session, turn_start_idx);
    terminal
}

/// 本轮当前要回应的用户消息（起轮消息，或运行中注入的最新引导消息）。
fn current_user_message(ctx: &mut TurnContext) -> Option<&mut Message> {
    let id = ctx.user_message_id.as_deref()?;
    ctx.session
        .messages
        .iter_mut()
        .find(|message| message.id == id)
}

/// 取消回滚（on_cancel）的宽限上限：正常回滚应为毫秒级，超时意味着插件或其
/// Sidecar 异常——丢弃剩余回滚以保证 turn 任务可结束、注册表槽位释放。
/// on_turn_finished 为通知型钩子，后台投递不等待，不受此限时约束（issue #404）。
#[cfg(not(test))]
const PLUGIN_FINISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// 测试使用短超时，便于验证超时保护行为。
#[cfg(test)]
const PLUGIN_FINISH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// 收尾降级为 Failed 时回收本轮已定格的最终答复：唯一 Summary 相位的
/// assistant 候选退回 React 过程相位——失败终态下未验证的候选不得保持
/// 最终答复身份（run_turn 收尾晚于 execute 的提交标记，需在此回收）。
/// 按**本轮候选 ID** 精确回收最终答复相位（不使用倒序查找）：插件在
/// on_turn_finished 中追加或修改 Summary 时，回收仍只作用于本轮候选。
fn demote_finalized_candidate(
    session: &mut crate::session::Session,
    stream_tx: &std::sync::mpsc::Sender<StreamEvent>,
    candidate_id: Option<String>,
) {
    let Some(message_id) = candidate_id else {
        return;
    };
    let Some(message) = session
        .messages
        .iter_mut()
        .find(|message| message.id == message_id)
    else {
        return;
    };
    if message.phase != crate::session::MessagePhase::Summary {
        return;
    }
    message.phase = crate::session::MessagePhase::React;
    let _ = stream_tx.send(StreamEvent::SessionMessageUpsert {
        message: session
            .messages
            .iter()
            .find(|message| message.id == message_id)
            .cloned()
            .expect("刚降级的消息必然存在"),
        deferred_tool_injections: None,
    });
}
