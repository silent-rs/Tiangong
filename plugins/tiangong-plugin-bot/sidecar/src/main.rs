//! Bot 插件 sidecar：一个插件对接飞书、QQ、微信三个平台。
//!
//! - 常驻主进程（`TIANGONG_PLUGIN_INSTANCE_ROLE=primary`）为每个已配置的平台在
//!   天工 server 可用时建立 IM 连接，把消息转发到 server；QQ / 微信按单连接
//!   运行，同机多个宿主只有取得连接锁的一方建连；
//! - 推送工具（list_push_targets / send_*_message）与设置页操作（扫码、
//!   凭据、推送目标）经 sidecar 请求到达，任何实例都可处理。
//!
//! 数据保存在插件数据目录 `TIANGONG_PLUGIN_DATA_DIR/<platform>/`。
use std::sync::Arc;

mod delivery;
mod feishu;
mod legacy;
mod local_media;
mod platform;
mod qq;
mod reply_quota;
mod service;
#[cfg(test)]
mod test_support;
mod tools;
mod weixin;

/// 插件支持的全部平台（设置页与工具按此顺序展示）。
pub(crate) fn all_platforms() -> Vec<Arc<dyn platform::BotPlatform>> {
    vec![
        Arc::new(feishu::Feishu::default()),
        Arc::new(qq::Qq::default()),
        Arc::new(weixin::Weixin::default()),
    ]
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // openlark_client 在 INFO 级别会输出带 access_key / ticket 的
                // 飞书 wss 地址；日志可在设置页查看，需压到 warn 避免泄露凭据。
                tracing_subscriber::EnvFilter::new("info,openlark_client=warn")
            }),
        )
        .init();
    feishu::install_rustls_provider();
    let config = tiangong_plugin_sidecar::SidecarConfig::new("bot");
    tiangong_plugin_sidecar::run(config, || {
        let service = Arc::new(service::BotService::from_env()?);
        service.start();
        Ok(service)
    })
    .await
}
