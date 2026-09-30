//! ReAct 上下文压缩任务的启动、取消、结果提交和通知。

use anyhow::{Result, bail};
use tokio::sync::mpsc as tokio_mpsc;

use crate::context::compressor::{
    CompressionError, CompressionUpdate, ContextCompressor, is_compressible, mark_compact_boundary,
};
use crate::context::organizer::ContextOrganizer;
use crate::core::command::Command;
use crate::session::{ContentBlock, Message, MessagePhase, MessageRole, Session};
use crate::turn_context::TurnContext;
use tiangong_types::TokenUsage;
use tiangong_types::{StreamEvent, stream::ContextCompressAction};

use super::cancel::abort_and_join;
use super::context::{emit_token_usage, rebuild_system_prompt_for_session};

pub(super) type CompressionResult = std::result::Result<CompressionUpdate, CompressionError>;
type CompressionTask = tokio::task::JoinHandle<CompressionResult>;

/// 压缩种类：三种场景的全部差异都在这里参数化（启动参数、用量归属、通知文案）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompressionKind {
    /// 请求前压力压缩：含当前任务续接，用量计入 turn 累计。
    Auto { observed_tokens: usize },
    /// 上下文溢出强制压缩：从强制分割点开始，用量计入 turn 累计。
    Forced,
    /// 手动压缩：不含当前任务续接，用量计入 session 并显式落盘。
    /// `notice` 为留痕文案——core 自身入口（压缩命令）与外部编排（模型
    /// 切换前、插件交接）各自注入，core 不感知上游业务概念。
    Manual {
        observed_tokens: usize,
        notice: String,
    },
}

/// 压缩留痕内容（不含类别前缀，由 [`Message::notice`] 统一补全）：按
/// 来源区分，用户在历史里能查证「这次压缩为什么发生」。
///
/// Auto/Forced 由 core 内部区分；Manual 的内容由发起方注入（见
/// [`CompressionKind::Manual`]）。Notice 角色本身被排除出模型上下文与
/// 压缩范围（`is_compressible`），文案再详细也不影响 KV cache 前缀。
fn notice_content(kind: &CompressionKind) -> String {
    match kind {
        CompressionKind::Auto { .. } => "上下文接近上限，已自动压缩".to_string(),
        CompressionKind::Forced => "上下文超限，已强制压缩".to_string(),
        CompressionKind::Manual { notice, .. } => notice.clone(),
    }
}

/// 压缩会话的中断原因（`run` 返回；取消类命令已在内部分流）。
pub(crate) enum CompressionInterrupt {
    Command(Command),
    Closed,
}

/// 一次统一的上下文压缩会话：启动 → 等待（命令可中断）→ 提交。
///
/// 三种压缩场景（请求前压力、溢出强制、手动）共用同一份等待与提交逻辑，
/// 差异全部由 [`CompressionKind`] 表达；压缩不再是 Agent 顶层阶段（ALR-303）。
pub(crate) struct ContextCompression {
    task: CompressionTask,
    kind: CompressionKind,
    call_id: String,
    pub(super) cancelled_usage: TokenUsage,
}

impl ContextCompression {
    /// 发起请求前压力压缩。
    pub(super) fn auto(
        ctx: &TurnContext,
        organizer: &ContextOrganizer,
        observed_tokens: usize,
    ) -> Self {
        Self::start(
            ctx,
            organizer,
            observed_tokens,
            CompressionKind::Auto { observed_tokens },
        )
    }

    /// 发起强制压缩（上下文溢出恢复）。
    pub(super) fn forced(ctx: &TurnContext, organizer: &ContextOrganizer) -> Self {
        Self::start(ctx, organizer, 0, CompressionKind::Forced)
    }

    /// 发起手动压缩。
    fn manual(
        ctx: &TurnContext,
        organizer: &ContextOrganizer,
        observed_tokens: usize,
        notice: String,
    ) -> Self {
        Self::start(
            ctx,
            organizer,
            observed_tokens,
            CompressionKind::Manual {
                observed_tokens,
                notice,
            },
        )
    }

    /// 统一启动：三种压缩共用同一分割点（保留最近一次完整交互，折叠更早
    /// 历史），产出只是 session 的一次调整（摘要 + 边界推进），不注入任何
    /// 合成消息——当前任务由 Loop 保留的锚点用户消息承载。
    fn start(
        ctx: &TurnContext,
        organizer: &ContextOrganizer,
        observed_tokens: usize,
        kind: CompressionKind,
    ) -> Self {
        notify_started(ctx);
        Self {
            task: start_task(
                ContextCompressor::new(
                    ctx.session.clone(),
                    ctx.client.clone(),
                    ctx.tools.clone(),
                    ctx.agent_config.reasoning_effort,
                ),
                organizer,
                observed_tokens,
                compression_split_point(&ctx.session),
            ),
            kind,
            call_id: scru128::new().to_string(),
            cancelled_usage: TokenUsage::default(),
        }
    }

    /// 取消压缩任务并通知取消（压缩被命令中断时）。
    pub(crate) async fn cancel(&mut self, ctx: &mut TurnContext) {
        let task = std::mem::replace(&mut self.task, noop_task());
        if let Ok(result) = abort_and_join(task).await {
            let usage = match &result {
                Ok(update) => &update.usage,
                Err(error) => &error.usage,
            };
            super::context::record_call_usage(
                ctx,
                &self.call_id,
                usage,
                "context_summary",
                tiangong_types::TurnStatus::Cancelled,
            );
            self.cancelled_usage = usage.clone();
            if matches!(self.kind, CompressionKind::Manual { .. }) {
                ctx.session.token_usage.accumulate(usage);
            }
            notify_usage(ctx, usage, None, "context_summary_cancelled");
            ctx.session.persist_to_disk();
        }
        notify_result(ctx, ContextCompressAction::Cancelled);
    }

