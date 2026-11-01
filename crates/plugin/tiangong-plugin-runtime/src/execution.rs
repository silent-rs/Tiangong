//! 插件同步调用的执行边界。

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};

use anyhow::{Result, anyhow};

/// 在 Tokio runtime 外执行同步插件调用。
///
/// sidecar 声明读取等同步路径会执行阻塞 I/O；若调用方正处于 Tokio runtime
/// 中，必须先切到普通 OS 线程，同时在边界处接住 host panic，避免单个插件
/// 把宿主进程带崩。
pub(crate) fn run_outside_tokio<R>(call: impl FnOnce() -> Result<R> + Send) -> Result<R>
where
    R: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::scope(|scope| {
            scope
                .spawn(move || catch_panic(call))
                .join()
                .map_err(|payload| anyhow!("插件同步调用线程异常: {}", panic_message(payload)))?
        });
    }

    catch_panic(call)
}

fn catch_panic<R>(call: impl FnOnce() -> Result<R>) -> Result<R> {
    catch_unwind(AssertUnwindSafe(call))
        .map_err(|payload| anyhow!("插件同步调用异常: {}", panic_message(payload)))?
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "未知 panic".to_string()
}
