use std::path::PathBuf;

pub(crate) fn storage_root() -> PathBuf {
    if let Some(root) = std::env::var_os(tiangong_plugin_runtime::sidecar::STORAGE_ROOT_ENV)
        .filter(|value| !value.is_empty())
    {
        return PathBuf::from(root);
    }
    user_home_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(".tiangong")
}

pub(crate) fn memory_data_dir() -> PathBuf {
    storage_root().join("memory")
}

fn user_home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
