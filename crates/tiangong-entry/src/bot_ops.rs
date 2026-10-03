//! Bot 运维的公共操作：`tiangong bot` 命令行与 `tiangong config` 配置页共用。
//!
//! - 以独立后台进程启动 Bot（setsid 脱离会话），不随调用方退出而停止；
//! - 把 Bot 声明的出站能力注册为普通 stdio MCP / 反向注销（与桌面端
//!   `ensure_bot_mcp_registered` / `unregister_bot_mcp` 判定逐字一致）。
//!
//! 这里只返回结果与说明文字，不直接打印，由调用方决定如何展示。

use std::time::Duration;

use anyhow::{Result, anyhow};
use tiangong_bots::{BotConfig, BotId, BotMcpConfig, BotRuntime};
use tiangong_plugin_mcp_protocol::MessageResponse;
use tiangong_plugin_mcp_protocol::config::{
    McpServerConfig, McpTransportMode, RegisterMcpServerOptions, RegisterMcpServerRequest,
    ResolvedMcpTransport,
};
use tiangong_plugin_mcp_protocol::management::{
    RemoveServerRequest, SERVER_REMOVE_OPERATION, SERVER_SET_ENABLED_OPERATION, ServersResponse,
    SetEnabledRequest,
};

/// 桌面应用运行时拒绝 Bot 写操作的说明（桌面端监督的 Bot 会被自动拉起）。
pub const DESKTOP_RUNNING_MESSAGE: &str =
    "天工桌面应用正在运行，Bot 由桌面端监督。请在桌面应用的设置中操作，或退出桌面应用后重试。";

/// 桌面应用是否正在运行（持有 desktop.lock）。
pub fn desktop_running() -> bool {
    tiangong_config::desktop_lock::is_desktop_running()
}

/// 以独立后台进程启动 Bot（不监督、不自动重启）。
///
/// 返回天工 Server 当前是否可达（不可达时 Bot 仍会启动，但暂无法调用 Agent）。
pub fn spawn_daemon(bot: &BotConfig) -> Result<bool> {
    let artifact = tiangong_bots::paths::bot_artifact_path(&bot.id);
    if !artifact.exists() {
        return Err(anyhow!("bot 制品未安装：{}（{}）", bot.id, bot.artifact_id));
    }
    let server_config = tiangong_config::load_server_config();
    let server_env = tiangong_bots::server_env(
        &server_config.host,
        server_config.port,
        server_config.auth_token.clone(),
    );
    // 统一构造完整启动 env（schema 凭证 + server env）。
    let env = tiangong_bots::build_launch_env(bot, &server_env)?;
    tiangong_bots::pid::spawn_detached(&bot.id, &artifact, &env)?;
    Ok(server_health_check(&server_config))
}

/// 经运行时 sidecar 通道调用 MCP 插件操作。
fn mcp_invoke(operation: &str, payload: serde_json::Value) -> Result<serde_json::Value> {
    tiangong_plugin_runtime::registry::invoke_sidecar(
        &tiangong_config::io::storage_root(),
        "mcp",
        operation,
        payload,
    )
}

/// 查询当前所有 MCP server 配置。
fn list_mcp_servers() -> Result<Vec<McpServerConfig>> {
    let response: ServersResponse =
        serde_json::from_value(mcp_invoke("mcp.server.list", serde_json::json!({}))?)
            .map_err(|error| anyhow!("解析 MCP server 列表失败: {error}"))?;
    Ok(response.servers)
}

/// 把 Bot 声明的出站能力注册为普通 stdio MCP（写 `~/.tiangong/mcp.json`）。
///
/// 返回结果说明；不支持 MCP 的 Bot 返回 `Ok(None)`。同名 MCP 已被其他配置占用时报错，
/// 不覆盖用户手动配置。
pub async fn ensure_mcp_registered(runtime: &BotRuntime, id: &BotId) -> Result<Option<String>> {
    if !runtime.supports_mcp(id).await? {
        return Ok(None);
    }
    let generated = runtime.generate_mcp_config(id).await?;

    if let Some(existing) = list_mcp_servers()?
        .into_iter()
        .find(|server| server.name == generated.name)
    {
        if !mcp_connection_matches(&existing, &generated) {
            return Err(anyhow!(
                "MCP 名称 {} 已被其他配置占用，未自动覆盖",
                generated.name
            ));
        }
        if !existing.enabled {
            let _: MessageResponse = serde_json::from_value(mcp_invoke(
                SERVER_SET_ENABLED_OPERATION,
                serde_json::to_value(SetEnabledRequest {
                    name: generated.name.clone(),
                    enabled: true,
                })?,
            )?)
            .map_err(|error| anyhow!("解析启停响应失败: {error}"))?;
            return Ok(Some(format!("已启用 MCP：{}", generated.name)));
        }
        return Ok(Some(format!("MCP 已注册：{}", generated.name)));
    }

    let request = mcp_registration_request(&generated, generated.enabled);
    let _: MessageResponse = serde_json::from_value(mcp_invoke(
        "mcp.server.register",
        serde_json::to_value(&request)?,
    )?)
    .map_err(|error| anyhow!("解析注册响应失败: {error}"))?;
    Ok(Some(format!(
        "已注册 MCP：{}（Agent 现可调用该 Bot 的工具）",
        generated.name
    )))
}