    /// 压缩与命令双路等待：命令优先（biased）；到达时取消压缩并上抛。
    pub(crate) async fn run(
        &mut self,
        ctx: &mut TurnContext,
        cmd_rx: &mut tokio_mpsc::UnboundedReceiver<Command>,
    ) -> std::result::Result<CompressionResult, CompressionInterrupt> {
        tokio::select! {
            biased;
            command = cmd_rx.recv() => {
                match command {
                    Some(command) => {
                        self.cancel(ctx).await;
                        Err(CompressionInterrupt::Command(command))
                    }
                    None => {
                        self.cancel(ctx).await;
                        Err(CompressionInterrupt::Closed)
                    }
                }
            }
            task_result = &mut self.task => Ok(resolve_task_result(task_result)),
        }
    }

    /// 手动压缩的等待语义：只接受引导消息（用户消息，取消压缩并立即
    /// 起新轮）与取消类（Cancel/Shutdown/通道关闭）；标题修改保存到当前
    /// 会话而不中断压缩；其余信号（插件
    /// 可用性广播、工具注入等）不接受——忽略并继续等待压缩收敛。
    pub(crate) async fn run_manual(
        mut self,
        ctx: &mut TurnContext,
        cmd_rx: &mut tokio_mpsc::UnboundedReceiver<Command>,
    ) -> ManualCompressionOutcome {
        loop {
            tokio::select! {
                biased;
                command = cmd_rx.recv() => {
                    match command {
                        Some(command @ (Command::Cancel
                            | Command::Shutdown
                            | Command::InjectUserMessage { .. })) => {
                            self.cancel(ctx).await;
                            return ManualCompressionOutcome::Interrupted(
                                CompressionInterrupt::Command(command),
                            );
                        }
                        Some(Command::SetTitle { title, only_if_default }) => {
                            if !only_if_default || crate::core::is_default_title(&ctx.session.title) {
                                let mut candidate = ctx.session.clone();
                                candidate.title = title.clone();
                                candidate.updated_at = tiangong_types::now_text();
                                match candidate.try_persist_to_disk() {
                                    Ok(()) => {
                                        ctx.session = candidate;
                                        let _ = ctx.stream_tx.send(StreamEvent::TitleChanged { title });
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            session_id = %ctx.session.id,
                                            %error,
                                            "手动压缩期间保存标题失败，保留原标题"
                                        );
                                    }
                                }
                            }
                        }
                        Some(_) => continue,
                        None => {
                            self.cancel(ctx).await;
                            return ManualCompressionOutcome::Interrupted(
                                CompressionInterrupt::Closed,
                            );
                        }
                    }
                }
                task_result = &mut self.task => {
                    let result = resolve_task_result(task_result);
                    return self.complete(ctx, result, None);
                }
            }
        }
    }

    /// 提交压缩结果：应用摘要、按种类累计用量并通知（ALR-307）。
    ///
    /// `result` 来自 `run` 的完成返回。Auto/Forced 的用量计入 `turn_usage`
    /// （turn 累计）；Manual 计入 session 并显式落盘。
    ///
    /// 返回值只对 Manual 有意义（供等待方判断终态）；Auto/Forced 返回
    /// `Noop` 占位，调用方忽略即可。
    pub(crate) fn complete(
        self,
        ctx: &mut TurnContext,
        result: CompressionResult,
        turn_usage: Option<&mut TokenUsage>,
    ) -> ManualCompressionOutcome {
        let Self {
            kind,
            task,
            call_id,
            ..
        } = self;
        drop(task);
        let (usage, status) = match &result {
            Ok(update) => (&update.usage, tiangong_types::TurnStatus::Success),
            Err(error) => (&error.usage, tiangong_types::TurnStatus::Failed),
        };
        super::context::record_call_usage(ctx, &call_id, usage, "context_summary", status);
        let notice = notice_content(&kind);
        match kind {
            CompressionKind::Forced => {
                let observed_tokens = ctx.context_limit;
                complete_with_turn_usage(
                    ctx,
                    turn_usage.expect("Forced 压缩必须提供 turn 用量"),
                    observed_tokens,
                    result,
                    &notice,
                );
                ManualCompressionOutcome::Noop
            }
            CompressionKind::Auto { observed_tokens } => {
                complete_with_turn_usage(
                    ctx,
                    turn_usage.expect("Auto 压缩必须提供 turn 用量"),
                    observed_tokens,
                    result,
                    &notice,
                );
                ManualCompressionOutcome::Noop
            }
            CompressionKind::Manual { .. } => complete_manual(ctx, result, &notice),
        }
    }
}

/// 取消后的占位任务（run 已返回，占位仅为保持结构可丢弃，永不完成）。
fn noop_task() -> CompressionTask {
    tokio::spawn(async { std::future::pending::<CompressionResult>().await })
}

/// Auto/Forced 的统一提交：应用、累计 turn 用量并通知。
fn complete_with_turn_usage(
    ctx: &mut TurnContext,
    turn_usage: &mut TokenUsage,
    observed_tokens: usize,
    result: CompressionResult,
    notice: &str,
) {
    match result {
        Ok(update) => {
            turn_usage.accumulate(&update.usage);
            match apply_compression(ctx, &update, false, notice) {
                Ok(current_tokens) => {
                    notify_auto_success(ctx, &update, current_tokens, observed_tokens);
                }
                Err(error) => {
                    notify_auto_failure(ctx, &update.usage, &error);
                }
            }
        }
        Err(error) => {
            turn_usage.accumulate(&error.usage);
            notify_auto_failure(ctx, &error.usage, &error);
        }
    }
}

