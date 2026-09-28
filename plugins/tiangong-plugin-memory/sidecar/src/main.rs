//! Memory System 独立 sidecar 进程。
//!
//! 默认（无参数）作为天工插件 sidecar 运行：竞争 Leader，承载全部存储
//! （SQLite/tantivy/lancedb），通过 TCP IPC / stdio 暴露给宿主。
//!
//! 通用模式（面向第三方 Agent，与天工共享同一份记忆与 Leader）：
//! - `--mcp`：stdio MCP Server；
//! - `--daemon`：后台 HTTP REST 服务（`--stop` 停止，`--foreground` 前台运行）；
//! - `--config`：打开浏览器配置页，点击"完成并关闭"后退出；
//! - `--check-update` / `--update`：从天工官方插件目录检查 / 自更新。
//!
//! 见 RFC docs/memory-system/11-memory-sidecar-wasm-bridge.md 与
//! docs/memory-system/14-通用运行模式.md。

mod daemon;
mod http;
mod mcp;
mod service;
mod update;

use std::sync::Arc;

use clap::{ArgGroup, Parser};
use tiangong_memory::MemoryConfig;
use tiangong_memory::election::{LeaderState, ProcessType, start_or_connect_with_options};

use crate::service::MemoryService;

/// 天工 Memory：可独立运行的长期记忆服务。
#[derive(Debug, Parser)]
#[command(name = "tiangong-memory-sidecar", version, about)]
#[command(group(
    ArgGroup::new("mode")
        .args(["mcp", "daemon", "stop", "config", "check_update", "update"])
        .multiple(false)
))]
struct Cli {
    /// 以 stdio MCP Server 运行，供第三方 Agent 接入
    #[arg(long)]
    mcp: bool,
    /// 在后台启动 HTTP REST 服务（需设置 --token）；用 --stop 停止
    #[arg(long)]
    daemon: bool,
    /// 与 --daemon 一起使用：在前台运行，不转入后台（适合 systemd/launchd 托管）
    #[arg(long, requires = "daemon")]
    foreground: bool,
    /// 停止后台运行的 daemon
    #[arg(long)]
    stop: bool,
    /// 打开浏览器配置页，点击"完成并关闭"后退出；配合 --host 可远程配置
    #[arg(long)]
    config: bool,
    /// 与 --config 一起使用：不自动打开浏览器，仅打印地址
    #[arg(long, requires = "config")]
    no_open: bool,
    /// 检查是否有新版本（不修改任何文件）
    #[arg(long)]
    check_update: bool,
    /// 从天工官方插件目录下载新版本并替换当前程序
    #[arg(long)]
    update: bool,
    /// --daemon / --config 监听地址；远程访问可设为 0.0.0.0 或本机网卡地址
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// --daemon / --config 监听端口（0 表示随机端口；缺省 daemon 为 7717，config 随机）
    #[arg(long)]
    port: Option<u16>,
    /// daemon 必填、--config 可选（缺省生成一次性令牌）的访问令牌，至少 8 个字符
    #[arg(long, env = "TIANGONG_MEMORY_TOKEN", hide_env_values = true)]
    token: Option<String>,
}

const DEFAULT_DAEMON_PORT: u16 = 7717;

impl Cli {
    fn standalone(&self) -> bool {
        self.mcp || self.daemon || self.stop || self.config || self.check_update || self.update
    }

    /// 校验 --host / --port / --token 只用于监听 HTTP 的模式。
    fn validate(&self) -> anyhow::Result<()> {
        let listens = self.daemon || self.config;
        if !listens && (self.port.is_some() || self.host != "127.0.0.1") {
            anyhow::bail!("--host / --port 只能与 --daemon 或 --config 一起使用");
        }
        Ok(())
    }

