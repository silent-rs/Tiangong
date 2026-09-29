//! 两层模型配置：Provider + Routing。
//!
//! 这里只保留 **纯路由/配置** 逻辑（解析、序列化、CLI 友好的增删改），
//! 不依赖任何 client / LlmConfig / ModelProviderConfig。后者（`from_legacy`、
//! `to_chat_provider_config`、`to_lite_provider_config`、`from_llm_config`）已随
//! ModelProviderConfig 一并移除。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::ProviderProtocol;

// ---------------------------------------------------------------------------
// 核心类型
// ---------------------------------------------------------------------------

/// 模型能力枚举 — 描述模型具备什么能力
///
/// 模型配置仅支持对话与多模态（图片理解）。多模态不再有独立路由，
/// 由 chat 路由指向的模型是否声明该能力决定。
///
/// `ImageGeneration` / `VideoGeneration` / `Stt` / `Tts` 已从模型配置中移除，
/// 仅为兼容仍引用它们的插件代码而保留：配置读取时不解析、不列入 [`Self::all`]，
/// 也不会解析出任何模型端点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    Chat,
    Multimodal,
    /// 已移除，仅保留代码兼容
    ImageGeneration,
    /// 已移除，仅保留代码兼容
    VideoGeneration,
    /// 已移除，仅保留代码兼容
    Stt,
    /// 已移除，仅保留代码兼容
    Tts,
}

/// 已迁出全局模型配置的旧能力 / 路由键。
///
/// Embedding 与 Rerank 归 Memory 插件独立管理（`~/.tiangong/memory/config.json`）。
/// 旧版 models.json 中残留的这些键在读取时静默忽略，保证旧文件仍可解析。
pub const RETIRED_MODEL_KEYS: &[&str] = &["embedding", "rerank"];

/// 已从模型配置中移除的能力键。
///
/// 旧版 models.json 中的这些能力在读取时忽略（记录告警），不阻断解析；
/// 仅声明了这些能力的模型条目整体丢弃，下次保存时从文件中清除。
pub const REMOVED_CAPABILITY_KEYS: &[&str] =
    &["image_generation", "video_generation", "stt", "tts"];

/// 已从模型配置中移除的路由槽位键。
///
/// 多模态改由 chat 模型的能力声明承担，其余为已移除的媒体能力。
/// 旧版 models.json 中的这些路由在读取时忽略（记录告警），下次保存时从文件中清除。
pub const REMOVED_ROUTING_KEYS: &[&str] = &[
    "multimodal",
    "image_generation",
    "video_generation",
    "stt",
    "tts",
];

/// 路由槽位枚举 — 描述哪个模型负责什么任务
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingSlot {
    Chat,
    Lite,
}

impl RoutingSlot {
    pub fn key(&self) -> &'static str {
        match self {
            RoutingSlot::Chat => "chat",
            RoutingSlot::Lite => "lite",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "chat" => Some(RoutingSlot::Chat),
            "lite" => Some(RoutingSlot::Lite),
            _ => None,
        }
    }

    pub fn all() -> &'static [RoutingSlot] {
        &[RoutingSlot::Chat, RoutingSlot::Lite]
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            RoutingSlot::Chat => "对话",
            RoutingSlot::Lite => "轻量文本",
        }
    }

    /// 路由槽位要求模型具备的能力（Lite 与 Chat 一样要求对话能力）
    pub fn capability(&self) -> ModelCapability {
        ModelCapability::Chat
    }
}

impl ModelCapability {
    /// 配置键（snake_case）
    pub fn key(&self) -> &'static str {
        match self {
            ModelCapability::Chat => "chat",
            ModelCapability::Multimodal => "multimodal",
            ModelCapability::ImageGeneration => "image_generation",
            ModelCapability::VideoGeneration => "video_generation",
            ModelCapability::Stt => "stt",
            ModelCapability::Tts => "tts",
        }
    }

    /// 从配置键解析能力（仅识别当前支持的能力，已移除的能力返回 None）
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "chat" => Some(ModelCapability::Chat),
            "multimodal" => Some(ModelCapability::Multimodal),
            _ => None,
        }
    }

    /// 返回所有能力的列表
    pub fn all() -> &'static [ModelCapability] {
        &[ModelCapability::Chat, ModelCapability::Multimodal]
    }

    /// 返回能力的显示名称
    pub fn display_name(&self) -> &'static str {
        match self {
            ModelCapability::Chat => "对话",
            ModelCapability::Multimodal => "多模态",
            ModelCapability::ImageGeneration => "图片生成",
            ModelCapability::VideoGeneration => "视频生成",
            ModelCapability::Stt => "语音识别",
            ModelCapability::Tts => "语音合成",
        }
    }
}

/// Provider 连接配置
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
    pub base_url: String,
    pub api_key: String, // 支持 ${ENV_VAR} 引用
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub protocol: ProviderProtocol,
}

fn default_timeout_ms() -> u64 {
    60_000
}

/// 单个模型配置
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub provider: String,
    pub model: String,
    #[serde(default, deserialize_with = "deserialize_capabilities_lenient")]
    pub capabilities: Vec<ModelCapability>,
    #[serde(default = "default_options")]
    pub options: Value,
    /// 模型上下文窗口（token 数）。仅 Chat / Multimodal 模型适用。
    /// None 或 0 时从 context_windows.json 映射表取默认值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
}

impl Default for ModelEntry {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: String::new(),
            capabilities: vec![],
            options: default_options(),
            context_window: None,
        }
    }
}

