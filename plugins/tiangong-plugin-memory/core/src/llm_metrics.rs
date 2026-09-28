use std::time::Duration;

use tiangong_llm::{LlmEndpointConfig, TokenUsageData};

pub(crate) fn log_memory_llm_call(
    task: &str,
    model: &LlmEndpointConfig,
    elapsed: Duration,
    usage: Option<&TokenUsageData>,
) {
    let (prompt_tokens, completion_tokens, total_tokens) = usage
        .map(|usage| {
            (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens,
            )
        })
        .unwrap_or_default();
    tracing::info!(
        target: "tiangong_memory::llm",
        task,
        provider = %model.protocol.as_str(),
        configured_provider = %model.source_provider_label(),
        model = %model.model,
        protocol = %model.protocol.as_str(),
        elapsed_ms = elapsed.as_millis(),
        prompt_tokens,
        completion_tokens,
        total_tokens,
        "Memory LLM 调用完成"
    );
}

pub(crate) fn log_memory_llm_failure<E>(
    task: &str,
    model: &LlmEndpointConfig,
    err: &E,
    message: &'static str,
) where
    E: std::fmt::Display + ?Sized,
{
    tracing::warn!(
        target: "tiangong_memory::llm",
        task,
        provider = %model.protocol.as_str(),
        configured_provider = %model.source_provider_label(),
        model = %model.model,
        protocol = %model.protocol.as_str(),
        error = %err,
        "{message}"
    );
}
