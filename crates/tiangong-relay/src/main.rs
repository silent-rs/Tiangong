//! `tiangong-relay`：天工远程访问中继服务。
//!
//! ```text
//! TIANGONG_RELAY_TOKEN=<至少16位随机串> tiangong-relay --listen 0.0.0.0:8790
//! ```
//!
//! 公网部署时请置于 HTTPS 反向代理之后（手机扫码打开的地址应为 https）。

use std::net::SocketAddr;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "tiangong-relay", version, about = "天工远程访问中继")]
struct Args {
    /// 监听地址
    #[arg(long, env = "TIANGONG_RELAY_LISTEN", default_value = "0.0.0.0:8790")]
    listen: SocketAddr,
    /// 桌面端接入令牌（与天工设置中填写的一致，至少 16 个字符）
    #[arg(long, env = "TIANGONG_RELAY_TOKEN", hide_env_values = true)]
    token: String,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    tiangong_relay::validate_token(&args.token)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(tiangong_relay::serve(args.listen, args.token))
}
