//! 运行态存储：`~/.tiangong/data/subagent/runtime/`。
//!
//! 维护会话激活关系、任务、运行、事件历史与待投递 Hook 队列。
//! 写入全部走原子替换；Hook 事件先落盘再投递（投递成功即移除）。

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use tiangong_plugin_subagent_protocol::hooks::HookEvent;
use tiangong_plugin_subagent_protocol::state::{
    ActivationRecord, AgentEventRecord, RunRecord, TaskRecord,
};

use crate::paths::{atomic_write, now_string, runtime_root, validate_id_segment};

#[derive(Debug, Default, Serialize, Deserialize)]
struct ActivationsFile {
    activations: Vec<ActivationRecord>,
}

pub struct RuntimeStore {
    root: PathBuf,
}

impl RuntimeStore {
    pub fn open() -> Result<Self> {
        let root = runtime_root()?;
        for sub in ["tasks", "runs", "hooks/queue", "events", "logs"] {
            std::fs::create_dir_all(root.join(sub))
                .with_context(|| format!("创建运行态目录失败: {}", root.join(sub).display()))?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    // ── 激活关系 ──────────────────────────────────────────────

    pub fn activations(&self) -> Vec<ActivationRecord> {
        self.read_activations().unwrap_or_default().activations
    }

    pub fn upsert_activation(&self, record: ActivationRecord) -> Result<()> {
        let mut file = self.read_activations()?;
        file.activations
            .retain(|a| a.activation_id != record.activation_id);
        file.activations.push(record);
        self.write_activations(&file)
    }

    pub fn replace_activation(&self, record: ActivationRecord) -> Result<()> {
        self.upsert_activation(record)
    }

    fn read_activations(&self) -> Result<ActivationsFile> {
        let path = self.root.join("activations.json");
        if !path.is_file() {
            return Ok(ActivationsFile::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("读取 activations.json 失败: {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| "解析 activations.json 失败".to_string())
    }

    fn write_activations(&self, file: &ActivationsFile) -> Result<()> {
        let body = serde_json::to_vec_pretty(file).context("序列化 activations 失败")?;
        atomic_write(&self.root.join("activations.json"), &body)
    }

    // ── 任务与运行 ────────────────────────────────────────────

    pub fn save_task(&self, task: &TaskRecord) -> Result<()> {
        let body = serde_json::to_vec_pretty(task).context("序列化任务失败")?;
        atomic_write(
            &self
                .root
                .join("tasks")
                .join(format!("{}.json", task.task_id)),
            &body,
        )
    }

    pub fn load_task(&self, task_id: &str) -> Result<TaskRecord> {
        validate_id_segment(task_id)?;
        let path = self.root.join("tasks").join(format!("{task_id}.json"));
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("任务不存在: {task_id}"))?;
        serde_json::from_str(&raw).with_context(|| format!("解析任务失败: {task_id}"))
    }

    pub fn list_tasks(&self) -> Vec<TaskRecord> {
        read_dir_json(&self.root.join("tasks"))
    }

    pub fn save_run(&self, run: &RunRecord) -> Result<()> {
        let body = serde_json::to_vec_pretty(run).context("序列化运行失败")?;
        atomic_write(
            &self.root.join("runs").join(format!("{}.json", run.run_id)),
            &body,
        )
    }

    pub fn load_run(&self, run_id: &str) -> Result<RunRecord> {
        validate_id_segment(run_id)?;
        let path = self.root.join("runs").join(format!("{run_id}.json"));
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("运行不存在: {run_id}"))?;
        serde_json::from_str(&raw).with_context(|| format!("解析运行失败: {run_id}"))
    }

    pub fn list_runs(&self) -> Vec<RunRecord> {
        read_dir_json(&self.root.join("runs"))
    }

    // ── 事件历史（append-only JSONL，event_id 为 scru128 天然时间有序）──

    pub fn append_event(&self, event: &AgentEventRecord) -> Result<()> {
        use std::io::Write;
        let path = self.root.join("events").join("events.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("打开事件日志失败: {}", path.display()))?;
        let line = serde_json::to_string(event).context("序列化事件失败")?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// 读取事件历史（倒序，最新在前）。
    pub fn list_events(
        &self,
        agent_id: Option<&str>,
        run_id: Option<&str>,
        limit: usize,
    ) -> Vec<AgentEventRecord> {
        let path = self.root.join("events").join("events.jsonl");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let mut events: Vec<AgentEventRecord> = raw
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|event: &AgentEventRecord| agent_id.is_none_or(|id| event.agent_id == id))
            .filter(|event: &AgentEventRecord| {
                run_id.is_none_or(|id| event.run_id.as_deref() == Some(id))
            })
            .collect();
        events.reverse();
        events.truncate(limit.max(1));
        events
    }

    // ── Hook 投递队列（先落盘，成功投递后移除） ────────────────

    pub fn enqueue_hook(&self, event: &HookEvent) -> Result<()> {
        let body = serde_json::to_vec_pretty(event).context("序列化 Hook 事件失败")?;
        atomic_write(
            &self
                .root
                .join("hooks/queue")
                .join(format!("{}.json", event.event_id)),
            &body,
        )
    }

    pub fn dequeue_hook(&self, event_id: &str) -> Result<()> {
        validate_id_segment(event_id)?;
        let path = self
            .root
            .join("hooks/queue")
            .join(format!("{event_id}.json"));
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(anyhow::anyhow!(error).context(format!("移除已投递 Hook 失败: {event_id}")))
            }
        }
    }

    pub fn queued_hooks(&self) -> Vec<HookEvent> {
        read_dir_json(&self.root.join("hooks/queue"))
    }

    pub fn update_hook(&self, event: &HookEvent) -> Result<()> {
        self.enqueue_hook(event)
    }

    // ── run 日志 ──────────────────────────────────────────────

    pub fn run_log_path(&self, run_id: &str) -> Result<PathBuf> {
        validate_id_segment(run_id)?;
        Ok(self.root.join("logs").join(format!("{run_id}.log")))
    }

    pub fn append_run_log(&self, run_id: &str, line: &str) {
        use std::io::Write;
        let Ok(path) = self.run_log_path(run_id) else {
            return;
        };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(file, "{} {}", now_string(), line);
        }
    }
}

/// 读取目录下全部 JSON 文件并反序列化（坏文件跳过并告警）。
fn read_dir_json<T: for<'de> Deserialize<'de>>(dir: &std::path::Path) -> Vec<T> {
    let mut items = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return items;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::new)
            .and_then(|raw| serde_json::from_str(&raw).map_err(anyhow::Error::new))
        {
            Ok(item) => items.push(item),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "跳过无法解析的运行态文件");
            }
        }
    }
    items
}
