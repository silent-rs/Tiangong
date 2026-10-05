//! `tiangong-relay`：天工远程访问中继服务（纯中继，部署时无需任何令牌）。
//!
//! ```text
//! tiangong-relay --listen 0.0.0.0:8790
//! ```
//!
//! 桌面端用自己生成的通道密钥接入，中继只负责转发。公网部署时请置于 HTTPS
//! 反向代理之后（手机扫码打开的地址应为 https）。安装、升级与守护进程见
//! `scripts/relay/install.sh`。
use std::net::SocketAddr;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "tiangong-relay", version, about = "天工远程访问中继")]
struct Args {
    /// 监听地址
    #[arg(long, env = "TIANGONG_RELAY_LISTEN", default_value = "0.0.0.0:8790")]
    listen: SocketAddr,
    /// 最多同时接入的天工桌面端数量
    #[arg(
        long,
        env = "TIANGONG_RELAY_MAX_AGENTS",
        default_value_t = tiangong_relay::DEFAULT_MAX_AGENTS
    )]
    max_agents: usize,
    /// HTTPS 证书（PEM，含完整证书链）；与 --tls-key 同时提供时中继直接以 HTTPS 监听
    #[arg(long, env = "TIANGONG_RELAY_TLS_CERT", requires = "tls_key")]
    tls_cert: Option<std::path::PathBuf>,
    /// HTTPS 私钥（PEM）
    #[arg(long, env = "TIANGONG_RELAY_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<std::path::PathBuf>,
}

/// 等待 Ctrl-C 或 SIGTERM（systemd 停止服务时发送）。
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("收到停止信号，正在退出");
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    if args.max_agents == 0 {
        anyhow::bail!("--max-agents 至少为 1");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let options = tiangong_relay::RelayOptions {
        max_agents: args.max_agents,
    };
    runtime.block_on(async move {
        match args.tls_cert.zip(args.tls_key) {
            Some((cert, key)) => {
                tiangong_relay::serve_tls(
                    args.listen,
                    tiangong_relay::TlsFiles { cert, key },
                    options,
                    shutdown_signal(),
                )
                .await
            }
            None => tiangong_relay::serve(args.listen, options, shutdown_signal()).await,
        }
    })
}
