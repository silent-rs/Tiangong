//! 插件存储目录定位。
//!
//! 沙箱内存储根只有 `data/<插件 id>`、`media/`、`tmp/` 可写，插件自有数据应
//! 落在 [`storage_data_dir`]。旧数据迁移属于插件自身行为，由各插件自行实现，
//! 不在本模块提供。

use std::path::PathBuf;

use tiangong_plugin_runtime::sidecar::{PLUGIN_STORAGE_DATA_DIR_ENV, STORAGE_ROOT_ENV};

/// 天工存储根：优先宿主注入的 `TIANGONG_STORAGE_ROOT`，否则 `~/.tiangong`。
pub fn storage_root() -> PathBuf {
    if let Some(root) = std::env::var_os(STORAGE_ROOT_ENV).filter(|value| !value.is_empty()) {
        return PathBuf::from(root);
    }
    crate::endpoint::home_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(".tiangong")
}

/// 插件自有数据目录 `<存储根>/data/<plugin_id>`。
///
/// 优先使用宿主注入的 `TIANGONG_PLUGIN_STORAGE_DATA_DIR`（stdio 传输由宿主
/// 预先创建）；未注入时按存储根推导。
pub fn storage_data_dir(plugin_id: &str) -> PathBuf {
    if let Some(dir) =
        std::env::var_os(PLUGIN_STORAGE_DATA_DIR_ENV).filter(|value| !value.is_empty())
    {
        return PathBuf::from(dir);
    }
    storage_root().join("data").join(plugin_id)
}
