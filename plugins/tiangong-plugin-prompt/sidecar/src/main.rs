//! Prompt 独立 sidecar 进程。
//!
//! 作为 Prompt 的唯一常驻进程运行，承载自定义指令读写、旧数据迁移与
//! 动态 prompt 段落（`prompt_sections`），通过 TCP IPC 暴露给运行时访问。
//!
//! 单例与 IPC 由 `tiangong-plugin-sidecar` 通用运行库提供，运行时按
//! `plugin.json` 启动本进程。

mod paths;
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
        business_protocol = tiangong_plugin_prompt_protocol::PROMPT_PROTOCOL_VERSION,
        "prompt sidecar 启动中..."
    );

    let config = tiangong_plugin_sidecar::SidecarConfig::new("prompt");
    tiangong_plugin_sidecar::run(config, || {
        Ok(std::sync::Arc::new(service::PromptService::new()?))
    })
    .await
}
