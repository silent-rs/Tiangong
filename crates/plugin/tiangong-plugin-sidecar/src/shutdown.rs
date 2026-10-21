//! 跨平台退出信号（Ctrl+C / SIGTERM）。

use anyhow::Result;

/// 阻塞等待终止信号（Leader sidecar 在服务期间调用）。
#[cfg(unix)]
pub async fn wait_for_shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {},
    }
    Ok(())
}

/// 阻塞等待终止信号（非 unix 回退：仅 Ctrl+C）。
#[cfg(not(unix))]
pub async fn wait_for_shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}
