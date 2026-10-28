//! Prompt 存储路径计算（sidecar 自治，不依赖 host 注入）。

use std::path::PathBuf;

use tiangong_plugin_sidecar::env::STORAGE_ROOT_ENV;

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

/// 天工存储根：优先宿主注入的 `TIANGONG_STORAGE_ROOT`，回退 `~/.tiangong`。
pub fn storage_root() -> PathBuf {
    if let Some(root) = std::env::var(STORAGE_ROOT_ENV)
        .ok()
        .filter(|s| !s.is_empty())
    {
        return PathBuf::from(root);
    }
    user_home_dir()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tiangong")
}

/// 插件数据目录：`<storage_root>/data/prompt`。
pub fn plugin_data_dir() -> PathBuf {
    storage_root().join("data").join("prompt")
}
