//! Agent 身份配置（`~/.tiangong/data/subagent/agents/<agent-id>/agent.toml）。

use serde::{Deserialize, Serialize};

/// 运行后端类型。
///
/// 首版仅实现 [`BackendKind::Cli`]（JSONL 非交互协议子进程）；其余为阶段二至四
/// 的规划后端，可先声明身份，激活时会被拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// JSONL 非交互协议子进程（首版）。
    Cli,
    /// 天工已有会话作为运行后端（阶段二）。
    TiangongSession,
    /// 天工原生 Subagent（agent-team，阶段二）。
    AgentTeam,
    /// Claude Code（阶段三）。
    ClaudeCode,
    /// Codex（阶段三）。
    Codex,
    /// OctoLoop 复合 Subagent（阶段四）。
    OctoLoop,
}

impl BackendKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Cli => "CLI 命令",
            Self::TiangongSession => "天工会话",
            Self::AgentTeam => "天工原生 Subagent",
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::OctoLoop => "OctoLoop",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "cli" => Self::Cli,
            "tiangong_session" => Self::TiangongSession,
            "agent_team" => Self::AgentTeam,
            "claude_code" => Self::ClaudeCode,
            "codex" => Self::Codex,
            "octoloop" => Self::OctoLoop,
            _ => return None,
        })
    }
}

/// Workspace 访问策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspacePolicy {
    /// 只读：分析、搜索和审查。
    ReadOnly,
    /// 独占写：允许修改，但同一 Workspace 同时只有一个写入者。
    ReadWriteExclusive,
    /// 隔离 worktree：为本次激活在独立 git worktree 中修改。
    IsolatedWorktree,
}

impl WorkspacePolicy {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ReadOnly => "只读",
            Self::ReadWriteExclusive => "独占写",
            Self::IsolatedWorktree => "隔离 worktree",
        }
    }

    pub fn allows_write(&self) -> bool {
        matches!(self, Self::ReadWriteExclusive | Self::IsolatedWorktree)
    }
}

impl std::fmt::Display for WorkspacePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Agent 身份配置。高频运行状态不写入本文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub description: String,
    pub backend: BackendKind,
    /// CLI 后端的启动命令（shell 命令行）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// 天工会话后端关联的源会话 ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub workspace_policy: WorkspacePolicy,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

fn default_true() -> bool {
    true
}

/// 运行后端实际支持的能力。上层不得把「重启并附带历史」描述为「原地恢复」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AdapterCapabilities {
    /// 支持普通消息通道。
    pub messaging: bool,
    /// 支持正式任务提交。
    pub tasks: bool,
    /// 支持流式输出事件。
    pub streaming: bool,
    /// 支持原地恢复被中断的运行。
    pub resume: bool,
    /// 支持纠偏（运行中注入用户消息）。
    pub correction: bool,
    /// 支持中断（保留现场）。
    pub interrupt: bool,
    /// 支持审批回调。
    pub approval: bool,
    /// 允许写绑定 Workspace。
    pub workspace_write: bool,
    /// 支持产物读取。
    pub artifacts: bool,
    /// 支持内部进度展示。
    pub internal_progress: bool,
}

impl BackendKind {
    /// 该后端当前实现的能力声明。
    pub fn capabilities(&self) -> AdapterCapabilities {
        match self {
            Self::Cli => AdapterCapabilities {
                messaging: true,
                tasks: true,
                streaming: true,
                resume: false,
                correction: true,
                interrupt: true,
                approval: true,
                workspace_write: true,
                artifacts: true,
                internal_progress: true,
            },
            // 天工会话后端：消息与任务经本机 server 投递，turn 完成经 WASM
            // 钩子回报；无流式与审批回流；中断为通知式（无法硬取消 turn）。
            Self::TiangongSession => AdapterCapabilities {
                messaging: true,
                tasks: true,
                streaming: false,
                resume: false,
                correction: true,
                interrupt: true,
                approval: false,
                workspace_write: true,
                artifacts: true,
                internal_progress: false,
            },
            // 天工原生 Subagent 后端：系统为 Agent 新建并独占一个专属
            // 天工运行时会话（首次运行时创建、跨任务延续上下文），执行
            // 与回报管线同会话后端；无需用户提供任何会话线索。
            Self::AgentTeam => AdapterCapabilities {
                messaging: true,
                tasks: true,
                streaming: false,
                resume: false,
                correction: true,
                interrupt: true,
                approval: false,
                workspace_write: true,
                artifacts: true,
                internal_progress: false,
            },
            // 未实现的后端一律声明为不可用，激活时被拒绝。
            _ => AdapterCapabilities {
                messaging: false,
                tasks: false,
                streaming: false,
                resume: false,
                correction: false,
                interrupt: false,
                approval: false,
                workspace_write: false,
                artifacts: false,
                internal_progress: false,
            },
        }
    }

    /// 后端是否已在当前版本实现。
    pub fn implemented(&self) -> bool {
        matches!(self, Self::Cli | Self::TiangongSession | Self::AgentTeam)
    }
}
