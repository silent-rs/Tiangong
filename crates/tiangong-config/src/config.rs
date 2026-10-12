//! TiangongConfig：完整应用配置

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tiangong_core::config::core::CoreConfig;
use tiangong_llm::models_config::ModelsConfig;
use tiangong_types::TrustMode;

/// Server 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// 监听地址
    #[serde(default = "default_host")]
    pub host: String,
    /// 监听端口
    #[serde(default = "default_port")]
    pub port: u16,
    /// API 认证 Token
    #[serde(default)]
    pub auth_token: Option<String>,
    /// 上次退出时 Server 是否在运行，用于重启后自动拉起
    #[serde(default)]
    pub enabled: bool,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    8080
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            auth_token: None,
            enabled: false,
        }
    }
}

/// Server 配置文件路径：~/.tiangong/server.json
pub fn server_config_path() -> PathBuf {
    crate::loader::default_tiangong_dir().join("server.json")
}

/// 从指定目录加载 Server 配置，文件不存在时返回默认值。
pub fn load_server_config_from_dir(dir: &std::path::Path) -> ServerConfig {
    let path = dir.join("server.json");
    if !path.exists() {
        return ServerConfig::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => ServerConfig::default(),
    }
}

/// 从 ~/.tiangong/server.json 加载配置，文件不存在时返回默认值
pub fn load_server_config() -> ServerConfig {
    load_server_config_from_dir(&crate::loader::default_tiangong_dir())
}

