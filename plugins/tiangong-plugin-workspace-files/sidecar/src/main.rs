//! Workspace Files 插件 sidecar：工作区内文件浏览、预览与编辑。
//!
//! 按需生命周期（每次调用独立进程），宿主以会话工作区作为沙箱可写域；
//! 本进程再把所有路径限定在页面声明的工作区根内（越界写入同时被 OS 沙箱拦截）。

mod files;
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
    let config = tiangong_plugin_sidecar::SidecarConfig::new("workspace-files");
    tiangong_plugin_sidecar::run(config, || {
        Ok(std::sync::Arc::new(service::WorkspaceFilesService))
    })
    .await
}
