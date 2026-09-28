//! 配置磁盘 IO
//!
//! 所有 `~/.tiangong` 下的配置文件读写集中于此。core 不做任何配置磁盘 IO，
//! 由本模块加载后转换为 core 所需的纯数据配置注入。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use tiangong_llm::models_config::ModelsConfig;
use tiangong_types::TrustMode;

const DEFAULT_CONTEXT_LIMIT: usize = 200_000;

// ---------------------------------------------------------------------------
// 路径
// ---------------------------------------------------------------------------

/// 用户主目录（兼容 HOME / USERPROFILE / HOMEDRIVE+HOMEPATH）。
pub fn user_home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(profile));
    }
    let drive = std::env::var_os("HOMEDRIVE").filter(|v| !v.is_empty());
    let path = std::env::var_os("HOMEPATH").filter(|v| !v.is_empty());
    match (drive, path) {
        (Some(drive), Some(path)) => {
            let mut buf = PathBuf::from(drive);
            buf.push(path);
            Some(buf)
        }
        _ => None,
    }
}

/// 天工存储根目录（`~/.tiangong`）。
///
/// 设置 `TIANGONG_STORAGE_ROOT` 时使用其指向的目录；与
/// `tiangong_plugin_runtime::sidecar::STORAGE_ROOT_ENV` 共用同一环境变量，
/// 用于测试与多实例隔离。
pub fn storage_root() -> PathBuf {
    if let Some(root) = std::env::var_os("TIANGONG_STORAGE_ROOT").filter(|v| !v.is_empty()) {
        return PathBuf::from(root);
    }
    user_home_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(".tiangong")
}

/// 自定义 Prompt 独立文件路径：`~/.tiangong/custom-prompt.md`。
pub fn custom_prompt_path() -> PathBuf {
    storage_root().join("custom-prompt.md")
}

// ---------------------------------------------------------------------------
// 应用长期配置
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct AppConfigFile<'a> {
    default_trust_mode: TrustMode,
    workspace_dir: &'a str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    sandbox_disabled: bool,
    #[serde(default, skip_serializing_if = "sandbox_policy_is_default")]
    sandbox_policy: &'a crate::config::SandboxUserPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    command_env_blocklist: &'a Vec<String>,
}

fn sandbox_policy_is_default(value: &&crate::config::SandboxUserPolicy) -> bool {
    **value == crate::config::SandboxUserPolicy::default()
}

