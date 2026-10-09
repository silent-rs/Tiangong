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

/// Bot ↔ MCP 配置的底层操作（CLI、配置页与桌面端共用）。
///
/// 直接读写 MCP 配置文件 `~/.tiangong/mcp.json`，不调用 MCP 插件：宿主与插件
/// 之间只约定这份文件格式。MCP 插件处理请求前会按修改时间重新加载外部改动，
/// 写入后无需通知插件。
///
/// 只增删改 Bot 自己的那一条 server，文件里的其他内容（含未知字段）原样保留；
/// 文件存在但无法解析时拒绝写入，避免覆盖用户配置。
pub mod mcp {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, anyhow};
    use serde::Deserialize;
    use serde_json::{Map, Value, json};
    use tiangong_bots::BotMcpConfig;

    /// MCP 配置文件中的一条 server（仅取连接判定所需字段）。
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

    /// MCP 配置文件路径。
    pub fn config_path() -> PathBuf {
        tiangong_config::io::storage_root().join("mcp.json")
    }

    /// 读取配置文件为 JSON 对象；文件不存在视为空配置。
    fn load(path: &Path) -> Result<Map<String, Value>> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Map::new());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("读取 MCP 配置失败：{}", path.display()));
            }
        };
        match serde_json::from_str::<Value>(&content)
            .with_context(|| format!("MCP 配置无法解析，拒绝修改：{}", path.display()))?
        {
            Value::Object(map) => Ok(map),
            _ => Err(anyhow!(
                "MCP 配置格式错误（顶层应为对象），拒绝修改：{}",
                path.display()
            )),
        }
    }

    /// 原子写回：先写同目录临时文件再改名，读者不会看到半写入的内容。
    fn save(path: &Path, config: &Map<String, Value>) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("MCP 配置路径无效：{}", path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 MCP 配置目录失败：{}", parent.display()))?;
        let content = serde_json::to_string_pretty(config).context("序列化 MCP 配置失败")?;
        let temp = parent.join(format!(".mcp.json.{}.tmp", std::process::id()));
        std::fs::write(&temp, content)
            .with_context(|| format!("写入 MCP 配置失败：{}", temp.display()))?;
        std::fs::rename(&temp, path).with_context(|| {
            let _ = std::fs::remove_file(&temp);
            format!("替换 MCP 配置失败：{}", path.display())
        })
    }

    /// 取 `servers` 数组（缺省时创建）。
    fn servers_mut(config: &mut Map<String, Value>) -> Result<&mut Vec<Value>> {
        match config
            .entry("servers")
            .or_insert_with(|| Value::Array(Vec::new()))
        {
            Value::Array(servers) => Ok(servers),
            _ => Err(anyhow!("MCP 配置中 servers 不是数组，拒绝修改")),
        }
    }

    fn position(servers: &[Value], name: &str) -> Option<usize> {
        servers
            .iter()
            .position(|server| server.get("name").and_then(Value::as_str) == Some(name))
    }

    fn parse_server(value: &Value) -> Result<McpServer> {
        serde_json::from_value(value.clone()).context("解析 MCP server 配置失败")
    }

    fn server_entry(generated: &BotMcpConfig, enabled: bool) -> Value {
        json!({
            "name": generated.name,
            "transport": "stdio",
            "command": generated.command,
            "args": generated.args,
            "enabled": enabled,
            "tags": generated.tags,
        })
    }

    /// 查找与 Bot 同名的 MCP server。
    pub fn find(generated: &BotMcpConfig) -> Result<Option<McpServer>> {
        find_at(&config_path(), generated)
    }

    fn find_at(path: &Path, generated: &BotMcpConfig) -> Result<Option<McpServer>> {
        let mut config = load(path)?;
        let servers = servers_mut(&mut config)?;
        position(servers, &generated.name)
            .map(|index| parse_server(&servers[index]))
            .transpose()
    }

    /// 以 stdio 方式注册 Bot 的 MCP；同名已存在时报错。返回说明文字。
    pub fn register(generated: &BotMcpConfig, enabled: bool) -> Result<String> {
        register_at(&config_path(), generated, enabled)
    }

    fn register_at(path: &Path, generated: &BotMcpConfig, enabled: bool) -> Result<String> {
        let mut config = load(path)?;
        let servers = servers_mut(&mut config)?;
        if position(servers, &generated.name).is_some() {
            return Err(anyhow!("MCP server 已存在：{}", generated.name));
        }
        servers.push(server_entry(generated, enabled));
        save(path, &config)?;
        Ok(format!("MCP server 已注册：{}", generated.name))
    }

    /// 移除指定名称的 MCP server。返回说明文字。
    pub fn remove(name: &str) -> Result<String> {
        remove_at(&config_path(), name)
    }

    fn remove_at(path: &Path, name: &str) -> Result<String> {
        let mut config = load(path)?;
        let servers = servers_mut(&mut config)?;
        let index = position(servers, name).ok_or_else(|| anyhow!("未找到 MCP server：{name}"))?;
        servers.remove(index);
        save(path, &config)?;
        Ok(format!("MCP server 已删除：{name}"))
    }

    /// 设置指定 MCP server 的启用状态。返回说明文字。
    pub fn set_enabled(name: &str, enabled: bool) -> Result<String> {
        set_enabled_at(&config_path(), name, enabled)
    }

    fn set_enabled_at(path: &Path, name: &str, enabled: bool) -> Result<String> {
        let mut config = load(path)?;
        let servers = servers_mut(&mut config)?;
        let index = position(servers, name).ok_or_else(|| anyhow!("未找到 MCP server：{name}"))?;
        let Value::Object(server) = &mut servers[index] else {
            return Err(anyhow!("MCP server 配置格式错误：{name}"));
        };
        server.insert("enabled".to_string(), Value::Bool(enabled));
        save(path, &config)?;
        Ok(format!("MCP server 状态已更新：{name} enabled={enabled}"))
    }

    /// 确保 Bot 的 MCP 已注册并启用；同名但连接不一致时报错，不覆盖用户配置。
    pub fn ensure_registered(generated: &BotMcpConfig) -> Result<EnsureOutcome> {
        ensure_registered_at(&config_path(), generated)
    }

    fn ensure_registered_at(path: &Path, generated: &BotMcpConfig) -> Result<EnsureOutcome> {
        let Some(existing) = find_at(path, generated)? else {
            register_at(path, generated, generated.enabled)?;
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
        set_enabled_at(path, &generated.name, true)?;
        Ok(EnsureOutcome::Enabled)
    }

    /// 注销 Bot 注册的 MCP（仅当同名且连接匹配时移除）。
    ///
    /// 返回被移除的原配置（供调用方失败回滚时按原启用状态恢复），未移除返回 `None`。
    pub fn unregister(generated: &BotMcpConfig) -> Result<Option<McpServer>> {
        unregister_at(&config_path(), generated)
    }

    fn unregister_at(path: &Path, generated: &BotMcpConfig) -> Result<Option<McpServer>> {
        let Some(existing) = find_at(path, generated)? else {
            return Ok(None);
        };
        if !existing.matches(generated) {
            return Ok(None);
        }
        remove_at(path, &generated.name)?;
        Ok(Some(existing))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn bot() -> BotMcpConfig {
            serde_json::from_value(json!({
                "schema_version": 1,
                "name": "bot-feishu",
                "transport": "stdio",
                "command": "/path/bot",
                "args": ["--mcp"],
                "tags": ["bot-outbound"],
            }))
            .unwrap()
        }

        fn read(path: &Path) -> Value {
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        }

        #[test]
        fn ensure_creates_file_and_is_idempotent() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("mcp.json");
            let generated = bot();
            assert_eq!(
                ensure_registered_at(&path, &generated).unwrap(),
                EnsureOutcome::Registered
            );
            let saved = read(&path);
            assert_eq!(saved["servers"][0]["name"], "bot-feishu");
            assert_eq!(saved["servers"][0]["transport"], "stdio");
            assert_eq!(saved["servers"][0]["args"], json!(["--mcp"]));
            assert_eq!(
                ensure_registered_at(&path, &generated).unwrap(),
                EnsureOutcome::AlreadyRegistered
            );
            assert_eq!(read(&path)["servers"].as_array().unwrap().len(), 1);
        }

        #[test]
        fn ensure_preserves_other_content_and_enables_disabled_entry() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("mcp.json");
            std::fs::write(
                &path,
                json!({
                    "enabled": true,
                    "timeout_ms": 30000,
                    "future_field": {"keep": 1},
                    "servers": [
                        {"name": "user", "command": "npx", "args": ["x"], "env": {"K": "V"}},
                        {"name": "bot-feishu", "transport": "stdio", "command": "/path/bot",
                         "args": ["--mcp"], "tags": ["bot-outbound"], "enabled": false}
                    ]
                })
                .to_string(),
            )
            .unwrap();
            assert_eq!(
                ensure_registered_at(&path, &bot()).unwrap(),
                EnsureOutcome::Enabled
            );
            let saved = read(&path);
            assert_eq!(saved["timeout_ms"], 30000);
            assert_eq!(saved["future_field"], json!({"keep": 1}));
            assert_eq!(saved["servers"][0]["env"], json!({"K": "V"}));
            assert_eq!(saved["servers"][1]["enabled"], true);
        }

        #[test]
        fn ensure_refuses_to_overwrite_conflicting_entry() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("mcp.json");
            let original = json!({"servers": [{"name": "bot-feishu", "command": "/other"}]});
            std::fs::write(&path, original.to_string()).unwrap();
            let error = ensure_registered_at(&path, &bot()).unwrap_err();
            assert!(error.to_string().contains("已被其他配置占用"), "{error}");
            assert_eq!(read(&path), original);
            // 不匹配的同名配置也不会被注销。
            assert!(unregister_at(&path, &bot()).unwrap().is_none());
            assert_eq!(read(&path), original);
        }

        #[test]
        fn unparsable_file_is_never_overwritten() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("mcp.json");
            std::fs::write(&path, "{ 损坏").unwrap();
            assert!(ensure_registered_at(&path, &bot()).is_err());
            assert!(unregister_at(&path, &bot()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ 损坏");
        }

        #[test]
        fn unregister_removes_only_matching_entry() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("mcp.json");
            std::fs::write(
                &path,
                json!({"servers": [
                    {"name": "user", "command": "npx"},
                    {"name": "bot-feishu", "transport": "stdio", "command": "/path/bot",
                     "args": ["--mcp"], "tags": ["bot-outbound"], "enabled": false}
                ]})
                .to_string(),
            )
            .unwrap();
            let removed = unregister_at(&path, &bot()).unwrap().unwrap();
            assert!(!removed.enabled);
            let saved = read(&path);
            assert_eq!(saved["servers"].as_array().unwrap().len(), 1);
            assert_eq!(saved["servers"][0]["name"], "user");
            assert!(unregister_at(&path, &bot()).unwrap().is_none());
        }
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
