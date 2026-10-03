//! 网页配置页的业务逻辑：直接读写 `<storage_root>` 下的配置文件。
//!
//! 与 HTTP 层解耦（入参为 JSON、出参为 JSON），便于在临时目录中测试。
//! 每次请求都从磁盘重新加载，避免覆盖桌面端或其他命令在此期间写入的配置。
//!
//! 安全约定：
//! - 明文 API Key 与 Server Token 不回传给页面，只返回脱敏信息；
//! - 远程（非回环）模式下，引用环境变量的 Provider 只有在启动时已登记过
//!   （同一变量名 + 同一 base_url）才允许发起测试 / 拉取模型请求，防止持有
//!   令牌的一方把服务器环境变量发往任意地址（与 Memory 配置页一致）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tiangong_core::permission::TrustMode;
use tiangong_llm::models_config::{
    ModelCapability, ModelEntry, ModelsConfig, ProviderConfig, RoutingSlot,
};
use tiangong_llm::{ModelEndpoint, ProviderProtocol, SingleProviderClient};

/// 业务错误：`status` 为 HTTP 状态码。
#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: u16,
    pub message: String,
}

pub(crate) type ApiResult = Result<Value, ApiError>;

fn bad(message: impl Into<String>) -> ApiError {
    ApiError {
        status: 400,
        message: message.into(),
    }
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError {
        status: 500,
        message: error.to_string(),
    }
}

fn parse<T: DeserializeOwned>(body: Value) -> Result<T, ApiError> {
    serde_json::from_value(body).map_err(|error| bad(format!("请求参数无效：{error}")))
}

/// 页面可选的协议（ChatGPT 账号走登录流程，不在此列）。
const PROTOCOLS: &[(&str, &str, &str, &str)] = &[
    (
        "deepseek",
        "DeepSeek",
        "https://api.deepseek.com",
        "deepseek-v4-flash",
    ),
    (
        "openai",
        "OpenAI Responses",
        "https://api.openai.com/v1",
        "gpt-5.6-sol",
    ),
    (
        "openai_chatcompletions",
        "OpenAI Chat Completions（兼容）",
        "https://api.openai.com/v1",
        "gpt-4.1-mini",
    ),
    (
        "anthropic",
        "Anthropic",
        "https://api.anthropic.com",
        "claude-sonnet-4-20250514",
    ),
];

/// 配置页运行期上下文。
pub(crate) struct ApiContext {
    dir: PathBuf,
    remote: bool,
    /// 启动时 models.json 中已登记的 `(环境变量名, base_url)`。
    trusted_env_refs: HashSet<(String, String)>,
    /// ChatGPT 登录后台任务的最近一次结果说明。
    login_note: Mutex<Option<String>>,
}

impl ApiContext {
    pub(crate) fn new(dir: PathBuf, remote: bool) -> Self {
        let trusted_env_refs = collect_env_refs(&load_models(&dir));
        Self {
            dir,
            remote,
            trusted_env_refs,
            login_note: Mutex::new(None),
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn is_remote(&self) -> bool {
        self.remote
    }

    fn set_login_note(&self, note: Option<String>) {
        if let Ok(mut guard) = self.login_note.lock() {
            *guard = note;
        }
    }

    fn login_note(&self) -> Option<String> {
        self.login_note.lock().ok().and_then(|guard| guard.clone())
    }

    /// 远程模式下拒绝用未登记的环境变量引用发起外部请求。
    fn check_env_ref(&self, provider: &ProviderConfig) -> Result<(), ApiError> {
        if !self.remote {
            return Ok(());
        }
        let Some(name) = env_ref_name(&provider.api_key) else {
            return Ok(());
        };
        if self
            .trusted_env_refs
            .contains(&(name.to_string(), provider.base_url.clone()))
        {
            return Ok(());
        }
        Err(bad(format!(
            "远程配置模式下，不能用新登记的环境变量引用 ${{{name}}} 发起请求（防止服务器环境变量被发往任意地址）。请在服务器本机运行 `tiangong config` 测试，或改为直接填写 API Key"
        )))
    }
}

fn load_models(dir: &Path) -> ModelsConfig {
    tiangong_config::io::load_models_config_at(dir)
}

fn save_models(dir: &Path, models: &ModelsConfig) -> Result<(), ApiError> {
    models.validate_headers().map_err(bad)?;
    tiangong_config::io::save_models_config_at(dir, models).map_err(internal)
}

/// 取出 `${VAR}` 中的变量名。
fn env_ref_name(raw: &str) -> Option<&str> {
    raw.trim()
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

fn collect_env_refs(models: &ModelsConfig) -> HashSet<(String, String)> {
    models
        .providers
        .values()
        .filter_map(|provider| {
            env_ref_name(&provider.api_key)
                .map(|name| (name.to_string(), provider.base_url.clone()))
        })
        .collect()
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// 明文密钥脱敏：只保留首尾少量字符。
fn mask_secret(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    if chars.len() <= 10 {
        return "****".to_string();
    }
    let head: String = chars[..3].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}

fn key_view(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({ "kind": "none" });
    }
    if let Some(name) = env_ref_name(raw) {
        let resolved = std::env::var(name).is_ok_and(|value| !value.is_empty());
        return json!({ "kind": "env", "env": name, "resolved": resolved });
    }
    json!({ "kind": "plain", "masked": mask_secret(raw) })
}

fn trust_mode_key(mode: TrustMode) -> &'static str {
    match mode {
        TrustMode::FullTrust => "full_trust",
        TrustMode::Supervised => "supervised",
    }
}

/// 路由条目在模型注册表中的名称。
fn registered_name<'a>(models: &'a ModelsConfig, entry: &ModelEntry) -> Option<&'a str> {
    models
        .models
        .iter()
        .find(|(_, model)| model.provider == entry.provider && model.model == entry.model)
        .map(|(name, _)| name.as_str())
}

