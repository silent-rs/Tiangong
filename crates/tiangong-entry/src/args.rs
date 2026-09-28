use clap::{Args, Parser, Subcommand, ValueEnum};

use tiangong_llm::ProviderProtocol;
use tiangong_plugin_mcp_protocol::config::McpTransportMode;

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
    #[command(about = "MCP 配置管理")]
    Mcp(McpArgs),
    #[command(about = "Bot 制品管理（下载/配置/安装/升级/启停）")]
    Bot(BotArgs),
    #[command(about = "模型配置管理（Provider / Model / Routing）")]
    Model(ModelArgs),
    #[command(about = "Memory 系统配置管理")]
    Memory(MemoryArgs),
    #[command(about = "Skill 配置管理")]
    Skill(SkillArgs),
    #[command(about = "自定义 Prompt 管理")]
    Prompt(PromptArgs),
    #[command(about = "通用配置查看与校验")]
    Config(ConfigArgs),
    #[command(about = "环境诊断")]
    Doctor(DoctorArgs),
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
    #[command(about = "查看 Server 运行状态")]
    Status,
    #[command(about = "交互式配置向导（引导完成监听地址与 Token）")]
    Configure,
    #[command(about = "管理 Server 监听配置")]
    Config {
        #[command(subcommand)]
        command: ServerConfigSubcommand,
    },
    #[command(about = "管理 Server 鉴权 Token")]
    Token {
        #[command(subcommand)]
        command: ServerTokenSubcommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ServerConfigSubcommand {
    #[command(about = "查看 Server 监听配置")]
    Show,
    #[command(about = "修改 Server 监听配置（host/port 可选）")]
    Set {
        #[arg(long, help = "监听地址")]
        host: Option<String>,
        #[arg(long, help = "监听端口")]
        port: Option<u16>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ServerTokenSubcommand {
    #[command(about = "查看当前 Token（脱敏）")]
    Show,
    #[command(about = "直接设置 Token")]
    Set {
        #[arg(help = "Token 值")]
        token: String,
    },
    #[command(about = "生成随机 Token 并写入配置")]
    Generate {
        #[arg(long, default_value_t = 32, help = "Token 长度（16-256）")]
        length: usize,
    },
}

#[derive(Debug, Args)]
pub(crate) struct McpArgs {
    #[command(subcommand)]
    pub(crate) command: McpSubcommand,
}

#[derive(Debug, Args)]
pub(crate) struct ModelArgs {
    #[command(subcommand)]
    pub(crate) command: ModelSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ModelSubcommand {
    #[command(about = "查看模型配置（providers / models / routes，默认全部）")]
    List {
        #[arg(help = "过滤范围：providers | models | routes")]
        scope: Option<String>,
    },
    #[command(about = "新增或覆盖模型供应商")]
    AddProvider {
        #[arg(help = "供应商名称")]
        name: String,
        #[arg(long, value_parser = parse_protocol, help = "协议（openai_chatcompletions / anthropic / deepseek）")]
        protocol: ProviderProtocol,
        #[arg(long = "base-url", help = "API base URL")]
        base_url: String,
        #[arg(
            long = "api-key",
            conflicts_with = "api_key_env",
            help = "明文 API Key（不推荐，建议用 --api-key-env）"
        )]
        api_key: Option<String>,
        #[arg(
            long = "api-key-env",
            conflicts_with = "api_key",
            help = "API Key 环境变量名（写入为 ${VAR} 模板）"
        )]
        api_key_env: Option<String>,
        #[arg(
            long = "timeout-ms",
            default_value_t = 60_000,
            help = "请求超时（毫秒）"
        )]
        timeout_ms: u64,
    },
    #[command(about = "删除模型供应商（--force 连同引用它的模型和路由一并删除）")]
    RemoveProvider {
        #[arg(help = "供应商名称")]
        name: String,
        #[arg(long, help = "强制删除，连同引用该供应商的模型与路由")]
        force: bool,
    },
    #[command(about = "新增或覆盖模型")]
    AddModel {
        #[arg(help = "模型名称（本地别名）")]
        name: String,
        #[arg(long, help = "所属供应商名称")]
        provider: String,
        #[arg(long = "model-id", help = "供应商侧的模型 ID")]
        model_id: String,
        #[arg(
            long = "capability",
            help = "模型能力（可重复）：chat/multimodal/image_generation/video_generation/stt/tts（embedding/rerank 已迁至 `tiangong memory config` 配置页）"
        )]
        capability: Vec<String>,
    },
    #[command(about = "删除模型")]
    RemoveModel {
        #[arg(help = "模型名称（本地别名）")]
        name: String,
    },
    #[command(about = "交互式配置向导（引导完成 provider → model → route）")]
    Configure,
    #[command(about = "设置路由槽位指向的模型")]
    Route {
        #[command(subcommand)]
        command: RouteSubcommand,
    },
    #[command(about = "校验模型配置结构（路由引用、provider 存在性等）")]
    Validate,
    #[command(about = "测试模型连通性（真实请求 /models）")]
    Test {
        #[arg(help = "测试目标：capability（chat/lite/...）或模型名；默认 chat")]
        target: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum RouteSubcommand {
    #[command(about = "查看当前路由表")]
    List,
    #[command(about = "设置 capability 路由指向某个已注册模型")]
    Set {
        #[arg(help = "能力槽位：chat/lite/multimodal/image_generation/video_generation/stt/tts")]
        capability: String,
        #[arg(help = "模型名称（本地别名）")]
        model: String,
    },
}

#[derive(Debug, Args)]
pub(crate) struct MemoryArgs {
    #[command(subcommand)]
    pub(crate) command: MemorySubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum MemorySubcommand {
    #[command(about = "打开 Memory 配置页（模型、检索与数据管理）；配合 --host 可远程配置")]
    Config(MemoryConfigArgs),
    #[command(about = "启用 Memory")]
    Enable,
    #[command(about = "禁用 Memory")]
    Disable,
    #[command(about = "查看 Memory 状态")]
    Status,
    #[command(about = "测试 Memory 模型连通性")]
    Test,
}

#[derive(Debug, Args)]
pub(crate) struct MemoryConfigArgs {
    /// 监听地址（缺省 127.0.0.1；远程配置可设为 0.0.0.0 或本机网卡地址）
    #[arg(long)]
    pub(crate) host: Option<String>,
    /// 监听端口（缺省随机）
    #[arg(long)]
    pub(crate) port: Option<u16>,
    /// 不自动打开浏览器，仅打印访问地址（远程/无图形环境使用）
    #[arg(long)]
    pub(crate) no_open: bool,
}

/// 解析 ProviderProtocol 字符串
fn parse_protocol(raw: &str) -> Result<ProviderProtocol, String> {
    raw.parse::<ProviderProtocol>()
        .map_err(|e| format!("无效的协议 {raw}：{e}"))
}

#[derive(Debug, Args)]
pub(crate) struct SkillArgs {
    #[command(subcommand)]
    pub(crate) command: SkillSubcommand,
}

#[derive(Debug, Args)]
pub(crate) struct PromptArgs {
    #[command(subcommand)]
    pub(crate) command: PromptSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum PromptSubcommand {
    #[command(about = "查看当前自定义 Prompt")]
    Show,
    #[command(about = "设置自定义 Prompt（直接传文本或用 --file 从文件读取）")]
    Set {
        /// Prompt 文本（与 --file 互斥）
        #[arg(help = "Prompt 文本")]
        text: Option<String>,
        /// 从文件读取 Prompt 内容
        #[arg(long = "file", value_name = "PATH", conflicts_with = "text")]
        file: Option<String>,
    },
    #[command(about = "通过 $EDITOR 编辑自定义 Prompt")]
    Edit,
    #[command(about = "清空自定义 Prompt")]
    Clear,
    #[command(about = "显示自定义 Prompt 存储路径")]
    Path,
}

#[derive(Debug, Args)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    pub(crate) command: ConfigSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigSubcommand {
    #[command(about = "列出全部配置文件路径")]
    Path,
    #[command(about = "配置概览（不展开 JSON）")]
    Show,
    #[command(about = "校验本地配置结构（不做外部连通性测试）")]
    Validate,
}

#[derive(Debug, Args)]
pub(crate) struct DoctorArgs {
    /// 深度诊断：执行模型连通性与端口探活（可能较慢）
    #[arg(long, help = "深度诊断（含模型连通性与端口探活）")]
    pub(crate) deep: bool,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum McpTransportArg {
    Auto,
    Stdio,
    Http,
}

impl From<McpTransportArg> for McpTransportMode {
    fn from(value: McpTransportArg) -> Self {
        match value {
            McpTransportArg::Auto => McpTransportMode::Auto,
            McpTransportArg::Stdio => McpTransportMode::Stdio,
            McpTransportArg::Http => McpTransportMode::Http,
        }
    }
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum McpSubcommand {
    #[command(about = "查看全部 MCP server")]
    List,
    #[command(about = "查看指定 MCP server（不传 name 时等同 list）")]
    Show {
        #[arg(help = "MCP server 名称")]
        name: Option<String>,
    },
    #[command(about = "注册 MCP server")]
    Add {
        #[arg(help = "MCP server 名称（非 JSON 导入模式必填）")]
        name: Option<String>,
        #[arg(help = "MCP server 命令（如 npx；HTTP 可直接填 endpoint）")]
        command: Option<String>,
        #[arg(
            long,
            help = "通过 JSON 字符串导入（支持单对象或 {\"mcpServers\": {...}}）",
            conflicts_with = "json_file"
        )]
        json: Option<String>,
        #[arg(
            long = "json-file",
            value_name = "PATH",
            help = "通过 JSON 文件导入（支持单对象或 {\"mcpServers\": {...}}）",
            conflicts_with = "json"
        )]
        json_file: Option<String>,
        #[arg(long, default_value_t = false, help = "同名 server 存在时覆盖")]
        force: bool,
        #[arg(
            long = "arg",
            short = 'a',
            allow_hyphen_values = true,
            help = "命令参数，可重复，如 -a -y -a @modelcontextprotocol/server-browser"
        )]
        args: Vec<String>,
        #[arg(long, value_delimiter = ',', help = "标签列表，逗号分隔")]
        tags: Vec<String>,
        #[arg(long, value_enum, help = "传输类型（auto/stdio/http）")]
        transport: Option<McpTransportArg>,
        #[arg(long, help = "HTTP MCP endpoint（如 https://example.com/mcp）")]
        endpoint: Option<String>,
        #[arg(long, help = "HTTP MCP Bearer Token（不带 Bearer 前缀）")]
        auth_header: Option<String>,
        #[arg(
            long = "header",
            value_parser = parse_key_value,
            help = "HTTP header，格式 key=value，可重复"
        )]
        headers: Vec<(String, String)>,
        #[arg(
            long = "env",
            value_parser = parse_key_value,
            help = "stdio env，格式 key=value，可重复"
        )]
        env: Vec<(String, String)>,
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "CMDLINE",
            help = "通过 -- 传入完整命令，如 -- npx -y @modelcontextprotocol/server-filesystem /path"
        )]
        cmdline: Vec<String>,
        #[arg(long, default_value_t = true, help = "是否启用（true/false）")]
        enabled: bool,
    },
    #[command(about = "删除 MCP server")]
    Remove {
        #[arg(help = "MCP server 名称")]
        name: String,
    },
    #[command(about = "启用 MCP server")]
    Enable {
        #[arg(help = "MCP server 名称")]
        name: String,
    },
    #[command(about = "禁用 MCP server")]
    Disable {
        #[arg(help = "MCP server 名称")]
        name: String,
    },
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum SkillSubcommand {
    #[command(about = "查看全部 Skill")]
    List,
    #[command(about = "查看指定 Skill（不传 id 时等同 list）")]
    Show {
        #[arg(help = "Skill ID")]
        id: Option<String>,
    },
    #[command(
        about = "初始化 Skill 脚手架（生成 SKILL.md 与 skill.toml）",
        visible_alias = "create"
    )]
    Init {
        #[arg(help = "目标目录")]
        path: String,
        #[arg(long, help = "Skill 名称")]
        name: Option<String>,
        #[arg(long, help = "Skill ID")]
        id: Option<String>,
        #[arg(long, default_value_t = false, help = "存在同名文件时是否覆盖")]
        force: bool,
    },
    #[command(about = "删除 Skill")]
    Remove {
        #[arg(help = "Skill ID")]
        id: String,
    },
    #[command(about = "启用 Skill")]
    Enable {
        #[arg(help = "Skill ID")]
        id: String,
    },
    #[command(about = "禁用 Skill")]
    Disable {
        #[arg(help = "Skill ID")]
        id: String,
    },
    #[command(about = "刷新 Skill 注册表（重扫 skills/<id>/）")]
    Refresh,
    #[command(about = "校验配置")]
    Validate,
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

pub(crate) fn parse_key_value(raw: &str) -> Result<(String, String), String> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("参数格式无效（需 key=value）：{raw}"))?;
    let key = key.trim();
    let value = value.trim();
    if key.is_empty() || value.is_empty() {
        return Err(format!("参数格式无效（key/value 不能为空）：{raw}"));
    }
    Ok((key.to_string(), value.to_string()))
}
