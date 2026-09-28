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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    Chat,
    Multimodal,
    ImageGeneration,
    VideoGeneration,
    Stt,
    Tts,
}

/// 已迁出全局模型配置的旧能力 / 路由键。
///
/// Embedding 与 Rerank 归 Memory 插件独立管理（`~/.tiangong/memory/config.json`）。
/// 旧版 models.json 中残留的这些键在读取时静默忽略，保证旧文件仍可解析。
pub const RETIRED_MODEL_KEYS: &[&str] = &["embedding", "rerank"];

/// 路由槽位枚举 — 描述哪个模型负责什么任务
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingSlot {
    Chat,
    Lite,
    Multimodal,
    ImageGeneration,
    VideoGeneration,
    Stt,
    Tts,
}

impl RoutingSlot {
    pub fn key(&self) -> &'static str {
        match self {
            RoutingSlot::Chat => "chat",
            RoutingSlot::Lite => "lite",
            RoutingSlot::Multimodal => "multimodal",
            RoutingSlot::ImageGeneration => "image_generation",
            RoutingSlot::VideoGeneration => "video_generation",
            RoutingSlot::Stt => "stt",
            RoutingSlot::Tts => "tts",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "chat" => Some(RoutingSlot::Chat),
            "lite" => Some(RoutingSlot::Lite),
            "multimodal" => Some(RoutingSlot::Multimodal),
            "image_generation" => Some(RoutingSlot::ImageGeneration),
            "video_generation" => Some(RoutingSlot::VideoGeneration),
            "stt" => Some(RoutingSlot::Stt),
            "tts" => Some(RoutingSlot::Tts),
            _ => None,
        }
    }

    pub fn all() -> &'static [RoutingSlot] {
        &[
            RoutingSlot::Chat,
            RoutingSlot::Lite,
            RoutingSlot::Multimodal,
            RoutingSlot::ImageGeneration,
            RoutingSlot::VideoGeneration,
            RoutingSlot::Stt,
            RoutingSlot::Tts,
        ]
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            RoutingSlot::Chat => "对话",
            RoutingSlot::Lite => "轻量文本",
            RoutingSlot::Multimodal => "多模态",
            RoutingSlot::ImageGeneration => "图片生成",
            RoutingSlot::VideoGeneration => "视频生成",
            RoutingSlot::Stt => "语音识别",
            RoutingSlot::Tts => "语音合成",
        }
    }

    /// 路由槽位对应的模型能力
    pub fn capability(&self) -> Option<ModelCapability> {
        match self {
            RoutingSlot::Chat => Some(ModelCapability::Chat),
            RoutingSlot::Lite => None, // Lite 是路由槽位，不对应模型能力
            RoutingSlot::Multimodal => Some(ModelCapability::Multimodal),
            RoutingSlot::ImageGeneration => Some(ModelCapability::ImageGeneration),
            RoutingSlot::VideoGeneration => Some(ModelCapability::VideoGeneration),
            RoutingSlot::Stt => Some(ModelCapability::Stt),
            RoutingSlot::Tts => Some(ModelCapability::Tts),
        }
    }

    /// 从模型能力获取对应的路由槽位
    pub fn from_capability(cap: ModelCapability) -> Self {
        match cap {
            ModelCapability::Chat => RoutingSlot::Chat,
            ModelCapability::Multimodal => RoutingSlot::Multimodal,
            ModelCapability::ImageGeneration => RoutingSlot::ImageGeneration,
            ModelCapability::VideoGeneration => RoutingSlot::VideoGeneration,
            ModelCapability::Stt => RoutingSlot::Stt,
            ModelCapability::Tts => RoutingSlot::Tts,
        }
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

    /// 从配置键解析能力
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "chat" => Some(ModelCapability::Chat),
            "multimodal" => Some(ModelCapability::Multimodal),
            "image_generation" => Some(ModelCapability::ImageGeneration),
            "video_generation" => Some(ModelCapability::VideoGeneration),
            "stt" => Some(ModelCapability::Stt),
            "tts" => Some(ModelCapability::Tts),
            _ => None,
        }
    }

    /// 返回所有能力的列表
    pub fn all() -> &'static [ModelCapability] {
        &[
            ModelCapability::Chat,
            ModelCapability::Multimodal,
            ModelCapability::ImageGeneration,
            ModelCapability::VideoGeneration,
            ModelCapability::Stt,
            ModelCapability::Tts,
        ]
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

    /// 返回用于意图分类的标签（大写英文）
    pub fn intent_label(&self) -> &'static str {
        match self {
            ModelCapability::Chat => "SIMPLE",
            ModelCapability::Multimodal => "MULTIMODAL",
            ModelCapability::ImageGeneration => "IMAGE",
            ModelCapability::VideoGeneration => "VIDEO",
            ModelCapability::Stt => "STT",
            ModelCapability::Tts => "TTS",
        }
    }

    /// 返回意图分类的描述
    pub fn intent_description(&self) -> &'static str {
        match self {
            ModelCapability::Chat => {
                "简单对话（问候、闲聊、知识问答、翻译、解释概念等不需要执行工具或命令的请求）"
            }
            ModelCapability::Multimodal => "多模态请求（需要理解图片、音频等多种输入形式）",
            ModelCapability::ImageGeneration => "图片生成请求（用户要求生成、绘制、创作图片）",
            ModelCapability::VideoGeneration => "视频生成请求（用户要求生成、制作视频）",
            ModelCapability::Stt => "语音识别请求（用户要求将语音/音频转为文字）",
            ModelCapability::Tts => "语音合成请求（用户要求将文字转为语音/朗读）",
        }
    }

    /// 除 Chat 外的多媒体能力列表（用于意图分类的动态加载）
    pub fn media_capabilities() -> &'static [ModelCapability] {
        &[
            ModelCapability::ImageGeneration,
            ModelCapability::VideoGeneration,
            ModelCapability::Stt,
            ModelCapability::Tts,
            ModelCapability::Multimodal,
        ]
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

