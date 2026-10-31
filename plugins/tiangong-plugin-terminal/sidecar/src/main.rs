//! 终端插件 sidecar：PTY 会话管理（spawn/write/resize/kill/读输出）。
//!
//! 完全插件化的终端核心——宿主零终端代码：
//! - 工具执行（run_command/run_shell/terminal_send）经 `sidecar.<操作>` 请求到达；
//! - PTY 输出经 `emit_notification("terminal.output", ...)` 流式推送，
//!   插件 UI（xterm.js）订阅 sidecar 事件渲染。

mod persist;
mod service;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    tracing::info!("terminal sidecar 启动中...");

    let config = tiangong_plugin_sidecar::SidecarConfig::new("terminal");
    tiangong_plugin_sidecar::run(config, || {
        let service = std::sync::Arc::new(service::TerminalService::new());
        service.start_idle_exit_monitor();
        Ok(service)
    })
    .await
}