/// 统一入口：`action` 对应页面操作。
pub(crate) async fn dispatch(ctx: &Arc<ApiContext>, action: &str, body: Value) -> ApiResult {
    if action.starts_with("plugins.") {
        return match super::plugins::dispatch(ctx, action, body).await {
            Some(result) => result,
            None => Err(ApiError {
                status: 404,
                message: format!("未知操作：{action}"),
            }),
        };
    }
    match action {
        "state" => state(ctx).await,
        "provider.save" => provider_save(ctx, parse(body)?),
        "provider.remove" => provider_remove(ctx, parse(body)?),
        "provider.fetch_models" => provider_fetch_models(ctx, parse(body)?).await,
        "provider.register_models" => provider_register_models(ctx, parse(body)?),
        "model.save" => model_save(ctx, parse(body)?),
        "model.remove" => model_remove(ctx, parse(body)?),
        "model.test" => model_test(ctx, parse(body)?).await,
        "route.set" => route_set(ctx, parse(body)?),
        "server.save" => server_save(ctx, parse(body)?),
        "general.save" => general_save(ctx, parse(body)?),
        "prompt.save" => prompt_save(ctx, parse(body)?),
        "chatgpt.status" => chatgpt_status(ctx).await,
        "chatgpt.login" => chatgpt_login(ctx, parse(body)?).await,
        "chatgpt.logout" => chatgpt_logout(ctx).await,
        other => Err(ApiError {
            status: 404,
            message: format!("未知操作：{other}"),
        }),
    }
}

// ── 读取 ──────────────────────────────────────────────────────────

async fn state(ctx: &ApiContext) -> ApiResult {
    let dir = &ctx.dir;
    let models = load_models(dir);

    let mut provider_names: Vec<&String> = models.providers.keys().collect();
    provider_names.sort();
    let providers: Vec<Value> = provider_names
        .into_iter()
        .map(|name| {
            let provider = &models.providers[name];
            json!({
                "name": name,
                "protocol": provider.protocol.as_str(),
                "base_url": provider.base_url,
                "timeout_ms": provider.timeout_ms,
                "headers": provider.headers.len(),
                "api_key": key_view(&provider.api_key),
            })
        })
        .collect();

    let mut model_names: Vec<&String> = models.models.keys().collect();
    model_names.sort();
    let model_list: Vec<Value> = model_names
        .into_iter()
        .map(|name| {
            let entry = &models.models[name];
            json!({
                "name": name,
                "provider": entry.provider,
                "model": entry.model,
                "capabilities": entry.capabilities.iter().map(|c| c.key()).collect::<Vec<_>>(),
                "context_window": entry.context_window,
            })
        })
        .collect();

    let routes: Vec<Value> = RoutingSlot::all()
        .iter()
        .map(|slot| {
            let entry = models.routing.get(slot);
            json!({
                "slot": slot.key(),
                "label": slot.display_name(),
                "model": entry.and_then(|entry| registered_name(&models, entry)),
                "provider": entry.map(|entry| entry.provider.clone()),
                "model_id": entry.map(|entry| entry.model.clone()),
            })
        })
        .collect();

    let server = tiangong_config::load_server_config_from_dir(dir);
    let app = tiangong_config::load_tiangong_config_from_dir(dir);
    let protocols: Vec<Value> = PROTOCOLS
        .iter()
        .map(|(value, label, base_url, model)| {
            json!({ "value": value, "label": label, "base_url": base_url, "model": model })
        })
        .collect();

    Ok(json!({
        "meta": {
            "version": env!("CARGO_PKG_VERSION"),
            "storage_root": dir.display().to_string(),
            "remote": ctx.remote,
            "protocols": protocols,
        },
        "providers": providers,
        "models": model_list,
        "routes": routes,
        "server": {
            "host": server.host,
            "port": server.port,
            "has_token": server.auth_token.as_deref().is_some_and(|t| !t.trim().is_empty()),
            "token": server.masked_auth_token(),
        },
        "general": {
            "default_trust_mode": trust_mode_key(app.default_trust_mode),
            "workspace_dir": app.workspace_dir,
        },
        "prompt": app.custom_system_prompt,
        "chatgpt": chatgpt_status_value(ctx).await,
    }))
}

