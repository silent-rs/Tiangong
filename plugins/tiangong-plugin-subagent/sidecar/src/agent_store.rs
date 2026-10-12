//! Agent 身份存储：`~/.tiangong/data/subagent/agents/<agent-id>/` 的扫描与 CRUD。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::paths::{agents_root, atomic_write, new_id, now_string, validate_id_segment};
use tiangong_plugin_subagent_protocol::config::{AgentConfig, BackendKind, WorkspacePolicy};

pub struct AgentStore {
    root: PathBuf,
}

/// Agent 可变字段的更新集合（None 表示不修改）。
#[derive(Debug, Default)]
pub struct AgentChanges<'a> {
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    pub command: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub workspace_policy: Option<WorkspacePolicy>,
    pub enabled: Option<bool>,
    pub instructions: Option<&'a str>,
}

impl AgentStore {
    pub fn open() -> Result<Self> {
        let root = agents_root()?;
        std::fs::create_dir_all(&root)
            .with_context(|| format!("创建 agents 目录失败: {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    fn agent_dir(&self, agent_id: &str) -> Result<PathBuf> {
        validate_id_segment(agent_id)?;
        Ok(self.root.join(agent_id))
    }

    /// 扫描全部持久 Agent（跳过隐藏目录与非目录项）。
    pub fn list(&self) -> Vec<AgentConfig> {
        let mut agents = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return agents;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            match self.load(name) {
                Ok(config) => agents.push(config),
                Err(error) => {
                    tracing::warn!(agent_id = name, %error, "跳过无法解析的 Agent 目录");
                }
            }
        }
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        agents
    }

    pub fn load(&self, agent_id: &str) -> Result<AgentConfig> {
        let dir = self.agent_dir(agent_id)?;
        let manifest_path = dir.join("agent.toml");
        if !manifest_path.is_file() {
            bail!("Agent 不存在: {agent_id}");
        }
        let raw = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("读取 agent.toml 失败: {}", manifest_path.display()))?;
        let config: AgentConfig =
            toml::from_str(&raw).with_context(|| format!("解析 agent.toml 失败: {agent_id}"))?;
        Ok(config)
    }

    pub fn instructions(&self, agent_id: &str) -> Result<String> {
        let dir = self.agent_dir(agent_id)?;
        let path = dir.join("instructions.md");
        if !path.is_file() {
            return Ok(String::new());
        }
        std::fs::read_to_string(&path)
            .with_context(|| format!("读取 instructions.md 失败: {agent_id}"))
    }

    /// 创建新 Agent（身份目录 + 空指令/记忆/产物目录）。
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        name: &str,
        description: &str,
        backend: BackendKind,
        command: Option<&str>,
        session_id: Option<&str>,
        workspace_policy: WorkspacePolicy,
        instructions: Option<&str>,
    ) -> Result<AgentConfig> {
        if name.trim().is_empty() {
            bail!("Agent 名称不能为空");
        }
        if backend == BackendKind::Cli && command.map(str::trim).unwrap_or("").is_empty() {
            bail!("CLI 后端必须提供启动命令");
        }
        // 天工会话后端不要求预绑定会话：成员×工作区专属会话按映射延迟创建。
        let session_id = session_id.map(str::trim).filter(|value| !value.is_empty());
        let agent_id = format!("agent-{}", new_id());
        let dir = self.agent_dir(&agent_id)?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建 Agent 目录失败: {}", dir.display()))?;
        let now = now_string();
        let config = AgentConfig {
            id: agent_id,
            name: name.trim().to_string(),
            description: description.trim().to_string(),
            backend,
            command: command
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            session_id: session_id.map(str::to_string),
            workspace_policy,
            enabled: true,
            created_at: now.clone(),
            updated_at: now,
        };
        self.save(&config)?;
        for sub in ["memory", "artifacts"] {
            std::fs::create_dir_all(dir.join(sub))
                .with_context(|| format!("创建 {sub} 目录失败"))?;
        }
        let body = instructions.unwrap_or("").trim();
        if !body.is_empty() {
            atomic_write(&dir.join("instructions.md"), body.as_bytes())?;
        } else {
            atomic_write(
                &dir.join("instructions.md"),
                format!(
                    "# {}\n\n（在此描述该 Agent 的长期职责与工作要求）\n",
                    config.name
                )
                .as_bytes(),
            )?;
        }
        Ok(config)
    }

    /// 更新可变字段（后端类型创建后不可变更）。
    pub fn update(&self, agent_id: &str, changes: AgentChanges<'_>) -> Result<AgentConfig> {
        let mut config = self.load(agent_id)?;
        if let Some(name) = changes.name.filter(|s| !s.trim().is_empty()) {
            config.name = name.trim().to_string();
        }
        if let Some(description) = changes.description {
            config.description = description.trim().to_string();
        }
        if let Some(command) = changes.command {
            config.command = Some(command.trim().to_string())
                .filter(|s| !s.is_empty() || config.backend != BackendKind::Cli);
            if config.backend == BackendKind::Cli
                && config.command.as_deref().unwrap_or("").is_empty()
            {
                bail!("CLI 后端必须提供启动命令");
            }
        }
        if let Some(session_id) = changes.session_id {
            let session_id = session_id.trim();
            if config.backend == BackendKind::TiangongSession && session_id.is_empty() {
                bail!("天工会话后端必须关联一个已有会话");
            }
            config.session_id = (!session_id.is_empty()).then(|| session_id.to_string());
        }
        if let Some(policy) = changes.workspace_policy {
            config.workspace_policy = policy;
        }
        if let Some(enabled) = changes.enabled {
            config.enabled = enabled;
        }
        config.updated_at = now_string();
        self.save(&config)?;
        if let Some(instructions) = changes.instructions {
            let dir = self.agent_dir(agent_id)?;
            atomic_write(&dir.join("instructions.md"), instructions.as_bytes())?;
        }
        Ok(config)
    }

    /// 删除 Agent 身份目录（显式用户操作；运行态历史保留）。
    pub fn delete(&self, agent_id: &str) -> Result<()> {
        let dir = self.agent_dir(agent_id)?;
        if !dir.is_dir() {
            bail!("Agent 不存在: {agent_id}");
        }
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("删除 Agent 目录失败: {}", dir.display()))?;
        Ok(())
    }

    fn save(&self, config: &AgentConfig) -> Result<()> {
        let dir = self.agent_dir(&config.id)?;
        let body = toml::to_string_pretty(config).context("序列化 agent.toml 失败")?;
        atomic_write(&dir.join("agent.toml"), body.as_bytes())
    }
}
