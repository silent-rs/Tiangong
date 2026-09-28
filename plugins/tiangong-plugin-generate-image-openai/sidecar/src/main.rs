//! Generate-Image-OpenAI 独立 sidecar 进程。

mod config;
mod extract;
mod service;
mod transport;

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
        business_protocol = tiangong_plugin_generate_image_openai_protocol::IMAGE_PROTOCOL_VERSION,
        "generate-image-openai sidecar 启动中..."
    );

    let config = tiangong_plugin_sidecar::SidecarConfig::new("generate-image-openai");
    tiangong_plugin_sidecar::run(config, || Ok(std::sync::Arc::new(service::ImageService))).await
}
