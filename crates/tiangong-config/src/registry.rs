//! 配置内存单例
//!
//! 进程级保留一份可变的 [`TiangongConfig`]，供尚未接入应用状态的插件和命令读取。
//! 应用宿主持有自己的运行期配置；配置变化时经 [`update_models`] 可靠落盘，并同步
//! 此兼容副本。
//!
//! ## 生命周期
//!
//! - 启动：入口层调 [`init`] 从磁盘加载到内存
//! - 读取：兼容调用方经 [`models`] / [`config`] 从内存取最新值
//! - 变更：[`update_models`] 先写盘成功再更新内存，返回 Result

use std::sync::{OnceLock, RwLock};

use crate::config::TiangongConfig;

static CONFIG: OnceLock<RwLock<Option<TiangongConfig>>> = OnceLock::new();

fn config_cell() -> &'static RwLock<Option<TiangongConfig>> {
    CONFIG.get_or_init(|| RwLock::new(None))
}

/// 启动时从默认目录加载配置到内存单例，并返回本次加载结果。
///
/// 返回值供应用状态直接持有，避免初始化后再从单例读取一次。
pub fn init() -> TiangongConfig {
    let cfg = crate::loader::load_tiangong_config();
    if let Ok(mut guard) = config_cell().write() {
        *guard = Some(cfg.clone());
    }
    cfg
}

/// 从指定目录加载配置到内存单例（供测试 / 自定义目录）。
pub fn init_from_dir(dir: &std::path::Path) -> TiangongConfig {
    let cfg = crate::loader::load_tiangong_config_from_dir(dir);
    if let Ok(mut guard) = config_cell().write() {
        *guard = Some(cfg.clone());
    }
    cfg
}

/// 读取内存中的完整配置克隆（未 init 时 panic）。
pub fn config() -> TiangongConfig {
    config_cell()
        .read()
        .ok()
        .and_then(|g| g.clone())
        .expect("config 未初始化：需在启动时调用 init")
}

/// 读取内存中的模型配置克隆（未 init 时 panic）。
pub fn models() -> tiangong_llm::models_config::ModelsConfig {
    config().models
}

/// 读取内存中的模型配置；入口尚未初始化配置时返回 None。
pub fn try_models() -> Option<tiangong_llm::models_config::ModelsConfig> {
    config_cell()
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .map(|config| config.models)
}

/// 读取用户"按需进程沙箱"开关；尚未初始化配置时返回 None（调用方按开启处理）。
pub fn try_sandbox_disabled() -> Option<bool> {
    config_cell()
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .map(|config| config.sandbox_disabled)
}

/// 宿主直跑时用户自定义的环境变量屏蔽清单（缺省为空）。
pub fn try_sandbox_policy() -> crate::config::SandboxUserPolicy {
    config_cell()
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .map(|config| config.sandbox_policy)
        .unwrap_or_default()
}

pub fn try_command_env_blocklist() -> Vec<String> {
    config_cell()
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .map(|config| {
            if config.sandbox_policy.environment_blocklist.is_empty() {
                config.command_env_blocklist
            } else {
                config.sandbox_policy.environment_blocklist
            }
        })
        .unwrap_or_default()
}

/// 可靠更新模型配置：先写盘成功，再更新兼容副本并返回新的完整配置。
///
/// 处理顺序：
/// 1. 写 models.json 到磁盘
/// 2. 写盘成功后更新内存
/// 3. 返回 Result
///
/// `current` 是应用状态持有的当前配置，确保其余字段和实际存储目录不会被兼容
/// 副本中的旧值覆盖。写盘失败时两份运行期配置都不变。
pub fn update_models(
    current: &TiangongConfig,
    new_models: tiangong_llm::models_config::ModelsConfig,
) -> anyhow::Result<TiangongConfig> {
    new_models.validate_headers().map_err(anyhow::Error::msg)?;
    let mut next = current.clone();
    let dir = next.storage_root.clone();
    // 先写盘——失败则内存不变。
    crate::io::save_models_config_at(&dir, &new_models)?;
    // 写盘成功，更新内存。
    next.models = new_models;
    if let Ok(mut guard) = config_cell().write() {
        *guard = Some(next.clone());
    }
    Ok(next)
}

