//! Fs 独立 sidecar 进程。
//!
//! 作为 Fs 的唯一常驻进程运行，承载全部文件读写（`std::fs`）、进程级文件锁表
//! （跨 wasm 实例共享）、路径解析与沙箱策略。通过 TCP IPC 暴露给运行时访问。
//!
//! 工作流程：
//! 1. 淘汰制单例判定（endpoint 文件 + TCP 端口可达性）
//! 2. 端口可达 → 已有实例，优雅退出（不重复运行）
//! 3. 端口不可达 → 起 IPC server + FsService → 阻塞运行
//!
//! 单例与 IPC 由 `tiangong-plugin-sidecar` 通用运行库提供，运行时按 `plugin.json`
//! 启动本进程。锁表天然落在 sidecar 进程内即保持全局唯一——主 Agent 与子 Agent
//! 各自独立的 wasm 实例经 host 路由到同一个 sidecar，共享同一份锁表。

mod file_lock;
mod handlers;
mod read_image;
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

    tracing::info!(
        business_protocol = tiangong_plugin_fs_protocol::FS_PROTOCOL_VERSION,
        "fs sidecar 启动中..."
    );

    let config = tiangong_plugin_sidecar::SidecarConfig::new("fs");
    tiangong_plugin_sidecar::run(config, || {
        Ok(std::sync::Arc::new(service::FsService::new()?))
    })
    .await
}
