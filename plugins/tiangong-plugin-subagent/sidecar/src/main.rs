//! Subagent 插件 sidecar：持久 Agent 统一交互总线。
//!
//! 完全插件化的 Subagent 核心——宿主零 Subagent 代码：
//! - AI 工具与管理页操作经 `sidecar.<operation>` 请求到达（工具直连）；
//! - 状态变化经 `emit_notification("subagent.event", ...)` 推送，管理页订阅刷新；
//! - 重要反馈（完成/阻塞/审批/失败）先落盘再经本机 server 投回激活会话；
//! - managed 运行实例随宿主退出清理（宿主先调 `subagent_shutdown` 优雅关闭，
//!   信号与进程组级联兜底）。

mod agent_store;
mod delivery;
mod mcp;
mod mcp_access;
mod memory;
mod migration;
mod paths;
mod runner;
mod runtime_store;
mod service;
mod sessions;
mod workspace_state;

use std::sync::Arc;

/// 预读首条非空帧（阻塞至对端发出首帧）：用于协议探测；EOF 返回 None
///（无对端时交还原生入口的常规行为）。
fn peek_first_frame() -> anyhow::Result<Option<String>> {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    let mut guard = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = guard.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            return Ok(Some(line));
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    tracing::info!("subagent sidecar 启动中...");

    // 协议分流按启动方区分，不做运行时嗅探：宿主 spawn 必带 stdio 传输
    // 标识（stdio_requested），直接进入原生 IPC 循环；无标识即外部 agent
    // 工具直起（标准 MCP JSON-RPC），预读首帧由插件自己的 MCP 循环消费。
    // 底层 sidecar 库不感知 MCP，也不提供首帧回喂。
    if tiangong_plugin_sidecar::stdio::stdio_requested() {
        return tiangong_plugin_sidecar::stdio::run_stdio(|| {
            let service = Arc::new(service::SubagentService::new()?);
            spawn_signal_cleanup(Arc::clone(&service));
            Ok(service)
        })
        .await;
    }
    // 外部直起（无宿主传输标识）：MCP JSON-RPC 入口，首帧由插件自己预读。
    let service = Arc::new(service::SubagentService::new_without_restore()?);
    let workspace = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| std::env::var("HOME").unwrap_or_else(|_| ".".to_string()));
    mcp::run_mcp(service, workspace, peek_first_frame()?).await
}

/// 终止信号兜底：宿主 SIGKILL 前的 SIGTERM / 手动 kill 时同步清理 managed 运行。
fn spawn_signal_cleanup(service: Arc<service::SubagentService>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("注册 SIGTERM 监听失败");
            let mut sigint =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .expect("注册 SIGINT 监听失败");
            tokio::select! {
                _ = sigterm.recv() => {}
                _ = sigint.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await.ok();
        }
        tracing::info!("收到终止信号，清理 managed 运行实例");
        service.begin_shutdown().await;
    });
}
