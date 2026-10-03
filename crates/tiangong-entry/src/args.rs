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
    #[command(about = "Bot 制品管理（下载/配置/安装/升级/启停）")]
    Bot(BotArgs),
    #[command(
        about = "打开网页配置页（模型 / Server / 通用 / Prompt / 插件管理与插件配置）；配合 --host 可远程配置服务器"
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

/// 网页配置页参数（与 `tiangong-memory-sidecar --config` 一致）。
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
    /// 固定访问令牌（至少 8 个字符；缺省每次随机生成一次性令牌）
    #[arg(long)]
    pub(crate) token: Option<String>,
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
    /// 查看已注册 bot 与已安装制品（含健康状态）
    #[command(about = "查看已注册 bot 与已安装制品")]
    List,
    /// 查看线上 bots-index 可安装制品
    #[command(about = "查看线上可安装的 bot 制品")]
    Available,
    /// 下载并安装 bot 制品（不自动注册配置）
    #[command(about = "下载并安装 bot 制品（不自动注册配置）")]
    Install {
        /// 制品 ID（如 feishu）
        #[arg(help = "制品 ID（如 feishu）")]
        artifact_id: String,
        /// bot 实例 ID（默认与制品 ID 相同）
        #[arg(long, help = "bot 实例 ID，默认与制品 ID 相同")]
        id: Option<String>,
        /// 指定版本（默认最新）
        #[arg(long, help = "指定版本，默认最新")]
        version: Option<String>,
    },
    /// 交互式配置 bot（扫码授权或手工填写凭证），配置完成自动启动
    #[command(about = "交互式配置 bot（扫码或手工填凭证），完成后自动启动")]
    Configure {
        /// bot 实例 ID
        #[arg(help = "bot 实例 ID")]
        id: String,
    },
    /// 查看单个 bot 详情（配置脱敏）
    #[command(about = "查看单个 bot 详情（配置脱敏）")]
    Show {
        #[arg(help = "bot 实例 ID")]
        id: String,
    },
    /// 在后台启动 bot（独立进程，不随 CLI 退出而停止）
    #[command(about = "在后台启动 bot 进程")]
    Start {
        #[arg(help = "bot 实例 ID")]
        id: String,
    },
    /// 停止 bot
    #[command(about = "停止 bot 进程")]
    Stop {
        #[arg(help = "bot 实例 ID")]
        id: String,
    },
    /// 升级 bot 到最新版本（停止 → 下载 → 写版本，运行中则自动恢复运行）
    #[command(about = "升级 bot 到最新版本")]
    Upgrade {
        #[arg(help = "bot 实例 ID")]
        id: String,
    },
    /// 检查是否有更新（不安装），不传 artifact_id 则检查全部已安装制品
    #[command(about = "检查是否有更新（不安装）")]
    CheckUpdate {
        #[arg(help = "制品 ID，不传则检查全部已安装制品")]
        artifact_id: Option<String>,
    },
    /// 删除 bot 配置（若运行中则先停止，保留已安装制品）
    #[command(about = "删除 bot 配置（保留已安装制品）")]
    Remove {
        #[arg(help = "bot 实例 ID")]
        id: String,
    },
    /// 查看 bot 日志尾部
    #[command(about = "查看 bot 日志尾部")]
    Log {
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
}
