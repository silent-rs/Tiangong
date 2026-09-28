//! Memory 系统独立配置（v2）。
//!
//! 该模块只依赖 `tiangong-memory` 和 `tiangong-llm`，让 Memory 的模型、
//! Embedding、Rerank 配置脱离全局模型路由：
//!
//! - **LLM**：推荐从 `models.json` 快速选择（`models_ref`），只保存 key，运行时解析；
//!   未指定 key 时跟随 `lite` 路由，`lite` 未配置时回落 `chat`。也可填写独立端点（`remote`）。
//! - **Embedding / Rerank**：只归 Memory 管理，来源为 `builtin`（按 `local_tier`
//!   本地运行，阶段 2 接入）或 `remote`（独立在线端点），不再引用 `models.json`。
//!
//! 旧版（v1）配置保存的是从 `models.json` 复制出的完整端点，加载时自动升级；
//! 宿主从 `models.json` 迁出的 embedding / rerank（`memory/legacy-models.json`）
//! 也在加载时并入并归档。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tiangong_config::io::{LEGACY_MEMORY_MODELS_FILE, LegacyMemoryEndpoint, LegacyMemoryModels};
use tiangong_llm::models_config::{ModelEntry, ModelsConfig, ResolvedModel, RoutingSlot};
use tiangong_llm::{
    EmbeddingEndpointConfig, LlmEndpointConfig, ProviderProtocol, RerankEndpointConfig,
};

use crate::{MemoryOptions, MemoryVectorMode};

const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// 当前配置版本。
pub const MEMORY_CONFIG_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// 配置结构
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryConfig {
    #[serde(default = "current_version")]
    pub version: u32,
    /// 本地内置模型档位，仅影响 `builtin` 来源的 Embedding / Rerank。
    #[serde(default)]
    pub local_tier: MemoryLocalTier,
    /// Memory LLM；缺省等价于 `models_ref` 且跟随 lite → chat。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<MemoryLlmSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<MemoryEmbeddingSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank: Option<MemoryRerankSource>,
    #[serde(default)]
    pub vector_mode: MemoryVectorMode,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            version: MEMORY_CONFIG_VERSION,
            local_tier: MemoryLocalTier::default(),
            model: None,
            embedding: None,
            rerank: None,
            vector_mode: MemoryVectorMode::Auto,
        }
    }
}

fn current_version() -> u32 {
    MEMORY_CONFIG_VERSION
}

/// 本地内置模型档位（低/中/高），同时决定 Embedding 与 Rerank 的本地模型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLocalTier {
    Low,
    #[default]
    Mid,
    High,
}

impl MemoryLocalTier {
    pub fn key(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Mid => "mid",
            Self::High => "high",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "mid" | "medium" => Some(Self::Mid),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

/// Memory LLM 来源。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum MemoryLlmSource {
    /// 引用 models.json 中的模型 key 或路由槽位；`key` 为空时跟随 lite → chat。
    ModelsRef {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// 独立在线端点（兼容旧配置中无法反查 key 的端点）。
    Remote(MemoryRemoteEndpoint),
}

/// Embedding 来源（不再引用 models.json）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum MemoryEmbeddingSource {
    /// 本地内置模型，按 `local_tier` 选择。
    Builtin,
    Remote {
        #[serde(flatten)]
        endpoint: MemoryRemoteEndpoint,
        dimension: usize,
    },
}

/// Rerank 来源（不再引用 models.json）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum MemoryRerankSource {
    Builtin,
    Remote(MemoryRemoteEndpoint),
}

