//! 插件存储目录定位。
//!
//! 沙箱内存储根只有 `data/<插件 id>`、`media/`、`tmp/` 可写，插件自有数据应
//! 落在 [`storage_data_dir`]。旧数据迁移属于插件自身行为，由各插件自行实现，
//! 不在本模块提供。

use std::path::PathBuf;

use crate::env::{PLUGIN_ID_ENV, PLUGIN_STORAGE_DATA_DIR_ENV, STORAGE_ROOT_ENV};

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
/// 预先创建）；未注入时按存储根推导。注入值只在 `TIANGONG_PLUGIN_ID` 与
/// `plugin_id` 一致（或未注入 ID）时采用：环境变量会被子进程继承，其他插件
/// 拉起的进程（如在终端插件里手动运行 sidecar）不能误用父插件的数据目录。
pub fn storage_data_dir(plugin_id: &str) -> PathBuf {
    let id_matches = std::env::var_os(PLUGIN_ID_ENV)
        .filter(|value| !value.is_empty())
        .is_none_or(|id| id == plugin_id);
    if id_matches
        && let Some(dir) =
            std::env::var_os(PLUGIN_STORAGE_DATA_DIR_ENV).filter(|value| !value.is_empty())
    {
        return PathBuf::from(dir);
    }
    storage_root().join("data").join(plugin_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvGuard {
        fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
            let saved = vars
                .iter()
                .map(|(key, _)| (*key, std::env::var_os(key)))
                .collect();
            for (key, value) in vars {
                // SAFETY: 测试经 serial 串行执行，不与其他线程并发读写环境变量。
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
            Self(saved)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                // SAFETY: 同上。
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn injected_data_dir_only_applies_to_matching_plugin() {
        let _guard = EnvGuard::set(&[
            (STORAGE_ROOT_ENV, Some("/root-x")),
            (PLUGIN_STORAGE_DATA_DIR_ENV, Some("/root-x/data/terminal")),
            (PLUGIN_ID_ENV, Some("terminal")),
        ]);
        assert_eq!(
            storage_data_dir("terminal"),
            PathBuf::from("/root-x/data/terminal")
        );
        assert_eq!(
            storage_data_dir("memory"),
            PathBuf::from("/root-x/data/memory"),
            "继承自其他插件的注入目录不得采用"
        );
    }

    #[test]
    #[serial_test::serial]
    fn falls_back_to_storage_root_without_injection() {
        let _guard = EnvGuard::set(&[
            (STORAGE_ROOT_ENV, Some("/root-y")),
            (PLUGIN_STORAGE_DATA_DIR_ENV, None),
            (PLUGIN_ID_ENV, None),
        ]);
        assert_eq!(storage_data_dir("mcp"), PathBuf::from("/root-y/data/mcp"));
    }
}
