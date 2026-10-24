//! 整理执行记录（审计日志）。
//!
//! 每次运行写入一个 JSON Lines 文件：每次删除/改写/新建记忆、改动关联、LLM
//! 判定都在**写入前**追加一行并立即落盘。被删除的记忆保存完整快照（含关联与
//! 证据路径），可据此追溯或人工恢复。记录写入失败时中断整理，保证「没有记录
//! 就不删除」。试运行同样记录（`dry_run: true`），便于完整审阅整理计划。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context as _;

/// 执行记录文件。
pub struct Journal {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl Journal {
    /// 创建记录文件（父目录不存在时自动创建）。
    pub fn create(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建记录目录失败：{}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("创建执行记录失败：{}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加一条记录并立即落盘：`{"ts":..,"event":..,...fields}`。
    pub fn record(&self, event: &str, fields: serde_json::Value) -> anyhow::Result<()> {
        let mut line = serde_json::Map::new();
        line.insert(
            "ts".to_string(),
            serde_json::Value::String(chrono::Local::now().naive_local().to_string()),
        );
        line.insert(
            "event".to_string(),
            serde_json::Value::String(event.to_string()),
        );
        if let serde_json::Value::Object(map) = fields {
            line.extend(map);
        }
        let mut text = serde_json::to_string(&serde_json::Value::Object(line))?;
        text.push('\n');
        let mut file = self
            .file
            .lock()
            .map_err(|_| anyhow::anyhow!("执行记录文件锁已损坏"))?;
        file.write_all(text.as_bytes())
            .and_then(|_| file.sync_data())
            .with_context(|| format!("写入执行记录失败：{}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_appended_as_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/journal.jsonl");
        let journal = Journal::create(&path).unwrap();
        journal
            .record("delete_node", serde_json::json!({"id": "a"}))
            .unwrap();
        journal
            .record("update_node", serde_json::json!({"id": "b"}))
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"], "delete_node");
        assert_eq!(lines[0]["id"], "a");
        assert!(lines[1]["ts"].is_string());
    }
}
