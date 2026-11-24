//! Skill 存储路径计算（sidecar 自治，不依赖 host 注入）。

use std::path::{Path, PathBuf};

/// 解析用户主目录（兼容 POSIX 与 Windows）。
pub fn user_home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(home));
        }
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(profile));
    }
    let drive = std::env::var_os("HOMEDRIVE");
    let path = std::env::var_os("HOMEPATH");
    match (drive, path) {
        (Some(d), Some(p)) if !d.is_empty() && !p.is_empty() => Some(PathBuf::from(d).join(p)),
        _ => None,
    }
}

/// 天工存储根：`~/.tiangong`。
pub fn storage_root() -> PathBuf {
    user_home_dir()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tiangong")
}

/// 旧版 skills 存储目录：优先 `TIANGONG_STORAGE_ROOT/skills`，回退 `~/.tiangong/skills`。
///
/// 只在迁入插件数据目录时读取（见 `migration::migrate_legacy_dir`），迁完后不再读写。
pub fn legacy_skills_storage_dir_path() -> PathBuf {
    if let Some(root) = std::env::var_os(tiangong_plugin_sidecar::env::STORAGE_ROOT_ENV)
        .filter(|value| !value.is_empty())
    {
        return PathBuf::from(root).join("skills");
    }
    storage_root().join("skills")
}

/// 宿主注入的插件自有数据目录（`<存储根>/data/skill`）；独立运行时为 None。
pub fn plugin_storage_data_dir() -> Option<PathBuf> {
    std::env::var_os(tiangong_plugin_sidecar::env::PLUGIN_STORAGE_DATA_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// 迁入插件数据目录后的 skills 存储目录：`<插件数据目录>/skills`。
pub fn skills_data_dir_path(data_dir: &Path) -> PathBuf {
    data_dir.join("skills")
}

/// mcp-lock.json 路径：位于 skills 存储目录内。
pub fn mcp_lock_path(skills_dir: &Path) -> PathBuf {
    skills_dir.join("mcp-lock.json")
}