/// 注销 Bot 注册的 MCP（仅当同名且连接匹配时移除，避免误删用户手动配置）。
///
/// 返回是否实际移除。不支持 MCP、无对应注册或制品未安装（无从读取 MCP 能力，
/// 也不可能已注册）时返回 `Ok(false)`。
pub async fn unregister_mcp(runtime: &BotRuntime, id: &BotId) -> Result<bool> {
    if !tiangong_bots::paths::bot_artifact_path(id).exists() {
        return Ok(false);
    }
    if !runtime.supports_mcp(id).await? {
        return Ok(false);
    }
    let generated = runtime.generate_mcp_config(id).await?;
    let Some(existing) = list_mcp_servers()?
        .into_iter()
        .find(|server| server.name == generated.name)
    else {
        return Ok(false);
    };
    if !mcp_connection_matches(&existing, &generated) {
        return Ok(false);
    }
    let _: MessageResponse = serde_json::from_value(mcp_invoke(
        SERVER_REMOVE_OPERATION,
        serde_json::to_value(RemoveServerRequest {
            name: generated.name,
        })?,
    )?)
    .map_err(|error| anyhow!("解析删除响应失败: {error}"))?;
    Ok(true)
}

/// 判断既有 MCP server 配置是否与 Bot 生成的 stdio 连接一致。
///
/// 与 Desktop `bot_mcp_connection_matches` 逐字对齐，保证两端判定一致。
fn mcp_connection_matches(existing: &McpServerConfig, generated: &BotMcpConfig) -> bool {
    existing.resolved_transport() == ResolvedMcpTransport::Stdio
        && existing.command == generated.command
        && existing.args == generated.args
        && existing.endpoint.is_empty()
        && existing.auth_header.is_empty()
        && existing.headers.is_empty()
        && existing.env.is_empty()
        && existing.tags == generated.tags
}

/// 由 Bot 生成的 MCP 配置构造注册请求（transport 固定 stdio）。
fn mcp_registration_request(generated: &BotMcpConfig, enabled: bool) -> RegisterMcpServerRequest {
    RegisterMcpServerRequest {
        name: generated.name.clone(),
        command: generated.command.clone(),
        args: generated.args.clone(),
        tags: generated.tags.clone(),
        enabled,
        options: RegisterMcpServerOptions {
            transport: Some(McpTransportMode::Stdio),
            ..Default::default()
        },
    }
}

/// 天工 Server 是否可达（`GET /api/v1/health` 返回 200）。
fn server_health_check(config: &tiangong_config::ServerConfig) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::net::ToSocketAddrs;

    let host = connect_host(&config.host);
    let Ok(mut addrs) = (host.as_str(), config.port).to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(150)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(150)));
    let request = format!(
        "GET /api/v1/health HTTP/1.1\r\nHost: {host}:{}\r\nConnection: close\r\n\r\n",
        config.port
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = [0u8; 1024];
    let Ok(len) = stream.read(&mut response) else {
        return false;
    };
    let response = String::from_utf8_lossy(&response[..len]);
    response.starts_with("HTTP/") && response.contains(" 200 ")
}

/// 规范化监听地址为可连接地址（通配/空 → 127.0.0.1）。
fn connect_host(host: &str) -> String {
    match host.trim() {
        "" | "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        value => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_host_normalizes_wildcards() {
        assert_eq!(connect_host(""), "127.0.0.1");
        assert_eq!(connect_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(connect_host("::"), "127.0.0.1");
        assert_eq!(connect_host(" 10.0.0.2 "), "10.0.0.2");
    }
}