fn default_options() -> Value {
    Value::Object(serde_json::Map::new())
}

/// 宽松解析能力列表：跳过已迁出（embedding/rerank）、已移除（图片/视频/语音）
/// 或未知的能力键，保证旧版 models.json 仍可读取。
fn deserialize_capabilities_lenient<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<ModelCapability>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<String>::deserialize(deserializer)?;
    let mut capabilities = Vec::new();
    for key in &raw {
        match ModelCapability::from_key(key) {
            Some(capability) if !capabilities.contains(&capability) => {
                capabilities.push(capability)
            }
            Some(_) => {}
            None if RETIRED_MODEL_KEYS.contains(&key.as_str()) => {}
            None if REMOVED_CAPABILITY_KEYS.contains(&key.as_str()) => {
                tracing::warn!("模型能力 {key} 已移除，读取时忽略");
            }
            None => tracing::warn!("忽略未知模型能力：{key}"),
        }
    }
    Ok(capabilities)
}

/// 判断模型条目是否只声明了已移除的能力（图片/视频/语音）。
///
/// 这类条目在新版中没有任何可用能力，若保留会以“无能力”形态出现在
/// 模型列表中并被当作可选的对话模型，因此读取时整体丢弃。
fn declares_only_removed_capabilities(entry: &Value) -> bool {
    let Some(capabilities) = entry.get("capabilities").and_then(Value::as_array) else {
        return false;
    };
    !capabilities.is_empty()
        && capabilities.iter().all(|capability| {
            capability
                .as_str()
                .is_some_and(|key| REMOVED_CAPABILITY_KEYS.contains(&key))
        })
}

/// 两层模型配置：Provider + Routing
///
/// routing 直接存储 ModelEntry，不再需要中间的 models 映射层。
/// 支持向后兼容：旧格式 routing 值为字符串（引用 models 中的 key），
/// 新格式 routing 值为 ModelEntry 对象。
///
/// 序列化时优先将 routing 值写为字符串引用（确保旧版本也能读取），
/// 仅当 models 中找不到匹配条目时才内联写入 ModelEntry。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelsConfig {
    pub providers: HashMap<String, ProviderConfig>,
    /// 模型注册表 — 存储所有已定义的模型，routing 从中选择
    pub models: HashMap<String, ModelEntry>,
    pub routing: HashMap<RoutingSlot, ModelEntry>,
}

/// 自定义序列化：routing 值优先写为字符串引用，确保旧版本可读取
impl serde::Serialize for ModelsConfig {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("ModelsConfig", 3)?;
        state.serialize_field("providers", &self.providers)?;
        state.serialize_field("models", &self.models)?;

        // routing: 优先写为字符串引用，找不到匹配时内联 ModelEntry
        let routing_compat: HashMap<RoutingSlot, serde_json::Value> = self
            .routing
            .iter()
            .map(|(slot, entry)| {
                let key = self
                    .models
                    .iter()
                    .find(|(_, m)| m.provider == entry.provider && m.model == entry.model)
                    .map(|(k, _)| serde_json::Value::String(k.clone()))
                    .unwrap_or_else(|| {
                        serde_json::to_value(entry).unwrap_or(serde_json::Value::Null)
                    });
                (*slot, key)
            })
            .collect();
        state.serialize_field("routing", &routing_compat)?;

        state.end()
    }
}

/// 向后兼容的反序列化：支持旧格式（routing 值为字符串引用 models）和新格式（routing 值为 ModelEntry）
impl<'de> serde::Deserialize<'de> for ModelsConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            providers: HashMap<String, ProviderConfig>,
            #[serde(default)]
            models: HashMap<String, Value>,
            #[serde(default)]
            routing: HashMap<String, RawRoutingValue>,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawRoutingValue {
            Key(String),
            Entry(ModelEntry),
        }

        let raw = Raw::deserialize(deserializer)?;

        // 仅声明了已移除能力（图片/视频/语音）的模型整体丢弃；其余条目逐个解析。
        let mut models = HashMap::with_capacity(raw.models.len());
        for (key, value) in raw.models {
            if declares_only_removed_capabilities(&value) {
                tracing::warn!("模型 {key} 仅声明了已移除的能力，读取时忽略");
                continue;
            }
            let entry = ModelEntry::deserialize(value).map_err(serde::de::Error::custom)?;
            models.insert(key, entry);
        }

        let routing: HashMap<RoutingSlot, ModelEntry> = raw
            .routing
            .into_iter()
            .filter_map(|(slot_key, val)| {
                let Some(slot) = RoutingSlot::from_key(&slot_key) else {
                    // embedding/rerank 已迁出到 Memory 独立配置，静默忽略；
                    // 已移除的槽位（多模态/图片/视频/语音）与未知槽位记录告警后忽略，
                    // 不阻断整个文件解析，下次保存时从文件中清除。
                    if REMOVED_ROUTING_KEYS.contains(&slot_key.as_str()) {
                        tracing::warn!("路由槽位 {slot_key} 已移除，读取时忽略");
                    } else if !RETIRED_MODEL_KEYS.contains(&slot_key.as_str()) {
                        tracing::warn!("忽略未知路由槽位：{slot_key}");
                    }
                    return None;
                };
                let entry = match val {
                    RawRoutingValue::Key(key) => models.get(&key).cloned().unwrap_or_else(|| {
                        tracing::warn!("路由引用了不存在的模型：{key}");
                        ModelEntry {
                            provider: String::new(),
                            model: key.clone(),
                            capabilities: vec![],
                            options: default_options(),
                            context_window: None,
                        }
                    }),
                    RawRoutingValue::Entry(entry) => entry,
                };
                Some((slot, entry))
            })
            .collect();

        Ok(ModelsConfig {
            providers: raw.providers,
            models,
            routing,
        })
    }
}