/// 手动压缩的终态结果。
///
/// 压缩任务无论走到哪个分支都会产出一个终态，供等待方（模型切换编排）
/// 结束等待；`Interrupted` 同时携带需要接续处理的中断原因。
pub(crate) enum ManualCompressionOutcome {
    /// 压缩已成功应用（摘要边界推进）。
    Succeeded,
    /// 无可压缩历史，未调用模型。
    Noop,
    /// 压缩执行失败（模型调用或应用摘要失败）。
    Failed(String),
    /// 压缩被命令中断（取消 / 关闭 / 引导消息）。
    Interrupted(CompressionInterrupt),
}

/// 在独立 turn task 中执行手动压缩。
///
/// 只接受引导消息与取消类命令：引导消息取消压缩并立即起新轮；其余
/// 信号在压缩状态下不接受（忽略），压缩照常收敛。
pub(crate) async fn run_manual_context_compression(
    mut ctx: TurnContext,
    cmd_rx: &mut tokio_mpsc::UnboundedReceiver<Command>,
    notice: String,
) -> ManualCompressionOutcome {
    let observed_tokens = ctx.session.current_tokens;
    let organizer = ContextOrganizer::new(ctx.context_limit);

    let compressor = ContextCompressor::new(
        ctx.session.clone(),
        ctx.client.clone(),
        ctx.tools.clone(),
        ctx.agent_config.reasoning_effort,
    );
    if !compressor.has_pending_messages() {
        notify_result(&ctx, ContextCompressAction::Noop);
        return ManualCompressionOutcome::Noop;
    }
    // 边界之后虽有消息，但全部属于「最近一次完整交互」时没有可折叠的历史。
    // 这不是错误：手动整理与模型切换前的整理都可能落在刚压缩过的会话上，
    // 此时保持现状即为正确结果。自动压缩另走 start_task 的错误分支——那里
    // 上下文确实已超限却压不动，必须如实报错。
    if compression_split_point(&ctx.session).is_none() {
        notify_result(&ctx, ContextCompressAction::Noop);
        return ManualCompressionOutcome::Noop;
    }
    let compression = ContextCompression::manual(&ctx, &organizer, observed_tokens, notice);
    compression.run_manual(&mut ctx, cmd_rx).await
}

pub(crate) fn notify_cleared(stream_tx: &std::sync::mpsc::Sender<StreamEvent>, session: &Session) {
    notify_session_result(stream_tx, session, ContextCompressAction::Clear);
}

pub(super) fn observed_total_tokens(usage: &TokenUsage) -> usize {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.prompt_tokens.saturating_add(usage.completion_tokens)
    }
}

/// 压缩分割点：保留最近一次完整交互（最新可见消息及其所属工具批次），
/// 之前的可压缩历史（含锚点用户消息本身）折叠为摘要。
///
/// 锚点被折叠时由 `apply_compression` 注入 LLM 可见、用户不可见的锚点消息
///（用户最近请求原文），满足 Provider「首条非 System 消息必须是 User」约束。
fn compression_split_point(session: &Session) -> Option<usize> {
    let start = session.summary_up_to.min(session.messages.len());
    let last_visible = (start..session.messages.len()).rev().find(|&index| {
        let message = &session.messages[index];
        message.role != MessageRole::System && message.role != MessageRole::Notice
    })?;
    let last_message = &session.messages[last_visible];
    let recent_start = if last_message.role == MessageRole::Tool {
        last_message
            .tool_call_id
            .as_deref()
            .and_then(|tool_call_id| {
                (start..last_visible).rev().find(|&index| {
                    let message = &session.messages[index];
                    message.role == MessageRole::Assistant
                        && message
                            .tool_calls
                            .iter()
                            .any(|call| call.id == tool_call_id)
                })
            })
            .unwrap_or(last_visible)
    } else {
        last_visible
    };

    session.messages[start..recent_start]
        .iter()
        .any(is_compressible)
        .then_some(recent_start)
}

/// 构造 LLM 可见、用户不可见的锚点消息：`User` 角色（满足 Provider 首条
/// 约束）+ `CompressedResume` 阶段（前端不展示/搜索/编辑）+ `ModelInstruction`
/// 内容块（进入模型请求，不属于用户可见文本）。内容是被折叠锚点的原文，
/// 由程序直接复制，不经过 LLM 提取。
fn build_anchor_resume_message(anchor_text: &str) -> Message {
    let mut message = Message::new(MessageRole::User, "");
    message.content = vec![ContentBlock::model_instruction(format!(
        "用户当前请求（压缩后原文保留）：\n\n{anchor_text}"
    ))];
    message.phase = MessagePhase::CompressedResume;
    message
}

/// 被折叠区间内最后一条可见用户消息的文本（锚点原文）。
///
/// 宿主注入消息（图片注入/压缩恢复锚点）role=User 但不是真实用户输入，
/// 其 text_content 为空，必须排除——否则折叠区间末尾恰是注入消息时
/// 锚点取到空文本被过滤，用户原始请求丢失（评审问题 3）。
fn folded_anchor_text(session: &Session, folded_end: usize) -> Option<String> {
    let start = session.summary_up_to.min(folded_end);
    (start..folded_end)
        .rev()
        .find(|index| {
            let message = &session.messages[*index];
            message.role == MessageRole::User && message.phase.is_user_input()
        })
        .map(|index| session.messages[index].text_content())
        .filter(|text| !text.trim().is_empty())
}

fn start_task(
    compressor: ContextCompressor,
    organizer: &ContextOrganizer,
    observed_tokens: usize,
    split_point: Option<usize>,
) -> CompressionTask {
    let output_budget = organizer.compression_output_budget(observed_tokens);
    tokio::spawn(async move {
        let split_point = split_point.ok_or_else(|| {
            CompressionError::new("上下文已超限，但没有可与最近交互分离的较早历史，无法压缩")
        })?;
        let output_budget = output_budget.ok_or_else(|| {
            CompressionError::new("上下文剩余空间不足 2048 tokens，无法生成有效摘要")
        })?;
        compressor.compress(split_point, output_budget).await
    })
}

