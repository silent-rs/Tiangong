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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub ok: bool,
    pub summary: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
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
