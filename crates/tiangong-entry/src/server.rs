use anyhow::Result;

use tiangong_config::load_server_config;

use crate::args::{ServerArgs, ServerSubcommand};

/// `tiangong server`：只负责启停；监听地址与 Token 在 `tiangong config` 网页中配置。
pub(crate) fn run_server_command(args: ServerArgs) -> Result<()> {
    if let Some(ServerSubcommand::Stop) = args.command {
        return tiangong_server::stop_daemon();
    }

    // 合并启动参数：命令行 > server.json 保存值 > 默认值
    let saved = load_server_config();
    let host = args.host.unwrap_or(saved.host);
    let port = args.port.unwrap_or(saved.port);
    let token = args.token.or(saved.auth_token);

    // daemon 模式：后台启动并退出主进程
    if args.daemon {
        return tiangong_server::run_daemon(&host, port, token);
    }

    // 前台运行
    tiangong_server::run_server(&host, port, token)
}
