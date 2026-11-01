//! 工具结果类型。
//!
//! LocalToolExecutor 及其内置工具（web_fetch / run_command）已全部迁出为进程内插件：
//! - 文件工具 → tiangong-plugin-fs（0.2 起合并为 file_read / file_write / current_time，按 action 分派）
//! - web_fetch → tiangong-plugin-fetch（CLI/Server）/ browser 插件（GUI）
//! - run_command / run_shell → tiangong-plugin-command（CLI/Server）/ terminal 插件（GUI）
//!
//! core 不再直接执行任何工具，仅保留 ToolResult 供插件 handler 返回。
//! 路径沙箱/命令白名单（原 common）已迁出为独立 crate tiangong-toolkit。

use serde::{Deserialize, Serialize};

/// 工具失败类型取值（插件结构化声明的 `error_kind`）。
///
/// 由插件按自身语义声明，core 未经声明时回落关键词推断
/// （`classify_tool_result_failure`）；声明值必须对齐此处的
/// snake_case 取值，非法值按未声明处理。
pub const TOOL_ERROR_KINDS: &[&str] = &[
    "argument_error",
    "user_rejected",
    "command_failed",
    "timeout",
    "environment_missing",
    "network_failure",
    "tool_internal_error",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub ok: bool,
    pub summary: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    /// 插件声明的结构化失败类型（`ok=false` 时有意义）：取值对齐
    /// [`TOOL_ERROR_KINDS`]。未声明（旧插件）时 core 回落关键词推断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    /// 插件声明的可重试性（覆盖 core 的默认判定）；仅声明 `error_kind`
    /// 或单独声明本字段时生效。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default)]
    pub execution: Option<ToolExecutionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionRecord {
    pub tool_name: String,
    pub args: Vec<String>,
    pub duration_ms: u64,
    pub ok: bool,
    pub exit_code: i32,
    pub summary: String,
}
