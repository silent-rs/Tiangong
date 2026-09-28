//! 配置加载器：从磁盘文件构建 TiangongConfig
//!
//! 默认配置目录：`~/.tiangong/`
//! 配置文件：
//! - `models.json` — 模型配置
//! - `app.json` — 应用长期配置
//! - `mcp.json` — MCP 服务配置
//! - `skills.json` — Skill 配置
//! - `server.json` — Server 配置
//! - `connectors.json` — Connector 配置
//! - `mcp-tools-cache.json` — MCP 能力缓存

use std::path::{Path, PathBuf};

use tiangong_llm::models_config::ModelsConfig;
use tiangong_types::TrustMode;

use crate::config::TiangongConfig;
use crate::io;

/// `app.json` 只提取长期配置；旧的运行状态字段由 serde 自动忽略。
#[derive(Default, serde::Deserialize)]
struct AppConfigFile {
    #[serde(default)]
    workspace_dir: String,
    #[serde(default)]
    default_trust_mode: Option<TrustMode>,
    #[serde(default)]
    sandbox_disabled: bool,
    #[serde(default)]
    sandbox_policy: crate::config::SandboxUserPolicy,
    #[serde(default)]
    command_env_blocklist: Vec<String>,
    #[serde(default)]
    custom_system_prompt: String,
    #[serde(default)]
    agent_config: Option<LegacyAgentConfig>,
}

/// 兼容旧 `app.json.agent_config` 中仍属于长期配置的字段。
#[derive(Default, serde::Deserialize)]
struct LegacyAgentConfig {
    #[serde(default)]
    trust_mode: Option<TrustMode>,
    #[serde(default)]
    default_trust_mode: Option<TrustMode>,
    #[serde(default)]
    custom_system_prompt: String,
}

impl AppConfigFile {
    fn resolved_default_trust_mode(&self) -> TrustMode {
        self.default_trust_mode
            .or_else(|| {
                self.agent_config
                    .as_ref()
                    .and_then(|config| config.default_trust_mode)
            })
            .or_else(|| {
                self.agent_config
                    .as_ref()
                    .and_then(|config| config.trust_mode)
            })
            .unwrap_or_default()
    }

    fn legacy_custom_system_prompt(&self) -> &str {
        if self.custom_system_prompt.trim().is_empty() {
            self.agent_config
                .as_ref()
                .map(|config| config.custom_system_prompt.as_str())
                .unwrap_or_default()
        } else {
            &self.custom_system_prompt
        }
    }
}

/// 获取默认配置目录（~/.tiangong/）
///
/// 设置 `TIANGONG_STORAGE_ROOT` 时使用其指向的目录（与 `io::storage_root`
/// 一致，用于测试与多实例隔离）。
pub fn default_tiangong_dir() -> PathBuf {
    if let Some(root) = std::env::var_os("TIANGONG_STORAGE_ROOT").filter(|v| !v.is_empty()) {
        return PathBuf::from(root);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".tiangong")
}

/// 当前进程的默认工作目录。
pub(crate) fn default_workspace_dir() -> String {
    std::env::current_dir()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 从默认目录加载完整配置
pub fn load_tiangong_config() -> TiangongConfig {
    load_tiangong_config_from_dir(&default_tiangong_dir())
}

/// 从指定目录加载完整配置
pub fn load_tiangong_config_from_dir(dir: &Path) -> TiangongConfig {
    let app = load_json_config::<AppConfigFile>(dir, "app.json").unwrap_or_default();
    let models = load_models_config(dir);
    let server = crate::config::load_server_config_from_dir(dir);

    let custom_system_prompt = io::load_custom_prompt_at(
        &dir.join("custom-prompt.md"),
        app.legacy_custom_system_prompt(),
    )
    .unwrap_or_else(|error| {
        tracing::warn!("读取 custom-prompt.md 失败：{error}");
        app.legacy_custom_system_prompt().to_string()
    });
    let mut sandbox_policy = app.sandbox_policy.clone();
    if sandbox_policy.environment_blocklist.is_empty() && !app.command_env_blocklist.is_empty() {
        sandbox_policy.environment_blocklist = app.command_env_blocklist.clone();
    }
    let workspace_dir =
        if app.workspace_dir.trim().is_empty() || !Path::new(&app.workspace_dir).is_dir() {
            default_workspace_dir()
        } else {
            app.workspace_dir.clone()
        };

    // 首次安装：释放默认 context_windows.json
    io::ensure_context_windows(dir);

    // MCP 配置（mcp.json）与 capability 缓存由 tiangong-plugin-mcp 自管：
    // plugin 在 register 时加载缓存 + 启动后台调度器 + 预热探测，config 不再参与。

    TiangongConfig {
        storage_root: dir.to_path_buf(),
        models,
        default_trust_mode: app.resolved_default_trust_mode(),
        custom_system_prompt,
        workspace_dir,
        sandbox_disabled: app.sandbox_disabled,
        sandbox_policy,
        command_env_blocklist: app.command_env_blocklist,
        server,
    }
}

/// 加载模型配置（仅从 `models.json` 读取；环境变量回退已移除）
///
/// 加载前先把 models.json 中残留的 embedding / rerank 配置迁出到
/// `memory/legacy-models.json`，由 Memory 插件接管；迁出失败不影响加载。
fn load_models_config(dir: &Path) -> ModelsConfig {
    if let Err(error) = io::extract_legacy_memory_models_at(dir) {
        tracing::warn!("迁出 models.json 中的 Memory 旧模型配置失败，下次启动重试：{error}");
    }
    io::load_models_config_at(dir)
}

/// 通用 JSON 配置文件加载
fn load_json_config<T: serde::de::DeserializeOwned>(dir: &Path, filename: &str) -> Option<T> {
    let path = dir.join(filename);
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!("读取 {filename} 失败：{err}");
            return None;
        }
    };
    match serde_json::from_str::<T>(&content) {
        Ok(config) => Some(config),
        Err(err) => {
            tracing::warn!("解析 {filename} 失败：{err}");
            None
        }
    }
}
