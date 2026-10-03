use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "tiangong",
    version,
    disable_help_subcommand = true,
    arg_required_else_help = false,
    about = "天工应用入口"
)]
pub(crate) struct MainArgs {
    #[command(subcommand)]
    pub(crate) command: Option<MainCommand>,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum MainCommand {
    #[command(about = "启动桌面 UI")]
    Ui,
    #[command(about = "启动 CLI 模式")]
    Cli {
        /// 信任模式（full_trust / supervised）
        #[arg(long = "trust-mode", value_enum, help = "强制指定信任模式")]
        trust_mode: Option<TrustModeArg>,
    },
    #[command(about = "启动 Server 模式")]
    Server(ServerArgs),
    #[command(
        about = "在后台启动已配置的 Bot；安装、配置、停止、升级与日志请使用 `tiangong config`"
    )]
    Bot(BotArgs),
    #[command(
        about = "打开网页配置页（智能体 / 模型 / Server / Bot / 插件管理与插件配置）；配合 --host 可远程配置服务器"
    )]
    Config(ConfigArgs),
    #[command(about = "检查并安装天工更新")]
    Update(UpdateArgs),
}

#[derive(Debug, Args)]
pub(crate) struct UpdateArgs {
    /// 只检查更新，不安装
    #[arg(long, help = "只检查更新，不安装")]
    pub(crate) check: bool,
    /// 更新源地址，默认使用 GitHub Release updater JSON
    #[arg(long, help = "覆盖更新源地址")]
    pub(crate) endpoint: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ServerArgs {
    #[command(subcommand)]
    pub(crate) command: Option<ServerSubcommand>,
    /// 监听地址（不传时使用 server.json 保存值，再回退 127.0.0.1）
    #[arg(long, help = "监听地址，覆盖 server.json 保存值")]
    pub(crate) host: Option<String>,
    /// 监听端口（不传时使用 server.json 保存值，再回退 8080）
    #[arg(long, help = "监听端口，覆盖 server.json 保存值")]
    pub(crate) port: Option<u16>,
    /// API 认证 Token（不传时使用 server.json 保存值）
    #[arg(long, help = "API 认证 Token，覆盖 server.json 保存值")]
    pub(crate) token: Option<String>,
    /// 后台运行
    #[arg(short, long, help = "后台运行")]
    pub(crate) daemon: bool,
    /// 信任模式
    #[arg(long = "trust-mode", value_enum, help = "强制指定信任模式")]
    pub(crate) trust_mode: Option<TrustModeArg>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ServerSubcommand {
    #[command(about = "停止后台 Server")]
    Stop,
}

/// 网页配置页参数。
#[derive(Debug, Args)]
pub(crate) struct ConfigArgs {
    /// 监听地址（缺省 127.0.0.1；远程配置可设为 0.0.0.0 或本机网卡地址）
    #[arg(long, default_value = "127.0.0.1")]
    pub(crate) host: String,
    /// 监听端口（缺省随机）
    #[arg(long)]
    pub(crate) port: Option<u16>,
    /// 不自动打开浏览器，仅打印访问地址（远程/无图形环境使用）
    #[arg(long)]
    pub(crate) no_open: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum TrustModeArg {
    /// 完全信任（工具自动执行，无审批）
    FullTrust,
    /// 监督模式（高风险工具需要用户审批）
    Supervised,
}

impl TrustModeArg {
    pub(crate) fn to_trust_mode(self) -> tiangong_core::permission::TrustMode {
        match self {
            TrustModeArg::FullTrust => tiangong_core::permission::TrustMode::FullTrust,
            TrustModeArg::Supervised => tiangong_core::permission::TrustMode::Supervised,
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct BotArgs {
    #[command(subcommand)]
    pub(crate) command: BotSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum BotSubcommand {
    /// 在后台启动已配置的 bot（独立进程，不随 CLI 退出而停止）
    #[command(about = "在后台启动已配置的 bot 进程")]
    Start {
        #[arg(help = "bot 实例 ID")]
        id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<MainArgs, clap::Error> {
        MainArgs::try_parse_from(std::iter::once("tiangong").chain(args.iter().copied()))
    }

    #[test]
    fn config_has_no_subcommands() {
        let args = parse(&["config", "--host", "0.0.0.0", "--port", "8800", "--no-open"])
            .expect("config 参数");
        let Some(MainCommand::Config(config)) = args.command else {
            panic!("应解析为 config");
        };
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, Some(8800));
        assert!(config.no_open);
        assert!(
            parse(&["config", "--token", "abcdefgh"]).is_err(),
            "访问令牌已移除"
        );
        assert!(parse(&["config", "web"]).is_err(), "web 子命令已移除");
        assert!(parse(&["config", "show"]).is_err(), "show 子命令已移除");
    }

    #[test]
    fn removed_config_commands_are_rejected() {
        for removed in [
            "model", "mcp", "skill", "memory", "prompt", "doctor", "plugin",
        ] {
            assert!(parse(&[removed]).is_err(), "{removed} 应已移除");
        }
        assert!(parse(&["server", "configure"]).is_err());
        assert!(parse(&["server", "token", "show"]).is_err());
        assert!(parse(&["server", "status"]).is_err());
        assert!(parse(&["server", "stop"]).is_ok());
        assert!(parse(&["server", "-d", "--port", "9000"]).is_ok());
    }

    #[test]
    fn bot_only_keeps_start() {
        assert!(parse(&["bot", "start", "feishu"]).is_ok());
        for removed in [
            &["bot", "configure", "feishu"][..],
            &["bot", "list"],
            &["bot", "available"],
            &["bot", "install", "feishu"],
            &["bot", "show", "feishu"],
            &["bot", "stop", "feishu"],
            &["bot", "upgrade", "feishu"],
            &["bot", "check-update"],
            &["bot", "remove", "feishu"],
            &["bot", "log", "feishu"],
        ] {
            assert!(parse(removed).is_err(), "{removed:?} 应已移入配置页");
        }
    }
}