fn resolve_task_result(
    result: std::result::Result<CompressionResult, tokio::task::JoinError>,
) -> CompressionResult {
    result.unwrap_or_else(|error| Err(CompressionError::new(error.to_string())))
}

/// 手动压缩提交：返回终态供等待方判断（成功 / 失败原因）。
fn complete_manual(
    ctx: &mut TurnContext,
    result: CompressionResult,
    notice: &str,
) -> ManualCompressionOutcome {
    match result {
        Ok(update) => match apply_compression(ctx, &update, true, notice) {
            Ok(current_tokens) => {
                notify_manual_success(ctx, &update, current_tokens);
                ManualCompressionOutcome::Succeeded
            }
            Err(error) => {
                ctx.session.token_usage.accumulate(&update.usage);
                ctx.session.persist_to_disk();
                let reason = error.to_string();
                notify_manual_failure(ctx, &update.usage, &error);
                ManualCompressionOutcome::Failed(reason)
            }
        },
        Err(error) => {
            ctx.session.token_usage.accumulate(&error.usage);
            ctx.session.persist_to_disk();
            let reason = error.to_string();
            notify_manual_failure(ctx, &error.usage, &error);
            ManualCompressionOutcome::Failed(reason)
        }
    }
}

fn apply_compression(
    ctx: &mut TurnContext,
    update: &CompressionUpdate,
    account_usage_in_session: bool,
    notice: &str,
) -> Result<usize> {
    if ctx.session.summary_up_to != update.previous_summary_up_to {
        bail!(
            "压缩期间摘要边界已变化：expected={}, actual={}",
            update.previous_summary_up_to,
            ctx.session.summary_up_to
        );
    }
    let Some(boundary) = update
        .summary_up_to
        .checked_sub(1)
        .and_then(|index| ctx.session.messages.get(index))
    else {
        bail!("压缩结果边界无效：{}", update.summary_up_to);
    };
    if boundary.id != update.boundary_message_id {
        bail!("压缩期间消息边界已变化，拒绝提交过期结果");
    }

    let mut candidate = ctx.session.clone();
    // 保留区不以 User 开头（锚点被折叠）时，注入 LLM 可见、用户不可见的
    // 锚点消息承载用户最近请求原文（程序复制，非 LLM 提取）。
    let needs_anchor = candidate
        .messages
        .get(update.summary_up_to)
        .is_none_or(|message| message.role != MessageRole::User);
    if needs_anchor
        && let Some(anchor_text) = folded_anchor_text(&ctx.session, update.summary_up_to)
    {
        candidate.messages.insert(
            update.summary_up_to,
            build_anchor_resume_message(&anchor_text),
        );
    }
    candidate.context_summary = Some(update.summary.clone());
    candidate.summary_up_to = update.summary_up_to;
    mark_compact_boundary(&mut candidate.messages, candidate.summary_up_to);
    if account_usage_in_session {
        candidate.token_usage.accumulate(&update.usage);
        candidate.active_agent_current_tokens = 0;
        candidate.agent_current_tokens.clear();
    }
    let current_tokens = update.usage.completion_tokens;
    candidate.current_tokens = current_tokens;
    // 用户可见的压缩记录：历史被折叠后仍能从消息列表查证发生过压缩
    // （#245 app-state 收窄时该消息随事件消费代码一起被误删，前端的
    // [上下文管理] 渲染分支一直空挂着）。
    //
    // 插在**摘要边界处**而不是消息末尾：边界之后可能已经有属于上一轮的
    // 助手回复（手动整理与切换前整理都发生在轮次之间），追加到末尾会让
    // 前端按轮次归组时把标记算进上一轮，显示在该轮的工具调用堆里。
    //
    // 用 Notice 而非 System：Notice 是「系统发给用户的通知」通道，角色
    // 本身保证排除出模型上下文与后续压缩范围；System 会被
    // build_provider_messages 拼进 system prompt，污染 prompt 并破坏
    // KV cache 前缀。
    let record_id = {
        let record = Message::notice(crate::core::CONTEXT_NOTICE_CATEGORY, notice);
        let id = record.id.clone();
        let at = candidate.summary_up_to.min(candidate.messages.len());
        candidate.messages.insert(at, record);
        id
    };
    // 摘要段变化需要重建 system prompt（内容一致时保留原消息）。
    rebuild_system_prompt_for_session(&mut candidate, &ctx.plugins);
    candidate
        .try_persist_to_disk()
        .map_err(anyhow::Error::msg)?;
    ctx.session = candidate;
    // 当轮即推送到前端，无需等切换会话重新读盘才显示。
    crate::react::message::emit_session_message_upsert(ctx, &record_id);
    Ok(current_tokens)
}

fn notify_started(ctx: &TurnContext) {
    let _ = ctx.stream_tx.send(StreamEvent::ContextCompressing {
        summary_up_to: ctx.session.summary_up_to,
        total_messages: ctx.session.messages.len(),
    });
}

fn notify_auto_success(
    ctx: &TurnContext,
    update: &CompressionUpdate,
    current_tokens: usize,
    observed_tokens: usize,
) {
    notify_result(ctx, ContextCompressAction::Auto);
    notify_usage(ctx, &update.usage, Some(current_tokens), "context_summary");
    tracing::info!(
        session_id = %ctx.session.id,
        observed_tokens,
        old_summary_up_to = update.previous_summary_up_to,
        summary_up_to = ctx.session.summary_up_to,
        total_messages = update.summary_up_to,
        "上下文摘要已更新"
    );
}

