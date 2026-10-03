//! `tiangong bot start`：在后台启动已配置的 Bot。
//!
//! Bot 的安装、配置（含扫码授权）、停止、升级、日志与推送目标统一在
//! `tiangong config` 配置页的「Bot」分区完成；命令行只保留启动，供无界面
//! 服务器在开机脚本或 systemd 中拉起 Bot。启动的 Bot 作为独立后台进程运行
//! （setsid 脱离会话），不随 CLI 退出而停止。

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use tiangong_bots::{BotId, BotRuntime, BotStore};

use crate::args::{BotArgs, BotSubcommand};
use crate::bot_ops;

pub(crate) fn run_bot_command(args: BotArgs) -> Result<()> {
    // 桌面应用运行时由桌面端监督 Bot，避免两边各自拉起同一个 Bot。
    if bot_ops::desktop_running() {
        return Err(anyhow!(bot_ops::DESKTOP_RUNNING_MESSAGE));
    }
    let store = Arc::new(BotStore::new().context("加载 bot 配置失败")?);
    let runtime = BotRuntime::new(store.clone()).context("构造 bot 运行时失败")?;
    // BotRuntime 的 MCP 注册是 async 方法，需要独立的 tokio runtime 驱动。
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("创建 tokio runtime 失败")?;
    match args.command {
        BotSubcommand::Start { id } => rt.block_on(cmd_start(&store, &runtime, id)),
    }
}

async fn cmd_start(store: &BotStore, runtime: &BotRuntime, id: String) -> Result<()> {
    let id = BotId::try_from(id.as_str()).map_err(|error| anyhow!("Bot ID 非法：{error}"))?;
    let bot = store.get(&id).ok_or_else(|| {
        anyhow!("bot 不存在：{id}，请先运行 `tiangong config` 在「Bot」分区安装并配置")
    })?;

    // 已在运行则拒绝（基于进程记录跨进程判断，避免重复拉起）。
    if tiangong_bots::pid::is_running(&id) {
        return Err(anyhow!("bot 已在运行：{id}"));
    }

    let server_reachable = bot_ops::spawn_daemon(&bot)
        .map_err(|error| anyhow!("{error:#}，请先运行 `tiangong config` 在「Bot」分区安装"))?;
    println!("bot 已在后台启动：{id}");
    if !server_reachable {
        println!("当前未检测到天工 Server，Bot 暂时无法调用 Agent。");
        println!("请执行：tiangong server --daemon");
    }
    // 注册 Bot 声明的 MCP（不支持则静默跳过）；失败不阻断启动。
    match bot_ops::ensure_mcp_registered(runtime, &id).await {
        Ok(Some(message)) => println!("✅ {message}"),
        Ok(None) => {}
        Err(error) => {
            eprintln!("⚠️ MCP 注册失败：{error:#}（Bot 已启动，但 Agent 暂无法调用其工具）")
        }
    }
    Ok(())
}
