//! ReAct 循环上下文管理：system prompt 重建与 token usage 上报。

use std::sync::mpsc::Sender as StdSender;

use crate::context::organizer::ContextOrganizer;
use crate::prompt::SystemPromptConfig;
use crate::session::Session;
use crate::turn_context::TurnContext;
use tiangong_llm::{ModelRequest, SingleProviderClient};
use tiangong_types::StreamEvent;
use tiangong_types::TokenUsage;

pub(super) fn record_call_usage(
    ctx: &mut TurnContext,
    message_id: &str,
    usage: &TokenUsage,
    source: &str,
    status: tiangong_types::TurnStatus,
) {
    if !usage.has_usage() {
        return;
    }
    let record = tiangong_types::token::MessageUsage {
        tokens: usage.clone(),
        model: ctx.client.model_name().to_string(),
        agent_id: ctx.session.id.clone(),
        turn_id: ctx.turn_id.clone(),
        source: source.to_string(),
        status,
    };
    if let Some(message) = ctx
        .session
        .messages
        .iter_mut()
        .find(|message| message.id == message_id)
    {
        message.set_usage(record);
    } else {
        // 无正文调用的用量不会进入模型历史。
        let mut message = crate::session::Message::new(
            crate::session::MessageRole::Notice,
            format!("[调用用量] {source}"),
        );
        message.id = message_id.to_string();
        message.set_usage(record);
        ctx.session.messages.push(message);
    }
    crate::react::message::emit_session_message_upsert(ctx, message_id);
}

/// 就地收集插件提示段落并重建 system prompt（内容一致时保留原消息）。
///
/// 产品身份 / 通用规则 / 自定义指令外围等文案由各插件经 `PromptSectionProvider`
/// 注入（产品基础文案见 `tiangong-plugin-prompt`），core 不再持有产品文案。
pub(crate) fn rebuild_system_prompt_for_session(
    session: &mut Session,
    plugins: &[std::sync::Arc<dyn crate::core::Plugin>],
) {
    let prompt_sections = plugins
        .iter()
        .flat_map(|plugin| plugin.prompt_sections())
        .collect();
    let config = SystemPromptConfig::from_plugin_sections(prompt_sections);
    session.rebuild_system_prompt(&config);
}

pub(crate) fn emit_token_usage(
    stream_tx: &StdSender<StreamEvent>,
    usage: &TokenUsage,
    current_tokens: Option<usize>,
    context_limit: usize,
    source: impl Into<String>,
    agent_id: Option<&str>,
) {
    if usage.total_tokens == 0 {
        return;
    }
    let source = source.into();
    // 按 source（阶段）记录 KV cache 命中率，便于实测 ReAct/Summary 各阶段命中情况。
    // hit_ratio = hit / (hit + miss)，仅当 DeepSeek 返回了 cache 字段时记录。
    if let (Some(hit), Some(miss)) = (
        usage.prompt_cache_hit_tokens,
        usage.prompt_cache_miss_tokens,
    ) {
        let total = hit + miss;
        let ratio = if total > 0 {
            hit as f64 / total as f64
        } else {
            0.0
        };
        tracing::info!(
            source = %source,
            agent_id = ?agent_id,
            prompt_tokens = usage.prompt_tokens,
            completion_tokens = usage.completion_tokens,
            prompt_cache_hit_tokens = hit,
            prompt_cache_miss_tokens = miss,
            cache_hit_ratio = format!("{:.2}%", ratio * 100.0),
            "kv cache 命中统计",
        );
    }
    let _ = stream_tx.send(StreamEvent::TokenUsage {
        usage: usage.clone(),
        current_tokens,
        compression_threshold_tokens: Some(ContextOrganizer::new(context_limit).token_threshold()),
        context_limit_tokens: Some(context_limit),
        source,
        agent_id: agent_id.map(|s| s.to_string()),
    });
}

pub(crate) fn select_client_for_request<'a>(
    ctx: &'a TurnContext,
    _req: &ModelRequest,
) -> &'a SingleProviderClient {
    ctx.client()
}

/// 将错误持久化到 session，作为 LLM 请求失败时的诊断痕迹。
///
/// 复用 `inject_tool_to_messages` 统一注入通道（合法的 assistant tool_call + tool result
/// 消息对），而非手工追加消息：
/// - 避免使用 `MessageRole::System`——它会被 `build_provider_messages` 的
///   `system_texts.clear()` 覆盖整个 system prompt，污染 prompt 并破坏 KV cache 前缀；
/// - 走标准注入通道保证 append-only、provider 序列化为合法 tool pair，cache 友好。
///
/// 去重边界：`inject_tool_to_messages` 仅对连续相同的 plugin_injection 结果去重，
/// 不与 `run_turn` 失败收尾追加的 `[错误]` Notice 消息跨格式去重——二者各司其职：
/// 本消息对模型可见（携带继续指令），`[错误]` 消息仅前端可见（Notice 角色在
/// 上下文构建、压缩与 provider 转换处整体排除）。
pub(crate) fn persist_error(ctx: &mut TurnContext, message: impl Into<String>) {
    let message = message.into();
    let payload = serde_json::json!({
        "error": message,
        "instruction": "上一步执行失败，请基于已有结果继续或向用户说明原因。",
    });
    crate::react::message::inject_tool_to_messages(&mut ctx.session, "react_loop_error", &payload);
    ctx.session.persist_to_disk();
}
