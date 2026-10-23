//! 命令执行链路操作（run_command / run_shell）。

use serde::{Deserialize, Serialize};

use crate::{CommandAccessContext, CommandOperation, TOOL_RUN_COMMAND, TOOL_RUN_SHELL};

/// 命令执行响应：保留与 core `ToolResult` 同构字段，便于 sidecar 直接构造。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecResponse {
    pub ok: bool,
    pub summary: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// `run_command` 工具请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunCommandRequest {
    /// 命令名（可含参数，由 sidecar 拆分）。
    pub cmd: String,
    /// 命令参数列表（追加到 cmd 拆分结果后）。
    #[serde(default)]
    pub args: Vec<String>,
    /// 工作目录（可选，默认会话工作目录）。
    #[serde(default)]
    pub cwd: Option<String>,
    /// 超时时间（秒），0 或不填表示不限时。
    #[serde(default)]
    pub timeout_secs: u64,
    #[serde(flatten)]
    pub access: CommandAccessContext,
}
pub struct RunCommand;
impl CommandOperation for RunCommand {
    const NAME: &'static str = TOOL_RUN_COMMAND;
    type Request = RunCommandRequest;
    type Response = ExecResponse;
}

/// `run_shell` 工具请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunShellRequest {
    /// shell 脚本文本。
    pub script: String,
    /// shell 类型：auto/bash/sh/powershell/pwsh，默认 auto。
    #[serde(default)]
    pub shell: String,
    /// 工作目录（可选）。
    #[serde(default)]
    pub cwd: Option<String>,
    /// 超时时间（秒），0 或不填表示不限时。
    #[serde(default)]
    pub timeout_secs: u64,
    #[serde(flatten)]
    pub access: CommandAccessContext,
}
pub struct RunShell;
impl CommandOperation for RunShell {
    const NAME: &'static str = TOOL_RUN_SHELL;
    type Request = RunShellRequest;
    type Response = ExecResponse;
}