/// 解析后的完整模型配置（Provider + Model 合并）
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub headers: std::collections::BTreeMap<String, String>,
    pub provider: String,
    pub base_url: String,
    pub api_key: String, // 已解析环境变量
    pub timeout_ms: u64,
    pub protocol: ProviderProtocol,
    pub model: String,
    pub options: Value,
    /// 模型上下文窗口（透传自 ModelEntry，None 表示用映射表默认）
    pub context_window: Option<usize>,
}

// ---------------------------------------------------------------------------
// 实现
// ---------------------------------------------------------------------------

impl ModelsConfig {
    pub fn validate_headers(&self) -> Result<(), String> {
        for (name, provider) in &self.providers {
            crate::headers::resolve_headers(&provider.headers, "validation-session")
                .map_err(|error| format!("模型服务 {name}：{error}"))?;
        }
        Ok(())
    }
    /// 检查指定能力是否已配置可用
    ///
    /// 多模态没有独立路由：chat 路由指向的模型声明了多模态能力时视为可用。
    pub fn has_capability(&self, capability: ModelCapability) -> bool {
        self.capability_slot(capability).is_some()
    }

    /// 能力当前由哪个路由槽位承担（未配置时为 None）。
    fn capability_slot(&self, capability: ModelCapability) -> Option<RoutingSlot> {
        match capability {
            ModelCapability::Chat => self
                .routing
                .contains_key(&RoutingSlot::Chat)
                .then_some(RoutingSlot::Chat),
            ModelCapability::Multimodal => self.chat_is_multimodal().then_some(RoutingSlot::Chat),
            // 已移除的能力没有路由
            ModelCapability::ImageGeneration
            | ModelCapability::VideoGeneration
            | ModelCapability::Stt
            | ModelCapability::Tts => None,
        }
    }

    /// 判断 chat 路由指向的模型是否支持直接处理图片。
    /// 以模型定义中声明的 capabilities 为准
    pub fn chat_is_multimodal(&self) -> bool {
        let Some(entry) = self.routing.get(&RoutingSlot::Chat) else {
            return false;
        };
        entry.capabilities.contains(&ModelCapability::Multimodal)
    }

    /// 返回当前已配置可用的能力列表
    pub fn available_capabilities(&self) -> Vec<ModelCapability> {
        ModelCapability::all()
            .iter()
            .copied()
            .filter(|cap| self.has_capability(*cap))
            .collect()
    }