// ── Provider ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ProviderSave {
    name: String,
    protocol: String,
    #[serde(default)]
    base_url: String,
    /// keep（保留原值）/ env（环境变量名）/ plain（明文）/ none（无需密钥）
    #[serde(default)]
    key_mode: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn provider_save(ctx: &ApiContext, req: ProviderSave) -> ApiResult {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(bad("服务名称不能为空"));
    }
    let protocol: ProviderProtocol = req
        .protocol
        .parse()
        .map_err(|error| bad(format!("{error}")))?;
    if protocol.uses_oauth() {
        return Err(bad("ChatGPT 账号请使用页面中的「登录 ChatGPT」完成配置"));
    }
    let base_url = req.base_url.trim().trim_end_matches('/').to_string();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(bad("Base URL 需以 http:// 或 https:// 开头"));
    }

    let mut models = load_models(&ctx.dir);
    let existing = models.providers.get(name).cloned();
    let key_mode = if req.key_mode.is_empty() {
        "keep"
    } else {
        req.key_mode.as_str()
    };
    let api_key = match key_mode {
        "keep" => existing
            .as_ref()
            .map(|provider| provider.api_key.clone())
            .ok_or_else(|| bad("新服务需要选择 API Key 方式"))?,
        "env" => {
            let env = req
                .api_key
                .trim()
                .trim_start_matches("${")
                .trim_end_matches('}');
            if !valid_env_name(env) {
                return Err(bad(format!("无效的环境变量名：{env}")));
            }
            format!("${{{env}}}")
        }
        "plain" => {
            let key = req.api_key.trim();
            if key.is_empty() {
                return Err(bad("API Key 不能为空"));
            }
            key.to_string()
        }
        "none" => String::new(),
        other => return Err(bad(format!("未知的 API Key 方式：{other}"))),
    };
    let timeout_ms = req
        .timeout_ms
        .or(existing.as_ref().map(|provider| provider.timeout_ms))
        .unwrap_or(60_000);
    if timeout_ms < 1_000 {
        return Err(bad("超时时间至少 1000 毫秒"));
    }
    models.providers.insert(
        name.to_string(),
        ProviderConfig {
            // 自定义请求头不在页面上编辑，编辑时原样保留。
            headers: existing
                .map(|provider| provider.headers)
                .unwrap_or_default(),
            base_url,
            api_key,
            timeout_ms,
            protocol,
        },
    );
    save_models(&ctx.dir, &models)?;
    Ok(json!({ "message": format!("已保存模型服务 {name}") }))
}

#[derive(Debug, Deserialize)]
struct ProviderRemove {
    name: String,
    #[serde(default)]
    force: bool,
}

fn provider_remove(ctx: &ApiContext, req: ProviderRemove) -> ApiResult {
    let mut models = load_models(&ctx.dir);
    if !models.providers.contains_key(&req.name) {
        return Err(bad(format!("模型服务 {} 不存在", req.name)));
    }
    let refs = models.provider_referenced_by(&req.name);
    let referenced = !refs.models.is_empty() || !refs.routes.is_empty();
    if referenced && !req.force {
        return Ok(json!({
            "needs_force": true,
            "models": refs.models,
            "routes": refs.routes,
        }));
    }
    models.remove_provider_force(&req.name);
    save_models(&ctx.dir, &models)?;
    Ok(json!({ "message": format!("已删除模型服务 {}", req.name) }))
}

