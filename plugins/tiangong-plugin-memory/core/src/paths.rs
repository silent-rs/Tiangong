use std::path::PathBuf;

pub(crate) fn storage_root() -> PathBuf {
    if let Some(root) = std::env::var_os(tiangong_plugin_sidecar::env::STORAGE_ROOT_ENV)
        .filter(|value| !value.is_empty())
    {
        return PathBuf::from(root);
    }
    user_home_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(".tiangong")
}

/// Memory 数据目录：插件自有数据目录 `<存储根>/data/memory`。
///
/// 早期版本使用 `<存储根>/memory`，由 [`crate::migration`] 在启动时迁入。
pub(crate) fn memory_data_dir() -> PathBuf {
    tiangong_plugin_sidecar::storage::storage_data_dir(PLUGIN_ID)
}

const PLUGIN_ID: &str = "memory";

fn user_home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