    /// 解析 api_key 中的 ${ENV_VAR} 引用
    pub fn resolve_api_key(raw: &str) -> String {
        if let Some(inner) = raw.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
            std::env::var(inner).unwrap_or_default()
        } else {
            raw.to_string()
        }
    }

    /// 获取承担指定能力的路由模型名称
    pub fn routed_model(&self, capability: ModelCapability) -> Option<&str> {
        let slot = self.capability_slot(capability)?;
        self.routing.get(&slot).map(|e| e.model.as_str())
    }

    /// 获取承担指定能力的完整配置（Provider + Model 合并）
    pub fn resolve_for_capability(&self, capability: ModelCapability) -> Option<ResolvedModel> {
        self.resolve_slot(self.capability_slot(capability)?)
    }

    /// 按路由槽位获取完整配置
    pub fn resolve_slot(&self, slot: RoutingSlot) -> Option<ResolvedModel> {
        let entry = self.routing.get(&slot)?;
        let provider = self.providers.get(&entry.provider)?;

        Some(ResolvedModel {
            headers: provider.headers.clone(),
            provider: entry.provider.clone(),
            base_url: provider.base_url.clone(),
            api_key: Self::resolve_api_key(&provider.api_key),
            timeout_ms: provider.timeout_ms,
            protocol: provider.protocol,
            model: entry.model.clone(),
            options: entry.options.clone(),
            context_window: entry.context_window,
        })
    }

    /// 检查 chat 能力是否已配置
    pub fn has_chat(&self) -> bool {
        self.has_capability(ModelCapability::Chat)
    }

    /// 检查配置是否为空（无 provider 也无 routing）
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty() && self.routing.is_empty()
    }

    /// 更新 chat 路由的模型名称，保留其他字段不变
    pub fn update_chat_model(&mut self, model: String) {
        if let Some(entry) = self.routing.get_mut(&RoutingSlot::Chat) {
            entry.model = model;
        } else {
            self.routing.insert(
                RoutingSlot::Chat,
                ModelEntry {
                    provider: "default".to_string(),
                    model,
                    capabilities: vec![ModelCapability::Chat],
                    options: default_options(),
                    context_window: None,
                },
            );
        }
    }

    // ── CLI 友好方法（RFC 0015 §6.1，供 tiangong model 命令使用） ──────

    /// 新增或覆盖 Provider。
    ///
    /// `api_key` 可为明文或 `${ENV_VAR}` 模板（由调用方决定）。
    pub fn upsert_provider(
        &mut self,
        name: &str,
        base_url: &str,
        api_key: &str,
        protocol: ProviderProtocol,
        timeout_ms: u64,
    ) {
        self.providers.insert(
            name.to_string(),
            ProviderConfig {
                headers: Default::default(),
                base_url: base_url.to_string(),
                api_key: api_key.to_string(),
                timeout_ms,
                protocol,
            },
        );
    }

    /// 删除 Provider。
    ///
    /// 若有模型或路由引用该 Provider，返回引用列表，调用方据此决定是否强制删除。
    pub fn provider_referenced_by(&self, name: &str) -> ProviderReferences {
        let mut models = Vec::new();
        for (key, entry) in &self.models {
            if entry.provider == name {
                models.push(key.clone());
            }
        }
        let mut routes = Vec::new();
        for (slot, entry) in &self.routing {
            if entry.provider == name {
                routes.push(slot.key().to_string());
            }
        }
        ProviderReferences { models, routes }
    }

    /// 强制删除 Provider（连同引用它的 model 注册项与路由）。
    pub fn remove_provider_force(&mut self, name: &str) -> usize {
        let mut removed = self.providers.remove(name).is_some() as usize;
        let model_keys: Vec<String> = self
            .models
            .iter()
            .filter(|(_, e)| e.provider == name)
            .map(|(k, _)| k.clone())
            .collect();
        for key in &model_keys {
            self.models.remove(key);
            removed += 1;
        }
        let slots: Vec<RoutingSlot> = self
            .routing
            .iter()
            .filter(|(_, e)| e.provider == name)
            .map(|(s, _)| *s)
            .collect();
        for slot in slots {
            self.routing.remove(&slot);
            removed += 1;
        }
        removed
    }

    /// 新增或覆盖模型注册项。
    pub fn upsert_model(
        &mut self,
        name: &str,
        provider: &str,
        model_id: &str,
        capabilities: Vec<ModelCapability>,
    ) {
        self.models.insert(
            name.to_string(),
            ModelEntry {
                provider: provider.to_string(),
                model: model_id.to_string(),
                capabilities,
                options: default_options(),
                context_window: None,
            },
        );
    }

    /// 批量注册供应商模型，并按服务端元信息同步已有条目。
    ///
    /// - 未注册的模型以 `capabilities` 新建（key 默认为模型 id，冲突时加供应商前缀）；
    /// - 该供应商已注册的模型补齐缺失能力，不移除用户手动增减的其他能力；
    /// - 服务端声明了上下文窗口时，覆盖模型条目及引用它的路由条目的
    ///   `context_window`，使其随服务端变化自动更新。
    ///
    /// 返回是否有变更。
    pub fn register_provider_models(
        &mut self,
        provider: &str,
        models: &[crate::model::ProviderModelInfo],
        capabilities: &[ModelCapability],
    ) -> bool {
        let mut changed = false;
        for info in models {
            let exists = self
                .models
                .values()
                .any(|entry| entry.provider == provider && entry.model == info.id);
            if exists {
                continue;
            }
            let key = if self.models.contains_key(&info.id) {
                format!("{provider}-{}", info.id)
            } else {
                info.id.clone()
            };
            self.upsert_model(&key, provider, &info.id, capabilities.to_vec());
            changed = true;
        }
        let windows: HashMap<&str, usize> = models
            .iter()
            .filter_map(|info| Some((info.id.as_str(), info.context_window?)))
            .collect();
        for entry in self.models.values_mut() {
            if entry.provider != provider {
                continue;
            }
            for capability in capabilities {
                if !entry.capabilities.contains(capability) {
                    entry.capabilities.push(*capability);
                    changed = true;
                }
            }
            if let Some(window) = windows.get(entry.model.as_str())
                && entry.context_window != Some(*window)
            {
                entry.context_window = Some(*window);
                changed = true;
            }
        }
        // 路由条目是模型条目的副本，同步窗口，避免本进程内继续按旧窗口判断压缩。
        for entry in self.routing.values_mut() {
            if entry.provider != provider {
                continue;
            }
            if let Some(window) = windows.get(entry.model.as_str())
                && entry.context_window != Some(*window)
            {
                entry.context_window = Some(*window);
                changed = true;
            }
        }
        changed
    }

    /// 删除模型注册项。
    ///
    /// 返回 (是否删除成功, 删除后变为悬空的路由槽位 key 列表)。
    /// 悬空路由指其 provider+model 在删除后无法在 models 注册表找到匹配的条目。
    pub fn remove_model(&mut self, name: &str) -> (bool, Vec<String>) {
        let removed = self.models.remove(name).is_some();
        let mut dangling_routes = Vec::new();
        for (slot, entry) in &self.routing {
            let still_referenced = self
                .models
                .iter()
                .any(|(_, m)| m.provider == entry.provider && m.model == entry.model);
            if !still_referenced {
                dangling_routes.push(slot.key().to_string());
            }
        }
        (removed, dangling_routes)
    }

    /// 设置路由槽位指向某个已注册的模型。
    ///
    /// `name` 必须是 models 注册表中的 key。
    /// 同时校验模型具备该槽位所需能力：chat 与 lite 槽位均要求模型声明 chat 能力
    /// （lite 用于轻量文本任务，同样由对话模型承担）。
    ///
    /// 返回 Ok(()) 或错误（模型不存在 / 能力不匹配）。
    pub fn set_route_by_name(
        &mut self,
        slot: RoutingSlot,
        name: &str,
    ) -> std::result::Result<(), String> {
        let entry = self
            .models
            .get(name)
            .ok_or_else(|| format!("模型 {name} 不存在于 models 注册表"))?
            .clone();

        // capability 校验：确保路由指向的模型确实具备该槽位所需能力
        let expected = slot.capability();
        if !entry.capabilities.contains(&expected) {
            let current = if entry.capabilities.is_empty() {
                "无".to_string()
            } else {
                entry
                    .capabilities
                    .iter()
                    .map(|c| c.key())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            return Err(format!(
                "模型 {name} 不具备 {} 能力（当前能力：{current}），不能设置到 {} 路由",
                expected.key(),
                slot.key()
            ));
        }

        self.routing.insert(slot, entry);
        Ok(())
    }

    /// 设置路由槽位（直接传入 provider + model_id）。
    ///
    /// 若 models 注册表有匹配条目则复用，否则内联创建路由条目。
    pub fn set_route_inline(&mut self, slot: RoutingSlot, provider: &str, model_id: &str) {
        if let Some(entry) = self
            .models
            .iter()
            .find(|(_, m)| m.provider == provider && m.model == model_id)
            .map(|(_, e)| e.clone())
        {
            self.routing.insert(slot, entry);
        } else {
            self.routing.insert(
                slot,
                ModelEntry {
                    provider: provider.to_string(),
                    model: model_id.to_string(),
                    capabilities: vec![],
                    options: default_options(),
                    context_window: None,
                },
            );
        }
    }
}

