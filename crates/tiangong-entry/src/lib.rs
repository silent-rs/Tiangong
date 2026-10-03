mod args;
mod bot;
mod interactive;
mod server;
mod update;

use clap::Parser;
use clap::error::ErrorKind;

use self::args::{MainArgs, MainCommand};

pub fn run() -> anyhow::Result<()> {
    let args = match MainArgs::try_parse() {
        Ok(args) => args,
        Err(err) => {
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                print!("{err}");
                return Ok(());
            }
            return Err(anyhow::anyhow!(err.to_string()));
        }
    };
    match args.command {
        Some(MainCommand::Server(args)) => server::run_server_command(args),
        Some(MainCommand::Bot(args)) => bot::run_bot_command(args),
        Some(MainCommand::Config(args)) => {
            tiangong_cli::web_config::run(tiangong_cli::web_config::WebConfigOptions {
                host: args.host,
                port: args.port,
                open_browser: !args.no_open,
                initial_tab: None,
            })
        }
        Some(MainCommand::Update(args)) => update::run_update_command(args),
        Some(MainCommand::Cli { trust_mode }) => {
            tiangong_cli::run_cli_with_trust_mode(trust_mode.map(|m| m.to_trust_mode()))
        }
        None | Some(MainCommand::Ui) => Err(anyhow::anyhow!("UI 模式请通过 Tauri 桌面应用启动")),
    }
}
