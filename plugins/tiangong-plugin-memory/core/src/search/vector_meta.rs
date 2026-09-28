//! 向量索引元数据：把 LanceDB 表与生成向量的模型身份绑定。
//!
//! 维度相同不代表语义空间相同（多数模型都是 1024 维），因此按
//! 「规范化模型名 + 维度」计算指纹，每个指纹对应独立的表：
//!
//! - 切换到新指纹时新建表并在后台回填，回填完成前不走语义召回；
//! - 保留上一代表，切回原模型时直接复用、无需重算；
//! - 旧版无元数据的 `memory_vectors` 表在首次打开时按当前配置接管。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 元数据文件名（位于 lancedb 目录内）。
pub(crate) const META_FILE: &str = "index_meta.json";
/// 旧版固定表名。
pub(crate) const LEGACY_TABLE_NAME: &str = "memory_vectors";
/// 旧版表来源模型未知时使用的占位模型名（指纹与任何真实模型都不同）。
pub(crate) const LEGACY_UNKNOWN_MODEL: &str = "legacy-unknown";

/// 生成向量的模型身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VectorIdentity {
    pub(crate) model: String,
    pub(crate) dimension: usize,
}

impl VectorIdentity {
    pub(crate) fn new(model: &str, dimension: usize) -> Self {
        Self {
            model: canonical_model(model),
            dimension,
        }
    }

    /// 指纹：`<规范化模型>@<维度>`。
    pub(crate) fn fingerprint(&self) -> String {
        format!("{}@{}", self.model, self.dimension)
    }

    /// 对应的表名，仅含 `[a-z0-9_]`。
    pub(crate) fn table_name(&self) -> String {
        let slug = self
            .model
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
            .collect::<String>();
        format!("memory_vectors_{slug}_{}", self.dimension)
    }
}