fn notify_auto_failure(ctx: &TurnContext, usage: &TokenUsage, error: &dyn std::fmt::Display) {
    notify_usage(ctx, usage, None, "context_summary_failed");
    tracing::warn!(
        session_id = %ctx.session.id,
        error = %error,
        "上下文压缩失败，保留原始上下文"
    );
    notify_result(ctx, ContextCompressAction::Failed);
}

fn notify_manual_success(ctx: &TurnContext, update: &CompressionUpdate, current_tokens: usize) {
    notify_usage(
        ctx,
        &update.usage,
        Some(current_tokens),
        "manual_context_compress",
    );
    notify_result(ctx, ContextCompressAction::Compress);
    tracing::info!(
        session_id = %ctx.session.id,
        summary_up_to = ctx.session.summary_up_to,
        "手动上下文摘要已更新"
    );
}

fn notify_manual_failure(ctx: &TurnContext, usage: &TokenUsage, error: &dyn std::fmt::Display) {
    notify_usage(ctx, usage, None, "manual_context_compress_failed");
    tracing::warn!(
        session_id = %ctx.session.id,
        error = %error,
        "手动上下文压缩失败，继续使用原始上下文"
    );
    let _ = ctx.stream_tx.send(StreamEvent::AgentNotification {
        agent_id: "system".to_string(),
        agent_label: "系统".to_string(),
        content: format!("手动压缩上下文失败：{error}"),
        level: "error".to_string(),
    });
    notify_result(ctx, ContextCompressAction::Failed);
}

fn notify_usage(
    ctx: &TurnContext,
    usage: &TokenUsage,
    current_tokens: Option<usize>,
    source: &'static str,
) {
    emit_token_usage(
        &ctx.stream_tx,
        usage,
        current_tokens,
        ctx.context_limit,
        source,
        None,
    );
}

fn notify_result(ctx: &TurnContext, action: ContextCompressAction) {
    notify_session_result(&ctx.stream_tx, &ctx.session, action);
}

