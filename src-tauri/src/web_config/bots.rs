//! 配置页的 Bot 管理命令（与桌面端 `bot_*` 命令同名同参，前端 BotPanel 直接复用）。
//!
//! 与桌面端的区别：配置页是临时服务，关闭即退出，因此启动的 Bot 以独立后台
//! 进程运行（与 `tiangong bot start` 一致），不随配置页退出而停止；启停时同步
//! 注册 / 注销 Bot 的 MCP。桌面应用运行时由桌面端监督 Bot，配置页拒绝写操作，
//! 避免两边各自拉起或停止同一个 Bot。

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use tiangong_bots::{BotId, BotRuntime, BotStore, QrSession, RegisterBotRequest, UpdateBotRequest};
use tiangong_entry::bot_ops;

use super::commands::{parse, to_json, CommandError, CommandResult};

/// 配置页内的 Bot 运行时（每次请求从磁盘读取 `bots.json`）。
pub(crate) struct BotContext {
    store: Arc<BotStore>,
    runtime: Arc<BotRuntime>,
}

impl BotContext {
    pub(crate) fn new() -> anyhow::Result<Self> {
        let store = Arc::new(BotStore::new()?);
        let runtime = Arc::new(BotRuntime::new(store.clone())?);
        Ok(Self { store, runtime })
    }
}

#[derive(Deserialize)]
struct IdArgs {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BotIdArgs {
    bot_id: String,
}

fn bot_id(raw: impl Into<String>) -> Result<BotId, CommandError> {
    BotId::try_from(raw.into()).map_err(|error| CommandError::bad(error.to_string()))
}

fn require_bot(ctx: &BotContext, id: &BotId) -> Result<tiangong_bots::BotConfig, CommandError> {
    ctx.store
        .get(id)
        .ok_or_else(|| CommandError::bad(format!("bot 不存在：{id}")))
}

/// 桌面应用运行时拒绝写操作。
fn ensure_writable() -> Result<(), CommandError> {
    if bot_ops::desktop_running() {
        return Err(CommandError::bad(bot_ops::DESKTOP_RUNNING_MESSAGE));
    }
    Ok(())
}

fn err(error: impl std::fmt::Display) -> CommandError {
    CommandError::bad(error.to_string())
}

/// 处理 `bot_*` 命令；不是 Bot 命令时返回 `None`。
pub(crate) async fn dispatch(
    ctx: &BotContext,
    command: &str,
    args: Value,
) -> Option<CommandResult> {
    let result = match command {
        // ── 只读 ──
        "bot_list" => to_json(ctx.store.list()),
        "bot_health" => health(ctx, args).await,
        "bot_log" => log(ctx, args),
        "bot_config_schema" => config_schema(ctx, args).await,
        "bot_available" => ctx
            .runtime
            .fetch_index()
            .await
            .map_err(|error| CommandError::bad(format!("加载线上 Bot 目录失败：{error}")))
            .and_then(to_json),
        "bot_scan_local" => to_json(ctx.runtime.scan_local_artifacts()),
        "bot_check_update" => check_update(ctx, args).await,
        "bot_push_targets" => push_targets(ctx, args).await,

        // ── 写操作 ──
        "bot_provision_begin" => provision_begin(ctx, args).await,
        "bot_provision_poll" => provision_poll(ctx, args).await,
        "bot_register" => register(ctx, args),
        "bot_update" => update(ctx, args),
        "bot_install" => install(ctx, args).await,
        "bot_start" => start(ctx, args).await,
        "bot_stop" => stop(ctx, args).await,
        "bot_remove" => remove(ctx, args).await,
        "bot_upgrade" => upgrade(ctx, args).await,
        "bot_register_mcp" => register_mcp(ctx, args).await,
        "bot_delete_push_target" => delete_push_target(ctx, args).await,
        _ => return None,
    };
    Some(result)
}

async fn health(ctx: &BotContext, args: Value) -> CommandResult {
    let IdArgs { id } = parse(args)?;
    to_json(ctx.runtime.health(&bot_id(id)?).await)
}

fn log(ctx: &BotContext, args: Value) -> CommandResult {
    let IdArgs { id } = parse(args)?;
    let id = bot_id(id)?;
    require_bot(ctx, &id)?;
    to_json(
        tiangong_bots::read_log_tail(&id)
            .map_err(|error| CommandError::bad(format!("读取 bot 日志失败：{error}")))?,
    )
}

/// 与桌面端 `bot_config_schema` 同序：本地自有 bot 现读 → 安装缓存 → 本地制品 → 线上预览。
async fn config_schema(ctx: &BotContext, args: Value) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Args {
        artifact_id: String,
        bot_id: Option<String>,
    }
    let Args {
        artifact_id,
        bot_id: raw_bot_id,
    } = parse(args)?;
    let target = raw_bot_id.map(bot_id).transpose()?;
    let local_artifacts = ctx.runtime.scan_local_artifacts();