/// Provider 引用情况（供 remove_provider 检查）。
#[derive(Debug, Clone, Default)]
pub struct ProviderReferences {
    /// 引用该 provider 的模型注册项 key 列表
    pub models: Vec<String>,
    /// 引用该 provider 的路由槽位 key 列表
    pub routes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_routing_as_string_reference_when_model_exists() {
        let mut config = ModelsConfig::default();
        config.providers.insert(
            "test".to_string(),
            ProviderConfig {
                headers: Default::default(),
                base_url: "https://api.test.com".to_string(),
                api_key: "key".to_string(),
                timeout_ms: 60_000,
                protocol: ProviderProtocol::OpenAiChatCompletions,
            },
        );
        config.models.insert(
            "my-chat".to_string(),
            ModelEntry {
                provider: "test".to_string(),
                model: "gpt-4".to_string(),
                capabilities: vec![ModelCapability::Chat],
                options: serde_json::json!({}),
                context_window: None,
            },
        );
        config.routing.insert(
            RoutingSlot::Chat,
            ModelEntry {
                provider: "test".to_string(),
                model: "gpt-4".to_string(),
                capabilities: vec![ModelCapability::Chat],
                options: serde_json::json!({}),
                context_window: None,
            },
        );

        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains(r#""chat":"my-chat""#),
            "routing 值应为字符串引用，实际输出：{json}"
        );
    }

    #[test]
    fn register_provider_models_adds_and_fills_capabilities() {
        use crate::model::ProviderModelInfo;
        let info = |id: &str, window: Option<usize>| ProviderModelInfo {
            id: id.to_string(),
            display_name: None,
            context_window: window,
        };
        let mut config = ModelsConfig::default();
        config.upsert_model("gpt-5.5", "other", "gpt-5.5", vec![ModelCapability::Chat]);
        config.upsert_model("old", "ChatGPT", "gpt-5.6-sol", vec![ModelCapability::Chat]);
        config
            .routing
            .insert(RoutingSlot::Chat, config.models["old"].clone());
        let caps = [ModelCapability::Chat, ModelCapability::Multimodal];
        let models = vec![info("gpt-5.6-sol", Some(872_000)), info("gpt-5.5", None)];

        assert!(config.register_provider_models("ChatGPT", &models, &caps));
        // 已有同供应商模型补齐多模态能力并同步窗口，不重复注册
        assert_eq!(config.models["old"].capabilities, caps.to_vec());
        assert_eq!(config.models["old"].context_window, Some(872_000));
        assert!(!config.models.contains_key("gpt-5.6-sol"));
        // 路由副本同步窗口
        assert_eq!(
            config.routing[&RoutingSlot::Chat].context_window,
            Some(872_000)
        );
        // key 冲突时加供应商前缀；服务端未给窗口时保持空；其他供应商不受影响
        assert_eq!(config.models["ChatGPT-gpt-5.5"].capabilities, caps.to_vec());
        assert_eq!(config.models["ChatGPT-gpt-5.5"].context_window, None);
        assert_eq!(
            config.models["gpt-5.5"].capabilities,
            vec![ModelCapability::Chat]
        );
        // 再次同步无变化
        assert!(!config.register_provider_models("ChatGPT", &models, &caps));
        // 服务端窗口变化时随之更新
        let models = vec![info("gpt-5.6-sol", Some(1_000_000))];
        assert!(config.register_provider_models("ChatGPT", &models, &caps));
        assert_eq!(config.models["old"].context_window, Some(1_000_000));
    }

    #[test]
    fn serialize_routing_as_inline_object_when_no_matching_model() {
        let mut config = ModelsConfig::default();
        config.providers.insert(
            "test".to_string(),
            ProviderConfig {
                headers: Default::default(),
                base_url: "https://api.test.com".to_string(),
                api_key: "key".to_string(),
                timeout_ms: 60_000,
                protocol: ProviderProtocol::OpenAiChatCompletions,
            },
        );
        config.routing.insert(
            RoutingSlot::Chat,
            ModelEntry {
                provider: "test".to_string(),
                model: "gpt-4".to_string(),
                capabilities: vec![ModelCapability::Chat],
                options: serde_json::json!({}),
                context_window: None,
            },
        );

        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains(r#""provider":"test""#) && json.contains(r#""model":"gpt-4""#),
            "无匹配模型时 routing 应内联对象，实际输出：{json}"
        );
    }

