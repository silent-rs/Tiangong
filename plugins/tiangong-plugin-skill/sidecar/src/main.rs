//! Skill 独立 sidecar 进程。
//!
//! 作为 Skill 的唯一常驻进程运行，承载 skill 注册表扫描、skill.toml 读写、
//! SKILL.md 加载、环境变量解析与审计日志，通过 TCP IPC 暴露给运行时访问。
//!
//! 工作流程：
//! 1. 竞争单例（淘汰制：已有健康实例则优雅退出）
//! 2. 成为唯一实例 → 起 IPC server + SkillService → 阻塞运行
//!
//! 单例与 IPC 由 `tiangong-plugin-sidecar` 通用运行库提供，运行时按 `plugin.json`
//! 启动本进程。

mod env_local;
mod mcp_lock;
mod paths;
mod registry;
mod service;
mod skill_init;

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
        business_protocol = tiangong_plugin_skill_protocol::SKILL_PROTOCOL_VERSION,
        "skill sidecar 启动中..."
    );

    let config = tiangong_plugin_sidecar::SidecarConfig::new("skill");
    tiangong_plugin_sidecar::run(config, || {
        Ok(std::sync::Arc::new(service::SkillService::new()?))
    })
    .await
}
