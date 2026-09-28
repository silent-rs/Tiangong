//! 模型配置加载与解析辅助（sidecar 用）。
//!
//! sidecar 经 `TIANGONG_STORAGE_ROOT` 定位天工数据目录，加载 `models.json`，
//! 根据插件保存的模型 key 或能力类型解析完整模型连接信息。
//!
//! 各插件 sidecar 自行适配供应商调用，本模块只提供配置读取与脱敏能力。
//! 解析出的 `ResolvedModel` 含 API Key，**不得出现在日志或返回给 WASM 的数据中**。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tiangong_llm::{ModelCapability, ModelEndpoint, ModelsConfig, ResolvedModel};

/// 从环境变量读取天工存储根目录（host spawn 时注入，仅可信插件可获得）。
pub fn storage_root() -> Result<PathBuf> {
    let value = std::env::var(tiangong_plugin_runtime::sidecar::STORAGE_ROOT_ENV)
        .context("TIANGONG_STORAGE_ROOT 未注入，sidecar 无法读取模型配置")?;
    Ok(PathBuf::from(value))
}

/// 加载天工 `models.json`。
///
/// 每次调用都重新读取文件（不缓存），确保用户修改模型配置后立即生效。
/// 文件不存在或解析失败返回空配置（Default）。
pub fn load_models_config() -> Result<ModelsConfig> {
    let root = storage_root()?;
    Ok(tiangong_config::io::load_models_config_at(&root))
}

/// 按模型能力解析端点（含 API Key）。
///
/// 返回的 `ResolvedModel` 含明文密钥，仅供 sidecar 内部构造供应商请求使用，
/// **不得序列化到日志、IPC 响应或设置页数据**。
pub fn resolve_for_capability(capability: ModelCapability) -> Result<ResolvedModel> {
    let models = load_models_config()?;
    models
        .resolve_for_capability(capability)
        .ok_or_else(|| anyhow::anyhow!("能力 {} 未配置模型端点", capability.key()))
}

/// 按模型 key（models.json 中 models 表的键）解析端点。
///
/// 先从 models 表查到 ModelEntry，再从 providers 表解析完整连接信息。
/// 返回的 `ResolvedModel` 含明文密钥。
pub fn resolve_for_model_key(key: &str) -> Result<ResolvedModel> {
    let models = load_models_config()?;
    let entry = models
        .models
        .get(key)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("模型 key '{key}' 不存在于 models.json"))?;
    let provider = models
        .providers
        .get(&entry.provider)
        .ok_or_else(|| anyhow::anyhow!("provider '{}' 未配置", entry.provider))?;
    Ok(ResolvedModel {
        headers: provider.headers.clone(),
        provider: entry.provider.clone(),
        base_url: provider.base_url.clone(),
        api_key: ModelsConfig::resolve_api_key(&provider.api_key),
        timeout_ms: provider.timeout_ms,
        protocol: provider.protocol,
        model: entry.model.clone(),
        options: entry.options.clone(),
        context_window: entry.context_window,
    })
}

/// 将 `ResolvedModel` 转为 `ModelEndpoint`（供 `SingleProviderClient` 使用）。
pub fn to_endpoint(resolved: ResolvedModel) -> ModelEndpoint {
    ModelEndpoint::from_resolved(resolved)
}

/// 判断指定能力是否已配置（用于设置页展示可用模型列表）。
pub fn has_capability(capability: ModelCapability) -> bool {
    load_models_config()
        .map(|models| models.resolve_for_capability(capability).is_some())
        .unwrap_or(false)
}

/// 脱敏的模型信息（用于设置页展示，不含 API Key）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelInfo {
    /// 模型 key（models.json 中 models 表的键）。
    pub key: String,
    /// provider 标识。
    pub provider: String,
    /// 模型名称。
    pub model: String,
    /// 该模型支持的能力列表。
    pub capabilities: Vec<String>,
    /// 是否已完成配置（provider 存在且 api_key 非空）。
    pub configured: bool,
}

/// 返回匹配指定能力的模型列表（脱敏，不含 API Key）。
///
/// 供设置页展示可选模型。遍历 models 表，筛选 capabilities 含目标能力的条目，
/// 检查对应 provider 是否已配置。
pub fn list_models_for_capability(capability: ModelCapability) -> Result<Vec<ModelInfo>> {
    let models = load_models_config()?;
    let result = models
        .models
        .iter()
        .filter(|(_, entry)| entry.capabilities.contains(&capability))
        .map(|(key, entry)| {
            let configured = models
                .providers
                .get(&entry.provider)
                .is_some_and(|p| !p.api_key.trim().is_empty());
            ModelInfo {
                key: key.clone(),
                provider: entry.provider.clone(),
                model: entry.model.clone(),
                capabilities: entry
                    .capabilities
                    .iter()
                    .map(|c| c.key().to_string())
                    .collect(),
                configured,
            }
        })
        .collect();
    Ok(result)
}

/// 对敏感字符串做日志脱敏（只保留前 4 位 + ***）。
pub fn mask_sensitive(value: &str) -> String {
    if value.len() <= 4 {
        "***".to_string()
    } else {
        format!("{}***", &value[..4])
    }
}

/// 校验文件路径在天工存储根目录内（防止路径逃逸）。
pub fn ensure_within_storage_root(path: &Path) -> Result<PathBuf> {
    let root = storage_root()?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("路径不存在或无法解析：{}", path.display()))?;
    if !canonical.starts_with(&root) {
        bail!("路径不在天工存储目录内：{}", canonical.display());
    }
    Ok(canonical)
}
