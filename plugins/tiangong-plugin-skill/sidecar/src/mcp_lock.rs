//! MCP 依赖锁同步（从原生插件 `mcp_lock.rs` 迁入）。
//!
//! 聚合所有 skill 的 `requires.mcp`，写入 `mcp-lock.json`。

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::mcp_lock_path;
use crate::registry::{SkillRegistry, read_skill_manifest};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpDependencyLockRecord {
    #[serde(default)]
    pub ref_count: usize,
}

/// 同步 MCP 依赖锁：扫描所有 skill 的 requires.mcp，聚合写入 mcp-lock.json。
pub fn sync_mcp_dependency_lock(registry: &SkillRegistry) -> Result<()> {
    // 锁文件与技能同处一个 skills 目录：注册表的根就是它。
    let storage_dir = registry.root().to_path_buf();
    if let Err(error) = fs::create_dir_all(&storage_dir) {
        tracing::warn!(%error, dir = %storage_dir.display(), "创建 skills 目录失败，跳过 mcp-lock 同步");
        return Ok(());
    }

    let view = registry.view();
    let mut lock: BTreeMap<String, McpDependencyLockRecord> = BTreeMap::new();

    for entry in view.entries.values() {
        let Ok(manifest) = read_skill_manifest(&entry.dir.join("skill.toml")) else {
            continue;
        };
        for req in &manifest.requires.mcp {
            let package = req.package.trim();
            if package.is_empty() {
                continue;
            }
            let version = req.version.trim();
            let key = if version.is_empty() {
                package.to_string()
            } else {
                format!("{package}@{version}")
            };
            lock.entry(key).or_default().ref_count += 1;
        }
    }

    let lock_path = mcp_lock_path(&storage_dir);
    let json = serde_json::to_string_pretty(&lock)
        .with_context(|| format!("序列化 mcp-lock 失败：{}", lock_path.display()))?;
    fs::write(&lock_path, json)
        .with_context(|| format!("写入 mcp-lock 失败：{}", lock_path.display()))?;
    Ok(())
}