/// 构造请求某个 Provider 的端点（`model` 可为空，用于列模型）。
fn provider_endpoint(
    ctx: &ApiContext,
    models: &ModelsConfig,
    provider_name: &str,
    entry: Option<&ModelEntry>,
) -> Result<ModelEndpoint, ApiError> {
    let provider = models
        .providers
        .get(provider_name)
        .ok_or_else(|| bad(format!("模型服务 {provider_name} 不存在")))?;
    ctx.check_env_ref(provider)?;
    let api_key = ModelsConfig::resolve_api_key(&provider.api_key);
    if api_key.trim().is_empty()
        && !provider.protocol.uses_oauth()
        && let Some(name) = env_ref_name(&provider.api_key)
    {
        return Err(bad(format!("环境变量 {name} 未设置或为空")));
    }
    Ok(ModelEndpoint {
        headers: provider.headers.clone(),
        base_url: provider.base_url.clone(),
        api_key,
        protocol: provider.protocol,
        timeout_ms: provider.timeout_ms,
        model: entry.map(|entry| entry.model.clone()).unwrap_or_default(),
        options: entry
            .map(|entry| entry.options.clone())
            .unwrap_or_else(|| json!({})),
        context_window: entry.and_then(|entry| entry.context_window),
    })
}

#[derive(Debug, Deserialize)]
struct ProviderName {
    name: String,
}

async fn provider_fetch_models(ctx: &ApiContext, req: ProviderName) -> ApiResult {
    let models = load_models(&ctx.dir);
    let endpoint = provider_endpoint(ctx, &models, &req.name, None)?;
    let infos = SingleProviderClient::list_model_infos_async(&endpoint)
        .await
        .map_err(|error| bad(format!("拉取模型列表失败：{error:#}")))?;
    let registered: HashSet<&str> = models
        .models
        .values()
        .filter(|entry| entry.provider == req.name)
        .map(|entry| entry.model.as_str())
        .collect();
    let list: Vec<Value> = infos
        .iter()
        .map(|info| {
            json!({
                "id": info.id,
                "display_name": info.display_name,
                "context_window": info.context_window,
                "registered": registered.contains(info.id.as_str()),
            })
        })
        .collect();
    Ok(json!({ "models": list }))
}

#[derive(Debug, Deserialize)]
struct RegisterItem {
    id: String,
    #[serde(default)]
    context_window: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RegisterModels {
    provider: String,
    models: Vec<RegisterItem>,
    #[serde(default)]
    capabilities: Vec<String>,
}

fn parse_capabilities(raw: &[String]) -> Result<Vec<ModelCapability>, ApiError> {
    let mut result = Vec::new();
    for key in raw {
        let capability = ModelCapability::from_key(key)
            .ok_or_else(|| bad(format!("无效的模型能力：{key}（可用 chat/multimodal）")))?;
        if !result.contains(&capability) {
            result.push(capability);
        }
    }
    Ok(result)
}

fn provider_register_models(ctx: &ApiContext, req: RegisterModels) -> ApiResult {
    let mut models = load_models(&ctx.dir);
    if !models.providers.contains_key(&req.provider) {
        return Err(bad(format!("模型服务 {} 不存在", req.provider)));
    }
    let capabilities = parse_capabilities(&req.capabilities)?;
    let mut added = Vec::new();
    for item in &req.models {
        let id = item.id.trim();
        if id.is_empty() {
            continue;
        }
        let exists = models
            .models
            .values()
            .any(|entry| entry.provider == req.provider && entry.model == id);
        if exists {
            continue;
        }
        let key = if models.models.contains_key(id) {
            format!("{}-{id}", req.provider)
        } else {
            id.to_string()
        };
        models.models.insert(
            key.clone(),
            ModelEntry {
                provider: req.provider.clone(),
                model: id.to_string(),
                capabilities: capabilities.clone(),
                context_window: item.context_window.filter(|window| *window > 0),
                ..Default::default()
            },
        );
        added.push(key);
    }
    save_models(&ctx.dir, &models)?;
    Ok(json!({ "message": format!("已添加 {} 个模型", added.len()), "added": added }))
}

// ── Model ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ModelSave {
    name: String,
    #[serde(default)]
    original_name: Option<String>,
    provider: String,
    model: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    context_window: Option<usize>,
}