fn notify_session_result(
    stream_tx: &std::sync::mpsc::Sender<StreamEvent>,
    session: &Session,
    action: ContextCompressAction,
) {
    let _ = stream_tx.send(StreamEvent::ContextCompressed {
        action,
        summary_up_to: session.summary_up_to,
        remaining_messages: session.messages.len().saturating_sub(session.summary_up_to),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::agent::AgentConfig;
    use crate::observe::Observer;
    use crate::permission::TrustMode;
    use crate::session::Message;
    use crate::session::MessagePhase;
    use crate::session::MessageToolCall;
    use tiangong_llm::SingleProviderClient;
    use tiangong_llm::{ModelEndpoint, ProviderProtocol};

    /// 评审问题 3 回归：折叠区间末尾是宿主注入消息（role=User 但无
    /// 真实输入文本）时，锚点必须越过它取到更早的真实用户消息，
    /// 而不是取到空文本后放弃导致用户原请求丢失。
    #[test]
    fn folded_anchor_text_skips_host_injected_and_finds_real_user_message() {
        let mut session = Session::new("anchor-test");
        session
            .messages
            .push(Message::new(MessageRole::User, "帮我总结群聊"));
        session
            .messages
            .push(Message::new(MessageRole::Assistant, "收到"));
        // 宿主注入消息：role=User、phase=HostInjected、无普通文本。
        let mut injected = Message::new(MessageRole::User, "");
        injected.content = vec![tiangong_types::ContentBlock::model_instruction(
            "[injected-images provenance]\n- 工具 desktop_screenshot 产出图片",
        )];
        injected.phase = MessagePhase::HostInjected;
        session.messages.push(injected);

        let anchor = folded_anchor_text(&session, session.messages.len());
        assert_eq!(
            anchor.as_deref(),
            Some("帮我总结群聊"),
            "锚点必须越过注入消息取到真实用户请求"
        );
    }

    fn test_context(mut session: Session) -> (TurnContext, tempfile::TempDir) {
        let root = tempfile::tempdir().expect("创建临时目录失败");
        session.bind_storage_root(root.path());
        let (stream_tx, _) = std::sync::mpsc::channel();
        let client = SingleProviderClient::new(ModelEndpoint {
            headers: Default::default(),
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "test-key".to_string(),
            model: "test-model".to_string(),
            protocol: ProviderProtocol::OpenAiChatCompletions,
            timeout_ms: 1_000,
            options: serde_json::Value::Object(serde_json::Map::new()),
            context_window: None,
        });
        let ctx = TurnContext::builder()
            .client(client)
            .session(session)
            .stream_tx(stream_tx)
            .plugins(Vec::new())
            .context_limit(200_000)
            .agent_config(AgentConfig::default())
            .trust_mode(TrustMode::FullTrust)
            .observer(Observer::new(std::env::temp_dir()))
            .tool_overrides(Default::default())
            .tools(Vec::new())
            .build();
        (ctx, root)
    }

    fn update_for(
        session: &Session,
        previous_summary_up_to: usize,
        summary: &str,
        summary_up_to: usize,
    ) -> CompressionUpdate {
        CompressionUpdate {
            summary: summary.to_string(),
            usage: TokenUsage::default(),
            previous_summary_up_to,
            summary_up_to,
            boundary_message_id: session.messages[summary_up_to - 1].id.clone(),
        }
    }

    #[test]
    fn observed_tokens_prefers_provider_total() {
        let usage = TokenUsage {
            prompt_tokens: 900,
            completion_tokens: 120,
            total_tokens: 1100,
            prompt_cache_hit_tokens: None,
            prompt_cache_miss_tokens: None,
        };

        assert_eq!(observed_total_tokens(&usage), 1100);
    }

    #[test]
    fn observed_tokens_fall_back_to_prompt_plus_completion() {
        let usage = TokenUsage {
            prompt_tokens: 900,
            completion_tokens: 120,
            total_tokens: 0,
            prompt_cache_hit_tokens: None,
            prompt_cache_miss_tokens: None,
        };

        assert_eq!(observed_total_tokens(&usage), 1020);
    }

    #[test]
    fn split_point_keeps_the_latest_complete_tool_batch() {
        let mut session = Session::new("test");
        session.append_message(MessageRole::User, "较早问题");
        session.append_message(MessageRole::Assistant, "较早回答");
        session.append_message(MessageRole::User, "锚点问题");

        let mut assistant = Message::new(MessageRole::Assistant, "");
        assistant.tool_calls = vec![MessageToolCall {
            id: "latest-call".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "latest.txt"}),
        }];
        session.messages.push(assistant);
        session.messages.push(Message::tool_result(
            "latest-call",
            "read_file",
            "最近工具结果",
            false,
        ));
        let hidden = Message::new(MessageRole::System, "仅前端可见状态");
        session.messages.push(hidden);

        // 锚点用户消息允许被折叠（由锚点消息机制续接）：保留最近工具批次。
        assert_eq!(compression_split_point(&session), Some(3));
    }

    #[test]
    fn split_point_requires_history_before_the_latest_interaction() {
        let mut session = Session::new("test");
        session.append_message(MessageRole::User, "当前问题");
        assert_eq!(compression_split_point(&session), None);

        session
            .messages
            .insert(0, Message::new(MessageRole::User, "较早问题"));
        assert_eq!(compression_split_point(&session), Some(1));
    }

    /// 压缩只是 session 的一次调整：摘要推进边界，不注入任何合成消息；
    /// 最近交互保留在模型上下文中（当前任务由 Loop 的锚点用户消息承载）。
    #[test]
    fn compression_folds_history_and_keeps_recent_interaction_in_context() {
        let mut session = Session::new("test");
        for round in ["第一轮", "第二轮", "第三轮"] {
            session.append_message(MessageRole::User, format!("{round}问题"));
            session.append_message(MessageRole::Assistant, format!("{round}回答"));
        }
        let (mut ctx, root) = test_context(session);

        // 模拟压缩分割点：保留第三轮（最近交互），折叠前两轮。
        let update = update_for(&ctx.session, 0, "前两轮摘要", 4);
        apply_compression(&mut ctx, &update, false, "测试压缩留痕").expect("压缩应成功");

        assert_eq!(ctx.session.context_summary.as_deref(), Some("前两轮摘要"));
        assert_eq!(ctx.session.summary_up_to, 4);
        let saved_prompt = serde_json::to_vec(&ctx.session.system_prompt_message).unwrap();
        let mut restored = Session::load_from_storage(root.path(), &ctx.session.id).unwrap();
        assert_eq!(restored.context_summary, ctx.session.context_summary);
        assert_eq!(restored.summary_up_to, ctx.session.summary_up_to);
        for question in ["继续", "再核对一次"] {
            restored.append_message(MessageRole::User, question);
            // 同一组插件段落重建应得到相同 prompt。
            rebuild_system_prompt_for_session(&mut restored, &ctx.plugins);
            assert_eq!(
                serde_json::to_vec(&restored.system_prompt_message).unwrap(),
                saved_prompt
            );
        }
        let context = ctx.session.context();
        assert_eq!(context[0].role, MessageRole::System);
        assert_eq!(context[1].text_content(), "第三轮问题");
        assert_eq!(context[2].text_content(), "第三轮回答");
        assert!(
            ctx.session
                .messages
                .iter()
                .all(|message| message.phase != MessagePhase::CompressedResume),
            "压缩不注入合成续接消息"
        );
    }

    /// 压缩记录必须落在摘要边界处，而不是消息末尾。
    ///
    /// 回归：早期实现用 `push` 追加到末尾。边界之后往往已有属于上一轮的
    /// 助手回复（手动整理与切换前整理都发生在轮次之间），记录因此被前端
    /// 按轮次归组时算进上一轮，显示在该轮的工具调用堆里而非压缩发生处。
    #[test]
    fn 压缩记录插入摘要边界而非末尾() {
        let mut session = Session::new("记录位置");
        for round in ["第一轮", "第二轮", "第三轮"] {
            session.append_message(MessageRole::User, format!("{round}问题"));
            session.append_message(MessageRole::Assistant, format!("{round}回答"));
        }
        let (mut ctx, _root) = test_context(session);

        // 折叠前两轮，边界落在 index 4（第三轮问题之前）。
        let update = update_for(&ctx.session, 0, "前两轮摘要", 4);
        apply_compression(&mut ctx, &update, false, "测试压缩留痕").expect("压缩应成功");

        let record_index = ctx
            .session
            .messages
            .iter()
            .position(|message| message.text_content().starts_with("[上下文管理]"))
            .expect("应写入压缩记录");
        assert_eq!(
            record_index, 4,
            "记录应插在摘要边界处，实际落在 {record_index}"
        );
        assert_ne!(
            record_index,
            ctx.session.messages.len() - 1,
            "记录不得追加到消息末尾"
        );
        assert_eq!(
            ctx.session.messages[record_index].role,
            MessageRole::Notice,
            "记录用 Notice 角色，保证排除出模型上下文"
        );

        // Notice 不进模型上下文：保留区首条仍是第三轮问题。
        let context = ctx.session.context();
        assert_eq!(context[1].text_content(), "第三轮问题");
        assert!(
            !context
                .iter()
                .any(|message| message.text_content().starts_with("[上下文管理]")),
            "压缩记录不得进入模型上下文"
        );
    }

    /// 压缩不改动插件声明：tools/prompt 由每轮实时收集维护（进程缓存
    /// 兜底），压缩只推进摘要边界与重建 system prompt；落盘失败整体回滚。
    #[test]
    fn compression_keeps_current_declarations_and_survives_persist_failure() {
        use crate::core::plugin::Plugin;
        use crate::tools::extension::{
            PromptSectionProvider, ToolOverrideHandler, ToolSpecProvider,
        };

        struct ChangingPlugin {
            fail: bool,
        }
        impl Plugin for ChangingPlugin {
            fn id(&self) -> &str {
                "changing"
            }
        }
        impl ToolOverrideHandler for ChangingPlugin {}
        impl ToolSpecProvider for ChangingPlugin {
            fn try_tool_specs(
                &self,
            ) -> std::result::Result<Vec<tiangong_llm::tool::ToolSpec>, String> {
                if self.fail {
                    return Err("offline".into());
                }
                Ok(vec![tiangong_llm::tool::ToolSpec {
                    name: "new_tool".into(),
                    description: "new".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                }])
            }
        }
        impl PromptSectionProvider for ChangingPlugin {
            fn prompt_sections(&self) -> Vec<String> {
                vec!["new prompt".into()]
            }
        }

        for (unreadable, fail_save) in [(false, false), (true, false), (false, true)] {
            let mut session = Session::new("压缩不改声明");
            session.append_message(MessageRole::User, "历史问题");
            session.append_message(MessageRole::Assistant, "历史回答");
            session.append_message(MessageRole::User, "当前问题");
            let (mut ctx, root) = test_context(session);
            ctx.plugins = vec![std::sync::Arc::new(ChangingPlugin { fail: unreadable })];
            // 模拟 turn 启动时的 system prompt 重拼（就地收集插件段落）。
            rebuild_system_prompt_for_session(&mut ctx.session, &ctx.plugins);
            let turn_tools = serde_json::to_value(&ctx.tools).unwrap();
            ctx.session.try_persist_to_disk().unwrap();
            let old_session = serde_json::to_value(&ctx.session).unwrap();
            let old_tools = serde_json::to_value(&ctx.tools).unwrap();
            if fail_save {
                crate::core::test_support::fail_next_persistence_for_session(&ctx.session.id);
            }
            let update = update_for(&ctx.session, 0, "新的摘要", 2);
            let result = apply_compression(&mut ctx, &update, false, "测试压缩留痕");
            let restored = Session::load_from_storage(root.path(), &ctx.session.id).unwrap();
            if fail_save {
                assert!(result.is_err());
                assert_eq!(serde_json::to_value(&ctx.session).unwrap(), old_session);
                assert_eq!(serde_json::to_value(&restored).unwrap(), old_session);
                assert_eq!(serde_json::to_value(&ctx.tools).unwrap(), old_tools);
            } else {
                result.unwrap();
                assert_eq!(restored.context_summary.as_deref(), Some("新的摘要"));
                // 压缩不改动 tools：保持 turn 开始的值。
                assert_eq!(serde_json::to_value(&ctx.tools).unwrap(), turn_tools);
                assert_eq!(
                    serde_json::to_value(restored.system_prompt_message).unwrap(),
                    serde_json::to_value(&ctx.session.system_prompt_message).unwrap()
                );
            }
        }
    }

    /// 压缩留痕内容按来源区分（不含类别前缀，形态由 Message::notice 统一）。
    #[test]
    fn 压缩留痕文案按来源区分() {
        assert_eq!(
            notice_content(&CompressionKind::Auto { observed_tokens: 1 }),
            "上下文接近上限，已自动压缩"
        );
        assert_eq!(
            notice_content(&CompressionKind::Forced),
            "上下文超限，已强制压缩"
        );
        assert_eq!(
            notice_content(&CompressionKind::Manual {
                observed_tokens: 0,
                notice: "切换模型前已整理上下文".to_string(),
            }),
            "切换模型前已整理上下文"
        );
    }

    /// 手动压缩保存标题但不中断任务；忽略工具注入，取消类立即终止。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manual_compression_ignores_non_steering_commands_and_finishes() {
        let mut session = Session::new("manual-cmd");
        for text in ["旧问题", "新问题"] {
            session.append_message(MessageRole::User, text);
        }
        let (mut ctx, root) = test_context(session);
        let session_id = ctx.session.id.clone();
        let organizer = ContextOrganizer::new(ctx.context_limit);
        let compression = ContextCompression::manual(
            &ctx,
            &organizer,
            ctx.session.current_tokens,
            "测试整理".to_string(),
        );
        let (cmd_tx, mut cmd_rx) = tokio_mpsc::unbounded_channel();
        // 压缩请求指向不可达端点，约 1 秒超时后任务收敛（失败也视为一次
        // 完整收敛闭合）；期间到达的广播一律不接受。
        cmd_tx
            .send(Command::InjectTool {
                tool_name: "plugin_availability".into(),
                payload: serde_json::json!({}),
            })
            .unwrap();
        cmd_tx
            .send(Command::SetTitle {
                title: "压缩中改标题".into(),
                only_if_default: false,
            })
            .unwrap();
        let handle =
            tokio::spawn(async move { compression.run_manual(&mut ctx, &mut cmd_rx).await });
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("等待 run_manual 超时")
            .unwrap();
        assert!(
            !matches!(outcome, ManualCompressionOutcome::Interrupted(_)),
            "非引导/取消命令不得中断手动压缩"
        );
        let restored = Session::load_from_storage(root.path(), &session_id).unwrap();
        assert_eq!(restored.title, "压缩中改标题");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manual_compression_only_reports_persisted_title_changes() {
        for fail_save in [false, true] {
            for cancel in [false, true] {
                let mut session = Session::new("原标题");
                for text in ["旧问题", "新问题"] {
                    session.append_message(MessageRole::User, text);
                }
                let (mut ctx, root) = test_context(session);
                ctx.session.try_persist_to_disk().unwrap();
                let (stream_tx, stream_rx) = std::sync::mpsc::channel();
                ctx.stream_tx = stream_tx;
                let organizer = ContextOrganizer::new(ctx.context_limit);
                let compression = ContextCompression::manual(
                    &ctx,
                    &organizer,
                    ctx.session.current_tokens,
                    "测试整理".to_string(),
                );
                let (cmd_tx, mut cmd_rx) = tokio_mpsc::unbounded_channel();
                cmd_tx
                    .send(Command::SetTitle {
                        title: "新标题".into(),
                        only_if_default: false,
                    })
                    .unwrap();
                if fail_save {
                    crate::core::test_support::fail_next_persistence_for_session(&ctx.session.id);
                }
                if cancel {
                    cmd_tx.send(Command::Cancel).unwrap();
                }
                let interrupt = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    compression.run_manual(&mut ctx, &mut cmd_rx),
                )
                .await
                .expect("手动压缩应及时结束");
                if cancel {
                    assert!(matches!(
                        interrupt,
                        ManualCompressionOutcome::Interrupted(CompressionInterrupt::Command(
                            Command::Cancel
                        ))
                    ));
                } else {
                    assert!(!matches!(
                        interrupt,
                        ManualCompressionOutcome::Interrupted(_)
                    ));
                }
                let expected = if fail_save { "原标题" } else { "新标题" };
                assert_eq!(ctx.session.title, expected);
                let restored = Session::load_from_storage(root.path(), &ctx.session.id).unwrap();
                assert_eq!(restored.title, expected);
                let titles: Vec<_> = stream_rx
                    .try_iter()
                    .filter_map(|event| match event {
                        StreamEvent::TitleChanged { title } => Some(title),
                        _ => None,
                    })
                    .collect();
                if fail_save {
                    assert!(titles.is_empty(), "保存失败不得报告标题修改成功");
                } else {
                    assert_eq!(titles, vec!["新标题"]);
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manual_compression_stops_immediately_on_cancel() {
        let mut session = Session::new("manual-cancel");
        for text in ["旧问题", "新问题"] {
            session.append_message(MessageRole::User, text);
        }
        let (mut ctx, _root) = test_context(session);
        let organizer = ContextOrganizer::new(ctx.context_limit);
        let compression = ContextCompression::manual(
            &ctx,
            &organizer,
            ctx.session.current_tokens,
            "测试整理".to_string(),
        );
        let (cmd_tx, mut cmd_rx) = tokio_mpsc::unbounded_channel();
        cmd_tx.send(Command::Cancel).unwrap();
        let interrupt = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            compression.run_manual(&mut ctx, &mut cmd_rx),
        )
        .await
        .expect("取消应立即返回");
        assert!(matches!(
            interrupt,
            ManualCompressionOutcome::Interrupted(CompressionInterrupt::Command(Command::Cancel))
        ));
    }

    #[tokio::test]
    async fn manual_compression_records_usage_and_counts_it_once() {
        let mut session = Session::new("usage-summary");
        for text in ["旧问题", "新问题"] {
            session.append_message(MessageRole::User, text);
        }
        let (mut ctx, root) = test_context(session);
        let mut update = update_for(&ctx.session, 0, "摘要", 1);
        update.usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            prompt_cache_hit_tokens: Some(80),
            prompt_cache_miss_tokens: Some(20),
        };
        let task = tokio::spawn(async { Err(CompressionError::new("unused")) });
        let call_id = scru128::new().to_string();
        ContextCompression {
            task,
            kind: CompressionKind::Manual {
                observed_tokens: 0,
                notice: "测试整理".to_string(),
            },
            call_id: call_id.clone(),
            cancelled_usage: TokenUsage::default(),
        }
        .complete(&mut ctx, Ok(update), None);
        assert_eq!(ctx.session.token_usage.total_tokens, 120);
        let loaded = Session::load_from_storage(root.path(), &ctx.session.id).unwrap();
        let message = loaded
            .messages
            .iter()
            .find(|message| message.id == call_id)
            .unwrap();
        assert_eq!(message.role, MessageRole::Notice);
        assert_eq!(message.usage.as_ref().unwrap().source, "context_summary");
        assert_eq!(
            message.usage.as_ref().unwrap().tokens.cache_hit_rate(),
            Some(0.8)
        );
        assert!(!loaded.context().iter().any(|message| message.id == call_id));
    }

    #[tokio::test]
    async fn completed_compression_usage_survives_a_racing_cancel() {
        let (mut ctx, root) = test_context(Session::new("cancel-summary-usage"));
        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            prompt_cache_hit_tokens: Some(80),
            prompt_cache_miss_tokens: Some(20),
        };
        let task = tokio::spawn(async move {
            Err(CompressionError {
                message: "摘要不完整".into(),
                usage,
            })
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let call_id = scru128::new().to_string();
        let mut compression = ContextCompression {
            task,
            kind: CompressionKind::Manual {
                observed_tokens: 0,
                notice: "测试整理".to_string(),
            },
            call_id: call_id.clone(),
            cancelled_usage: TokenUsage::default(),
        };
        compression.cancel(&mut ctx).await;
        assert_eq!(ctx.session.token_usage.total_tokens, 120);
        let loaded = Session::load_from_storage(root.path(), &ctx.session.id).unwrap();
        let record = loaded
            .messages
            .iter()
            .find(|message| message.id == call_id)
            .unwrap()
            .usage
            .as_ref()
            .unwrap();
        assert_eq!(record.status, tiangong_types::TurnStatus::Cancelled);
        assert_eq!(record.tokens.total_tokens, 120);
    }
}
