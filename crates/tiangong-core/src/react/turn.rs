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
    // 起轮时已知本轮用户消息（`turn_id`）：on_turn_finished 按它定位本轮范围
    // （按 ID 而非位置：运行中压缩会在锚点前插入消息，位置会后移），最终
    // turn_status/elapsed_ms 也写在它上面。
    let stream_tx = ctx.stream_tx.clone();
    let turn_started = std::time::Instant::now();
    let Some(turn_id) = ctx.turn_id.clone() else {
        let event = StreamEvent::Error {
            message: "本轮缺少用户消息".to_string(),
        };
        let _ = stream_tx.send(event.clone());
        return event;
    };
    let elapsed_timer = TurnElapsedTimer::start(turn_started, stream_tx.clone());

    // ── 启动插件生命周期 ──
    for plugin in &ctx.plugins {
        plugin.on_turn_started();
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
        demote_finalized_candidate(
            &mut ctx.session,
            &stream_tx,
            &turn_id,
            finalized_candidate_id.take(),
        );
    }

    // ── 提交轮次状态 ──
    // 测试同步点：Agent Loop 已提交结果，turn 尚未执行最终收尾。
    #[cfg(test)]
    crate::core::test_support::turn_finish_barrier(&ctx.session.id).await;
    // 结果写入本轮起轮的用户消息（turn_id）。
    elapsed_timer.stop().await;
    let elapsed_ms = turn_started.elapsed().as_millis() as u64;
    let status = outcome.status();
    if let Some(message) = current_user_message(&mut ctx) {
        message.set_turn_result(elapsed_ms, status);
    }
    // 成功轮次把最终答复记在起点用户消息上（与终态同次落盘）；失败或已回收
    // 的候选在此之前已被 take，不会写入。
    if matches!(outcome, TurnExecutionOutcome::Success)
        && let Some(candidate_id) = finalized_candidate_id.clone()
        && let Some(message) = current_user_message(&mut ctx)
    {
        message.set_final_reply(Some(candidate_id));
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
            demote_finalized_candidate(
                &mut ctx.session,
                &stream_tx,
                &turn_id,
                finalized_candidate_id.take(),
            );
        }
        if let Some(message) = current_user_message(&mut ctx) {
            message.set_turn_result(elapsed_ms, tiangong_types::TurnStatus::Failed);
        }
        let _ = ctx.session.try_persist_to_disk();
    }

    // 最终答复记在本轮起点用户消息的 `final_reply` 上，随下方终态前的
    // 用户消息快照一并发布；失败路径已在上方回收，不会发布未验证的候选。

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
    // 事件累计，事件链路波动时会缺失。终态前补发含最终 elapsed_ms/turn_status/
    // final_reply 的用户消息快照，回复底部与用户消息旁的「执行总时长」始终有
    // 精确值，最终答复标记也随之送达。
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
                plugin.on_cancel().await;
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
    crate::core::plugin::notify_turn_finished(&ctx.plugins, &ctx.session, &turn_id);
    terminal
}

/// 本轮发起时的用户消息（轮次状态的落点）。
fn current_user_message(ctx: &mut TurnContext) -> Option<&mut Message> {
    let id = ctx.turn_id.as_deref()?;
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

/// 收尾降级为 Failed 时回收本轮已定格的最终答复：清空本轮起点用户消息上
/// 指向该候选的 `final_reply`——失败终态下未验证的候选不得保持最终答复身份
/// （run_turn 收尾晚于 execute 的提交标记，需在此回收）。只在 `final_reply`
/// 仍指向**本轮候选 ID** 时回收：插件在 on_turn_finished 中改写的答复不受影响。
fn demote_finalized_candidate(
    session: &mut crate::session::Session,
    stream_tx: &std::sync::mpsc::Sender<StreamEvent>,
    anchor_id: &str,
    candidate_id: Option<String>,
) {
    let Some(candidate_id) = candidate_id else {
        return;
    };
    let Some(anchor) = session
        .messages
        .iter_mut()
        .find(|message| message.id == anchor_id)
    else {
        return;
    };
    if anchor.final_reply() != Some(candidate_id.as_str()) {
        return;
    }
    anchor.set_final_reply(None);
    let _ = stream_tx.send(StreamEvent::SessionMessageUpsert {
        message: anchor.stable(),
        deferred_tool_injections: None,
    });
}