fn model_save(ctx: &ApiContext, req: ModelSave) -> ApiResult {
    let name = req.name.trim();
    let model_id = req.model.trim();
    if name.is_empty() || model_id.is_empty() {
        return Err(bad("模型名称与模型 ID 不能为空"));
    }
    let mut models = load_models(&ctx.dir);
    if !models.providers.contains_key(&req.provider) {
        return Err(bad(format!("模型服务 {} 不存在", req.provider)));
    }
    let capabilities = parse_capabilities(&req.capabilities)?;
    let original = req
        .original_name
        .as_deref()
        .map(str::trim)
        .filter(|original| !original.is_empty());
    if original != Some(name) && models.models.contains_key(name) {
        return Err(bad(format!("模型名称 {name} 已存在")));
    }
    let previous = match original {
        Some(original) => Some(
            models
                .models
                .remove(original)
                .ok_or_else(|| bad(format!("模型 {original} 不存在")))?,
        ),
        None => None,
    };
    let entry = ModelEntry {
        provider: req.provider.clone(),
        model: model_id.to_string(),
        capabilities,
        options: previous
            .as_ref()
            .map(|previous| previous.options.clone())
            .unwrap_or_else(|| json!({})),
        context_window: req.context_window.filter(|window| *window > 0),
    };
    // 路由保存的是模型条目副本：同步指向旧条目的路由，避免改名/改 ID 后路由悬空。
    let mut dropped_routes = Vec::new();
    if let Some(previous) = &previous {
        for (slot, route) in models.routing.iter_mut() {
            if route.provider == previous.provider && route.model == previous.model {
                *route = entry.clone();
            }
            if !route.capabilities.contains(&slot.capability()) {
                dropped_routes.push(*slot);
            }
        }
    }
    let mut message = format!("已保存模型 {name}");
    if !dropped_routes.is_empty() {
        for slot in &dropped_routes {
            models.routing.remove(slot);
        }
        let keys: Vec<&str> = dropped_routes.iter().map(|slot| slot.key()).collect();
        message.push_str(&format!(
            "；模型不再具备对话能力，已清除路由：{}",
            keys.join(", ")
        ));
    }
    models.models.insert(name.to_string(), entry);
    save_models(&ctx.dir, &models)?;
    Ok(json!({ "message": message }))
}

#[derive(Debug, Deserialize)]
struct ModelName {
    name: String,
}

fn model_remove(ctx: &ApiContext, req: ModelName) -> ApiResult {
    let mut models = load_models(&ctx.dir);
    let (removed, dangling) = models.remove_model(&req.name);
    if !removed {
        return Err(bad(format!("模型 {} 不存在", req.name)));
    }
    // 悬空路由若保留，保存时会被重新登记为模型，等于删除未生效，这里一并清除。
    for key in &dangling {
        if let Some(slot) = RoutingSlot::from_key(key) {
            models.routing.remove(&slot);
        }
    }
    save_models(&ctx.dir, &models)?;
    let mut message = format!("已删除模型 {}", req.name);
    if !dangling.is_empty() {
        message.push_str(&format!("，并清除路由：{}", dangling.join(", ")));
    }
    Ok(json!({ "message": message }))
}

async fn model_test(ctx: &ApiContext, req: ModelName) -> ApiResult {
    let models = load_models(&ctx.dir);
    let entry = models
        .models
        .get(&req.name)
        .ok_or_else(|| bad(format!("模型 {} 不存在", req.name)))?;
    let endpoint = provider_endpoint(ctx, &models, &entry.provider, Some(entry))?;
    let list = SingleProviderClient::list_models_async(&endpoint)
        .await
        .map_err(|error| bad(format!("连通性测试失败：{error:#}")))?;
    let listed = list.iter().any(|id| id == &entry.model);
    Ok(json!({
        "message": if listed {
            format!("连通成功，服务返回 {} 个模型，包含 {}", list.len(), entry.model)
        } else {
            format!("连通成功，服务返回 {} 个模型（未在列表中找到 {}，请确认模型 ID）", list.len(), entry.model)
        },
        "count": list.len(),
        "listed": listed,
    }))
}

// ── Route ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RouteSet {
    slot: String,
    #[serde(default)]
    model: Option<String>,
}

fn route_set(ctx: &ApiContext, req: RouteSet) -> ApiResult {
    let slot = RoutingSlot::from_key(&req.slot)
        .ok_or_else(|| bad(format!("无效的路由槽位：{}（可用 chat/lite）", req.slot)))?;
    let mut models = load_models(&ctx.dir);
    let message = match req
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        Some(model) => {
            models.set_route_by_name(slot, model).map_err(bad)?;
            format!("已设置 {} 路由 → {model}", slot.display_name())
        }
        None => {
            models.routing.remove(&slot);
            format!("已清除 {} 路由", slot.display_name())
        }
    };
    save_models(&ctx.dir, &models)?;
    Ok(json!({ "message": message }))
}

// ── Server / 通用 / Prompt ────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ServerSave {
    host: String,
    port: u16,
    /// keep / generate / set / clear
    #[serde(default)]
    token_mode: String,
    #[serde(default)]
    token: String,
}