    #[test]
    fn deserialize_string_routing_compat() {
        let json = r#"{
            "providers": {
                "test": {
                    "base_url": "https://api.test.com",
                    "api_key": "key",
                    "timeout_ms": 60000,
                    "protocol": "open_ai_compatible"
                }
            },
            "models": {
                "my-chat": {
                    "provider": "test",
                    "model": "gpt-4",
                    "capabilities": ["chat"],
                    "options": {}
                }
            },
            "routing": {
                "chat": "my-chat"
            }
        }"#;

        let config: ModelsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.routing.get(&RoutingSlot::Chat).unwrap().model,
            "gpt-4"
        );
    }

    #[test]
    fn deserialize_object_routing_compat() {
        let json = r#"{
            "providers": {
                "test": {
                    "base_url": "https://api.test.com",
                    "api_key": "key",
                    "timeout_ms": 60000,
                    "protocol": "open_ai_compatible"
                }
            },
            "models": {},
            "routing": {
                "chat": {
                    "provider": "test",
                    "model": "gpt-4",
                    "capabilities": ["chat"],
                    "options": {}
                }
            }
        }"#;

        let config: ModelsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.routing.get(&RoutingSlot::Chat).unwrap().model,
            "gpt-4"
        );
    }

    #[test]
    fn roundtrip_serialize_deserialize_preserves_data() {
        let mut config = ModelsConfig::default();
        config.providers.insert(
            "test".to_string(),
            ProviderConfig {
                headers: Default::default(),
                base_url: "https://api.test.com".to_string(),
                api_key: "key".to_string(),
                timeout_ms: 60_000,
                protocol: ProviderProtocol::OpenAiChatCompletions,
            },
        );
        config.models.insert(
            "my-chat".to_string(),
            ModelEntry {
                provider: "test".to_string(),
                model: "gpt-4".to_string(),
                capabilities: vec![ModelCapability::Chat],
                options: serde_json::json!({"temperature": 0.7}),
                context_window: None,
            },
        );
        config.routing.insert(
            RoutingSlot::Chat,
            ModelEntry {
                provider: "test".to_string(),
                model: "gpt-4".to_string(),
                capabilities: vec![ModelCapability::Chat],
                options: serde_json::json!({"temperature": 0.7}),
                context_window: None,
            },
        );

        let json = serde_json::to_string_pretty(&config).unwrap();
        let restored: ModelsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.routing.get(&RoutingSlot::Chat).unwrap().model,
            "gpt-4"
        );
        assert_eq!(
            restored
                .routing
                .get(&RoutingSlot::Chat)
                .unwrap()
                .options
                .get("temperature")
                .unwrap(),
            &serde_json::json!(0.7)
        );
    }

    // ── CLI 友好方法测试（RFC 0015 §6.1） ──

    fn sample_provider() -> ProviderConfig {
        ProviderConfig {
            headers: Default::default(),
            base_url: "https://api.deepseek.com".to_string(),
            api_key: "${DEEPSEEK_API_KEY}".to_string(),
            timeout_ms: 60_000,
            protocol: ProviderProtocol::DeepSeek,
        }
    }

    #[test]
    fn upsert_and_reference_provider() {
        let mut config = ModelsConfig::default();
        config.upsert_provider(
            "deepseek",
            "https://api.deepseek.com",
            "${DEEPSEEK_API_KEY}",
            ProviderProtocol::DeepSeek,
            60_000,
        );
        assert!(config.providers.contains_key("deepseek"));

        // 添加引用该 provider 的模型
        config.upsert_model(
            "ds-chat",
            "deepseek",
            "deepseek-v4-pro",
            vec![ModelCapability::Chat],
        );
        let refs = config.provider_referenced_by("deepseek");
        assert_eq!(refs.models, vec!["ds-chat".to_string()]);
        assert!(refs.routes.is_empty());
    }

    #[test]
    fn remove_provider_force_cascades() {
        let mut config = ModelsConfig::default();
        config.providers.insert("p".to_string(), sample_provider());
        config.upsert_model("m1", "p", "model-1", vec![ModelCapability::Chat]);
        config.set_route_by_name(RoutingSlot::Chat, "m1").unwrap();

        let refs = config.provider_referenced_by("p");
        assert_eq!(refs.models.len(), 1);
        assert_eq!(refs.routes.len(), 1);

        let removed = config.remove_provider_force("p");
        assert!(removed >= 3); // provider + model + route
        assert!(!config.providers.contains_key("p"));
        assert!(config.models.is_empty());
        assert!(config.routing.is_empty());
    }

    #[test]
    fn set_route_by_name_requires_registered_model() {
        let mut config = ModelsConfig::default();
        let result = config.set_route_by_name(RoutingSlot::Chat, "nonexistent");
        assert!(result.is_err());

        config.upsert_model("ds", "p", "deepseek-v4-pro", vec![ModelCapability::Chat]);
        config.set_route_by_name(RoutingSlot::Chat, "ds").unwrap();
        assert_eq!(
            config.routing.get(&RoutingSlot::Chat).unwrap().model,
            "deepseek-v4-pro"
        );
    }

    #[test]
    fn set_route_by_name_rejects_capability_mismatch() {
        // P1 回归：route 设置必须校验模型 capability
        let mut config = ModelsConfig::default();
        // 只有多模态能力、不具备对话能力的模型不能设置到 chat / lite 路由
        config.upsert_model("vision-only", "p", "vl", vec![ModelCapability::Multimodal]);
        let err = config.set_route_by_name(RoutingSlot::Chat, "vision-only");
        assert!(err.unwrap_err().contains("chat"), "应报告缺少 chat 能力");
        assert!(
            config
                .set_route_by_name(RoutingSlot::Lite, "vision-only")
                .is_err()
        );
        // 无能力声明的模型同样拒绝
        config.upsert_model("bare", "p", "m", vec![]);
        assert!(config.set_route_by_name(RoutingSlot::Chat, "bare").is_err());
    }

    #[test]
    fn set_route_lite_accepts_chat_capability() {
        // Lite 槽位与 chat 一样要求对话能力
        let mut config = ModelsConfig::default();
        config.upsert_model(
            "lite",
            "p",
            "deepseek-v4-flash",
            vec![ModelCapability::Chat],
        );
        config.set_route_by_name(RoutingSlot::Lite, "lite").unwrap();
        assert_eq!(
            config.routing.get(&RoutingSlot::Lite).unwrap().model,
            "deepseek-v4-flash"
        );
    }

    #[test]
    fn multimodal_is_resolved_through_chat_route() {
        let mut config = ModelsConfig::default();
        config.providers.insert("p".to_string(), sample_provider());
        config.upsert_model("text", "p", "text-model", vec![ModelCapability::Chat]);
        config.upsert_model(
            "vision",
            "p",
            "vision-model",
            vec![ModelCapability::Chat, ModelCapability::Multimodal],
        );

        // chat 模型不支持图片：多模态不可用
        config.set_route_by_name(RoutingSlot::Chat, "text").unwrap();
        assert!(!config.has_capability(ModelCapability::Multimodal));
        assert!(
            config
                .resolve_for_capability(ModelCapability::Multimodal)
                .is_none()
        );
        assert_eq!(config.routed_model(ModelCapability::Multimodal), None);

        // chat 模型支持图片：多模态由 chat 路由承担
        config
            .set_route_by_name(RoutingSlot::Chat, "vision")
            .unwrap();
        assert!(config.has_capability(ModelCapability::Multimodal));
        assert_eq!(
            config
                .resolve_for_capability(ModelCapability::Multimodal)
                .unwrap()
                .model,
            "vision-model"
        );
        assert_eq!(
            config.routed_model(ModelCapability::Multimodal),
            Some("vision-model")
        );
        assert_eq!(
            config.available_capabilities(),
            vec![ModelCapability::Chat, ModelCapability::Multimodal]
        );
    }

    #[test]
    fn legacy_media_routes_and_capabilities_are_dropped_on_load() {
        // 旧版 models.json：含多模态/图片/视频/语音路由与能力。
        // 应能正常解析，已移除的内容被忽略，保存后从文件中清除。
        let json = r#"{
            "providers": {
                "p": { "base_url": "https://api.test.com", "api_key": "key" }
            },
            "models": {
                "gpt": {
                    "provider": "p",
                    "model": "gpt-4o",
                    "capabilities": ["chat", "multimodal", "tts", "chat"]
                },
                "vl": { "provider": "p", "model": "qwen-vl", "capabilities": ["multimodal"] },
                "flux": {
                    "provider": "p",
                    "model": "flux-1",
                    "capabilities": ["image_generation"],
                    "options": { "size": "1024x1024" }
                },
                "whisper": { "provider": "p", "model": "whisper-1", "capabilities": ["stt"] },
                "voice": {
                    "provider": "p",
                    "model": "tts-1",
                    "capabilities": ["tts"],
                    "options": { "voice": "alloy" }
                },
                "sora": { "provider": "p", "model": "sora", "capabilities": ["video_generation"] },
                "bare": { "provider": "p", "model": "bare-model" }
            },
            "routing": {
                "chat": "gpt",
                "lite": { "provider": "p", "model": "gpt-4o-mini", "capabilities": ["chat"] },
                "multimodal": "vl",
                "image_generation": "flux",
                "video_generation": "sora",
                "stt": "whisper",
                "tts": { "provider": "p", "model": "tts-1", "capabilities": ["tts"] }
            }
        }"#;

        let config: ModelsConfig = serde_json::from_str(json).expect("旧配置应可解析");

        // 路由只保留 chat / lite
        assert_eq!(config.routing.len(), 2);
        assert_eq!(config.routing[&RoutingSlot::Chat].model, "gpt-4o");
        assert_eq!(config.routing[&RoutingSlot::Lite].model, "gpt-4o-mini");
        // 混合能力模型只保留 chat / multimodal，且去重
        assert_eq!(
            config.models["gpt"].capabilities,
            vec![ModelCapability::Chat, ModelCapability::Multimodal]
        );
        assert_eq!(
            config.routing[&RoutingSlot::Chat].capabilities,
            vec![ModelCapability::Chat, ModelCapability::Multimodal]
        );
        // 仅多模态的模型保留（仍是合法能力）；仅媒体能力的模型整体丢弃
        assert_eq!(
            config.models["vl"].capabilities,
            vec![ModelCapability::Multimodal]
        );
        for removed in ["flux", "whisper", "voice", "sora"] {
            assert!(!config.models.contains_key(removed), "{removed} 应被丢弃");
        }
        // 未声明能力的模型不受影响
        assert!(config.models["bare"].capabilities.is_empty());
        // 多模态改由 chat 模型承担
        assert!(config.has_capability(ModelCapability::Multimodal));

        // 保存后不再写出已移除的路由与能力，且再次读取结果一致
        let saved = serde_json::to_string(&config).unwrap();
        for key in REMOVED_ROUTING_KEYS
            .iter()
            .filter(|key| **key != "multimodal")
        {
            assert!(
                !saved.contains(&format!("\"{key}\"")),
                "保存后不应再写出 {key}"
            );
        }
        let routing: serde_json::Value =
            serde_json::from_str::<serde_json::Value>(&saved).unwrap()["routing"].clone();
        assert!(
            routing.get("multimodal").is_none(),
            "保存后不应再写出 multimodal 路由"
        );
        let reloaded: ModelsConfig = serde_json::from_str(&saved).unwrap();
        assert_eq!(reloaded, config);
    }

    #[test]
    fn legacy_route_referencing_dropped_model_does_not_break_chat() {
        // chat 路由引用了一个仅有媒体能力（被丢弃）的模型：不报错，按旧逻辑回退为占位条目。
        let json = r#"{
            "providers": { "p": { "base_url": "https://api.test.com", "api_key": "key" } },
            "models": {
                "voice": { "provider": "p", "model": "tts-1", "capabilities": ["tts"] }
            },
            "routing": { "chat": "voice" }
        }"#;
        let config: ModelsConfig = serde_json::from_str(json).expect("旧配置应可解析");
        assert!(config.models.is_empty());
        assert_eq!(config.routing[&RoutingSlot::Chat].model, "voice");
        assert!(config.routing[&RoutingSlot::Chat].provider.is_empty());
        assert!(config.resolve_slot(RoutingSlot::Chat).is_none());
    }

    #[test]
    fn malformed_model_entry_still_fails_to_parse() {
        // 兼容处理只针对已移除能力，不吞掉真正损坏的条目
        let json = r#"{ "models": { "broken": { "capabilities": ["chat"] } } }"#;
        assert!(serde_json::from_str::<ModelsConfig>(json).is_err());
    }

    #[test]
    fn legacy_embedding_rerank_entries_are_ignored_on_load() {
        // embedding/rerank 已迁出到 Memory 独立配置：旧 models.json 仍应可解析，
        // 旧能力与旧路由被忽略，其余配置保持不变。
        let json = r#"{
            "providers": {
                "p": { "base_url": "https://api.test.com", "api_key": "key" }
            },
            "models": {
                "chat": { "provider": "p", "model": "gpt", "capabilities": ["chat"] },
                "bge": {
                    "provider": "p",
                    "model": "bge-m3",
                    "capabilities": ["embedding"],
                    "options": { "dimension": 1024 }
                },
                "mixed": { "provider": "p", "model": "m", "capabilities": ["chat", "rerank"] }
            },
            "routing": { "chat": "chat", "embedding": "bge", "rerank": "mixed" }
        }"#;

        let config: ModelsConfig = serde_json::from_str(json).expect("旧配置应可解析");
        assert_eq!(config.routing.len(), 1, "仅保留 chat 路由");
        assert!(config.routing.contains_key(&RoutingSlot::Chat));
        assert!(config.models["bge"].capabilities.is_empty());
        assert_eq!(
            config.models["mixed"].capabilities,
            vec![ModelCapability::Chat]
        );

        let saved = serde_json::to_string(&config).unwrap();
        assert!(!saved.contains("\"embedding\""), "保存后不再写出旧路由");
    }

    #[test]
    fn set_route_inline_reuses_registered_entry() {
        let mut config = ModelsConfig::default();
        config.providers.insert("p".to_string(), sample_provider());
        config.upsert_model(
            "ds",
            "p",
            "deepseek-v4-pro",
            vec![ModelCapability::Chat, ModelCapability::Multimodal],
        );
        config.set_route_inline(RoutingSlot::Chat, "p", "deepseek-v4-pro");
        // 复用注册项时应携带 capabilities
        assert_eq!(
            config
                .routing
                .get(&RoutingSlot::Chat)
                .unwrap()
                .capabilities
                .len(),
            2
        );
    }

    #[test]
    fn set_route_inline_creates_bare_entry_when_no_match() {
        let mut config = ModelsConfig::default();
        config.providers.insert("p".to_string(), sample_provider());
        config.set_route_inline(RoutingSlot::Lite, "p", "deepseek-v4-flash");
        let entry = config.routing.get(&RoutingSlot::Lite).unwrap();
        assert_eq!(entry.model, "deepseek-v4-flash");
        assert!(entry.capabilities.is_empty());
    }

    #[test]
    fn remove_model_reports_dangling_routes() {
        let mut config = ModelsConfig::default();
        config.providers.insert("p".to_string(), sample_provider());
        config.upsert_model("ds", "p", "deepseek-v4-pro", vec![ModelCapability::Chat]);
        config.set_route_by_name(RoutingSlot::Chat, "ds").unwrap();

        let (removed, dangling) = config.remove_model("ds");
        assert!(removed);
        assert_eq!(dangling, vec!["chat".to_string()]);
    }

    #[test]
    fn remove_model_not_found() {
        let mut config = ModelsConfig::default();
        let (removed, _) = config.remove_model("nope");
        assert!(!removed);
    }
}
