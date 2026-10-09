//! Bot 运维的公共操作：`tiangong bot` 命令行与 `tiangong config` 配置页共用。
//!
//! - 以独立后台进程启动 Bot（setsid 脱离会话），不随调用方退出而停止；
//! - 把 Bot 声明的出站能力注册为普通 stdio MCP / 反向注销；底层 MCP 操作见
//!   [`mcp`]，桌面端复用同一套判定。
//!
//! 这里只返回结果与说明文字，不直接打印，由调用方决定如何展示。

use std::time::Duration;

use anyhow::{Result, anyhow};
use tiangong_bots::{BotConfig, BotId, BotRuntime};

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

/// 把 Bot 声明的出站能力注册为普通 stdio MCP（写 `~/.tiangong/mcp.json`）。
///
/// 返回结果说明；不支持 MCP 的 Bot 返回 `Ok(None)`。同名 MCP 已被其他配置占用时报错，
/// 不覆盖用户手动配置。
pub async fn ensure_mcp_registered(runtime: &BotRuntime, id: &BotId) -> Result<Option<String>> {
    if !runtime.supports_mcp(id).await? {
        return Ok(None);
    }
    let generated = runtime.generate_mcp_config(id).await?;
    Ok(Some(match mcp::ensure_registered(&generated)? {
        mcp::EnsureOutcome::AlreadyRegistered => format!("MCP 已注册：{}", generated.name),
        mcp::EnsureOutcome::Enabled => format!("已启用 MCP：{}", generated.name),
        mcp::EnsureOutcome::Registered => format!(
            "已注册 MCP：{}（Agent 现可调用该 Bot 的工具）",
            generated.name
        ),
    }))
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
    Ok(mcp::unregister(&generated)?.is_some())
}

/// Bot ↔ MCP 插件的底层操作（CLI、配置页与桌面端共用）。
///
/// 经 runtime sidecar 通道以 JSON 调用 MCP 插件，只声明本处用到的字段，
/// 不在编译期依赖插件协议 crate；插件新增字段不影响这里的解析。
pub mod mcp {
    use std::collections::BTreeMap;

    use anyhow::{Result, anyhow};
    use serde::Deserialize;
    use serde_json::{Value, json};
    use tiangong_bots::BotMcpConfig;

    const SERVER_LIST: &str = "mcp.server.list";
    const SERVER_REGISTER: &str = "mcp.server.register";
    const SERVER_REMOVE: &str = "mcp.server.remove";
    const SERVER_SET_ENABLED: &str = "mcp.server.set_enabled";

    /// MCP 插件 `mcp.server.list` 返回的 server 配置（仅取连接判定所需字段）。
    #[derive(Debug, Clone, Deserialize)]
    pub struct McpServer {
        pub name: String,
        #[serde(default)]
        pub transport: String,
        #[serde(default)]
        pub command: String,
        #[serde(default)]
        pub args: Vec<String>,
        #[serde(default)]
        pub endpoint: String,
        #[serde(default)]
        pub auth_header: String,
        #[serde(default)]
        pub headers: BTreeMap<String, String>,
        #[serde(default)]
        pub env: BTreeMap<String, String>,
        #[serde(default = "default_true")]
        pub enabled: bool,
        #[serde(default)]
        pub tags: Vec<String>,
    }

    fn default_true() -> bool {
        true
    }

    impl McpServer {
        /// 是否为 stdio 连接（与 MCP 插件的传输解析规则一致：显式 stdio，
        /// 或 auto 且无 endpoint、command 非空且不是 http(s) 地址）。
        fn is_stdio(&self) -> bool {
            match self.transport.as_str() {
                "stdio" => true,
                "http" => false,
                _ => {
                    let command = self.command.trim().to_ascii_lowercase();
                    self.endpoint.trim().is_empty()
                        && !command.is_empty()
                        && !command.starts_with("http://")
                        && !command.starts_with("https://")
                }
            }
        }

        /// 判断既有配置是否与 Bot 生成的 stdio 连接一致。
        pub fn matches(&self, generated: &BotMcpConfig) -> bool {
            self.is_stdio()
                && self.command == generated.command
                && self.args == generated.args
                && self.endpoint.is_empty()
                && self.auth_header.is_empty()
                && self.headers.is_empty()
                && self.env.is_empty()
                && self.tags == generated.tags
        }
    }

