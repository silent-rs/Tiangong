//! 插件配置持久化。
//!
//! 配置文件位于插件自有数据目录 `~/.tiangong/data/generate-image-openai/config.json`，
//! 存储用户选择的模型来源、手动端点和已解析缓存的端点。
//!
//! 早期版本放在存储根下的 `generate-image-openai/config.json`；沙箱收拢写域后
//! 存储根只读，首次访问配置时由本插件把旧文件复制进新目录并写完成标记
//! （幂等，旧文件保留）。迁移失败时拒绝保存，避免新目录先落下默认配置后
//! 旧配置再也迁不过来。
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use tiangong_plugin_generate_image_openai_protocol::{
    ConfigSelection, ImageGenConfig, ResolvedEndpoint,
};

const PLUGIN_ID: &str = "generate-image-openai";
const CONFIG_FILE: &str = "config.json";
/// 旧数据迁移完成标记文件名（位于插件自有数据目录内）。
const LEGACY_MIGRATION_MARKER: &str = ".legacy-migrated";

/// 插件配置文件所在目录：`~/.tiangong/data/generate-image-openai/`（迁移后）。
fn config_dir() -> Result<PathBuf> {
    static MIGRATION: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    let dir = tiangong_plugin_sidecar::storage::storage_data_dir(PLUGIN_ID);
    MIGRATION
        .get_or_init(|| {
            let legacy = tiangong_plugin_sidecar::storage::storage_root()
                .join(PLUGIN_ID)
                .join(CONFIG_FILE);
            migrate_legacy_config(&legacy, &dir).map_err(|error| format!("{error:#}"))
        })
        .clone()
        .map_err(|error| anyhow::anyhow!("旧配置迁移未完成：{error}"))?;
    Ok(dir)
}

/// 旧配置存在且新目录尚无配置时复制进来（临时文件 + 改名），全部完成后写标记。
fn migrate_legacy_config(legacy: &Path, dir: &Path) -> Result<()> {
    let marker = dir.join(LEGACY_MIGRATION_MARKER);
    if marker.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("创建配置目录失败：{}", dir.display()))?;
    let target = dir.join(CONFIG_FILE);
    let legacy_exists = match std::fs::metadata(legacy) {
        Ok(meta) => meta.is_file(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("读取旧配置失败：{}", legacy.display()));
        }
    };
    if legacy_exists && !target.exists() {
        let staged = dir.join(format!(".{CONFIG_FILE}.migrating"));
        std::fs::copy(legacy, &staged)
            .with_context(|| format!("复制旧配置失败：{}", legacy.display()))?;
        std::fs::rename(&staged, &target)
            .with_context(|| format!("落盘迁移配置失败：{}", target.display()))?;
        tracing::info!(from = %legacy.display(), to = %target.display(), "旧配置已迁移");
    }
    std::fs::write(&marker, format!("{}\n", legacy.display()))
        .with_context(|| format!("写入迁移完成标记失败：{}", marker.display()))?;
    Ok(())
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_config_is_copied_once_without_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(PLUGIN_ID).join(CONFIG_FILE);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "{\"a\":1}").unwrap();
        let dir = root.path().join("data").join(PLUGIN_ID);

        migrate_legacy_config(&legacy, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap(),
            "{\"a\":1}"
        );
        assert!(dir.join(LEGACY_MIGRATION_MARKER).is_file());
        assert!(legacy.is_file(), "旧配置保留");

        std::fs::write(dir.join(CONFIG_FILE), "{}").unwrap();
        migrate_legacy_config(&legacy, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap(),
            "{}"
        );
    }

    #[test]
    fn missing_legacy_only_writes_marker() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("data").join(PLUGIN_ID);
        migrate_legacy_config(&root.path().join("absent.json"), &dir).unwrap();
        assert!(!dir.join(CONFIG_FILE).exists());
        assert!(dir.join(LEGACY_MIGRATION_MARKER).is_file());
    }
}