    if let Some(id) = &target {
        if local_artifacts
            .iter()
            .any(|local| local.id == id.as_str() && local.version.is_empty())
        {
            return tiangong_bots::describe_and_cache(id)
                .await
                .map_err(|error| CommandError::bad(format!("读取本地 Bot 配置失败：{error}")))
                .and_then(to_json);
        }
        if let Some(schema) = tiangong_bots::cached_schema(id) {
            return to_json(schema);
        }
    }
    for local in local_artifacts {
        if local.artifact_id == artifact_id {
            let local_id = bot_id(local.id)?;
            if !local.version.is_empty() {
                if let Some(schema) = tiangong_bots::cached_schema(&local_id) {
                    return to_json(schema);
                }
            }
            return tiangong_bots::describe_and_cache(&local_id)
                .await
                .map_err(|error| CommandError::bad(format!("读取本地 Bot 配置失败：{error}")))
                .and_then(to_json);
        }
    }
    let index = ctx.runtime.fetch_index().await.map_err(|error| {
        CommandError::bad(format!(
            "获取 schema 失败（线上不可达且本地无匹配）：{error}"
        ))
    })?;
    let manifest = index
        .bots
        .into_iter()
        .find(|manifest| manifest.id == artifact_id)
        .ok_or_else(|| CommandError::bad(format!("未找到制品：{artifact_id}")))?;
    to_json(manifest.config_schema)
}

async fn check_update(ctx: &BotContext, args: Value) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Args {
        artifact_id: String,
    }
    let Args { artifact_id } = parse(args)?;
    to_json(ctx.runtime.check_update(&artifact_id).await.map_err(err)?)
}

async fn push_targets(ctx: &BotContext, args: Value) -> CommandResult {
    let IdArgs { id } = parse(args)?;
    let id = bot_id(id)?;
    require_bot(ctx, &id)?;
    to_json(ctx.runtime.push_targets(&id).await.map_err(err)?)
}

async fn provision_begin(ctx: &BotContext, args: Value) -> CommandResult {
    ensure_writable()?;
    let BotIdArgs { bot_id: raw } = parse(args)?;
    to_json(
        ctx.runtime
            .provision_begin(&bot_id(raw)?)
            .await
            .map_err(err)?,
    )
}

async fn provision_poll(ctx: &BotContext, args: Value) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Args {
        bot_id: String,
        session: QrSession,
    }
    ensure_writable()?;
    let Args {
        bot_id: raw,
        session,
    } = parse(args)?;
    to_json(
        ctx.runtime
            .provision_poll(&bot_id(raw)?, &session)
            .await
            .map_err(err)?,
    )
}

fn register(ctx: &BotContext, args: Value) -> CommandResult {
    #[derive(Deserialize)]
    struct Args {
        request: RegisterBotRequest,
    }
    ensure_writable()?;
    let Args { request } = parse(args)?;
    bot_id(request.id.as_str())?;
    to_json(ctx.store.register(request).map_err(err)?)
}