fn server_save(ctx: &ApiContext, req: ServerSave) -> ApiResult {
    let host = req.host.trim();
    if host.is_empty() {
        return Err(bad("监听地址不能为空"));
    }
    if req.port == 0 {
        return Err(bad("端口必须是 1-65535 的整数"));
    }
    let mut config = tiangong_config::load_server_config_from_dir(&ctx.dir);
    config.host = host.to_string();
    config.port = req.port;
    let mut generated = None;
    match req.token_mode.as_str() {
        "" | "keep" => {}
        "generate" => {
            let token = tiangong_config::generate_token(32);
            generated = Some(token.clone());
            config.auth_token = Some(token);
        }
        "set" => {
            let token = req.token.trim();
            if token.chars().count() < 8 {
                return Err(bad("Token 至少 8 个字符"));
            }
            config.auth_token = Some(token.to_string());
        }
        "clear" => config.auth_token = None,
        other => return Err(bad(format!("未知的 Token 方式：{other}"))),
    }
    tiangong_config::save_server_config_to_dir(&ctx.dir, &config).map_err(internal)?;
    Ok(json!({
        "message": format!("已保存 Server 配置 {}:{}（运行中的 Server 需重启后生效）", config.host, config.port),
        "generated_token": generated,
    }))
}

#[derive(Debug, Deserialize)]
struct GeneralSave {
    default_trust_mode: String,
    workspace_dir: String,
}

fn general_save(ctx: &ApiContext, req: GeneralSave) -> ApiResult {
    let trust_mode = match req.default_trust_mode.as_str() {
        "full_trust" => TrustMode::FullTrust,
        "supervised" => TrustMode::Supervised,
        other => return Err(bad(format!("无效的信任模式：{other}"))),
    };
    let workspace_dir = req.workspace_dir.trim();
    if !Path::new(workspace_dir).is_dir() {
        return Err(bad(format!("工作区目录不存在：{workspace_dir}")));
    }
    let app = tiangong_config::load_tiangong_config_from_dir(&ctx.dir);
    tiangong_config::io::save_app_config_at(
        &ctx.dir,
        trust_mode,
        workspace_dir,
        app.sandbox_disabled,
        &app.sandbox_policy,
        &app.command_env_blocklist,
    )
    .map_err(internal)?;
    Ok(json!({ "message": "已保存通用设置" }))
}

#[derive(Debug, Deserialize)]
struct PromptSave {
    #[serde(default)]
    content: String,
}

fn prompt_save(ctx: &ApiContext, req: PromptSave) -> ApiResult {
    let path = ctx.dir.join("custom-prompt.md");
    if req.content.trim().is_empty() {
        tiangong_config::io::clear_custom_prompt_at(&path).map_err(internal)?;
        return Ok(json!({ "message": "已清空自定义 Prompt" }));
    }
    tiangong_config::io::save_custom_prompt_at(&path, &req.content).map_err(internal)?;
    Ok(json!({ "message": "已保存自定义 Prompt" }))
}

// ── ChatGPT 账号 ──────────────────────────────────────────────────

async fn chatgpt_status_value(ctx: &ApiContext) -> Value {
    let status = tiangong_llm::providers::codex::status().await;
    let mut value = serde_json::to_value(status).unwrap_or_else(|_| json!({}));
    if let Some(note) = ctx.login_note() {
        value["note"] = Value::String(note);
    }
    value
}

async fn chatgpt_status(ctx: &ApiContext) -> ApiResult {
    Ok(chatgpt_status_value(ctx).await)
}

#[derive(Debug, Deserialize)]
struct ChatgptLogin {
    #[serde(default)]
    device: bool,
}

async fn chatgpt_login(ctx: &Arc<ApiContext>, req: ChatgptLogin) -> ApiResult {
    if ctx.remote && !req.device {
        return Err(bad(
            "远程配置时浏览器回调无法到达服务器，请改用「设备码登录」",
        ));
    }
    let start = if req.device {
        tiangong_llm::providers::codex::start_device_login().await
    } else {
        tiangong_llm::providers::codex::start_browser_login().await
    }
    .map_err(|error| bad(format!("发起 ChatGPT 登录失败：{error:#}")))?;
    ctx.set_login_note(None);
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let note = match tiangong_llm::providers::codex::wait_login().await {
            Ok(_) => match register_codex(&ctx.dir).await {
                Ok(count) => format!("登录成功，已同步 {count} 个 ChatGPT 模型"),
                Err(error) => {
                    format!("登录成功，但同步模型失败（{error:#}），可稍后在模型服务中拉取")
                }
            },
            Err(error) => format!("登录未完成：{error:#}"),
        };
        ctx.set_login_note(Some(note));
    });
    Ok(json!({ "url": start.url, "user_code": start.user_code }))
}

/// ChatGPT（Codex 登录）固定供应商名，与桌面端预设一致。
const CODEX_PROVIDER_NAME: &str = "ChatGPT";