/// 更新内存配置并落盘。调用方负责通知 core/plugin 刷新。
pub fn update(new_config: TiangongConfig) -> anyhow::Result<()> {
    new_config.save_to_disk()?;
    if let Ok(mut guard) = config_cell().write() {
        *guard = Some(new_config);
    }
    Ok(())
}

impl TiangongConfig {
    /// 落盘到配置自身的数据目录（issue #245：不再吞错误）。
    pub fn save_to_disk(&self) -> anyhow::Result<()> {
        let dir = &self.storage_root;
        crate::io::save_models_config_at(dir, &self.models)?;
        let prompt_path = dir.join("custom-prompt.md");
        if self.custom_system_prompt.trim().is_empty() {
            crate::io::clear_custom_prompt_at(&prompt_path)?;
        } else {
            crate::io::save_custom_prompt_at(&prompt_path, &self.custom_system_prompt)?;
        }
        crate::io::save_app_config_at(
            dir,
            self.default_trust_mode,
            &self.workspace_dir,
            self.sandbox_disabled,
            &self.sandbox_policy,
            &self.command_env_blocklist,
        )?;
        Ok(())
    }
}

/// 插件能力集合签名——用于检测配置变化是否影响插件注册集合
/// （新增/删除能力 vs 仅 endpoint 变更）。
///
/// 模型配置只保留对话与多模态；图片/视频/语音能力已移除，
/// 看图插件仅在 chat 模型自身不支持图片时才需要，而多模态已无独立路由，
/// 因此当前没有随模型配置变化的插件注册集合。保留签名结构供调用方比较。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginSetSignature {
    /// chat 模型是否支持图片理解（影响附件的处理方式）。
    pub chat_multimodal: bool,
}

/// 从 ModelsConfig 计算插件能力集合签名。
pub fn plugin_set_signature(
    models: &tiangong_llm::models_config::ModelsConfig,
) -> PluginSetSignature {
    PluginSetSignature {
        chat_multimodal: models.chat_is_multimodal(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_llm::ProviderProtocol;
    use tiangong_llm::models_config::{
        ModelCapability, ModelEntry, ModelsConfig, ProviderConfig, RoutingSlot,
    };

    fn models_with_chat(capabilities: Vec<ModelCapability>) -> ModelsConfig {
        let mut m = ModelsConfig::default();
        m.providers.insert(
            "p".to_string(),
            ProviderConfig {
                headers: Default::default(),
                base_url: "https://api.test.com".to_string(),
                api_key: "k".to_string(),
                timeout_ms: 60_000,
                protocol: ProviderProtocol::OpenAiChatCompletions,
            },
        );
        m.routing.insert(
            RoutingSlot::Chat,
            ModelEntry {
                provider: "p".to_string(),
                model: "test-model".to_string(),
                capabilities,
                options: serde_json::json!({}),
                context_window: None,
            },
        );
        m
    }

    #[test]
    fn plugin_set_signature_detects_chat_multimodal_toggle() {
        let text = models_with_chat(vec![ModelCapability::Chat]);
        let vision = models_with_chat(vec![ModelCapability::Chat, ModelCapability::Multimodal]);

        assert!(!plugin_set_signature(&text).chat_multimodal);
        assert!(plugin_set_signature(&vision).chat_multimodal);
        assert_ne!(plugin_set_signature(&text), plugin_set_signature(&vision));
    }

    #[test]
    fn plugin_set_signature_unchanged_when_only_endpoint_diff() {
        let m1 = models_with_chat(vec![ModelCapability::Chat]);
        let mut m2 = m1.clone();
        if let Some(entry) = m2.routing.get_mut(&RoutingSlot::Chat) {
            entry.model = "different-model".to_string();
        }

        assert_eq!(
            plugin_set_signature(&m1),
            plugin_set_signature(&m2),
            "仅 endpoint 变化时签名应不变"
        );
    }
}