fn update(ctx: &BotContext, args: Value) -> CommandResult {
    #[derive(Deserialize)]
    struct Args {
        id: String,
        request: UpdateBotRequest,
    }
    ensure_writable()?;
    let Args { id, request } = parse(args)?;
    to_json(ctx.store.update(&bot_id(id)?, request).map_err(err)?)
}

async fn install(ctx: &BotContext, args: Value) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Args {
        artifact_id: String,
        dest_bot_id: String,
    }
    ensure_writable()?;
    let Args {
        artifact_id,
        dest_bot_id,
    } = parse(args)?;
    let dest = bot_id(dest_bot_id)?;
    let index = ctx.runtime.fetch_index().await.map_err(err)?;
    let manifest = index
        .bots
        .into_iter()
        .find(|manifest| manifest.id == artifact_id)
        .ok_or_else(|| CommandError::bad(format!("bots-index 中未找到制品：{artifact_id}")))?;
    // 配置页没有事件推送，下载进度不上报（前端显示"安装中"）。
    ctx.runtime
        .install(manifest, &dest, None)
        .await
        .map_err(err)?;
    to_json("制品安装完成")
}

/// 以后台独立进程启动并标记自动运行（不随配置页退出而停止）。
async fn start(ctx: &BotContext, args: Value) -> CommandResult {
    ensure_writable()?;
    let IdArgs { id } = parse(args)?;
    let id = bot_id(id)?;
    let bot = require_bot(ctx, &id)?;
    if tiangong_bots::pid::is_running(&id) {
        return Err(CommandError::bad(format!("bot 已在运行：{id}")));
    }
    let server_reachable = bot_ops::spawn_daemon(&bot).map_err(err)?;
    if let Err(save_error) = ctx.store.set_enabled(&id, true) {
        let rollback = tiangong_bots::pid::stop_bot(&id)
            .map(|()| "已撤销本次启动".to_string())
            .unwrap_or_else(|stop_error| format!("且 bot 未能停止：{stop_error}"));
        return Err(CommandError::bad(format!(
            "保存自动运行状态失败：{save_error}；{rollback}"
        )));
    }
    if !server_reachable {
        return to_json(format!(
            "bot 已启动：{id}\n当前未检测到天工 Server，Bot 暂时无法调用 Agent。请启动 Server 后 Bot 将自动恢复连接。"
        ));
    }
    to_json(format!("bot 已启动：{id}"))
}

/// 注销 MCP → 取消自动运行 → 停止进程；停止失败时恢复原状态。
async fn stop(ctx: &BotContext, args: Value) -> CommandResult {
    ensure_writable()?;
    let IdArgs { id } = parse(args)?;
    let id = bot_id(id)?;
    let original = require_bot(ctx, &id)?;
    let mcp_removed = bot_ops::unregister_mcp(&ctx.runtime, &id)
        .await
        .map_err(err)?;
    ctx.store
        .set_enabled(&id, false)
        .map_err(|error| CommandError::bad(format!("取消自动运行失败，bot 未停止：{error}")))?;
    if let Err(stop_error) = tiangong_bots::pid::stop_bot(&id) {
        if original.enabled {
            let _ = ctx.store.set_enabled(&id, true);
        }
        let recovery = restore_mcp(ctx, &id, mcp_removed).await;
        return Err(CommandError::bad(format!(
            "停止 bot 失败：{stop_error}；{recovery}"
        )));
    }
    to_json("bot 已停止")
}

