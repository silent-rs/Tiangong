//! Runtime 插件统一调用生命周期。
//!
//! Core 的 Plugin/ToolOverrideHandler 调度保持不变。WASM、sidecar 与 TS UI
//! Adapter 进入 Runtime 后，都用本对象统一身份、上下文、取消、进度和闭合。

use std::future::Future;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use tiangong_core::core::plugin::PluginFeedbackTx;
use tiangong_core::tools::result::ToolResult;
use tiangong_llm::tool::ToolCall;
use tiangong_types::StreamEvent;

const RUNNING: u8 = 0;
const COMPLETED: u8 = 1;
const CANCELLED: u8 = 2;

type CancelHook = Box<dyn FnOnce() + Send + 'static>;

#[derive(Clone)]
pub(crate) struct RuntimeInvocation {
    inner: Arc<InvocationInner>,
}

struct InvocationInner {
    plugin_id: String,
    call: ToolCall,
    context: crate::protocol::RequestInvocationContext,
    feedback: Option<PluginFeedbackTx>,
    state: AtomicU8,
    result: Mutex<Option<ToolResult>>,
    cancel_hooks: Mutex<Vec<CancelHook>>,
}

impl RuntimeInvocation {
    pub(crate) fn new(
        plugin_id: impl Into<String>,
        call: ToolCall,
        session_id: impl Into<String>,
        workspace: impl Into<String>,
        actor_id: impl Into<String>,
        feedback: Option<PluginFeedbackTx>,
    ) -> Self {
        let context = crate::protocol::RequestInvocationContext {
            session_id: session_id.into(),
            invocation_id: scru128::new().to_string(),
            workspace: workspace.into(),
            actor_id: actor_id.into(),
            deadline_ms: None,
        };
        Self {
            inner: Arc::new(InvocationInner {
                plugin_id: plugin_id.into(),
                call,
                context,
                feedback,
                state: AtomicU8::new(RUNNING),
                result: Mutex::new(None),
                cancel_hooks: Mutex::new(Vec::new()),
            }),
        }
    }

    pub(crate) fn plugin_id(&self) -> &str {
        &self.inner.plugin_id
    }

    pub(crate) fn call(&self) -> &ToolCall {
        &self.inner.call
    }

    pub(crate) fn context(&self) -> &crate::protocol::RequestInvocationContext {
        &self.inner.context
    }

    pub(crate) fn on_cancel(&self, hook: impl FnOnce() + Send + 'static) {
        if self.inner.state.load(Ordering::Acquire) == CANCELLED {
            hook();
            return;
        }
        let Ok(mut hooks) = self.inner.cancel_hooks.lock() else {
            return;
        };
        if self.inner.state.load(Ordering::Acquire) == CANCELLED {
            drop(hooks);
            hook();
        } else {
            hooks.push(Box::new(hook));
        }
    }

    /// 统一处理 Handler 进度。Runtime 控制反馈先消费，普通 JSON StreamEvent
    /// 或文本再投递到当前 turn；turn 已结束时自然拒绝迟到反馈。
    pub(crate) fn progress(&self, message: String) {
        if self.inner.state.load(Ordering::Acquire) != RUNNING {
            return;
        }
        if crate::bridge::handle_runtime_feedback(self.plugin_id(), &message) {
            return;
        }
        let Some(feedback) = &self.inner.feedback else {
            return;
        };
        match serde_json::from_str::<StreamEvent>(&message) {
            Ok(event) => feedback.send_stream_event(event),
            Err(_) => feedback.send_stream_event(StreamEvent::ReactText {
                message_id: self.call().id.clone(),
                content: message,
            }),
        }
    }

    #[cfg(test)]
    fn result(&self) -> Option<ToolResult> {
        self.inner
            .result
            .lock()
            .ok()
            .and_then(|result| result.clone())
    }