/// 保存应用长期配置到 `app.json`。
///
/// 自定义 Prompt 继续由 `custom-prompt.md` 保存；旧 `app.json` 中的运行状态字段
/// 不再写回。
pub fn save_app_config_at(
    dir: &Path,
    default_trust_mode: TrustMode,
    workspace_dir: &str,
    sandbox_disabled: bool,
    sandbox_policy: &crate::config::SandboxUserPolicy,
    command_env_blocklist: &Vec<String>,
) -> Result<()> {
    let path = dir.join("app.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败：{}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(&AppConfigFile {
        default_trust_mode,
        workspace_dir,
        sandbox_disabled,
        sandbox_policy,
        command_env_blocklist,
    })
    .context("序列化应用配置失败")?;
    std::fs::write(&path, content)
        .with_context(|| format!("写入 app.json 失败：{}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ModelsConfig
// ---------------------------------------------------------------------------

/// 从指定目录加载 models.json，文件不存在或解析失败返回空配置。
pub fn load_models_config_at(dir: &Path) -> ModelsConfig {
    let path = dir.join("models.json");
    if !path.exists() {
        return ModelsConfig::default();
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return ModelsConfig::default(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// 保存 ModelsConfig 到指定目录的 models.json。
///
/// 保存前自动将 routing 中未在 models 注册表里的条目补入 models，
/// 确保序列化时 routing 值能写为字符串引用（旧版本兼容）。
pub fn save_models_config_at(dir: &Path, config: &ModelsConfig) -> Result<()> {
    // 覆盖写之前先把磁盘上残留的 embedding/rerank 迁出：ModelsConfig 已不再承载
    // 这些键，未经 loader 的保存路径（如 CLI 直接读写）否则会静默丢失旧配置。
    extract_legacy_memory_models_at(dir)
        .with_context(|| "迁出 models.json 中的 Memory 旧模型配置失败，已中止保存")?;
    let mut cfg = config.clone();
    ensure_routing_models_registered(&mut cfg);

    let path = dir.join("models.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败：{}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(&cfg).with_context(|| "序列化 ModelsConfig 失败")?;
    std::fs::write(&path, content)
        .with_context(|| format!("写入 models.json 失败：{}", path.display()))?;
    Ok(())
}

/// 将 routing 中未在 models 注册表中的条目自动补入 models。
fn ensure_routing_models_registered(cfg: &mut ModelsConfig) {
    for entry in cfg.routing.values() {
        let exists = cfg
            .models
            .iter()
            .any(|(_, m)| m.provider == entry.provider && m.model == entry.model);
        if !exists {
            let key = format!("{}-{}", entry.provider, entry.model);
            if cfg.models.contains_key(&key) {
                continue;
            }
            cfg.models.insert(key, entry.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Memory 旧模型配置交接
// ---------------------------------------------------------------------------

/// 交接文件名（位于 `~/.tiangong/memory/`）。
pub const LEGACY_MEMORY_MODELS_FILE: &str = "legacy-models.json";

/// 交接文件路径：`<storage_root>/memory/legacy-models.json`。
///
/// 宿主从 models.json 迁出的 embedding / rerank 配置先落到此独立文件，
/// 由 Memory 插件在下次加载配置时并入自身的新配置并归档。
pub fn legacy_memory_models_path(dir: &Path) -> PathBuf {
    dir.join("memory").join(LEGACY_MEMORY_MODELS_FILE)
}

/// 从 models.json 迁出的 Memory 旧模型配置。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LegacyMemoryModels {
    /// 旧 `routing.embedding` 解析出的端点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<LegacyMemoryEndpoint>,
    /// 旧 `routing.rerank` 解析出的端点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank: Option<LegacyMemoryEndpoint>,
    /// 仅具备 embedding / rerank 能力、已从 models 注册表移除的模型条目（原样保留）。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub retired_models: std::collections::BTreeMap<String, Value>,
}

/// 旧路由解析出的完整端点（api_key 保持原始写法，可能是 `${ENV}` 引用）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LegacyMemoryEndpoint {
    pub provider: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub protocol: tiangong_llm::ProviderProtocol,
    #[serde(default = "default_legacy_timeout_ms")]
    pub timeout_ms: u64,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<usize>,
}

fn default_legacy_timeout_ms() -> u64 {
    60_000
}

/// 读取交接文件；不存在返回 `Ok(None)`。
pub fn load_legacy_memory_models(path: &Path) -> Result<Option<LegacyMemoryModels>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取 Memory 旧模型交接文件失败：{}", path.display()))?;
    serde_json::from_str(&content)
        .map(Some)
        .with_context(|| format!("解析 Memory 旧模型交接文件失败：{}", path.display()))
}

/// 把 models.json 中残留的 embedding / rerank 配置迁出到独立交接文件。
///
/// 处理顺序保证不丢数据：
/// 1. 从原始 JSON 中摘出 `routing.embedding/rerank` 与能力标记；
/// 2. 先写交接文件（与尚未被 Memory 消费的旧交接内容合并，已有值优先）；
/// 3. 交接文件写入成功后才改写 models.json。
///
/// 任一步失败时 models.json 保持原样，下次加载会重试（合并幂等）。
/// 返回是否发生了迁出。
pub fn extract_legacy_memory_models_at(dir: &Path) -> Result<bool> {
    let models_path = dir.join("models.json");
    if !models_path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(&models_path)
        .with_context(|| format!("读取 models.json 失败：{}", models_path.display()))?;
    let Ok(mut raw) = serde_json::from_str::<Value>(&content) else {
        // 解析失败的文件交给常规加载路径报告，这里不做改写。
        return Ok(false);
    };
    let Some(root) = raw.as_object_mut() else {
        return Ok(false);
    };

    let providers = root.get("providers").cloned().unwrap_or(Value::Null);
    let registry = root.get("models").cloned().unwrap_or(Value::Null);
    let retired_keys = tiangong_llm::models_config::RETIRED_MODEL_KEYS;
    let mut legacy = LegacyMemoryModels::default();
    let mut changed = false;

    if let Some(routing) = root.get_mut("routing").and_then(Value::as_object_mut) {
        for key in retired_keys {
            let Some(route) = routing.get(*key) else {
                continue;
            };
            // 解析不出完整端点时保留原路由：直接移除会让用户彻底失去这份
            // 配置，且无法在 Memory 页面看到迁移前的值。
            let Some(endpoint) = legacy_endpoint_from_route(route, &providers, &registry) else {
                tracing::warn!("models.json 旧 {key} 路由无法解析为完整端点，保留原值待人工处理");
                continue;
            };
            routing.remove(*key);
            changed = true;
            match *key {
                "embedding" => legacy.embedding = Some(endpoint),
                _ => legacy.rerank = Some(endpoint),
            }
        }
    }

    if let Some(models) = root.get_mut("models").and_then(Value::as_object_mut) {
        let mut fully_retired = Vec::new();
        for (key, entry) in models.iter_mut() {
            let original = entry.clone();
            let Some(capabilities) = entry.get_mut("capabilities").and_then(Value::as_array_mut)
            else {
                continue;
            };
            let before = capabilities.len();
            capabilities.retain(|capability| {
                !capability
                    .as_str()
                    .is_some_and(|capability| retired_keys.contains(&capability))
            });
            if capabilities.len() == before {
                continue;
            }
            changed = true;
            if capabilities.is_empty() {
                fully_retired.push((key.clone(), original));
            }
        }
        for (key, original) in fully_retired {
            models.remove(&key);
            legacy.retired_models.insert(key, original);
        }
    }

    if !changed {
        return Ok(false);
    }

    let handoff_path = legacy_memory_models_path(dir);
    // 交接文件是旧 embedding / rerank 配置的唯一副本，读不出来时宁可整体
    // 放弃本次迁出，也不能覆盖写入把里面的内容冲掉。
    let mut merged = load_legacy_memory_models(&handoff_path)
        .with_context(|| {
            format!(
                "Memory 旧模型交接文件无法解析，已跳过 models.json 迁出：{}",
                handoff_path.display()
            )
        })?
        .unwrap_or_default();
    merged.embedding = merged.embedding.or(legacy.embedding);
    merged.rerank = merged.rerank.or(legacy.rerank);
    for (key, entry) in legacy.retired_models {
        merged.retired_models.entry(key).or_insert(entry);
    }
    if let Some(parent) = handoff_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败：{}", parent.display()))?;
    }
    let handoff = serde_json::to_string_pretty(&merged).context("序列化 Memory 交接配置失败")?;
    std::fs::write(&handoff_path, handoff)
        .with_context(|| format!("写入 Memory 交接文件失败：{}", handoff_path.display()))?;

    let rewritten = serde_json::to_string_pretty(&raw).context("序列化 models.json 失败")?;
    std::fs::write(&models_path, rewritten)
        .with_context(|| format!("改写 models.json 失败：{}", models_path.display()))?;
    tracing::info!(
        handoff = %handoff_path.display(),
        "已将 models.json 中的 embedding/rerank 配置迁出，交由 Memory 插件接管"
    );
    Ok(true)
}

/// 把旧路由值（模型 key 字符串或内联条目）解析为完整端点。
fn legacy_endpoint_from_route(
    route: &Value,
    providers: &Value,
    registry: &Value,
) -> Option<LegacyMemoryEndpoint> {
    let entry = match route {
        Value::String(key) => registry.get(key)?,
        Value::Object(_) => route,
        _ => return None,
    };
    let provider_name = entry.get("provider")?.as_str()?.to_string();
    let model = entry.get("model")?.as_str()?.trim().to_string();
    let provider = providers.get(&provider_name)?;
    let base_url = provider.get("base_url")?.as_str()?.trim().to_string();
    if model.is_empty() || base_url.is_empty() {
        return None;
    }
    let protocol = provider
        .get("protocol")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    Some(LegacyMemoryEndpoint {
        provider: provider_name,
        base_url,
        api_key: provider
            .get("api_key")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        protocol,
        timeout_ms: provider
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or_else(default_legacy_timeout_ms),
        model,
        dimension: entry
            .get("options")
            .and_then(|options| options.get("dimension"))
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0),
    })
}

// ---------------------------------------------------------------------------
// 自定义 Prompt
// ---------------------------------------------------------------------------

/// 读取自定义 Prompt，优先 `custom-prompt.md`，回退 `legacy`（旧字段值）。
///
/// - 文件存在 → 读取其内容（去除首尾空白后非空才采用）。
/// - 否则返回 `legacy`。
pub fn load_custom_prompt_at(path: &Path, legacy: &str) -> Result<String> {
    if !path.exists() {
        return Ok(legacy.to_string());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取自定义 Prompt 失败：{}", path.display()))?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        Ok(legacy.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

/// 读取默认路径（`~/.tiangong/custom-prompt.md`）的自定义 Prompt。
pub fn load_custom_prompt(legacy: &str) -> Result<String> {
    load_custom_prompt_at(&custom_prompt_path(), legacy)
}

/// 保存自定义 Prompt 到指定路径（覆盖写，自动创建父目录）。
pub fn save_custom_prompt_at(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败：{}", parent.display()))?;
    }
    std::fs::write(path, content)
        .with_context(|| format!("写入自定义 Prompt 失败：{}", path.display()))?;
    Ok(())
}

/// 保存自定义 Prompt 到默认路径。
pub fn save_custom_prompt(content: &str) -> Result<()> {
    save_custom_prompt_at(&custom_prompt_path(), content)
}

/// 清除指定路径的自定义 Prompt（删除文件，文件不存在视为成功）。
pub fn clear_custom_prompt_at(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("删除自定义 Prompt 失败：{}", path.display()))?;
    }
    Ok(())
}

/// 清除默认路径的自定义 Prompt。
pub fn clear_custom_prompt() -> Result<()> {
    clear_custom_prompt_at(&custom_prompt_path())
}

// ---------------------------------------------------------------------------
// context_windows
// ---------------------------------------------------------------------------

/// 内嵌的默认 context_windows.json 内容（首次安装释放到用户目录）。
pub fn default_context_windows_json() -> &'static str {
    include_str!("resources/context_windows.json")
}

/// 启动时用内嵌默认表直接覆盖 `dir/context_windows.json`，
/// 让默认映射随程序版本更新自动同步；该文件按约定不承载用户自定义。
pub fn ensure_context_windows(dir: &Path) {
    let path = dir.join("context_windows.json");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = std::fs::write(&path, default_context_windows_json()) {
        tracing::warn!("写入 context_windows.json 失败：{err}");
    }
}

/// 解析 context_window，优先使用模型配置的 override。
///
/// - `override` 非空非 0：直接返回（用户显式配置优先）
/// - 否则走 `resolve_context_limit_at`（context_windows.json 映射）
pub fn resolve_context_limit_with_override(
    dir: &Path,
    model_name: &str,
    override_value: Option<usize>,
) -> usize {
    if let Some(ctx) = override_value
        && ctx > 0
    {
        return ctx;
    }
    resolve_context_limit_at(dir, model_name)
}

/// 根据模型名称从预设映射表解析 context_window。
///
/// 读取 `dir/context_windows.json`（不存在则用内嵌默认表）。这是**前端编辑
/// 模型时的预填预设**，不是运行期真相——会话实际使用的窗口来自 `models.json`
/// 中该模型条目的 `context_window`（经 `ModelEndpoint` 随模型切换同步）。
///
/// 精确匹配 > 最长键匹配（无 `*` 的键按前缀匹配，含 `*` 的键按通配符匹配）
/// > `DEFAULT_CONTEXT_LIMIT`。
pub fn resolve_context_limit_at(dir: &Path, model_name: &str) -> usize {
    const DEFAULT_MAP: &str = include_str!("resources/context_windows.json");

    let path = dir.join("context_windows.json");
    let content = if path.exists() {
        std::fs::read_to_string(&path).unwrap_or_else(|_| DEFAULT_MAP.to_string())
    } else {
        DEFAULT_MAP.to_string()
    };

    let map: std::collections::HashMap<String, Value> = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!(
                "解析 context_windows.json 失败：{err}，使用默认值 {DEFAULT_CONTEXT_LIMIT}"
            );
            return DEFAULT_CONTEXT_LIMIT;
        }
    };

    // 精确匹配
    if let Some(Some(n)) = map.get(model_name).map(|v| v.as_u64()) {
        return n as usize;
    }

    // 最长键匹配：无 `*` 的键按前缀匹配，含 `*` 的键按通配符匹配
    let mut best_match: Option<usize> = None;
    let mut best_len = 0;
    for (key, val) in &map {
        if key.starts_with('_') {
            continue;
        }
        let hit = if key.contains('*') {
            wildcard_match(key, model_name)
        } else {
            model_name.starts_with(key)
        };
        if hit
            && key.len() > best_len
            && let Some(n) = val.as_u64()
        {
            best_match = Some(n as usize);
            best_len = key.len();
        }
    }
    best_match.unwrap_or(DEFAULT_CONTEXT_LIMIT)
}

/// 通配符匹配：`*` 匹配任意（含空）字符串，其余字符字面相等。
/// 如 `glm-4.5*` 匹配 glm-4.5 及其变体，`*-flash` 匹配任意 flash 后缀，
/// `gpt-*-mini` 匹配中间任意。
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let Some((first, rest)) = parts.split_first() else {
        return false;
    };
    let Some(mut s) = name.strip_prefix(first) else {
        return false;
    };
    let Some((last, mid)) = rest.split_last() else {
        return true;
    };
    for part in mid {
        match s.find(part) {
            Some(i) => s = &s[i + part.len()..],
            None => return false,
        }
    }
    s.ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_llm::ProviderProtocol;
    use tiangong_llm::models_config::{
        ModelCapability, ModelEntry, ModelsConfig, ProviderConfig, RoutingSlot,
    };

    fn write_legacy_models_json(dir: &Path) {
        std::fs::write(
            dir.join("models.json"),
            r#"{
              "providers": {
                "lm": { "base_url": "http://127.0.0.1:1234/v1", "api_key": "${LM_KEY}",
                        "timeout_ms": 300000, "protocol": "openai_chatcompletions" },
                "cloud": { "base_url": "https://api.example.com/v1", "api_key": "sk" }
              },
              "models": {
                "chat": { "provider": "cloud", "model": "gpt", "capabilities": ["chat"] },
                "bge": { "provider": "lm", "model": "text-embedding-bge-m3",
                         "capabilities": ["embedding"], "options": { "dimension": 1024 } },
                "both": { "provider": "cloud", "model": "m", "capabilities": ["chat", "rerank"] }
              },
              "routing": {
                "chat": "chat",
                "embedding": "bge",
                "rerank": { "provider": "lm", "model": "bge-reranker-v2-m3" }
              }
            }"#,
        )
        .unwrap();
    }

    /// models.json 中的 embedding/rerank 应迁出到 memory/legacy-models.json，
    /// models.json 其余内容保持可用，且迁出幂等。
    #[test]
    fn extract_legacy_memory_models_moves_embedding_and_rerank_out() {
        let dir = tempfile::tempdir().unwrap();
        write_legacy_models_json(dir.path());

        assert!(extract_legacy_memory_models_at(dir.path()).unwrap());

        let legacy = load_legacy_memory_models(&legacy_memory_models_path(dir.path()))
            .unwrap()
            .expect("应生成交接文件");
        let embedding = legacy.embedding.expect("embedding 端点应迁出");
        assert_eq!(embedding.model, "text-embedding-bge-m3");
        assert_eq!(embedding.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(embedding.api_key, "${LM_KEY}", "密钥保持原始引用写法");
        assert_eq!(embedding.timeout_ms, 300_000);
        assert_eq!(embedding.dimension, Some(1024));
        let rerank = legacy.rerank.expect("内联 rerank 路由应迁出");
        assert_eq!(rerank.model, "bge-reranker-v2-m3");
        assert!(
            legacy.retired_models.contains_key("bge"),
            "纯 embedding 模型归档"
        );
        assert!(!legacy.retired_models.contains_key("both"));

        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("models.json")).unwrap())
                .unwrap();
        assert!(raw["routing"].get("embedding").is_none());
        assert!(raw["routing"].get("rerank").is_none());
        assert!(raw["models"].get("bge").is_none());
        assert_eq!(
            raw["models"]["both"]["capabilities"],
            serde_json::json!(["chat"])
        );

        let models = load_models_config_at(dir.path());
        assert_eq!(models.routing.len(), 1);
        assert!(models.routing.contains_key(&RoutingSlot::Chat));

        // 再次执行无变化，且不覆盖已生成的交接内容。
        assert!(!extract_legacy_memory_models_at(dir.path()).unwrap());
        assert!(
            load_legacy_memory_models(&legacy_memory_models_path(dir.path()))
                .unwrap()
                .is_some_and(|legacy| legacy.embedding.is_some())
        );
    }

    /// 加载完整配置时自动完成迁出；无旧配置时不生成交接文件。
    #[test]
    fn loader_extracts_legacy_memory_models_on_load() {
        let dir = tempfile::tempdir().unwrap();
        write_legacy_models_json(dir.path());
        let config = crate::loader::load_tiangong_config_from_dir(dir.path());
        assert!(config.models.routing.contains_key(&RoutingSlot::Chat));
        assert!(legacy_memory_models_path(dir.path()).exists());

        let clean = tempfile::tempdir().unwrap();
        std::fs::write(
            clean.path().join("models.json"),
            r#"{"providers":{},"models":{},"routing":{}}"#,
        )
        .unwrap();
        crate::loader::load_tiangong_config_from_dir(clean.path());
        assert!(!legacy_memory_models_path(clean.path()).exists());
    }

    /// 未经 loader 直接保存（CLI 读-改-写路径）时，也必须先迁出旧配置再覆盖。
    #[test]
    fn save_extracts_legacy_memory_models_before_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        write_legacy_models_json(dir.path());
        let config = load_models_config_at(dir.path());
        save_models_config_at(dir.path(), &config).unwrap();

        let legacy = load_legacy_memory_models(&legacy_memory_models_path(dir.path()))
            .unwrap()
            .expect("保存前应先生成交接文件");
        assert_eq!(
            legacy.embedding.map(|endpoint| endpoint.dimension),
            Some(Some(1024))
        );
    }

    /// 按需进程沙箱开关持久化：关闭状态写入 app.json 并能读回；
    /// 默认开启时序列化省略该字段（旧版文件兼容）。
    #[test]
    fn app_config_roundtrips_sandbox_disabled() {
        let dir = tempfile::tempdir().unwrap();
        save_app_config_at(
            dir.path(),
            TrustMode::default(),
            dir.path().to_str().unwrap(),
            true,
            &crate::config::SandboxUserPolicy::default(),
            &vec!["MY_SECRET_TOKEN".to_string()],
        )
        .unwrap();
        assert!(crate::loader::load_tiangong_config_from_dir(dir.path()).sandbox_disabled);
        assert_eq!(
            crate::loader::load_tiangong_config_from_dir(dir.path()).command_env_blocklist,
            vec!["MY_SECRET_TOKEN".to_string()]
        );

        save_app_config_at(
            dir.path(),
            TrustMode::default(),
            dir.path().to_str().unwrap(),
            false,
            &crate::config::SandboxUserPolicy::default(),
            &Vec::new(),
        )
        .unwrap();
        let raw = std::fs::read_to_string(dir.path().join("app.json")).unwrap();
        assert!(
            !raw.contains("sandbox_disabled"),
            "默认开启时不应写入字段: {raw}"
        );
        assert!(!crate::loader::load_tiangong_config_from_dir(dir.path()).sandbox_disabled);
    }

    #[test]
    fn app_config_roundtrips_user_sandbox_policy_without_restricting_paths() {
        let dir = tempfile::tempdir().unwrap();
        let policy = crate::config::SandboxUserPolicy {
            directory_allowlist: vec!["/".to_string()],
            environment_blocklist: vec!["SECRET".to_string()],
        };
        save_app_config_at(
            dir.path(),
            TrustMode::default(),
            dir.path().to_str().unwrap(),
            false,
            &policy,
            &Vec::new(),
        )
        .unwrap();
        assert_eq!(
            crate::loader::load_tiangong_config_from_dir(dir.path()).sandbox_policy,
            policy
        );
    }

    /// save_models_config_at 应把 routing 中未注册到 models 的条目自动补入 models。
    #[test]
    fn save_auto_registers_routing_entries_to_models() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = ModelsConfig::default();
        config.providers.insert(
            "p".to_string(),
            ProviderConfig {
                headers: Default::default(),
                base_url: "https://api.test.com".to_string(),
                api_key: "k".to_string(),
                timeout_ms: 60_000,
                protocol: ProviderProtocol::OpenAiChatCompletions,
            },
        );
        config.routing.insert(
            RoutingSlot::Chat,
            ModelEntry {
                provider: "p".to_string(),
                model: "gpt-4".to_string(),
                capabilities: vec![ModelCapability::Chat],
                options: serde_json::json!({}),
                context_window: None,
            },
        );

        save_models_config_at(dir.path(), &config).unwrap();
        let reloaded = load_models_config_at(dir.path());
        assert!(reloaded.models.values().any(|m| m.model == "gpt-4"));
    }

    #[test]
    fn resolve_context_limit_exact_and_prefix_match() {
        let dir = tempfile::tempdir().unwrap();
        ensure_context_windows(dir.path());
        // 内嵌默认表含 gpt-4o 与 glm-4.5 前缀
        assert!(resolve_context_limit_at(dir.path(), "gpt-4o") > 0);
        assert!(resolve_context_limit_at(dir.path(), "glm-4.5-flash") > 0);
        // 未知模型回退默认值
        assert_eq!(
            resolve_context_limit_at(dir.path(), "totally-unknown-model"),
            DEFAULT_CONTEXT_LIMIT
        );
    }

    /// 通配符键（含 `*`）按 glob 匹配：尾部、后缀、中间通配各自生效，
    /// 与其他键同时命中时取最长键；不命中任何键回退默认值。
    #[test]
    fn resolve_context_limit_wildcard_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("context_windows.json"),
            r#"{
              "_default": 200000,
              "glm-4.5*": 128000,
              "glm-4*": 300000,
              "*-flash": 400000,
              "gpt-*-mini": 500000,
              "glm-4.5": 600000
            }"#,
        )
        .unwrap();

        // 同时命中 glm-4.5*（最长）与 glm-4*、*-flash
        assert_eq!(
            resolve_context_limit_at(dir.path(), "glm-4.5-flash"),
            128000
        );
        // 尾部通配可匹配空串，精确键优先于通配符键
        assert_eq!(resolve_context_limit_at(dir.path(), "glm-4.5"), 600000);
        assert_eq!(resolve_context_limit_at(dir.path(), "glm-4.5-air"), 128000);
        // 仅命中更短的 glm-4*
        assert_eq!(resolve_context_limit_at(dir.path(), "glm-4.6"), 300000);
        // 后缀通配
        assert_eq!(
            resolve_context_limit_at(dir.path(), "deepseek-v4-flash"),
            400000
        );
        // 中间通配
        assert_eq!(resolve_context_limit_at(dir.path(), "gpt-4.1-mini"), 500000);
        // 不命中任何键
        assert_eq!(
            resolve_context_limit_at(dir.path(), "gpt-4.1"),
            DEFAULT_CONTEXT_LIMIT
        );
    }

    /// ensure 时无条件用内嵌默认表覆盖用户目录文件：旧内容被还原为新表，
    /// 手改过的键同样在下次 ensure 时被还原。
    #[test]
    fn context_windows_overwritten_on_ensure() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("context_windows.json");

        // 任意旧内容覆盖为新默认表
        std::fs::write(&file, r#"{ "gpt-4o": 1, "_default": 200000 }"#).unwrap();
        ensure_context_windows(dir.path());
        assert_eq!(resolve_context_limit_at(dir.path(), "gpt-4o"), 128000);

        // 手改后在下次 ensure 被还原
        let content = std::fs::read_to_string(&file).unwrap();
        let customized = content.replace("\"gpt-5.6*\": 1050000", "\"gpt-5.6*\": 999999");
        assert_ne!(customized, content, "替换目标键应存在于默认表");
        std::fs::write(&file, customized).unwrap();
        ensure_context_windows(dir.path());
        assert_eq!(resolve_context_limit_at(dir.path(), "gpt-5.6-sol"), 1050000);
    }
}
