//! MCP 存储路径自管。
//!
//! 配置与能力缓存放在插件自有数据目录 `~/.tiangong/data/mcp/`（沙箱内存储根
//! 只有该目录可写）。早期版本放在存储根下，首次启动时由 [`migrate_legacy`]
//! 复制进新目录（见 [`crate::migration`]），旧文件保留。
use std::path::PathBuf;

use tiangong_plugin_sidecar::storage::{storage_data_dir, storage_root};

use crate::migration::migrate_legacy_files;

const PLUGIN_ID: &str = "mcp";
const CONFIG_FILE: &str = "mcp.json";
const CAPABILITY_CACHE_FILE: &str = "mcp-tools-cache.json";

fn data_dir() -> PathBuf {
    storage_data_dir(PLUGIN_ID)
}

/// MCP 配置文件路径：`~/.tiangong/data/mcp/mcp.json`。
pub fn default_mcp_config_path() -> PathBuf {
    data_dir().join(CONFIG_FILE)
}

/// MCP tools 缓存路径：`~/.tiangong/data/mcp/mcp-tools-cache.json`。
pub fn default_mcp_capability_cache_path() -> PathBuf {
    data_dir().join(CAPABILITY_CACHE_FILE)
}

/// 把存储根下的旧 `mcp.json` 与能力缓存复制进插件自有数据目录（幂等）。
pub fn migrate_legacy() -> anyhow::Result<()> {
    let root = storage_root();
    let result = migrate_legacy_files(
        &data_dir(),
        &[
            (root.join(CONFIG_FILE), CONFIG_FILE),
            (root.join(CAPABILITY_CACHE_FILE), CAPABILITY_CACHE_FILE),
        ],
    )?;
    if result.performed {
        tracing::info!(copied = result.copied.len(), "MCP 旧数据迁移完成");
    }
    Ok(())
}
