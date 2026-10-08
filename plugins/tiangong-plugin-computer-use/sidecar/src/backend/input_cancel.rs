//! 键鼠手势的请求级取消。
//!
//! 键鼠手势在 `spawn_blocking` 线程里逐事件投递。宿主取消请求时，sidecar
//! 运行库只丢弃 dispatch future，阻塞线程本身不会停止：长文本 `type` 会在
//! 取消后继续打完，并一直占住进程级手势锁。
//!
//! 处理方式：service 为每次键鼠请求创建 [`CancelToken`]，用 [`scope`] 放进
//! 任务上下文，同时持有 [`CancelOnDrop`]。dispatch future 因取消或连接断开
//! 被丢弃时，守卫随之析构并置位令牌。后端在进入阻塞线程前用 [`current`]
//! 取出令牌，手势循环每投递一个事件前检查一次，置位后立即收尾返回。
//! 请求正常完成后守卫也会置位，此时阻塞工作已结束，没有副作用。
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// 键鼠手势取消令牌：克隆共享同一标志。
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// 新建未取消的令牌。
    pub fn new() -> Self {
        Self::default()
    }

    /// 置位取消标志。
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// 是否已取消。
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// 析构时置位令牌：绑定 dispatch future 的生命周期。
pub struct CancelOnDrop(CancelToken);

impl CancelOnDrop {
    /// 绑定令牌。
    pub fn new(token: CancelToken) -> Self {
        Self(token)
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

tokio::task_local! {
    static CURRENT: CancelToken;
}

/// 在给定令牌的任务上下文中执行 future。
pub async fn scope<F: Future>(token: CancelToken, future: F) -> F::Output {
    CURRENT.scope(token, future).await
}

/// 当前任务上下文的令牌；不在 [`scope`] 内时返回永不取消的新令牌。
///
/// 必须在进入 `spawn_blocking` 之前调用（阻塞线程不继承任务上下文）。
pub fn current() -> CancelToken {
    CURRENT.try_with(Clone::clone).unwrap_or_default()
}

/// 取消时的人读说明。
pub fn cancelled_message(what: &str) -> String {
    format!("{what}已取消（请求被中止）")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use super::*;

    #[test]
    fn guard_drop_cancels_token() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        drop(CancelOnDrop::new(token.clone()));
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn current_outside_scope_never_cancels() {
        assert!(!current().is_cancelled());
    }

    /// 模拟 sidecar 运行库取消：dispatch future 被丢弃后，阻塞线程中的
    /// 逐事件循环应在下一次检查时停止，而不是跑完全部事件。
    #[tokio::test]
    async fn dropping_dispatch_future_stops_blocking_loop() {
        let posted = Arc::new(AtomicUsize::new(0));
        let posted_in_task = Arc::clone(&posted);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let dispatch = async move {
            let token = CancelToken::new();
            let _guard = CancelOnDrop::new(token.clone());
            scope(token, async move {
                let token = current();
                tokio::task::spawn_blocking(move || {
                    for _ in 0..1000 {
                        if token.is_cancelled() {
                            break;
                        }
                        posted_in_task.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    let _ = done_tx.send(());
                })
                .await
                .ok();
            })
            .await
        };
        // 等价于 server.rs 的 select!：取消分支先完成，dispatch future 被丢弃。
        tokio::select! {
            _ = dispatch => panic!("循环不应自然跑完"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        tokio::time::timeout(Duration::from_secs(2), done_rx)
            .await
            .expect("阻塞循环应在取消后及时退出")
            .expect("阻塞线程应发送完成信号");
        let count = posted.load(Ordering::SeqCst);
        assert!(count > 0 && count < 1000, "实际投递 {count} 个事件");
    }
}