/// 确保固定供应商 ChatGPT 为账号登录模式（Codex 协议，无 api_key）。
///
/// 已存在（如之前用 API Key 模式配置过）时切换为账号登录，保留超时与自定义请求头。
fn ensure_codex_provider(config: &mut ModelsConfig) {
    if let Some(provider) = config.providers.get_mut(CODEX_PROVIDER_NAME) {
        provider.protocol = ProviderProtocol::Codex;
        provider.base_url = tiangong_llm::providers::codex::CODEX_BASE_URL.to_string();
        provider.api_key.clear();
        return;
    }
    config.upsert_provider(
        CODEX_PROVIDER_NAME,
        tiangong_llm::providers::codex::CODEX_BASE_URL,
        "",
        ProviderProtocol::Codex,
        300_000,
    );
}

/// 登录后确保固定供应商 ChatGPT 存在并同步模型列表（全系 chat + multimodal）。
async fn register_codex(dir: &Path) -> anyhow::Result<usize> {
    let mut models = load_models(dir);
    ensure_codex_provider(&mut models);
    let endpoint = ModelEndpoint {
        base_url: tiangong_llm::providers::codex::CODEX_BASE_URL.to_string(),
        protocol: ProviderProtocol::Codex,
        timeout_ms: 60_000,
        ..Default::default()
    };
    let infos = SingleProviderClient::list_model_infos_async(&endpoint).await;
    let count = match &infos {
        Ok(infos) => {
            models.register_provider_models(
                CODEX_PROVIDER_NAME,
                infos,
                &[ModelCapability::Chat, ModelCapability::Multimodal],
            );
            infos.len()
        }
        Err(_) => 0,
    };
    tiangong_config::io::save_models_config_at(dir, &models)?;
    infos.map(|_| count)
}

