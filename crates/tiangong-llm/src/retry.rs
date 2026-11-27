//! 适配器共同的有限重试执行器；不包含模型/会话策略。
use crate::error::LlmError;
use std::{future::Future, sync::Arc, time::Duration};

/// 默认请求重试次数，不含首次尝试。
pub(crate) const DEFAULT_MAX_RETRIES: u32 = 9;

pub(crate) type RetryNotifier = Arc<dyn Fn(u32, u32, u64, &str) + Send + Sync>;

pub(crate) async fn run<F, Fut, T>(
    operation: &'static str,
    model: &str,
    max_retries: u32,
    notifier: Option<&RetryNotifier>,
    mut execute: F,
) -> Result<T, LlmError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, LlmError>>,
{
    let mut attempt = 0;
    loop {
        let started = std::time::Instant::now();
        take_attempt()?;
        match execute().await {
            Ok(value) => {
                tracing::debug!(
                    operation,
                    model,
                    attempt,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "模型请求完成"
                );
                return Ok(value);
            }
            Err(error) if attempt < max_retries && error.is_retryable() => {
                attempt += 1;
                let delay_ms = 1000u64
                    .saturating_mul(1u64.checked_shl(attempt - 1).unwrap_or(u64::MAX))
                    .min(30_000);
                if let Some(notify) = notifier {
                    notify(attempt, max_retries, delay_ms, &error.to_string());
                }
                tracing::warn!(
                    operation,
                    model,
                    attempt,
                    delay_ms,
                    "模型请求将在预算内重试"
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

// 在一次逻辑生成的异步作用域内共用建立阶段与流中断重试预算。
// task-local 随 future 取消释放，不跨会话共享，不修改 SDK 自身的公开接口。
tokio::task_local! {
    static BUDGET: std::sync::Arc<std::sync::atomic::AtomicUsize>;
}

pub(crate) async fn with_budget<T>(attempts: usize, future: impl Future<Output = T>) -> T {
    BUDGET
        .scope(
            Arc::new(std::sync::atomic::AtomicUsize::new(attempts)),
            future,
        )
        .await
}

pub(crate) fn take_attempt() -> Result<(), LlmError> {
    let permitted = BUDGET
        .try_with(|budget| {
            budget
                .try_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
        })
        .unwrap_or(true);
    if permitted {
        Ok(())
    } else {
        Err(LlmError::InvalidRequest("本次生成的重试预算已耗尽".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn generation_defaults_allow_nine_retries() {
        use crate::providers::{
            anthropic::AnthropicConfig, deepseek::DeepSeekConfig, openai::OpenAiResponsesConfig,
            openai_chatcompletions::OpenAiChatConfig,
        };

        assert_eq!(crate::Options::default().max_retries, 9);
        assert_eq!(crate::LlmEndpointConfig::new("", "", "").max_retries, 9);
        assert_eq!(AnthropicConfig::new("").max_retries, 9);
        assert_eq!(DeepSeekConfig::new("").max_retries, 9);
        assert_eq!(OpenAiResponsesConfig::new("", "").max_retries, 9);
        assert_eq!(OpenAiChatConfig::new("", "").max_retries, 9);
    }

    #[tokio::test(start_paused = true)]
    async fn default_retry_limit_stops_after_ten_attempts() {
        let mut attempts = 0;
        let result = with_budget(10, async {
            run(
                "probe",
                "test",
                crate::Options::default().max_retries,
                None,
                || {
                    attempts += 1;
                    async { Err::<(), _>(LlmError::Transport("reset".into())) }
                },
            )
            .await
        })
        .await;

        assert!(matches!(result, Err(LlmError::Transport(_))));
        assert_eq!(attempts, 10);
    }

    #[tokio::test]
    async fn logical_budget_caps_nested_attempts_and_is_isolated() {
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let result = with_budget(2, async {
            run("probe", "test", 10, None, || {
                observed.fetch_add(1, Ordering::SeqCst);
                async { Err::<(), _>(LlmError::Transport("reset".into())) }
            })
            .await
        })
        .await;
        assert!(result.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert!(with_budget(1, async { take_attempt() }).await.is_ok());
        assert!(take_attempt().is_ok());
    }
    #[tokio::test]
    async fn permanent_error_does_not_retry_and_backoff_cancels() {
        let count = AtomicUsize::new(0);
        let result = run("probe", "test", 3, None, || {
            count.fetch_add(1, Ordering::SeqCst);
            async { Err::<(), _>(LlmError::Serialization("invalid JSON".into())) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let count = Arc::new(AtomicUsize::new(0));
        let cloned = count.clone();
        let task = tokio::spawn(async move {
            run("probe", "test", 10, None, || {
                cloned.fetch_add(1, Ordering::SeqCst);
                async { Err::<(), _>(LlmError::Transport("reset".into())) }
            })
            .await
        });
        while count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