/// 在线端点配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryRemoteEndpoint {
    /// 来源标注（如迁移前的 provider 名），仅用于展示。
    #[serde(default, alias = "provider", skip_serializing_if = "Option::is_none")]
    pub provider_key: Option<String>,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub model: String,
    #[serde(default)]
    pub protocol: ProviderProtocol,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for MemoryRemoteEndpoint {
    fn default() -> Self {
        Self {
            provider_key: None,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            // Memory 相关端点通常为第三方 OpenAI 兼容服务，默认走 Chat Completions。
            protocol: ProviderProtocol::OpenAiChatCompletions,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

impl MemoryRemoteEndpoint {
    fn is_complete(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.model.trim().is_empty()
    }

    fn resolved_api_key(&self) -> String {
        resolve_api_key(&self.api_key)
    }

    fn from_legacy(endpoint: &LegacyMemoryEndpoint) -> Self {
        Self {
            provider_key: Some(endpoint.provider.clone()).filter(|value| !value.is_empty()),
            base_url: endpoint.base_url.clone(),
            api_key: endpoint.api_key.clone(),
            model: endpoint.model.clone(),
            protocol: endpoint.protocol,
            timeout_ms: endpoint.timeout_ms,
        }
    }
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

// ---------------------------------------------------------------------------
// 路径与开关
// ---------------------------------------------------------------------------

pub fn default_memory_config_path() -> PathBuf {
    crate::paths::memory_data_dir().join("config.json")
}

/// 宿主从 models.json 迁出的旧 embedding / rerank 交接文件。
pub fn legacy_models_handoff_path() -> PathBuf {
    crate::paths::memory_data_dir().join(LEGACY_MEMORY_MODELS_FILE)
}

/// Memory 禁用标记文件路径：~/.tiangong/memory/.disabled
///
/// 用于 `tiangong memory enable/disable` 实现对称开关（RFC 0015 §6.3）。
/// MemoryConfig 无顶层 enabled 字段，改用此标记文件存在性表示禁用，
/// 不破坏 MemoryConfig 结构、不丢失端点配置。
pub fn memory_disabled_marker_path() -> PathBuf {
    crate::paths::memory_data_dir().join(".disabled")
}

/// 判断 Memory 是否被显式禁用（标记文件存在即禁用）。
pub fn is_memory_disabled() -> bool {
    memory_disabled_marker_path().exists()
}

/// 禁用 Memory（创建标记文件）。
pub fn disable_memory() -> Result<()> {
    disable_memory_at(&memory_disabled_marker_path())
}

/// 在指定路径创建禁用标记（供测试使用）。
pub fn disable_memory_at(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败：{}", parent.display()))?;
    }
    fs::write(path, "disabled by `tiangong memory disable`\n")
        .with_context(|| format!("写入禁用标记失败：{}", path.display()))
}

/// 启用 Memory（删除标记文件）。文件不存在视为已启用。
pub fn enable_memory() -> Result<()> {
    enable_memory_at(&memory_disabled_marker_path())
}

/// 在指定路径删除禁用标记（供测试使用）。
pub fn enable_memory_at(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(path).with_context(|| format!("删除禁用标记失败：{}", path.display()))
}

fn resolve_api_key(value: &str) -> String {
    ModelsConfig::resolve_api_key(value)
}

// ---------------------------------------------------------------------------
// 加载 / 升级 / 保存
// ---------------------------------------------------------------------------

impl MemoryConfig {
    /// 从默认位置加载并完成迁移：v1 升级、并入宿主交接文件，发生变化时落盘。
    pub fn load() -> Result<Self> {
        let storage_root = crate::paths::storage_root();
        let models = tiangong_config::io::load_models_config_at(&storage_root);
        Self::load_and_migrate(
            &default_memory_config_path(),
            &legacy_models_handoff_path(),
            &models,
        )
    }

    pub fn load_or_default() -> Self {
        match Self::load() {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!("读取 Memory 独立配置失败，使用默认配置: {err}");
                Self::default()
            }
        }
    }

    /// 仅读取并升级（不并入交接文件、不写盘）。
    pub fn load_from_path(path: &Path) -> Result<Self> {
        Self::read_upgraded(path, &ModelsConfig::default()).map(|(config, _)| config)
    }

    /// 读取配置，完成 v1 升级与交接文件并入；有变化时先备份旧文件再写回，
    /// 写回成功后把交接文件归档为 `*.migrated`。
    pub fn load_and_migrate(
        config_path: &Path,
        handoff_path: &Path,
        models: &ModelsConfig,
    ) -> Result<Self> {
        let (mut config, upgraded) = Self::read_upgraded(config_path, models)?;
        let handoff = match tiangong_config::io::load_legacy_memory_models(handoff_path) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!("Memory 旧模型交接文件不可读，暂不并入：{error}");
                None
            }
        };
        let outcome = handoff
            .as_ref()
            .map(|legacy| config.merge_legacy_models(legacy))
            .unwrap_or_default();
        let merged = outcome.changed;

        if upgraded || merged {
            if upgraded && config_path.exists() {
                let backup = config_path.with_extension("json.v1.bak");
                if !backup.exists() {
                    fs::copy(config_path, &backup)
                        .with_context(|| format!("备份旧 Memory 配置失败：{}", backup.display()))?;
                }
            }
            config.save_to_path(config_path)?;
            tracing::info!(
                upgraded,
                merged,
                "Memory 配置已迁移到 v{MEMORY_CONFIG_VERSION}"
            );
        }
        // 仍有未并入的旧配置时保留交接文件，等用户补全后再消费。
        if handoff.is_some() && !outcome.pending {
            archive_handoff(handoff_path);
        }
        Ok(config)
    }

    /// 读取磁盘配置；返回 (配置, 是否由旧格式升级)。
    fn read_upgraded(path: &Path, models: &ModelsConfig) -> Result<(Self, bool)> {
        if !path.exists() {
            return Ok((Self::default(), false));
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("读取 Memory 配置失败：{}", path.display()))?;
        let raw: Value = serde_json::from_str(&content)
            .with_context(|| format!("解析 Memory 配置失败：{}", path.display()))?;
        let version = raw.get("version").and_then(Value::as_u64).unwrap_or(1);
        if version >= u64::from(MEMORY_CONFIG_VERSION) {
            let config = serde_json::from_value(raw)
                .with_context(|| format!("解析 Memory 配置失败：{}", path.display()))?;
            return Ok((config, false));
        }
        let legacy: LegacyMemoryConfigV1 = serde_json::from_value(raw)
            .with_context(|| format!("解析旧版 Memory 配置失败：{}", path.display()))?;
        Ok((legacy.upgrade(models), true))
    }

    /// 并入宿主迁出的旧 embedding / rerank；仅在对应项未配置时生效。
    /// 返回配置是否发生变化。
    pub fn merge_legacy_models(&mut self, legacy: &LegacyMemoryModels) -> LegacyMergeOutcome {
        let mut outcome = LegacyMergeOutcome::default();
        if self.embedding.is_none()
            && let Some(endpoint) = legacy.embedding.as_ref()
        {
            match endpoint.dimension {
                Some(dimension) => {
                    self.embedding = Some(MemoryEmbeddingSource::Remote {
                        endpoint: MemoryRemoteEndpoint::from_legacy(endpoint),
                        dimension,
                    });
                    outcome.changed = true;
                }
                None => {
                    // 缺维度无法构造运行配置；标记未消费，交接文件保持原地，
                    // 用户在 Memory 页面补全维度后下次启动仍可并入。
                    outcome.pending = true;
                    tracing::warn!(
                        model = %endpoint.model,
                        "旧 embedding 路由缺少 options.dimension，未自动并入，请在 Memory 页面重新配置"
                    );
                }
            }
        }
        if self.rerank.is_none()
            && let Some(endpoint) = legacy.rerank.as_ref()
        {
            self.rerank = Some(MemoryRerankSource::Remote(
                MemoryRemoteEndpoint::from_legacy(endpoint),
            ));
            outcome.changed = true;
        }
        outcome
    }

    pub fn save(&self) -> Result<()> {
        self.save_to_path(&default_memory_config_path())
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建 Memory 配置目录失败：{}", parent.display()))?;
        }
        let mut config = self.clone();
        config.version = MEMORY_CONFIG_VERSION;
        let content = serde_json::to_string_pretty(&config).context("序列化 Memory 配置失败")?;
        // 临时文件名带唯一后缀：多个进程同时保存时不会互相截断对方的半成品。
        let tmp = path.with_extension(format!("json.tmp.{}", scru128::new_string()));
        fs::write(&tmp, content)
            .with_context(|| format!("写入 Memory 配置失败：{}", tmp.display()))?;
        if let Err(error) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(error).with_context(|| format!("替换 Memory 配置失败：{}", path.display()));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 运行参数解析
    // -----------------------------------------------------------------------

    /// 解析为 actor 运行参数（读取当前 models.json 解析 LLM 引用）。
    pub fn to_options(&self) -> MemoryOptions {
        let models = tiangong_config::io::load_models_config_at(&crate::paths::storage_root());
        self.to_options_with(&models)
    }

    /// 基于给定模型配置解析运行参数。
    pub fn to_options_with(&self, models: &ModelsConfig) -> MemoryOptions {
        let mut options = MemoryOptions::new();
        match self.resolve_llm(models) {
            Ok(Some(model)) => options = options.with_model(model),
            Ok(None) => tracing::debug!("Memory LLM 未配置（lite/chat 路由均缺失）"),
            Err(err) => tracing::warn!("Memory LLM 配置不可用: {err}"),
        }
        if let Some(embedding) = self.embedding_endpoint() {
            options = options.with_embedding(embedding);
        } else if matches!(self.embedding, Some(MemoryEmbeddingSource::Builtin)) {
            options = options.with_local_embedding(self.local_tier);
        }
        if let Some(rerank) = self.rerank_endpoint() {
            options = options.with_rerank(rerank);
        } else if matches!(self.rerank, Some(MemoryRerankSource::Builtin)) {
            options = options.with_local_rerank(self.local_tier);
        }
        options.with_vector_mode(self.vector_mode)
    }

    /// 解析 Memory LLM；`models_ref` 未指定 key 时跟随 lite → chat。
    pub fn resolve_llm(&self, models: &ModelsConfig) -> Result<Option<LlmEndpointConfig>> {
        match &self.model {
            Some(MemoryLlmSource::Remote(endpoint)) => Ok(endpoint
                .is_complete()
                .then(|| llm_endpoint_from_remote(endpoint))),
            Some(MemoryLlmSource::ModelsRef { key: Some(key) }) if !key.trim().is_empty() => {
                resolved_model_by_key(models, key.trim())
                    .map(|resolved| Some(llm_from_resolved(resolved)))
            }
            _ => Ok(default_llm_route(models).map(llm_from_resolved)),
        }
    }

    fn embedding_endpoint(&self) -> Option<EmbeddingEndpointConfig> {
        match self.embedding.as_ref()? {
            MemoryEmbeddingSource::Remote {
                endpoint,
                dimension,
            } if endpoint.is_complete() && *dimension > 0 => Some(EmbeddingEndpointConfig {
                base_url: endpoint.base_url.clone(),
                api_key: endpoint.resolved_api_key(),
                model: endpoint.model.clone(),
                protocol: endpoint.protocol,
                timeout: Duration::from_millis(endpoint.timeout_ms),
                dimension: *dimension,
            }),
            MemoryEmbeddingSource::Remote { .. } => {
                tracing::warn!("Memory Embedding 在线端点配置不完整（地址/模型/维度），跳过向量层");
                None
            }
            // 内置模型由 Actor 后台下载加载，不走在线端点。
            MemoryEmbeddingSource::Builtin => None,
        }
    }

    fn rerank_endpoint(&self) -> Option<RerankEndpointConfig> {
        match self.rerank.as_ref()? {
            MemoryRerankSource::Remote(endpoint) if endpoint.is_complete() => {
                Some(RerankEndpointConfig {
                    base_url: endpoint.base_url.clone(),
                    api_key: endpoint.resolved_api_key(),
                    model: endpoint.model.clone(),
                    protocol: endpoint.protocol,
                    timeout: Duration::from_millis(endpoint.timeout_ms),
                })
            }
            MemoryRerankSource::Remote(_) => {
                tracing::warn!("Memory Rerank 在线端点配置不完整（地址/模型），跳过模型精排");
                None
            }
            MemoryRerankSource::Builtin => None,
        }
    }

    /// 将已解析的运行参数转换为可通过 IPC 传递的配置（全部以在线端点表示，
    /// 对端 `to_options` 后得到等价运行参数）。
    pub fn from_options(options: &MemoryOptions) -> Self {
        let remote = |base_url: &str, api_key: &str, model: &str, protocol, timeout: Duration| {
            MemoryRemoteEndpoint {
                provider_key: None,
                base_url: base_url.to_string(),
                api_key: api_key.to_string(),
                model: model.to_string(),
                protocol,
                timeout_ms: duration_millis(timeout),
            }
        };
        Self {
            version: MEMORY_CONFIG_VERSION,
            local_tier: options
                .local_embedding
                .or(options.local_rerank)
                .unwrap_or_default(),
            model: options.model.as_ref().map(|model| {
                let mut endpoint = remote(
                    &model.base_url,
                    &model.api_key,
                    &model.model,
                    model.protocol,
                    model.timeout,
                );
                endpoint.provider_key = model.source_provider.clone();
                MemoryLlmSource::Remote(endpoint)
            }),
            embedding: match (&options.embedding, options.local_embedding) {
                (None, Some(_)) => Some(MemoryEmbeddingSource::Builtin),
                (embedding, _) => {
                    embedding
                        .as_ref()
                        .map(|embedding| MemoryEmbeddingSource::Remote {
                            endpoint: remote(
                                &embedding.base_url,
                                &embedding.api_key,
                                &embedding.model,
                                embedding.protocol,
                                embedding.timeout,
                            ),
                            dimension: embedding.dimension,
                        })
                }
            },
            rerank: match (&options.rerank, options.local_rerank) {
                (None, Some(_)) => Some(MemoryRerankSource::Builtin),
                (rerank, _) => rerank.as_ref().map(|rerank| {
                    MemoryRerankSource::Remote(remote(
                        &rerank.base_url,
                        &rerank.api_key,
                        &rerank.model,
                        rerank.protocol,
                        rerank.timeout,
                    ))
                }),
            },
            vector_mode: options.vector_mode,
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn llm_endpoint_from_remote(endpoint: &MemoryRemoteEndpoint) -> LlmEndpointConfig {
    LlmEndpointConfig {
        source_provider: endpoint.provider_key.clone(),
        base_url: endpoint.base_url.clone(),
        api_key: endpoint.resolved_api_key(),
        model: endpoint.model.clone(),
        protocol: endpoint.protocol,
        timeout: Duration::from_millis(endpoint.timeout_ms),
        max_retries: 3,
    }
}

fn llm_from_resolved(resolved: ResolvedModel) -> LlmEndpointConfig {
    LlmEndpointConfig {
        source_provider: Some(resolved.provider),
        base_url: resolved.base_url,
        api_key: resolved.api_key,
        model: resolved.model,
        protocol: resolved.protocol,
        timeout: Duration::from_millis(resolved.timeout_ms),
        max_retries: 3,
    }
}

/// 默认 LLM：优先 lite 路由，未配置时回落 chat。
fn default_llm_route(models: &ModelsConfig) -> Option<ResolvedModel> {
    models
        .resolve_slot(RoutingSlot::Lite)
        .or_else(|| models.resolve_slot(RoutingSlot::Chat))
}

/// 按 key 解析：路由槽位名（chat/lite…）→ models 注册表 key → routing 中的模型名。
fn resolved_model_by_key(models: &ModelsConfig, model_key: &str) -> Result<ResolvedModel> {
    if let Some(slot) = RoutingSlot::from_key(model_key) {
        return models
            .resolve_slot(slot)
            .ok_or_else(|| anyhow::anyhow!("路由 {model_key} 未配置"));
    }

    let resolve_entry = |entry: &ModelEntry| {
        let provider = models.providers.get(&entry.provider)?;
        Some(ResolvedModel {
            headers: provider.headers.clone(),
            provider: entry.provider.clone(),
            base_url: provider.base_url.clone(),
            api_key: ModelsConfig::resolve_api_key(&provider.api_key),
            timeout_ms: provider.timeout_ms,
            protocol: provider.protocol,
            context_window: entry.context_window,
            model: entry.model.clone(),
            options: entry.options.clone(),
        })
    };

    if let Some(entry) = models.models.get(model_key) {
        return resolve_entry(entry)
            .ok_or_else(|| anyhow::anyhow!("模型 {model_key} 引用的 Provider 不存在"));
    }

    models
        .routing
        .values()
        .find_map(|entry| {
            (entry.model == model_key)
                .then(|| resolve_entry(entry))
                .flatten()
        })
        .ok_or_else(|| anyhow::anyhow!("模型不存在：{model_key}"))
}

/// 按端点反查 models.json 中的模型 key（仅用于 v1 升级）。
fn find_model_key(
    models: &ModelsConfig,
    base_url: &str,
    model_name: &str,
    protocol: ProviderProtocol,
) -> Option<String> {
    let mut candidates = models
        .models
        .iter()
        .filter_map(|(key, entry)| {
            let provider = models.providers.get(&entry.provider)?;
            (provider.base_url == base_url
                && provider.protocol == protocol
                && entry.model == model_name)
                .then(|| key.clone())
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.into_iter().next()
}

/// 交接文件并入后归档，避免重复并入；归档失败仅告警（并入逻辑本身幂等）。
/// 交接文件并入结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LegacyMergeOutcome {
    /// 配置是否发生变化（需要写回磁盘）。
    pub changed: bool,
    /// 是否仍有旧配置无法并入（信息不足），交接文件需保留。
    pub pending: bool,
}

fn archive_handoff(path: &Path) {
    let archived = path.with_extension("json.migrated");
    if let Err(error) = fs::rename(path, &archived) {
        tracing::warn!("归档 Memory 旧模型交接文件失败：{error}");
    }
}

// ---------------------------------------------------------------------------
// v1 兼容
// ---------------------------------------------------------------------------

/// v1 配置：三类模型都保存从 models.json 复制出的完整端点。
#[derive(Debug, Clone, Default, Deserialize)]
struct LegacyMemoryConfigV1 {
    #[serde(default)]
    model: Option<LegacyEndpointV1>,
    #[serde(default)]
    embedding: Option<LegacyEndpointV1>,
    #[serde(default)]
    rerank: Option<LegacyEndpointV1>,
    #[serde(default)]
    vector_mode: MemoryVectorMode,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LegacyEndpointV1 {
    #[serde(default, alias = "provider")]
    provider_key: Option<String>,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    protocol: ProviderProtocol,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    dimension: usize,
}

impl LegacyEndpointV1 {
    fn is_blank(&self) -> bool {
        self.base_url.trim().is_empty() && self.model.trim().is_empty()
    }

    fn into_remote(self) -> MemoryRemoteEndpoint {
        MemoryRemoteEndpoint {
            provider_key: self.provider_key,
            base_url: self.base_url,
            api_key: self.api_key,
            model: self.model,
            protocol: self.protocol,
            timeout_ms: self.timeout_ms,
        }
    }
}

impl LegacyMemoryConfigV1 {
    fn upgrade(self, models: &ModelsConfig) -> MemoryConfig {
        let model =
            self.model.filter(|endpoint| !endpoint.is_blank()).map(
                |endpoint| match find_model_key(
                    models,
                    &endpoint.base_url,
                    &endpoint.model,
                    endpoint.protocol,
                ) {
                    Some(key) => MemoryLlmSource::ModelsRef { key: Some(key) },
                    None => MemoryLlmSource::Remote(endpoint.into_remote()),
                },
            );
        let embedding = self
            .embedding
            .filter(|endpoint| !endpoint.is_blank())
            .map(|endpoint| {
                let dimension = endpoint.dimension;
                MemoryEmbeddingSource::Remote {
                    endpoint: endpoint.into_remote(),
                    dimension,
                }
            });
        let rerank = self
            .rerank
            .filter(|endpoint| !endpoint.is_blank())
            .map(|endpoint| MemoryRerankSource::Remote(endpoint.into_remote()));
        MemoryConfig {
            version: MEMORY_CONFIG_VERSION,
            local_tier: MemoryLocalTier::default(),
            model,
            embedding,
            rerank,
            vector_mode: self.vector_mode,
        }
    }
}

// ---------------------------------------------------------------------------
// 页面 / CLI 视图（不回传密钥）
// ---------------------------------------------------------------------------

/// 页面与 CLI 使用的配置视图。
///
/// 页面只看到 `has_api_key`，不接触密钥明文；保存时 `api_key` 为空表示保留原值。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MemoryConfigSelection {
    #[serde(default)]
    pub local_tier: String,
    #[serde(default)]
    pub llm: MemoryLlmSelection,
    #[serde(default)]
    pub embedding: MemoryComponentSelection,
    #[serde(default)]
    pub rerank: MemoryComponentSelection,
    #[serde(default = "default_vector_mode_selection")]
    pub vector_mode: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MemoryLlmSelection {
    /// `models_ref` | `remote`
    #[serde(default = "default_llm_source")]
    pub source: String,
    /// `models_ref` 的 key；为空表示跟随 lite → chat。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<MemoryRemoteSelection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MemoryComponentSelection {
    /// `disabled` | `builtin` | `remote`
    #[serde(default = "default_component_source")]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<MemoryRemoteSelection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MemoryRemoteSelection {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub timeout_ms: u64,
    /// 仅 Embedding 使用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<usize>,
    /// 只写：非空时替换密钥，空值保留原密钥。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// 只读：当前是否已保存密钥。
    #[serde(default)]
    pub has_api_key: bool,
}

fn default_vector_mode_selection() -> String {
    "auto".to_string()
}

fn default_llm_source() -> String {
    "models_ref".to_string()
}

fn default_component_source() -> String {
    "disabled".to_string()
}

impl MemoryRemoteSelection {
    fn from_endpoint(endpoint: &MemoryRemoteEndpoint, dimension: Option<usize>) -> Self {
        Self {
            base_url: endpoint.base_url.clone(),
            model: endpoint.model.clone(),
            protocol: endpoint.protocol.as_str().to_string(),
            timeout_ms: endpoint.timeout_ms,
            dimension,
            api_key: None,
            has_api_key: !endpoint.api_key.trim().is_empty(),
        }
    }

    /// 探测用端点：不校验维度，api_key 留空时复用已保存密钥。
    pub fn endpoint_for_probe(
        &self,
        previous: Option<&MemoryRemoteEndpoint>,
    ) -> Result<MemoryRemoteEndpoint> {
        self.to_endpoint(previous, "探测")
    }

    /// 转为端点；`previous` 用于在 api_key 留空时保留原密钥。
    fn to_endpoint(
        &self,
        previous: Option<&MemoryRemoteEndpoint>,
        label: &str,
    ) -> Result<MemoryRemoteEndpoint> {
        let base_url = self.base_url.trim().to_string();
        let model = self.model.trim().to_string();
        if base_url.is_empty() || model.is_empty() {
            bail!("{label} 在线端点需要填写地址与模型");
        }
        let protocol = if self.protocol.trim().is_empty() {
            ProviderProtocol::OpenAiChatCompletions
        } else {
            self.protocol
                .parse()
                .with_context(|| format!("{label} 协议无效：{}", self.protocol))?
        };
        // 密钥留空表示"保留原值"，但只在端点身份未变时成立：地址或协议变了
        // 就是换了服务方，继续沿用旧凭据会把密钥发往新地址。此时要求重新填写。
        let same_endpoint = previous.is_some_and(|endpoint| {
            endpoint.base_url.trim().eq_ignore_ascii_case(&base_url)
                && endpoint.protocol == protocol
        });
        let api_key = match self.api_key.as_deref().map(str::trim) {
            Some(value) if !value.is_empty() => {
                // `${VAR}` 会在运行时读环境变量。本机配置页的使用者就是本机
                // 用户，允许自由引用；但远程配置（--host 非回环）的调用者只
                // 持有一个配置令牌，不应借此把任意环境变量发往任意端点，
                // 因此远程会话只接受已在本机配置里登记过的引用。
                if env_ref_policy() == EnvRefPolicy::RegisteredOnly
                    && let Some(name) = env_ref_name(value)
                    && !is_registered_env_ref(name, previous)
                {
                    bail!(
                        "{label} API Key 引用了未登记的环境变量 ${{{name}}}；\
                         请直接填写密钥，或先在 models.json 中登记该引用"
                    );
                }
                value.to_string()
            }
            _ if same_endpoint => previous
                .map(|endpoint| endpoint.api_key.clone())
                .unwrap_or_default(),
            _ if previous.is_some_and(|endpoint| !endpoint.api_key.trim().is_empty()) => {
                bail!("{label} 端点地址或协议已更改，请重新填写 API Key")
            }
            _ => String::new(),
        };
        Ok(MemoryRemoteEndpoint {
            // provider_key 只是来源标注，跨端点不再沿用。
            provider_key: previous
                .filter(|_| same_endpoint)
                .and_then(|endpoint| endpoint.provider_key.clone()),
            base_url,
            api_key,
            model,
            protocol,
            timeout_ms: if self.timeout_ms == 0 {
                DEFAULT_TIMEOUT_MS
            } else {
                self.timeout_ms
            },
        })
    }
}

/// 环境变量引用策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvRefPolicy {
    /// 允许任意 `${VAR}` 引用（本机配置页，调用者即本机用户）。
    Any,
    /// 只允许已在本机配置中登记过的引用（远程配置会话）。
    RegisteredOnly,
}

static ENV_REF_POLICY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 收紧环境变量引用策略；远程监听的配置服务在启动时调用。
pub fn restrict_env_refs() {
    ENV_REF_POLICY.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn env_ref_policy() -> EnvRefPolicy {
    if ENV_REF_POLICY.load(std::sync::atomic::Ordering::SeqCst) {
        EnvRefPolicy::RegisteredOnly
    } else {
        EnvRefPolicy::Any
    }
}

/// 取出 `${VAR}` 中的变量名（不是该形式时返回 None）。
fn env_ref_name(value: &str) -> Option<&str> {
    value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// 该环境变量引用是否已在本机配置中登记过（当前端点的旧值或 models.json 的 provider）。
fn is_registered_env_ref(name: &str, previous: Option<&MemoryRemoteEndpoint>) -> bool {
    let matches_ref = |raw: &str| env_ref_name(raw).is_some_and(|existing| existing == name);
    if previous.is_some_and(|endpoint| matches_ref(&endpoint.api_key)) {
        return true;
    }
    // models.json 中任一 provider 已使用该引用即视为已登记。
    let models = tiangong_config::io::load_models_config_at(&crate::paths::storage_root());
    models
        .providers
        .values()
        .any(|provider| matches_ref(&provider.api_key))
}

impl MemoryConfigSelection {
    pub fn from_memory(config: &MemoryConfig) -> Self {
        let llm = match &config.model {
            None => MemoryLlmSelection {
                source: default_llm_source(),
                key: None,
                remote: None,
            },
            Some(MemoryLlmSource::ModelsRef { key }) => MemoryLlmSelection {
                source: default_llm_source(),
                key: key.clone().filter(|key| !key.trim().is_empty()),
                remote: None,
            },
            Some(MemoryLlmSource::Remote(endpoint)) => MemoryLlmSelection {
                source: "remote".to_string(),
                key: None,
                remote: Some(MemoryRemoteSelection::from_endpoint(endpoint, None)),
            },
        };
        let embedding = match &config.embedding {
            None => MemoryComponentSelection {
                source: default_component_source(),
                remote: None,
            },
            Some(MemoryEmbeddingSource::Builtin) => MemoryComponentSelection {
                source: "builtin".to_string(),
                remote: None,
            },
            Some(MemoryEmbeddingSource::Remote {
                endpoint,
                dimension,
            }) => MemoryComponentSelection {
                source: "remote".to_string(),
                remote: Some(MemoryRemoteSelection::from_endpoint(
                    endpoint,
                    Some(*dimension),
                )),
            },
        };
        let rerank = match &config.rerank {
            None => MemoryComponentSelection {
                source: default_component_source(),
                remote: None,
            },
            Some(MemoryRerankSource::Builtin) => MemoryComponentSelection {
                source: "builtin".to_string(),
                remote: None,
            },
            Some(MemoryRerankSource::Remote(endpoint)) => MemoryComponentSelection {
                source: "remote".to_string(),
                remote: Some(MemoryRemoteSelection::from_endpoint(endpoint, None)),
            },
        };
        Self {
            local_tier: config.local_tier.key().to_string(),
            llm,
            embedding,
            rerank,
            vector_mode: vector_mode_key(config.vector_mode).to_string(),
        }
    }

    /// 转为新配置；`previous` 用于保留未重新填写的密钥。
    pub fn to_memory(&self, previous: &MemoryConfig) -> Result<MemoryConfig> {
        let local_tier = if self.local_tier.trim().is_empty() {
            previous.local_tier
        } else {
            MemoryLocalTier::parse(&self.local_tier)
                .ok_or_else(|| anyhow::anyhow!("本地档位无效：{}", self.local_tier))?
        };

        let previous_llm_remote = match &previous.model {
            Some(MemoryLlmSource::Remote(endpoint)) => Some(endpoint),
            _ => None,
        };
        let model = match self.llm.source.as_str() {
            "" | "models_ref" => Some(MemoryLlmSource::ModelsRef {
                key: self
                    .llm
                    .key
                    .as_deref()
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                    .map(str::to_string),
            }),
            "remote" => {
                let remote = self
                    .llm
                    .remote
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Memory LLM 在线端点缺少配置"))?;
                Some(MemoryLlmSource::Remote(
                    remote.to_endpoint(previous_llm_remote, "Memory LLM")?,
                ))
            }
            other => bail!("Memory LLM 来源无效：{other}"),
        };

        let previous_embedding_remote = match &previous.embedding {
            Some(MemoryEmbeddingSource::Remote { endpoint, .. }) => Some(endpoint),
            _ => None,
        };
        let embedding = match self.embedding.source.as_str() {
            "" | "disabled" => None,
            "builtin" => Some(MemoryEmbeddingSource::Builtin),
            "remote" => {
                let remote = self
                    .embedding
                    .remote
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Embedding 在线端点缺少配置"))?;
                let dimension = remote
                    .dimension
                    .filter(|value| *value > 0)
                    .ok_or_else(|| anyhow::anyhow!("Embedding 在线端点需要填写向量维度"))?;
                Some(MemoryEmbeddingSource::Remote {
                    endpoint: remote.to_endpoint(previous_embedding_remote, "Embedding")?,
                    dimension,
                })
            }
            other => bail!("Embedding 来源无效：{other}"),
        };

        let previous_rerank_remote = match &previous.rerank {
            Some(MemoryRerankSource::Remote(endpoint)) => Some(endpoint),
            _ => None,
        };
        let rerank = match self.rerank.source.as_str() {
            "" | "disabled" => None,
            "builtin" => Some(MemoryRerankSource::Builtin),
            "remote" => {
                let remote = self
                    .rerank
                    .remote
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Rerank 在线端点缺少配置"))?;
                Some(MemoryRerankSource::Remote(
                    remote.to_endpoint(previous_rerank_remote, "Rerank")?,
                ))
            }
            other => bail!("Rerank 来源无效：{other}"),
        };

        Ok(MemoryConfig {
            version: MEMORY_CONFIG_VERSION,
            local_tier,
            model,
            embedding,
            rerank,
            vector_mode: parse_vector_mode(&self.vector_mode),
        })
    }
}

fn vector_mode_key(mode: MemoryVectorMode) -> &'static str {
    match mode {
        MemoryVectorMode::Auto => "auto",
        MemoryVectorMode::Disabled => "disabled",
        MemoryVectorMode::EmbeddedLanceDb => "embedded_lance_db",
    }
}

fn parse_vector_mode(value: &str) -> MemoryVectorMode {
    match value.trim().to_ascii_lowercase().as_str() {
        "disabled" => MemoryVectorMode::Disabled,
        "embedded" | "lancedb" | "embedded_lancedb" | "embedded_lance_db" => {
            MemoryVectorMode::EmbeddedLanceDb
        }
        _ => MemoryVectorMode::Auto,
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_llm::models_config::{ModelCapability, ProviderConfig};

    fn models_with_routes(lite: bool, chat: bool) -> ModelsConfig {
        let mut models = ModelsConfig::default();
        models.providers.insert(
            "step".to_string(),
            ProviderConfig {
                headers: Default::default(),
                base_url: "https://api.stepfun.com/v1".to_string(),
                api_key: "sk-step".to_string(),
                timeout_ms: 30_000,
                protocol: ProviderProtocol::Anthropic,
            },
        );
        models.upsert_model("step-big", "step", "step-5", vec![ModelCapability::Chat]);
        models.upsert_model(
            "step-mini",
            "step",
            "step-mini",
            vec![ModelCapability::Chat],
        );
        if chat {
            models
                .set_route_by_name(RoutingSlot::Chat, "step-big")
                .unwrap();
        }
        if lite {
            models
                .set_route_by_name(RoutingSlot::Lite, "step-mini")
                .unwrap();
        }
        models
    }

    #[test]
    fn default_llm_follows_lite_then_chat() {
        let config = MemoryConfig::default();
        let both = models_with_routes(true, true);
        assert_eq!(
            config.resolve_llm(&both).unwrap().unwrap().model,
            "step-mini",
            "默认跟随 lite"
        );
        let chat_only = models_with_routes(false, true);
        assert_eq!(
            config.resolve_llm(&chat_only).unwrap().unwrap().model,
            "step-5",
            "lite 未配置时回落 chat"
        );
        assert!(
            config
                .resolve_llm(&ModelsConfig::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn explicit_llm_key_overrides_default_route() {
        let config = MemoryConfig {
            model: Some(MemoryLlmSource::ModelsRef {
                key: Some("step-big".to_string()),
            }),
            ..Default::default()
        };
        let models = models_with_routes(true, true);
        assert_eq!(
            config.resolve_llm(&models).unwrap().unwrap().model,
            "step-5"
        );
    }

    #[test]
    fn upgrades_v1_config_and_keeps_remote_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            r#"{
              "model": { "provider_key": "step", "base_url": "https://api.stepfun.com/v1",
                         "api_key": "sk-step", "model": "step-5", "protocol": "anthropic" },
              "embedding": { "provider_key": "LM studio", "base_url": "http://127.0.0.1:1234/v1",
                             "api_key": "lm", "model": "text-embedding-bge-m3",
                             "timeout_ms": 300000, "dimension": 1024 },
              "rerank": { "base_url": "http://127.0.0.1:1234/v1", "api_key": "lm",
                          "model": "bge-reranker-v2-m3" },
              "vector_mode": "auto"
            }"#,
        )
        .unwrap();
        let handoff = dir.path().join("legacy-models.json");
        let models = models_with_routes(true, true);

        let config = MemoryConfig::load_and_migrate(&path, &handoff, &models).unwrap();

        assert_eq!(config.version, MEMORY_CONFIG_VERSION);
        assert_eq!(
            config.model,
            Some(MemoryLlmSource::ModelsRef {
                key: Some("step-big".to_string())
            }),
            "能反查到 key 的 LLM 转为模型引用"
        );
        match config.embedding.as_ref().unwrap() {
            MemoryEmbeddingSource::Remote {
                endpoint,
                dimension,
            } => {
                assert_eq!(*dimension, 1024);
                assert_eq!(endpoint.model, "text-embedding-bge-m3");
                assert_eq!(endpoint.timeout_ms, 300_000);
            }
            other => panic!("应升级为在线 embedding：{other:?}"),
        }
        assert!(matches!(config.rerank, Some(MemoryRerankSource::Remote(_))));
        assert!(
            dir.path().join("config.json.v1.bak").exists(),
            "应保留 v1 备份"
        );

        let reread: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reread["version"], 2);
        assert_eq!(reread["embedding"]["source"], "remote");

        // 二次加载不再改写。
        let again = MemoryConfig::load_and_migrate(&path, &handoff, &models).unwrap();
        assert_eq!(again, config);
    }

    #[test]
    fn v1_llm_without_matching_key_stays_remote() {
        let legacy = LegacyMemoryConfigV1 {
            model: Some(LegacyEndpointV1 {
                base_url: "https://other.example/v1".into(),
                api_key: "sk".into(),
                model: "custom".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let config = legacy.upgrade(&models_with_routes(true, true));
        assert!(matches!(config.model, Some(MemoryLlmSource::Remote(_))));
    }

    #[test]
    fn merges_handoff_only_when_component_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let handoff = dir.path().join("legacy-models.json");
        let legacy = LegacyMemoryModels {
            embedding: Some(LegacyMemoryEndpoint {
                provider: "lm".into(),
                base_url: "http://127.0.0.1:1234/v1".into(),
                api_key: "${LM_KEY}".into(),
                protocol: ProviderProtocol::OpenAiChatCompletions,
                timeout_ms: 300_000,
                model: "text-embedding-bge-m3".into(),
                dimension: Some(1024),
            }),
            rerank: Some(LegacyMemoryEndpoint {
                provider: "lm".into(),
                base_url: "http://127.0.0.1:1234/v1".into(),
                api_key: String::new(),
                protocol: ProviderProtocol::OpenAiChatCompletions,
                timeout_ms: 60_000,
                model: "bge-reranker-v2-m3".into(),
                dimension: None,
            }),
            retired_models: Default::default(),
        };
        fs::write(&handoff, serde_json::to_string(&legacy).unwrap()).unwrap();
        // 已有 rerank 配置：不应被交接内容覆盖。
        MemoryConfig {
            rerank: Some(MemoryRerankSource::Builtin),
            ..Default::default()
        }
        .save_to_path(&path)
        .unwrap();

        let config =
            MemoryConfig::load_and_migrate(&path, &handoff, &ModelsConfig::default()).unwrap();

        match config.embedding.as_ref().unwrap() {
            MemoryEmbeddingSource::Remote {
                endpoint,
                dimension,
            } => {
                assert_eq!(*dimension, 1024);
                assert_eq!(endpoint.api_key, "${LM_KEY}", "密钥保持原始引用写法");
                assert_eq!(endpoint.provider_key.as_deref(), Some("lm"));
            }
            other => panic!("交接 embedding 应并入：{other:?}"),
        }
        assert_eq!(config.rerank, Some(MemoryRerankSource::Builtin));
        assert!(!handoff.exists(), "交接文件并入后归档");
        assert!(dir.path().join("legacy-models.json.migrated").exists());

        let reloaded = MemoryConfig::load_from_path(&path).unwrap();
        assert_eq!(reloaded, config, "并入结果已落盘");
    }

    #[test]
    fn handoff_embedding_without_dimension_is_not_merged() {
        let mut config = MemoryConfig::default();
        let changed = config.merge_legacy_models(&LegacyMemoryModels {
            embedding: Some(LegacyMemoryEndpoint {
                provider: "p".into(),
                base_url: "http://x/v1".into(),
                api_key: String::new(),
                protocol: ProviderProtocol::OpenAiChatCompletions,
                timeout_ms: 60_000,
                model: "e".into(),
                dimension: None,
            }),
            ..Default::default()
        });
        assert!(!changed.changed, "缺维度不构成配置变化");
        assert!(changed.pending, "缺维度应标记为待处理，交接文件不归档");
        assert!(config.embedding.is_none());
    }

    #[test]
    fn handoff_is_kept_when_legacy_embedding_cannot_be_merged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let handoff = dir.path().join("legacy-models.json");
        let legacy = LegacyMemoryModels {
            embedding: Some(LegacyMemoryEndpoint {
                provider: "p".into(),
                base_url: "http://x/v1".into(),
                api_key: String::new(),
                protocol: ProviderProtocol::OpenAiChatCompletions,
                timeout_ms: 60_000,
                model: "e".into(),
                dimension: None,
            }),
            ..Default::default()
        };
        std::fs::write(&handoff, serde_json::to_string(&legacy).unwrap()).unwrap();

        let config =
            MemoryConfig::load_and_migrate(&path, &handoff, &ModelsConfig::default()).unwrap();

        assert!(config.embedding.is_none(), "缺维度不应并入");
        assert!(handoff.exists(), "未消费的交接文件必须保留待人工补全");
        assert!(
            !dir.path().join("legacy-models.json.migrated").exists(),
            "未消费完不应归档"
        );
    }

    #[test]
    #[serial_test::serial]
    fn remote_session_rejects_unregistered_env_ref() {
        let selection = MemoryRemoteSelection {
            base_url: "https://api.example/v1".into(),
            model: "m".into(),
            api_key: Some("${UNREGISTERED_KEY}".into()),
            protocol: String::new(),
            timeout_ms: 0,
            dimension: None,
            has_api_key: false,
        };
        // 本机配置页：允许任意引用。
        assert!(selection.endpoint_for_probe(None).is_ok());

        // 远程配置会话：未登记的引用必须被拒绝。
        restrict_env_refs();
        let error = selection
            .endpoint_for_probe(None)
            .expect_err("远程会话应拒绝未登记引用");
        assert!(
            format!("{error:#}").contains("未登记的环境变量"),
            "{error:#}"
        );
        ENV_REF_POLICY.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn selection_hides_api_key_and_keeps_it_on_blank_save() {
        let previous = MemoryConfig {
            embedding: Some(MemoryEmbeddingSource::Remote {
                endpoint: MemoryRemoteEndpoint {
                    base_url: "http://127.0.0.1:1234/v1".into(),
                    api_key: "secret".into(),
                    model: "bge-m3".into(),
                    ..Default::default()
                },
                dimension: 1024,
            }),
            ..Default::default()
        };
        let selection = MemoryConfigSelection::from_memory(&previous);
        let remote = selection.embedding.remote.as_ref().unwrap();
        assert!(remote.has_api_key);
        assert!(remote.api_key.is_none());
        assert!(
            !serde_json::to_string(&selection)
                .unwrap()
                .contains("secret")
        );

        let saved = selection.to_memory(&previous).unwrap();
        assert_eq!(saved.embedding, previous.embedding, "空密钥保存保留原值");

        let mut replaced = selection.clone();
        replaced.embedding.remote.as_mut().unwrap().api_key = Some("new".into());
        let saved = replaced.to_memory(&previous).unwrap();
        match saved.embedding.unwrap() {
            MemoryEmbeddingSource::Remote { endpoint, .. } => assert_eq!(endpoint.api_key, "new"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn standalone_llm_remote_endpoint_is_configurable() {
        // 独立运行无 models.json：LLM 直接配置在线端点，并可被解析使用。
        let previous = MemoryConfig::default();
        let selection = MemoryConfigSelection {
            llm: MemoryLlmSelection {
                source: "remote".into(),
                key: None,
                remote: Some(MemoryRemoteSelection {
                    base_url: " https://api.example.com/v1 ".into(),
                    model: "gpt-mini".into(),
                    protocol: "anthropic".into(),
                    api_key: Some("${EXAMPLE_KEY}".into()),
                    ..Default::default()
                }),
            },
            ..MemoryConfigSelection::from_memory(&previous)
        };
        let config = selection.to_memory(&previous).unwrap();
        let Some(MemoryLlmSource::Remote(endpoint)) = &config.model else {
            panic!("应保存为在线端点：{:?}", config.model);
        };
        assert_eq!(endpoint.base_url, "https://api.example.com/v1");
        assert_eq!(endpoint.protocol, ProviderProtocol::Anthropic);
        assert_eq!(endpoint.api_key, "${EXAMPLE_KEY}");
        assert_eq!(endpoint.timeout_ms, DEFAULT_TIMEOUT_MS);

        let resolved = config
            .resolve_llm(&ModelsConfig::default())
            .unwrap()
            .expect("无模型列表时仍可解析在线端点");
        assert_eq!(resolved.model, "gpt-mini");

        // 回显不含密钥；空密钥再次保存保留原值；切回模型引用清除在线端点。
        let echoed = MemoryConfigSelection::from_memory(&config);
        let remote = echoed.llm.remote.as_ref().unwrap();
        assert_eq!(echoed.llm.source, "remote");
        assert!(remote.has_api_key && remote.api_key.is_none());
        assert_eq!(remote.protocol, "anthropic");
        assert_eq!(echoed.to_memory(&config).unwrap().model, config.model);
        let mut back = echoed.clone();
        back.llm = MemoryLlmSelection {
            source: "models_ref".into(),
            key: None,
            remote: None,
        };
        assert_eq!(
            back.to_memory(&config).unwrap().model,
            Some(MemoryLlmSource::ModelsRef { key: None })
        );

        let mut invalid = selection.clone();
        invalid.llm.remote.as_mut().unwrap().model = String::new();
        assert!(invalid.to_memory(&previous).is_err());
    }

    #[test]
    fn selection_switches_sources_and_validates() {
        let previous = MemoryConfig::default();
        let selection = MemoryConfigSelection {
            local_tier: "high".into(),
            llm: MemoryLlmSelection {
                source: "models_ref".into(),
                key: Some(" ".into()),
                remote: None,
            },
            embedding: MemoryComponentSelection {
                source: "builtin".into(),
                remote: None,
            },
            rerank: MemoryComponentSelection {
                source: "disabled".into(),
                remote: None,
            },
            vector_mode: "auto".into(),
        };
        let config = selection.to_memory(&previous).unwrap();
        assert_eq!(config.local_tier, MemoryLocalTier::High);
        assert_eq!(config.model, Some(MemoryLlmSource::ModelsRef { key: None }));
        assert_eq!(config.embedding, Some(MemoryEmbeddingSource::Builtin));
        assert!(config.rerank.is_none());

        let mut missing_dimension = selection.clone();
        missing_dimension.embedding = MemoryComponentSelection {
            source: "remote".into(),
            remote: Some(MemoryRemoteSelection {
                base_url: "http://x/v1".into(),
                model: "e".into(),
                ..Default::default()
            }),
        };
        assert!(missing_dimension.to_memory(&previous).is_err());
    }

    #[test]
    fn to_options_resolves_remote_and_builtin_components() {
        let config = MemoryConfig {
            embedding: Some(MemoryEmbeddingSource::Remote {
                endpoint: MemoryRemoteEndpoint {
                    base_url: "http://127.0.0.1:1234/v1".into(),
                    model: "bge-m3".into(),
                    ..Default::default()
                },
                dimension: 1024,
            }),
            rerank: Some(MemoryRerankSource::Builtin),
            vector_mode: MemoryVectorMode::EmbeddedLanceDb,
            ..Default::default()
        };
        let options = config.to_options_with(&models_with_routes(true, true));
        assert_eq!(options.model.as_ref().unwrap().model, "step-mini");
        assert_eq!(options.embedding.as_ref().unwrap().dimension, 1024);
        assert!(options.rerank.is_none(), "内置 rerank 不走在线端点");
        assert_eq!(options.local_rerank, Some(MemoryLocalTier::Mid));
        assert_eq!(options.local_embedding, None, "在线 embedding 优先");
        assert!(options.needs_local_models());
        // IPC 往返后仍保持内置来源。
        let roundtrip = MemoryConfig::from_options(&options);
        assert_eq!(roundtrip.rerank, Some(MemoryRerankSource::Builtin));
        assert!(matches!(
            roundtrip.embedding,
            Some(MemoryEmbeddingSource::Remote { .. })
        ));
        assert_eq!(options.vector_mode, MemoryVectorMode::EmbeddedLanceDb);
    }

    #[test]
    fn disable_enable_marker_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(".disabled");

        assert!(!marker.exists());
        disable_memory_at(&marker).unwrap();
        assert!(marker.exists());

        enable_memory_at(&marker).unwrap();
        assert!(!marker.exists());
        // 再次启用不报错
        enable_memory_at(&marker).unwrap();
    }
}