    /// `ensure_registered` 的结果。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum EnsureOutcome {
        /// 已注册且已启用，未做改动。
        AlreadyRegistered,
        /// 已注册但被停用，本次启用。
        Enabled,
        /// 新注册。
        Registered,
    }

    fn invoke(operation: &str, payload: Value) -> Result<Value> {
        tiangong_plugin_runtime::registry::invoke_sidecar(
            &tiangong_config::io::storage_root(),
            "mcp",
            operation,
            payload,
        )
    }

    /// 写操作统一返回 `{ "message": ... }`。
    fn invoke_message(operation: &str, payload: Value) -> Result<String> {
        #[derive(Deserialize)]
        struct MessageResponse {
            message: String,
        }
        let response: MessageResponse = serde_json::from_value(invoke(operation, payload)?)
            .map_err(|error| anyhow!("解析 {operation} 响应失败: {error}"))?;
        Ok(response.message)
    }

    /// 查询当前所有 MCP server 配置。
    pub fn list_servers() -> Result<Vec<McpServer>> {
        #[derive(Deserialize)]
        struct ServersResponse {
            servers: Vec<McpServer>,
        }
        let response: ServersResponse = serde_json::from_value(invoke(SERVER_LIST, json!({}))?)
            .map_err(|error| anyhow!("解析 MCP server 列表失败: {error}"))?;
        Ok(response.servers)
    }

    /// 查找与 Bot 同名的 MCP server。
    pub fn find(generated: &BotMcpConfig) -> Result<Option<McpServer>> {
        Ok(list_servers()?
            .into_iter()
            .find(|server| server.name == generated.name))
    }

    /// 以 stdio 方式注册 Bot 的 MCP，返回插件给出的说明。
    pub fn register(generated: &BotMcpConfig, enabled: bool) -> Result<String> {
        invoke_message(
            SERVER_REGISTER,
            json!({
                "name": generated.name,
                "command": generated.command,
                "args": generated.args,
                "tags": generated.tags,
                "enabled": enabled,
                "options": { "transport": "stdio" },
            }),
        )
    }

    /// 移除指定名称的 MCP server，返回插件给出的说明。
    pub fn remove(name: &str) -> Result<String> {
        invoke_message(SERVER_REMOVE, json!({ "name": name }))
    }

    /// 设置指定 MCP server 的启用状态，返回插件给出的说明。
    pub fn set_enabled(name: &str, enabled: bool) -> Result<String> {
        invoke_message(
            SERVER_SET_ENABLED,
            json!({ "name": name, "enabled": enabled }),
        )
    }

    /// 确保 Bot 的 MCP 已注册并启用；同名但连接不一致时报错，不覆盖用户配置。
    pub fn ensure_registered(generated: &BotMcpConfig) -> Result<EnsureOutcome> {
        let Some(existing) = find(generated)? else {
            register(generated, generated.enabled)?;
            return Ok(EnsureOutcome::Registered);
        };
        if !existing.matches(generated) {
            return Err(anyhow!(
                "MCP 名称 {} 已被其他配置占用，未自动覆盖",
                generated.name
            ));
        }
        if existing.enabled {
            return Ok(EnsureOutcome::AlreadyRegistered);
        }
        set_enabled(&generated.name, true)?;
        Ok(EnsureOutcome::Enabled)
    }

    /// 注销 Bot 注册的 MCP（仅当同名且连接匹配时移除）。
    ///
    /// 返回被移除的原配置（供调用方失败回滚时按原启用状态恢复），未移除返回 `None`。
    pub fn unregister(generated: &BotMcpConfig) -> Result<Option<McpServer>> {
        let Some(existing) = find(generated)? else {
            return Ok(None);
        };
        if !existing.matches(generated) {
            return Ok(None);
        }
        remove(&generated.name)?;
        Ok(Some(existing))
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

    fn bot_mcp() -> tiangong_bots::BotMcpConfig {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "name": "bot-feishu",
            "transport": "stdio",
            "command": "/path/bot",
            "args": ["mcp"],
            "tags": ["bot"],
        }))
        .unwrap()
    }

    fn server(value: serde_json::Value) -> mcp::McpServer {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn mcp_matches_stdio_connection_same_as_plugin_rules() {
        let generated = bot_mcp();
        let base = serde_json::json!({
            "name": "bot-feishu",
            "command": "/path/bot",
            "args": ["mcp"],
            "tags": ["bot"],
        });
        // auto 且 command 为本地程序 → stdio，匹配；enabled 缺省为 true。
        let auto = server(base.clone());
        assert!(auto.enabled);
        assert!(auto.matches(&generated));
        // 显式 stdio 匹配。
        let mut stdio = base.clone();
        stdio["transport"] = "stdio".into();
        assert!(server(stdio).matches(&generated));
        // 显式 http 不匹配。
        let mut http = base.clone();
        http["transport"] = "http".into();
        assert!(!server(http).matches(&generated));
        // auto 但带 endpoint → http，不匹配。
        let mut with_endpoint = base.clone();
        with_endpoint["endpoint"] = "https://example.com/mcp".into();
        assert!(!server(with_endpoint).matches(&generated));
        // 用户追加了 env → 视为用户配置，不匹配。
        let mut with_env = base.clone();
        with_env["env"] = serde_json::json!({ "K": "V" });
        assert!(!server(with_env).matches(&generated));
        // 参数不同不匹配。
        let mut other_args = base;
        other_args["args"] = serde_json::json!(["other"]);
        assert!(!server(other_args).matches(&generated));
    }
}