    fn port(&self) -> u16 {
        self.port
            .unwrap_or(if self.daemon { DEFAULT_DAEMON_PORT } else { 0 })
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    cli.validate()?;
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // 通用模式默认安静，避免干扰 MCP 客户端日志面板与终端输出。
                let default = if cli.standalone() { "warn" } else { "info" };
                tracing_subscriber::EnvFilter::new(default)
            }),
        )
        .init();

    // 不需要记忆运行时的命令先处理，且必须在创建 tokio runtime 之前
    // 完成后台进程派生，避免 fork 多线程进程。
    if cli.stop {
        return run_stop();
    }
    if cli.daemon && !cli.foreground {
        let token = daemon::require_token(cli.token.clone())?;
        let info = daemon::start_background(&cli.host, cli.port(), &token)?;
        println!("daemon 已在后台启动");
        println!("  pid : {}", info.pid);
        println!("  地址: {}/api/v1", info.url);
        println!("  日志: {}", daemon::log_path().display());
        println!("停止：tiangong-memory-sidecar --stop");
        return Ok(());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> anyhow::Result<()> {
    if cli.check_update || cli.update {
        return run_update(cli.update).await;
    }

    if let Some(plugin_data_dir) =
        std::env::var_os("TIANGONG_PLUGIN_DATA_DIR").filter(|value| !value.is_empty())
    {
        tiangong_memory::recover_plugin_data_dir(std::path::Path::new(&plugin_data_dir))?;
    }

    if cli.standalone() {
        return run_standalone(cli).await;
    }
    run_plugin_sidecar().await
}

fn run_stop() -> anyhow::Result<()> {
    match daemon::stop()? {
        Some(info) => println!("daemon 已停止（pid={}，{}）", info.pid, info.url),
        None => println!("没有运行中的 daemon"),
    }
    Ok(())
}

async fn run_update(apply: bool) -> anyhow::Result<()> {
    use update::UpdateStatus;

    let updater = update::SelfUpdater::official()?;
    let status = if apply {
        updater.update_current().await?
    } else {
        updater.check().await?
    };
    match status {
        UpdateStatus::UpToDate { current } => println!("已是最新版本 {current}"),
        UpdateStatus::Available { current, version } => {
            println!("发现新版本 {version}（当前 {current}），运行 --update 进行更新");
        }
        UpdateStatus::Updated {
            previous,
            version,
            path,
        } => {
            println!("已从 {previous} 更新到 {version}：{}", path.display());
            if let Some(info) = daemon::running() {
                println!(
                    "后台 daemon（pid={}）仍在运行旧版本，请执行 --stop 后重新 --daemon 启动",
                    info.pid
                );
            }
        }
    }
    Ok(())
}

/// 通用模式：接入（或成为）Memory Leader 后提供对应入口。
///
/// 与天工同时运行时本进程为 Follower，经 IPC 访问天工的 Leader，
/// Leader 退出后由选举监控自动接替，记忆始终只有一个写入者。
async fn run_standalone(cli: Cli) -> anyhow::Result<()> {
    // daemon 前台模式同样要求用户设置 token（后台模式在派生前已校验）。
    let daemon_token = if cli.daemon {
        Some(daemon::require_token(cli.token.clone())?)
    } else {
        None
    };
    let options = MemoryConfig::load_or_default().to_options();
    let managed = start_or_connect_with_options(options, ProcessType::Sidecar).await?;
    tracing::info!(state = ?managed.state(), "memory 通用模式已接入");
    let service = MemoryService::new(Arc::new(managed));
    let port = cli.port();
    if cli.mcp {
        mcp::serve_stdio(service).await
    } else if let Some(token) = daemon_token {
        http::run_daemon(
            service,
            http::DaemonOptions {
                host: cli.host,
                port,
                token,
            },
        )
        .await
    } else {
        http::run_config(
            service,
            http::ConfigOptions {
                host: cli.host,
                port,
                open_browser: !cli.no_open,
                token: cli.token,
            },
        )
        .await
    }
}

/// 天工插件 sidecar 模式（原有行为）。
async fn run_plugin_sidecar() -> anyhow::Result<()> {
    tracing::info!(
        business_protocol = tiangong_plugin_memory_protocol::MEMORY_PROTOCOL_VERSION,
        "memory sidecar 启动中..."
    );
    // 加载 memory 配置（模型端点、embedding 等）。
    let options = MemoryConfig::load_or_default().to_options();
    // 竞争 Leader lease。抢到就起 actor + IPC server，没抢到就当 follower。
    let managed = start_or_connect_with_options(options, ProcessType::Sidecar).await?;

    // stdio 传输：宿主一对一管理生命周期，无论本进程是 Leader 还是 Follower
    // 都必须应答——外部 `--daemon` / `--mcp` 可能先占住 Leader，此时经 IPC
    // 转发，Leader 退出后由选举监控自动接替。
    if tiangong_plugin_sidecar::stdio::stdio_requested() {
        match managed.state() {
            LeaderState::Leader => tracing::info!("memory sidecar 已成为 Leader，开始服务"),
            LeaderState::Follower { pid } => tracing::info!(
                "已有 memory Leader 运行中（pid={pid}），以 Follower 身份经 IPC 转发宿主请求"
            ),
        }
        let managed = Arc::new(managed);
        return tiangong_plugin_sidecar::stdio::run_stdio(move || {
            Ok(Arc::new(MemoryStdioService { managed }))
        })
        .await;
    }

    match managed.state() {
        LeaderState::Leader => {
            tracing::info!("memory sidecar 已成为 Leader，开始服务");
            // 阻塞等待终止信号（Ctrl+C / SIGTERM）。
            // ManagedMemory 持有 actor + IPC bridge + 心跳，Drop 时自动清理。
            wait_for_shutdown_signal().await?;
            tracing::info!("收到终止信号，memory sidecar 退出");
        }
        LeaderState::Follower { pid } => {
            tracing::info!("已有 memory Leader 运行中（pid={pid}），本 sidecar 无需重复启动，退出");
        }
    }

    // managed Drop：停心跳、删 endpoint 文件、释放 leader.lock
    drop(managed);
    Ok(())
}

/// stdio 传输适配：把通用帧协议接到 memory 的插件请求分发。
///
/// 每次请求从 `ManagedMemory` 取当前句柄：Follower 接替为 Leader 后
/// 句柄随之切换为本地 actor。
struct MemoryStdioService {
    managed: Arc<tiangong_memory::election::ManagedMemory>,
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for MemoryStdioService {
    async fn dispatch(
        &self,
        request: tiangong_plugin_runtime::protocol::Request,
    ) -> tiangong_plugin_runtime::protocol::Response {
        tiangong_memory::ipc::dispatch_checked_plugin_request(self.managed.handle(), request).await
    }
}

#[cfg(unix)]
pub(crate) async fn wait_for_shutdown_signal() -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {},
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) async fn wait_for_shutdown_signal() -> anyhow::Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("bin").chain(args.iter().copied()))
    }

    #[test]
    fn cli_parses_modes() {
        let cli = parse(&[]).expect("默认模式");
        assert!(!cli.standalone());
        assert!(parse(&["--mcp"]).expect("mcp").mcp);
        let cli = parse(&["--daemon", "--port", "0", "--token", "12345678"]).expect("daemon");
        assert!(cli.daemon && !cli.foreground);
        assert_eq!(cli.port(), 0);
        assert_eq!(cli.token.as_deref(), Some("12345678"));
        assert_eq!(parse(&["--daemon"]).unwrap().port(), DEFAULT_DAEMON_PORT);
        assert!(
            parse(&["--daemon", "--foreground"])
                .expect("前台")
                .foreground
        );
        assert!(parse(&["--stop"]).expect("stop").stop);
        let cli = parse(&["--config", "--no-open"]).expect("config");
        assert_eq!(cli.port(), 0, "配置页缺省随机端口");
        let cli = parse(&["--config", "--host", "0.0.0.0", "--port", "8800"]).expect("远程配置");
        assert!(cli.validate().is_ok());
        assert_eq!((cli.host.as_str(), cli.port()), ("0.0.0.0", 8800));
        assert!(parse(&["--check-update"]).expect("check").check_update);
        assert!(parse(&["--update"]).expect("update").update);
    }

    #[test]
    fn cli_rejects_invalid_combinations() {
        assert!(parse(&["--demon"]).is_err(), "--demon 已移除");
        assert!(parse(&["--mcp", "--daemon"]).is_err());
        assert!(parse(&["--daemon", "--stop"]).is_err());
        assert!(parse(&["--config", "--update"]).is_err());
        assert!(parse(&["--foreground"]).is_err());
        assert!(parse(&["--port", "1"]).unwrap().validate().is_err());
        assert!(
            parse(&["--mcp", "--host", "0.0.0.0"])
                .unwrap()
                .validate()
                .is_err()
        );
    }
}