/// 规范化模型名：同一模型在不同服务上的命名视为同一身份，
/// 例如 `BAAI/bge-m3`、`text-embedding-bge-m3`、`bge-m3` 都归为 `bge-m3`。
pub(crate) fn canonical_model(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    let base = base.split(':').next().unwrap_or(base);
    let base = base.strip_prefix("text-embedding-").unwrap_or(base);
    let base = base
        .strip_suffix("-gguf")
        .or_else(|| base.strip_suffix("-onnx"))
        .unwrap_or(base);
    base.to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct VectorIndexMeta {
    /// 当前使用的指纹。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) active: Option<String>,
    /// 上一代指纹（保留以便切回）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) previous: Option<String>,
    /// 指纹 → 表信息。
    #[serde(default)]
    pub(crate) tables: BTreeMap<String, VectorTableMeta>,
    /// 已从 `tables` 摘除但尚未成功删除的表名：删表失败时留在这里，
    /// 下次打开索引继续重试，避免留下永远无人清理的孤儿表。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) pending_drop: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct VectorTableMeta {
    pub(crate) table: String,
    pub(crate) model: String,
    pub(crate) dimension: usize,
    /// 回填是否完成；未完成时语义召回不可用。
    #[serde(default)]
    pub(crate) complete: bool,
    /// 回填游标（已处理到的最大节点 ID）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cursor: Option<String>,
    #[serde(default)]
    pub(crate) created_at: String,
}

impl VectorTableMeta {
    pub(crate) fn new(table: String, identity: &VectorIdentity, complete: bool) -> Self {
        Self {
            table,
            model: identity.model.clone(),
            dimension: identity.dimension,
            complete,
            cursor: None,
            created_at: chrono::Local::now().naive_local().to_string(),
        }
    }
}

pub(crate) fn meta_path(lancedb_dir: &Path) -> PathBuf {
    lancedb_dir.join(META_FILE)
}

impl VectorIndexMeta {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("读取向量索引元数据失败: {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("解析向量索引元数据失败: {}", path.display()))
    }

    /// 原子写入（临时文件 + rename）。
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        let content = serde_json::to_string_pretty(self).context("序列化向量索引元数据失败")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, content)
            .with_context(|| format!("写入向量索引元数据失败: {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("替换向量索引元数据失败: {}", path.display()))
    }

    /// 切换当前指纹，返回需要删除的过期表名（既非当前也非上一代）。
    pub(crate) fn activate(&mut self, fingerprint: &str) -> Vec<String> {
        if self.active.as_deref() != Some(fingerprint) {
            self.previous = self.active.take().filter(|value| value != fingerprint);
            self.active = Some(fingerprint.to_string());
        }
        if self.previous.as_deref() == Some(fingerprint) {
            self.previous = None;
        }
        let keep = [self.active.clone(), self.previous.clone()];
        let stale = self
            .tables
            .keys()
            .filter(|key| !keep.iter().flatten().any(|kept| kept == *key))
            .cloned()
            .collect::<Vec<_>>();
        // 摘除记录的同时登记待删列表：删表是易失败的外部操作，元数据里留一份
        // 待办才能在下次打开时继续清理。
        for key in stale {
            if let Some(entry) = self.tables.remove(&key)
                && !self.pending_drop.contains(&entry.table)
            {
                self.pending_drop.push(entry.table);
            }
        }
        // 仍在册的表不该出现在待删列表里（可能上次删除失败后又被重新登记）。
        let live = self
            .tables
            .values()
            .map(|entry| entry.table.clone())
            .collect::<Vec<_>>();
        self.pending_drop.retain(|table| !live.contains(table));
        self.pending_drop.clone()
    }

    /// 标记某个表已成功删除（从待删列表移除）。
    pub(crate) fn mark_dropped(&mut self, table: &str) {
        self.pending_drop.retain(|value| value != table);
    }

    /// 更新某指纹的回填进度。
    pub(crate) fn record_progress(
        &mut self,
        fingerprint: &str,
        cursor: Option<&str>,
        complete: bool,
    ) {
        if let Some(entry) = self.tables.get_mut(fingerprint) {
            if let Some(cursor) = cursor {
                entry.cursor = Some(cursor.to_string());
            }
            entry.complete = complete;
            if complete {
                entry.cursor = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_model_unifies_provider_naming() {
        for name in [
            "bge-m3",
            "BAAI/bge-m3",
            "text-embedding-bge-m3",
            "bge-m3:latest",
            "BAAI/bge-m3-onnx",
        ] {
            assert_eq!(canonical_model(name), "bge-m3", "{name}");
        }
        assert_ne!(canonical_model("bge-large-zh-v1.5"), "bge-m3");
    }

    #[test]
    fn same_dimension_different_model_has_different_fingerprint() {
        let a = VectorIdentity::new("text-embedding-bge-m3", 1024);
        let b = VectorIdentity::new("BAAI/bge-m3", 1024);
        let c = VectorIdentity::new("bge-large-zh-v1.5", 1024);
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
        assert_eq!(a.table_name(), "memory_vectors_bge_m3_1024");
    }

    #[test]
    fn activate_keeps_one_previous_generation() {
        let mut meta = VectorIndexMeta::default();
        for (fp, table) in [("a@1", "ta"), ("b@1", "tb"), ("c@1", "tc")] {
            meta.tables.insert(
                fp.to_string(),
                VectorTableMeta::new(table.to_string(), &VectorIdentity::new(fp, 1), true),
            );
        }
        assert!(meta.activate("a@1").contains(&"tb".to_string()));
        meta.tables.insert(
            "b@1".into(),
            VectorTableMeta::new("tb".into(), &VectorIdentity::new("b", 1), true),
        );
        meta.tables.insert(
            "c@1".into(),
            VectorTableMeta::new("tc".into(), &VectorIdentity::new("c", 1), true),
        );
        let dropped = meta.activate("b@1");
        assert_eq!(meta.previous.as_deref(), Some("a@1"));
        // tb 已重新登记，只剩 tc 待删。
        assert_eq!(dropped, vec!["tc".to_string()]);
        assert_eq!(meta.pending_drop, vec!["tc".to_string()]);

        // 切回上一代：直接复用，旧 active 成为上一代。
        let dropped = meta.activate("a@1");
        // tc 上次没删成功，仍在待删列表里继续重试。
        assert_eq!(dropped, vec!["tc".to_string()]);
        assert_eq!(meta.active.as_deref(), Some("a@1"));
        assert_eq!(meta.previous.as_deref(), Some("b@1"));

        // 删除成功后不再重复返回。
        meta.mark_dropped("tc");
        assert!(meta.activate("a@1").is_empty());
    }

    #[test]
    fn meta_roundtrip_and_progress() {
        let dir = tempfile::tempdir().unwrap();
        let path = meta_path(dir.path());
        let identity = VectorIdentity::new("bge-m3", 1024);
        let mut meta = VectorIndexMeta::default();
        meta.tables.insert(
            identity.fingerprint(),
            VectorTableMeta::new(identity.table_name(), &identity, false),
        );
        meta.activate(&identity.fingerprint());
        meta.record_progress(&identity.fingerprint(), Some("node-9"), false);
        meta.save(&path).unwrap();

        let loaded = VectorIndexMeta::load(&path).unwrap();
        assert_eq!(loaded, meta);
        assert_eq!(
            loaded.tables[&identity.fingerprint()].cursor.as_deref(),
            Some("node-9")
        );
    }
}