async fn chatgpt_logout(ctx: &ApiContext) -> ApiResult {
    tiangong_llm::providers::codex::logout()
        .await
        .map_err(|error| bad(format!("退出登录失败：{error:#}")))?;
    ctx.set_login_note(None);
    Ok(json!({ "message": "已退出 ChatGPT 账号" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(remote: bool) -> (tempfile::TempDir, Arc<ApiContext>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = Arc::new(ApiContext::new(dir.path().to_path_buf(), remote));
        (dir, ctx)
    }

    fn run(ctx: &Arc<ApiContext>, action: &str, body: Value) -> ApiResult {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(dispatch(ctx, action, body))
    }

    #[test]
    fn provider_model_route_round_trip() {
        let (dir, ctx) = context(false);
        run(
            &ctx,
            "provider.save",
            json!({"name":"ds","protocol":"deepseek","base_url":"https://api.deepseek.com/","key_mode":"env","api_key":"DS_KEY"}),
        )
        .expect("保存服务");
        run(
            &ctx,
            "model.save",
            json!({"name":"ds-chat","provider":"ds","model":"deepseek-chat","capabilities":["chat"]}),
        )
        .expect("保存模型");
        run(&ctx, "route.set", json!({"slot":"chat","model":"ds-chat"})).expect("设置路由");

        let models = load_models(dir.path());
        let provider = &models.providers["ds"];
        assert_eq!(provider.api_key, "${DS_KEY}");
        assert_eq!(provider.base_url, "https://api.deepseek.com");
        assert_eq!(models.routing[&RoutingSlot::Chat].model, "deepseek-chat");

        // 改名 + 改 ID 后路由跟随，不留悬空条目。
        run(
            &ctx,
            "model.save",
            json!({"name":"main","original_name":"ds-chat","provider":"ds","model":"deepseek-v4","capabilities":["chat"]}),
        )
        .expect("改名");
        let models = load_models(dir.path());
        assert!(!models.models.contains_key("ds-chat"));
        assert_eq!(models.routing[&RoutingSlot::Chat].model, "deepseek-v4");
        assert_eq!(models.models.len(), 1);

        let state = run(&ctx, "state", json!({})).expect("state");
        assert_eq!(state["routes"][0]["model"], "main");
        assert_eq!(state["providers"][0]["api_key"]["kind"], "env");

        // 删除模型同时清除路由。
        run(&ctx, "model.remove", json!({"name":"main"})).expect("删除模型");
        let models = load_models(dir.path());
        assert!(models.routing.is_empty() && models.models.is_empty());
    }

    #[test]
    fn plaintext_key_is_never_returned() {
        let (_dir, ctx) = context(false);
        run(
            &ctx,
            "provider.save",
            json!({"name":"o","protocol":"openai","base_url":"https://api.openai.com/v1","key_mode":"plain","api_key":"sk-secret-value-123456"}),
        )
        .unwrap();
        // keep 模式保留原密钥。
        run(
            &ctx,
            "provider.save",
            json!({"name":"o","protocol":"openai","base_url":"https://example.com/v1","key_mode":"keep"}),
        )
        .unwrap();
        let state = run(&ctx, "state", json!({})).unwrap();
        let text = state.to_string();
        assert!(!text.contains("sk-secret-value-123456"));
        assert_eq!(state["providers"][0]["api_key"]["masked"], "sk-****3456");
        assert_eq!(
            load_models(&ctx.dir).providers["o"].api_key,
            "sk-secret-value-123456"
        );
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        let (_dir, ctx) = context(false);
        let new_without_key = run(
            &ctx,
            "provider.save",
            json!({"name":"x","protocol":"openai","base_url":"https://a"}),
        );
        assert!(new_without_key.is_err());
        assert!(
            run(
                &ctx,
                "provider.save",
                json!({"name":"x","protocol":"openai","base_url":"ftp://a","key_mode":"none"})
            )
            .is_err()
        );
        assert!(
            run(
                &ctx,
                "provider.save",
                json!({"name":"x","protocol":"codex","base_url":"https://a","key_mode":"none"})
            )
            .is_err()
        );
        assert!(
            run(
                &ctx,
                "provider.save",
                json!({"name":"x","protocol":"openai","base_url":"https://a","key_mode":"env","api_key":"1BAD"})
            )
            .is_err()
        );
        assert!(run(&ctx, "route.set", json!({"slot":"embedding","model":"a"})).is_err());
        assert_eq!(run(&ctx, "nope", json!({})).unwrap_err().status, 404);
    }

    #[test]
    fn provider_remove_requires_force_when_referenced() {
        let (dir, ctx) = context(false);
        run(
            &ctx,
            "provider.save",
            json!({"name":"p","protocol":"openai","base_url":"https://a","key_mode":"none"}),
        )
        .unwrap();
        run(
            &ctx,
            "provider.register_models",
            json!({"provider":"p","models":[{"id":"m1","context_window":1000},{"id":"m1"}],"capabilities":["chat"]}),
        )
        .unwrap();
        let first = run(&ctx, "provider.remove", json!({"name":"p"})).unwrap();
        assert_eq!(first["needs_force"], true);
        run(&ctx, "provider.remove", json!({"name":"p","force":true})).unwrap();
        assert!(load_models(dir.path()).is_empty());
    }

    #[test]
    fn remote_mode_blocks_unregistered_env_refs() {
        let (_dir, ctx) = context(true);
        run(
            &ctx,
            "provider.save",
            json!({"name":"p","protocol":"openai","base_url":"https://evil.example","key_mode":"env","api_key":"HOME"}),
        )
        .unwrap();
        let error = run(&ctx, "provider.fetch_models", json!({"name":"p"})).unwrap_err();
        assert!(error.message.contains("远程配置模式"), "{}", error.message);
    }

    #[test]
    fn server_general_prompt_save() {
        let (dir, ctx) = context(false);
        let result = run(
            &ctx,
            "server.save",
            json!({"host":"0.0.0.0","port":9000,"token_mode":"generate"}),
        )
        .unwrap();
        let token = result["generated_token"].as_str().unwrap().to_string();
        let server = tiangong_config::load_server_config_from_dir(dir.path());
        assert_eq!(server.port, 9000);
        assert_eq!(server.auth_token.as_deref(), Some(token.as_str()));
        assert!(
            run(
                &ctx,
                "server.save",
                json!({"host":"h","port":1,"token_mode":"set","token":"short"})
            )
            .is_err()
        );

        let workspace = dir.path().to_string_lossy().to_string();
        run(
            &ctx,
            "general.save",
            json!({"default_trust_mode":"full_trust","workspace_dir":workspace}),
        )
        .unwrap();
        let app = tiangong_config::load_tiangong_config_from_dir(dir.path());
        assert_eq!(app.default_trust_mode, TrustMode::FullTrust);
        assert!(
            run(
                &ctx,
                "general.save",
                json!({"default_trust_mode":"supervised","workspace_dir":"/no/such/dir/xyz"})
            )
            .is_err()
        );

        run(&ctx, "prompt.save", json!({"content":"用中文回答"})).unwrap();
        let state = run(&ctx, "state", json!({})).unwrap();
        assert_eq!(state["prompt"], "用中文回答");
        run(&ctx, "prompt.save", json!({"content":"  "})).unwrap();
        assert!(!dir.path().join("custom-prompt.md").exists());
    }

    #[test]
    fn secret_masking() {
        assert_eq!(mask_secret("short"), "****");
        assert_eq!(mask_secret("abcdefghijklmn"), "abc****klmn");
        assert_eq!(env_ref_name("${ A }"), Some("A"));
        assert!(valid_env_name("_A1") && !valid_env_name("1A") && !valid_env_name(""));
    }
}