/// 宽松解析能力列表：跳过已迁出（embedding/rerank）或未知的能力键，
/// 保证旧版 models.json 仍可读取。
fn deserialize_capabilities_lenient<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<ModelCapability>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<String>::deserialize(deserializer)?;
    Ok(raw
        .iter()
        .filter_map(|key| {
            let capability = ModelCapability::from_key(key);
            if capability.is_none() && !RETIRED_MODEL_KEYS.contains(&key.as_str()) {
                tracing::warn!("忽略未知模型能力：{key}");
            }
            capability
        })
        .collect())
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
            models: HashMap<String, ModelEntry>,
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

        let routing: HashMap<RoutingSlot, ModelEntry> = raw
            .routing
            .into_iter()
            .filter_map(|(slot_key, val)| {
                let Some(slot) = RoutingSlot::from_key(&slot_key) else {
                    // embedding/rerank 已迁出到 Memory 独立配置，静默忽略；
                    // 其它未知槽位记录告警后忽略，不阻断整个文件解析。
                    if !RETIRED_MODEL_KEYS.contains(&slot_key.as_str()) {
                        tracing::warn!("忽略未知路由槽位：{slot_key}");
                    }
                    return None;
                };
                let entry = match val {
                    RawRoutingValue::Key(key) => {
                        raw.models.get(&key).cloned().unwrap_or_else(|| {
                            tracing::warn!("路由引用了不存在的模型：{key}");
                            ModelEntry {
                                provider: String::new(),
                                model: key.clone(),
                                capabilities: vec![],
                                options: default_options(),
                                context_window: None,
                            }
                        })
                    }
                    RawRoutingValue::Entry(entry) => entry,
                };
                Some((slot, entry))
            })
            .collect();

        Ok(ModelsConfig {
            providers: raw.providers,
            models: raw.models,
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
    /// 对于 Multimodal，除了检查独立路由外，也检查 chat 模型是否自带此能力
    pub fn has_capability(&self, capability: ModelCapability) -> bool {
        let slot = RoutingSlot::from_capability(capability);
        if self.routing.contains_key(&slot) {
            return true;
        }
        // chat 模型自带 multimodal 能力时，视为多模态可用
        if capability == ModelCapability::Multimodal && self.chat_is_multimodal() {
            return true;
        }
        false
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

    /// 获取指定路由槽位的模型名称
    pub fn routed_model(&self, capability: ModelCapability) -> Option<&str> {
        let slot = RoutingSlot::from_capability(capability);
        self.routing.get(&slot).map(|e| e.model.as_str())
    }

    /// 获取指定路由槽位的完整配置（Provider + Model 合并）
    pub fn resolve_for_capability(&self, capability: ModelCapability) -> Option<ResolvedModel> {
        let slot = RoutingSlot::from_capability(capability);
        self.resolve_slot(slot)
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
    /// 同时校验模型具备该槽位所需能力：
    /// - 有明确能力映射的槽位（chat/multimodal/embedding 等）要求模型声明该能力。
    /// - Lite 槽位无对应能力枚举，宽松接受 chat 能力（轻量文本任务用 chat 模型降级）。
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
        match slot.capability() {
            Some(expected) if !entry.capabilities.contains(&expected) => {
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
            None if slot == RoutingSlot::Lite
                && !entry.capabilities.contains(&ModelCapability::Chat) =>
            {
                // Lite 槽位要求 chat 能力（轻量文本任务降级）
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
                    "模型 {name} 不具备 chat 能力（当前能力：{current}），不能设置到 lite 路由"
                ));
            }
            _ => {}
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
        // chat 模型不应能设置到 tts 路由
        config.upsert_model("chat-only", "p", "gpt", vec![ModelCapability::Chat]);
        let err = config.set_route_by_name(RoutingSlot::Tts, "chat-only");
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("tts"), "应报告缺少 tts 能力");
        // tts 模型不应能设置到 chat 路由
        config.upsert_model("tts-only", "p", "tts-1", vec![ModelCapability::Tts]);
        let err = config.set_route_by_name(RoutingSlot::Chat, "tts-only");
        assert!(err.is_err());
    }

    #[test]
    fn set_route_lite_accepts_chat_capability() {
        // Lite 槽位无对应能力枚举，接受 chat 能力（轻量文本降级）
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
    fn set_route_lite_rejects_non_chat() {
        // Lite 槽位要求 chat 能力，tts 模型不能设为 lite
        let mut config = ModelsConfig::default();
        config.upsert_model("speech", "p", "tts-1", vec![ModelCapability::Tts]);
        let err = config.set_route_by_name(RoutingSlot::Lite, "speech");
        assert!(err.is_err());
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