/// 删除配置（运行中先停止），保留已安装制品。
async fn remove(ctx: &BotContext, args: Value) -> CommandResult {
    ensure_writable()?;
    let IdArgs { id } = parse(args)?;
    let id = bot_id(id)?;
    require_bot(ctx, &id)?;
    let mcp_removed = bot_ops::unregister_mcp(&ctx.runtime, &id)
        .await
        .map_err(err)?;
    if let Err(stop_error) = tiangong_bots::pid::stop_bot(&id) {
        let recovery = restore_mcp(ctx, &id, mcp_removed).await;
        return Err(CommandError::bad(format!(
            "停止 bot 失败，未删除配置：{stop_error}；{recovery}"
        )));
    }
    if let Err(remove_error) = ctx.store.remove(&id) {
        let recovery = restore_mcp(ctx, &id, mcp_removed).await;
        return Err(CommandError::bad(format!(
            "删除 bot 配置失败：{remove_error}；{recovery}"
        )));
    }
    to_json("bot 配置已删除，已安装程序保留")
}

/// 升级：停止运行中的进程 → 下载新版本 → 恢复运行与 MCP 注册。
async fn upgrade(ctx: &BotContext, args: Value) -> CommandResult {
    ensure_writable()?;
    let BotIdArgs { bot_id: raw } = parse(args)?;
    let id = bot_id(raw)?;
    let bot = require_bot(ctx, &id)?;
    let manifest = ctx
        .runtime
        .check_update(&bot.artifact_id)
        .await
        .map_err(err)?
        .ok_or_else(|| CommandError::bad("已是最新版本"))?;
    let was_running = tiangong_bots::pid::is_running(&id);
    let mcp_removed = bot_ops::unregister_mcp(&ctx.runtime, &id)
        .await
        .map_err(err)?;
    if was_running {
        if let Err(stop_error) = tiangong_bots::pid::stop_bot(&id) {
            let recovery = restore_mcp(ctx, &id, mcp_removed).await;
            return Err(CommandError::bad(format!(
                "升级前停止 bot 失败，未开始升级：{stop_error}；{recovery}"
            )));
        }
    }
    let upgrade_error = ctx.runtime.upgrade(&id, manifest, None).await.err();
    // 无论升级成败都恢复原运行状态（失败时 runtime 已回滚到旧版本）。
    let mut notes = Vec::new();
    if was_running {
        let latest = ctx.store.get(&id).unwrap_or(bot);
        match bot_ops::spawn_daemon(&latest) {
            Ok(_) => notes.push("已恢复运行".to_string()),
            Err(error) => notes.push(format!("恢复运行失败：{error:#}")),
        }
    }
    if mcp_removed || was_running {
        notes.push(restore_mcp(ctx, &id, true).await);
    }
    let notes = notes.join("；");
    match upgrade_error {
        Some(error) => Err(CommandError::bad(format!("升级失败：{error:#}；{notes}"))),
        None if notes.is_empty() => to_json("已升级到最新版本"),
        None => to_json(format!("已升级到最新版本；{notes}")),
    }
}

async fn register_mcp(ctx: &BotContext, args: Value) -> CommandResult {
    ensure_writable()?;
    let IdArgs { id } = parse(args)?;
    let id = bot_id(id)?;
    require_bot(ctx, &id)?;
    bot_ops::ensure_mcp_registered(&ctx.runtime, &id)
        .await
        .map_err(err)?
        .ok_or_else(|| CommandError::bad("该 Bot 不支持 MCP"))
        .and_then(to_json)
}

async fn delete_push_target(ctx: &BotContext, args: Value) -> CommandResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Args {
        id: String,
        target_id: String,
    }
    ensure_writable()?;
    let Args { id, target_id } = parse(args)?;
    let id = bot_id(id)?;
    require_bot(ctx, &id)?;
    ctx.runtime
        .delete_push_target(&id, &target_id)
        .await
        .map_err(err)?;
    to_json("推送授权已删除")
}

/// 失败回滚时恢复 MCP 注册，返回说明文字。
async fn restore_mcp(ctx: &BotContext, id: &BotId, removed: bool) -> String {
    if !removed {
        return "MCP 状态无需恢复".to_string();
    }
    match bot_ops::ensure_mcp_registered(&ctx.runtime, id).await {
        Ok(_) => "已恢复 MCP 注册".to_string(),
        Err(error) => format!("恢复 MCP 注册失败：{error:#}"),
    }
}
