mod commands;
mod completion;
mod input;
mod modal;
mod output;
mod repl;
pub mod web_config;

pub fn run_cli() -> anyhow::Result<()> {
    repl::run(None)
}

pub fn run_cli_with_trust_mode(
    trust_mode: Option<tiangong_core::permission::TrustMode>,
) -> anyhow::Result<()> {
    repl::run(trust_mode)
}
