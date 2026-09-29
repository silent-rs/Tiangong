//! 插件配置持久化。
//!
//! 配置文件位于插件 data 目录（宿主注入 `TIANGONG_PLUGIN_DATA_DIR`）下的
//! `config.json`，与插件安装目录一起管理，不依赖天工存储根与 models.json。

use std::path::PathBuf;

use anyhow::{Context, Result};
use tiangong_plugin_runtime::sidecar::PLUGIN_DATA_DIR_ENV;
use tiangong_plugin_volcengine_protocol::VolcengineConfig;

fn config_path() -> Result<PathBuf> {
    let data_dir = std::env::var_os(PLUGIN_DATA_DIR_ENV)
        .filter(|value| !value.is_empty())
        .context("TIANGONG_PLUGIN_DATA_DIR 未注入，sidecar 无法定位配置目录")?;
    Ok(PathBuf::from(data_dir).join("config.json"))
}

/// 读取已保存的配置；文件不存在时返回默认配置。
pub fn load() -> Result<VolcengineConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(VolcengineConfig::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("解析配置文件失败：{}", path.display()))
}

/// 保存配置（整体覆盖）。
pub fn save(config: &VolcengineConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建配置目录失败：{}", dir.display()))?;
    }
    let content = serde_json::to_string_pretty(config).context("序列化配置失败")?;
    std::fs::write(&path, content).with_context(|| format!("写入配置文件失败：{}", path.display()))
}

/// 解析 `${ENV_VAR}` 形式的 API Key 引用。
pub fn resolve_api_key(raw: &str) -> String {
    let raw = raw.trim();
    match raw
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    {
        Some(name) => std::env::var(name).unwrap_or_default(),
        None => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_api_key_reads_env_reference() {
        // SAFETY: 测试内设置仅本测试使用的环境变量名。
        unsafe { std::env::set_var("TIANGONG_VOLCENGINE_TEST_KEY", "secret") };
        assert_eq!(resolve_api_key("${TIANGONG_VOLCENGINE_TEST_KEY}"), "secret");
        assert_eq!(resolve_api_key(" plain "), "plain");
        assert_eq!(resolve_api_key("${TIANGONG_VOLCENGINE_MISSING_KEY}"), "");
    }
}
