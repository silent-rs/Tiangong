//! MCP 独立 sidecar 进程。
//!
//! 作为 MCP 的唯一常驻进程运行，承载全部 rmcp 客户端（stdio/HTTP transport）、
//! capability 后台探测与缓存，并通过 TCP IPC 暴露给运行时访问。
//!
//! 工作流程：
//! 1. 竞争 Leader lease（leader.lock 原子文件互斥）
//! 2. 抢到 Leader → 起 IPC server + capability 调度 → 阻塞运行
//! 3. 已有 Leader → 优雅退出（不重复运行）
//!
//! 选举与 IPC 由 `tiangong-plugin-sidecar` 通用运行库提供，运行时按 `plugin.json`
//! 启动本进程。

mod capability;
mod client;
mod execution;
mod migration;
mod paths;
mod service;
mod validate;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!(
        business_protocol = tiangong_plugin_mcp_protocol::MCP_PROTOCOL_VERSION,
        "mcp sidecar 启动中..."
    );

    let config = tiangong_plugin_sidecar::SidecarConfig::new("mcp");
    tiangong_plugin_sidecar::run(config, || {
        let service = service::McpService::new()?;
        service.prime();
        Ok(std::sync::Arc::new(service))
    })
    .await
}