/// 保存 Server 配置到指定目录的 server.json
pub fn save_server_config_to_dir(
    dir: &std::path::Path,
    config: &ServerConfig,
) -> anyhow::Result<()> {
    let path = dir.join("server.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(config)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// 保存 Server 配置到 ~/.tiangong/server.json
pub fn save_server_config(config: &ServerConfig) -> anyhow::Result<()> {
    save_server_config_to_dir(&crate::loader::default_tiangong_dir(), config)
}

impl ServerConfig {
    /// 返回脱敏后的 auth_token（显示前 4 位 + ****）
    pub fn masked_auth_token(&self) -> String {
        match &self.auth_token {
            None => "(未设置)".to_string(),
            Some(token) if token.trim().is_empty() => "(空)".to_string(),
            Some(token) if token.len() <= 4 => "****".to_string(),
            Some(token) => format!("{}****", &token[..4]),
        }
    }
}

/// 生成随机 Server Token。
///
/// 使用 scru128 生成具备时间序与随机性的标识，加 `tg_` 前缀。
/// 长度 `length` 指定 token 主体（不含前缀）的近似字节数；
/// 实际长度按 scru128 段落数向上取整。
pub fn generate_token(length: usize) -> String {
    let normalized = length.clamp(16, 256);
    let mut out = String::from("tg_");
    while out.len() - 3 < normalized {
        out.push_str(&scru128::new().to_string());
    }
    // 截断到目标长度（含前缀）
    if out.len() > 3 + normalized {
        out.truncate(3 + normalized);
    }
    out
}

/// 用户可自主维护的沙箱策略。目录白名单额外开放读写，环境黑名单
/// 从进程环境移除；路径范围不做业务限制，仅由设置入口规范化。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct SandboxUserPolicy {
    #[serde(default, alias = "write_allowlist")]
    pub directory_allowlist: Vec<String>,
    #[serde(default)]
    pub environment_blocklist: Vec<String>,
}

/// 天工完整应用配置
///
/// 包含 Core 所需的配置（models/trust_mode）以及应用层配置（server）。
///
/// MCP 配置已脱离（由 tiangong-plugin-mcp 自管 ~/.tiangong/data/mcp/mcp.json）。
#[derive(Debug, Clone)]
pub struct TiangongConfig {
    /// 本配置对应的数据根目录。只参与运行时定位，不写入配置文件。
    pub storage_root: PathBuf,
    // ===== Core 所需配置 =====
    /// LLM 模型配置
    pub models: ModelsConfig,
    /// 新对话默认权限信任模式
    pub default_trust_mode: TrustMode,
    /// 自定义系统 Prompt（从 custom-prompt.md 加载，注入 system prompt）
    pub custom_system_prompt: String,

    // ===== 应用层配置 =====
    /// 默认工作目录
    pub workspace_dir: String,
    /// 用户显式关闭按需进程沙箱（全局；关闭时 terminal/command/解释器等按需 sidecar 以完整用户权限运行）
    pub sandbox_disabled: bool,
    /// 用户沙箱路径与环境授权。用户规则可开放任意数据路径；宿主管理面
    /// 保护由运行时强制叠加，不能通过本配置覆盖。
    pub sandbox_policy: SandboxUserPolicy,
    /// 旧版命令环境变量黑名单兼容字段；加载后合并进 sandbox_policy.environment_blocklist。
    pub command_env_blocklist: Vec<String>,
    /// Server 配置
    pub server: ServerConfig,
}

impl Default for TiangongConfig {
    fn default() -> Self {
        Self {
            storage_root: crate::loader::default_tiangong_dir(),
            models: ModelsConfig::default(),
            default_trust_mode: TrustMode::default(),
            custom_system_prompt: String::new(),
            workspace_dir: crate::loader::default_workspace_dir(),
            sandbox_disabled: false,
            sandbox_policy: SandboxUserPolicy::default(),
            command_env_blocklist: Vec::new(),
            server: ServerConfig::default(),
        }
    }
}

impl TiangongConfig {
    /// 转换为 CoreConfig（提取 Core 所需的最小子集）
    ///
    /// 将 ModelsConfig（3 层）解析为 LlmConfig（扁平端点）。
    /// 自定义 Prompt 来自加载时读取的 custom-prompt.md（见 load_tiangong_config_from_dir）。
    ///
    /// 上下文窗口不在此配置中：它跟随 `ModelEndpoint.context_window`，切换
    /// 模型时同步变化。
    pub fn to_core_config(&self) -> CoreConfig {
        CoreConfig {
            trust_mode: self.default_trust_mode,
            default_trust_mode: self.default_trust_mode,
            custom_system_prompt: self.custom_system_prompt.clone(),
            reasoning_effort: tiangong_llm::request::ReasoningEffort::Medium,
        }
    }

    /// 解析路由 Chat 槽位的默认模型端点（含解析后的上下文窗口）。
    ///
    /// 模型端点不再属于 `CoreConfig`——会话实际模型由宿主解析后交给 Core。
    /// 本方法供宿主在「用户未指定模型」时确定目标端点。
    pub fn default_chat_endpoint(&self) -> Option<tiangong_llm::ModelEndpoint> {
        default_chat_endpoint_at(&self.models)
    }

    /// 路由 Chat 槽位当前指向的模型注册表 key（未配置或不在注册表时 None）。
    pub fn default_chat_model_ref(&self) -> Option<String> {
        let routed = self
            .models
            .routing
            .get(&tiangong_llm::models_config::RoutingSlot::Chat)?;
        self.models
            .models
            .iter()
            .find(|(_, entry)| entry.provider == routed.provider && entry.model == routed.model)
            .map(|(key, _)| key.clone())
    }

    /// 创建 CoreConfigProvider（用于注入 TiangongCore）
    pub fn into_core_config_provider(self) -> tiangong_core::config::core::CoreConfigProvider {
        tiangong_core::config::core::CoreConfigProvider::new(self.to_core_config())
    }
}

/// 解析路由 Chat 槽位的默认模型端点。
///
/// 供只持有 `ModelsConfig` 与存储根的调用方（如 CoreManager）使用，避免各自
/// 重复解析 Chat 槽位。`context_window` 由 `ModelEntry` 原样透传——真实值存在
/// `models.json` 的模型条目上，`context_windows.json` 只是前端编辑模型时的
/// 预填预设，运行期不再回查。
pub fn default_chat_endpoint_at(
    models: &tiangong_llm::models_config::ModelsConfig,
) -> Option<tiangong_llm::ModelEndpoint> {
    models
        .resolve_slot(tiangong_llm::models_config::RoutingSlot::Chat)
        .map(tiangong_llm::ModelEndpoint::from_resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_default() {
        let config = ServerConfig::default();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 8080);
        assert_eq!(config.auth_token, None);
        assert!(!config.enabled);
    }

    #[test]
    fn server_config_serde_roundtrip_with_enabled() {
        let config = ServerConfig {
            host: "0.0.0.0".to_string(),
            port: 9000,
            auth_token: Some("tg_secret".to_string()),
            enabled: true,
        };
        let json = serde_json::to_string(&config).unwrap();
        // enabled 应被序列化
        assert!(json.contains("\"enabled\":true"));
        let parsed: ServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.host, config.host);
        assert_eq!(parsed.port, config.port);
        assert_eq!(parsed.auth_token, config.auth_token);
        assert!(parsed.enabled);
    }

    #[test]
    fn server_config_back_compat_without_enabled() {
        // 旧的 server.json（无 enabled 字段）应能反序列化，enabled 默认 false
        let legacy_json = r#"{"host":"127.0.0.1","port":8080,"auth_token":null}"#;
        let parsed: ServerConfig = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 8080);
        assert!(!parsed.enabled);
    }

    #[test]
    fn masked_auth_token_variants() {
        let none = ServerConfig {
            auth_token: None,
            ..Default::default()
        };
        assert_eq!(none.masked_auth_token(), "(未设置)");

        let empty = ServerConfig {
            auth_token: Some("".to_string()),
            ..Default::default()
        };
        assert_eq!(empty.masked_auth_token(), "(空)");

        let short = ServerConfig {
            auth_token: Some("ab".to_string()),
            ..Default::default()
        };
        assert_eq!(short.masked_auth_token(), "****");

        let long = ServerConfig {
            auth_token: Some("tg_abcd1234567890".to_string()),
            ..Default::default()
        };
        assert_eq!(long.masked_auth_token(), "tg_a****");
    }

    #[test]
    fn generate_token_has_prefix_and_length() {
        let token = generate_token(32);
        assert!(token.starts_with("tg_"));
        // 主体长度应接近 32（向上取整到 scru128 段落边界）
        let body = &token[3..];
        assert!(body.len() >= 32, "body len = {}", body.len());
        assert!(token.len() < 3 + 32 + 40); // 不应远超目标
    }

    #[test]
    fn generate_token_clamps_extremes() {
        let too_short = generate_token(4);
        let too_long = generate_token(1000);
        // 过短被钳制到最小 16
        assert!(too_short.len() >= 3 + 16);
        // 过长被钳制到最大 256
        assert!(too_long.len() <= 3 + 256 + 40);
    }

    #[test]
    fn generate_token_unique() {
        let a = generate_token(32);
        let b = generate_token(32);
        assert_ne!(a, b, "连续生成的 token 不应相同");
    }

    #[test]
    fn to_core_config_carries_custom_system_prompt() {
        // P0 回归：to_core_config 必须携带 custom_system_prompt，
        // 否则 CLI/Server 启动的 Core 拿不到自定义 Prompt。
        let config = TiangongConfig {
            custom_system_prompt: "总是用简体中文".to_string(),
            ..Default::default()
        };
        let core = config.to_core_config();
        assert_eq!(core.custom_system_prompt, "总是用简体中文");
    }

    #[test]
    fn to_core_config_default_prompt_empty() {
        let config = TiangongConfig::default();
        let core = config.to_core_config();
        assert!(core.custom_system_prompt.is_empty());
    }

    #[test]
    fn 默认端点携带上下文窗口而核心配置只保留通用兜底() {
        use tiangong_llm::model::ProviderProtocol;
        use tiangong_llm::models_config::{
            ModelCapability, ModelEntry, ProviderConfig, RoutingSlot,
        };

        let mut config = TiangongConfig::default();
        config.models.providers.insert(
            "provider".to_string(),
            ProviderConfig {
                headers: Default::default(),
                base_url: "https://example.com".to_string(),
                api_key: "key".to_string(),
                timeout_ms: 60_000,
                protocol: ProviderProtocol::OpenAiChatCompletions,
            },
        );
        config.models.routing.insert(
            RoutingSlot::Chat,
            ModelEntry {
                provider: "provider".to_string(),
                model: "chat-model".to_string(),
                capabilities: vec![ModelCapability::Chat],
                options: serde_json::json!({}),
                context_window: Some(131_072),
            },
        );

        // 窗口跟随端点：切换模型时随之变化，CoreConfig 不再承载该字段
        // （否则它由路由默认模型推导，会话切到别的模型后不会变）。
        assert_eq!(
            config.default_chat_endpoint().unwrap().context_window,
            Some(131_072)
        );
    }

    #[test]
    fn load_custom_prompt_from_dir_reads_md_file() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        // 写入 custom-prompt.md
        fs::write(dir.path().join("custom-prompt.md"), "测试 Prompt 内容").unwrap();

        // 直接验证 load_custom_prompt_at 行为（与 load_tiangong_config_from_dir 一致）
        let prompt =
            crate::io::load_custom_prompt_at(&dir.path().join("custom-prompt.md"), "").unwrap();
        assert_eq!(prompt, "测试 Prompt 内容");
    }
}
