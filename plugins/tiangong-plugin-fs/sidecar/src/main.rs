//! Fs sidecar 进程：fs 插件的全部逻辑。
//!
//! 宿主按清单 `tools` 把 `file_read` / `file_write` / `current_time` 直连到
//! 本进程（无 WASM 桥接层）：参数解析、文件读写（`std::fs`）、写操作锁、
//! 路径解析与读图规范化都在这里完成。生命周期为按需（每次调用独立进程，
//! 无跨请求状态），工作目录取自宿主注入的请求上下文。
mod dispatch;
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
