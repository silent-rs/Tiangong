//! Scheduler 独立 sidecar 进程。
//!
//! 作为 Scheduler 的唯一常驻进程运行，承载 cron 调度、JobStore 与到点 HTTP 投递，
//! 并通过 TCP IPC 暴露给运行时访问。
//!
//! 工作流程：
//! 1. 竞争单例（淘汰制：已有健康实例则优雅退出）
//! 2. 成为唯一实例 → 起 IPC server + SchedulerService → 阻塞运行
//! 3. 到点触发时经 HTTP 调本机 server 的 `POST /api/v1/messages` 投递消息
//!
//! 单例与 IPC 由 `tiangong-plugin-sidecar` 通用运行库提供，运行时按 `plugin.json`
//! 启动本进程。

mod migration;
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
        business_protocol = tiangong_plugin_scheduler_protocol::SCHEDULER_PROTOCOL_VERSION,
        "scheduler sidecar 启动中..."
    );

    let config = tiangong_plugin_sidecar::SidecarConfig::new("scheduler");
    tiangong_plugin_sidecar::run(config, || {
        Ok(std::sync::Arc::new(service::SchedulerService::new()?))
    })
    .await
}
