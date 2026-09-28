//! 插件配置持久化。
//!
//! 配置文件位于 `~/.tiangong/generate-image-openai/config.json`，
//! 存储用户选择的模型来源、手动端点和已解析缓存的端点。

use std::path::PathBuf;

use anyhow::{Context, Result};
use tiangong_plugin_generate_image_openai_protocol::{
    ConfigSelection, ImageGenConfig, ResolvedEndpoint,
};
use tiangong_plugin_runtime::sidecar::STORAGE_ROOT_ENV;

/// 插件配置文件所在目录：`~/.tiangong/generate-image-openai/`。
fn config_dir() -> Result<PathBuf> {
    let storage_root = std::env::var(STORAGE_ROOT_ENV)
        .context("TIANGONG_STORAGE_ROOT 未注入，sidecar 无法定位配置目录")?;
    Ok(PathBuf::from(storage_root).join("generate-image-openai"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

/// 读取已保存的配置，文件不存在或解析失败时返回默认配置。
pub fn load_or_default() -> ImageGenConfig {
    match load() {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!(error = %err, "读取 generate-image-openai 配置失败，使用默认配置");
            ImageGenConfig::default()
        }
    }
}

pub fn load() -> Result<ImageGenConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(ImageGenConfig::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
    let config: ImageGenConfig = serde_json::from_str(&content)
        .with_context(|| format!("解析配置文件失败：{}", path.display()))?;
    Ok(config)
}

/// 保存配置到磁盘。
pub fn save(config: &ImageGenConfig) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建配置目录失败：{}", dir.display()))?;
    let path = config_path()?;
    let content = serde_json::to_string_pretty(config).context("序列化配置失败")?;
    std::fs::write(&path, content)
        .with_context(|| format!("写入配置文件失败：{}", path.display()))?;
    Ok(())
}

/// 把 UI 保存的选择写为完整配置（含已解析的端点缓存）。
pub fn save_selection(
    selection: &ConfigSelection,
    resolved: ResolvedEndpoint,
) -> Result<ImageGenConfig> {
    let config = ImageGenConfig {
        source: selection.source.clone(),
        global_model_key: selection.global_model_key.clone(),
        chatgpt_model: selection.chatgpt_model.clone(),
        manual_endpoint: selection.manual_endpoint.clone(),
        resolved,
        extra_prompt: selection.extra_prompt.clone(),
    };
    save(&config)?;
    Ok(config)
}

/// 仅更新已缓存的端点（on_config_updated 触发时使用）。
pub fn save_resolved(config: &ImageGenConfig) -> Result<()> {
    save(config)
}