    fn complete(&self, result: Option<&ToolResult>) {
        if let Some(result) = result
            && let Ok(mut stored) = self.inner.result.lock()
        {
            *stored = Some(result.clone());
        }
        if self
            .inner
            .state
            .compare_exchange(RUNNING, COMPLETED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && let Ok(mut hooks) = self.inner.cancel_hooks.lock()
        {
            hooks.clear();
        }
    }

    fn cancel(&self) {
        if self
            .inner
            .state
            .compare_exchange(RUNNING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let hooks = self
            .inner
            .cancel_hooks
            .lock()
            .map(|mut hooks| std::mem::take(&mut *hooks))
            .unwrap_or_default();
        for hook in hooks {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(hook));
        }
    }
}

struct InvocationGuard {
    invocation: RuntimeInvocation,
    completed: bool,
}

impl Drop for InvocationGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.invocation.cancel();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandlerKind {
    Ui,
    Sidecar,
}

/// 验证能力是否声明接管目标工具：精确名 `tool:<name>` 或全量 `tool:*`。
pub(crate) fn capabilities_handle_tool(capabilities: &[String], tool_name: &str) -> bool {
    capabilities
        .iter()
        .any(|capability| capability == &format!("tool:{tool_name}") || capability == "tool:*")
}

/// Runtime 内唯一的 Handler 选择入口（纯同步，不启动进程）。
///
/// 路由消费最近一次成功握手保存的验证能力：无 UI sidecar 由结构决定
/// 直连——记录缺失（安装后运行检查未完成或失败）时仍走 sidecar 真实
/// 调用，运行异常以真实错误返回；有 UI 的插件仅在已验证 sidecar 声明
/// 接管目标工具时走 sidecar，记录缺失或未声明时回退 UI Handler。
pub(crate) fn select_ts_handler(
    sidecar_direct: bool,
    has_sidecar: bool,
    has_ui: bool,
    verified_capabilities: Option<&[String]>,
    tool_name: &str,
) -> HandlerKind {
    if sidecar_direct {
        return HandlerKind::Sidecar;
    }
    if has_ui
        && has_sidecar
        && verified_capabilities
            .is_some_and(|capabilities| capabilities_handle_tool(capabilities, tool_name))
    {
        return HandlerKind::Sidecar;
    }
    HandlerKind::Ui
}

/// Runtime 所有 Adapter 的共同闭合点。future 被 Core 丢弃即触发统一取消；
/// Handler 返回（含业务失败）均视为一次完整闭合。
pub(crate) async fn dispatch<F>(invocation: RuntimeInvocation, future: F) -> Option<ToolResult>
where
    F: Future<Output = Option<ToolResult>>,
{
    let mut guard = InvocationGuard {
        invocation: invocation.clone(),
        completed: false,
    };
    let result = future.await;
    invocation.complete(result.as_ref());
    guard.completed = true;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn invocation() -> RuntimeInvocation {
        RuntimeInvocation::new(
            "demo",
            ToolCall {
                id: "call-a".into(),
                name: "demo".into(),
                arguments: serde_json::json!({}),
            },
            "session-a",
            "/workspace",
            "agent-a",
            None,
        )
    }

    #[tokio::test]
    async fn completed_dispatch_does_not_cancel() {
        let invocation = invocation();
        let hits = Arc::new(AtomicUsize::new(0));
        let target = Arc::clone(&hits);
        invocation.on_cancel(move || {
            target.fetch_add(1, Ordering::SeqCst);
        });
        let expected = ToolResult {
            ok: true,
            summary: "done".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            execution: None,
        };
        let result = dispatch(invocation.clone(), async { Some(expected) }).await;
        assert_eq!(
            result.as_ref().map(|result| result.summary.as_str()),
            Some("done")
        );
        assert_eq!(
            invocation.result().map(|result| result.summary),
            Some("done".into())
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn dropped_dispatch_cancels_once() {
        let invocation = invocation();
        let hits = Arc::new(AtomicUsize::new(0));
        let target = Arc::clone(&hits);
        invocation.on_cancel(move || {
            target.fetch_add(1, Ordering::SeqCst);
        });
        let task = tokio::spawn(dispatch(invocation.clone(), std::future::pending()));
        task.abort();
        let _ = task.await;
        invocation.cancel();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn handler_selection_consumes_verified_capabilities_only() {
        let verified = vec!["tool:demo".to_string()];
        // 无 UI sidecar：结构直连；记录缺失时仍走 sidecar 真实调用，
        // 运行异常以真实错误呈现（安装后运行检查语义）。
        assert_eq!(
            select_ts_handler(true, true, false, Some(&verified), "demo"),
            HandlerKind::Sidecar
        );
        assert_eq!(
            select_ts_handler(true, true, false, None, "demo"),
            HandlerKind::Sidecar
        );
        // 有 UI：声明接管走 sidecar，未声明或记录缺失回退 UI。
        assert_eq!(
            select_ts_handler(false, true, true, Some(&verified), "demo"),
            HandlerKind::Sidecar
        );
        assert_eq!(
            select_ts_handler(false, true, true, Some(&verified), "other"),
            HandlerKind::Ui
        );
        assert_eq!(
            select_ts_handler(false, true, true, None, "demo"),
            HandlerKind::Ui
        );
        // 通配能力接管全部工具。
        let wildcard = vec!["tool:*".to_string()];
        assert_eq!(
            select_ts_handler(false, true, true, Some(&wildcard), "any"),
            HandlerKind::Sidecar
        );
        // 只有 UI：始终 UI。
        assert_eq!(
            select_ts_handler(false, false, true, Some(&wildcard), "any"),
            HandlerKind::Ui
        );
    }
}
