//! sidecar 操作（页面直调与内部编排）的请求/响应载荷。

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SpawnRequest {
    /// App 实例编号。UI 创建终端时由调用方指定，保证 App 与 PTY 一一对应。
    #[serde(default)]
    pub(super) session_id: Option<String>,
    #[serde(default)]
    pub(super) cmd: String,
    #[serde(default)]
    pub(super) args: Vec<String>,
    #[serde(default)]
    pub(super) script: Option<String>,
    #[serde(default)]
    pub(super) cwd: Option<String>,
    /// 宿主会话标识：终端跟随会话切换的锚点。
    #[serde(default)]
    pub(super) scope_id: Option<String>,
    /// Agent 选择后立即预留，避免并行命令复用同一终端。
    #[serde(default)]
    pub(super) reserve: bool,
    #[serde(default = "default_cols")]
    pub(super) cols: u16,
    #[serde(default = "default_rows")]
    pub(super) rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecRequest {
    pub(super) session_id: String,
    #[serde(default)]
    pub(super) cmd: String,
    #[serde(default)]
    pub(super) args: Vec<String>,
    #[serde(default)]
    pub(super) script: Option<String>,
    #[serde(default)]
    pub(super) timeout: Option<u64>,
    #[serde(default)]
    pub(super) interactive: bool,
    #[serde(default)]
    pub(super) cwd: Option<String>,
}

pub(super) fn default_cols() -> u16 {
    80
}

pub(super) fn default_rows() -> u16 {
    24
}

#[derive(Debug, Serialize)]
pub(super) struct SpawnResponse {
    pub(super) session_id: String,
    /// 首批输出（shell 提示符等）：随响应返回给 UI 作渲染基线。
    /// 冷启动窗口内宿主通知监听可能尚未连上（sidecar 通知无订阅者即
    /// 丢弃），首批走通知会永久丢失——终端黑屏的根因；随响应走则
    /// 不依赖通知时序。首批已落盘/入历史，聚合线程不再重发。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) boot_output: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct ExecResponse {
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) exit_code: i32,
    pub(super) timed_out: bool,
    pub(super) cwd_after: String,
    pub(super) interactive_mode: bool,
    /// 命令使 shell 本身退出（如 exit/登出）：终端已不可复用，
    /// 下次调用将自动新建终端。
    pub(super) session_ended: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WriteRequest {
    pub(super) session_id: String,
    pub(super) data: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ScreenUpdateRequest {
    pub(super) session_id: String,
    pub(super) screen: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AcquireRequest {
    pub(super) scope_id: String,
}

#[derive(Debug, Serialize)]
pub(super) struct AcquireResponse {
    pub(super) session_id: Option<String>,
    pub(super) history: String,
    pub(super) reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SendRequest {
    pub(super) scope_id: String,
    pub(super) session_id: String,
    pub(super) input: String,
    #[serde(default)]
    pub(super) wait: Option<u64>,
    /// 为 true 时去掉输入末尾的换行，正文写入后单独补发一次回车（`\r`）；
    /// 缺省 false 原样写入，供 vi 等编辑场景保留纯文本。
    #[serde(default)]
    pub(super) enter: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct SendResponse {
    pub(super) session_id: String,
    pub(super) stdout: String,
    pub(super) exit_code: i32,
    pub(super) interactive_mode: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResizeRequest {
    pub(super) session_id: String,
    pub(super) cols: u16,
    pub(super) rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionIdRequest {
    pub(super) session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CloseRequest {
    #[serde(default)]
    pub(super) session_id: Option<String>,
    pub(super) scope_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TerminalGcRequest {
    pub(super) session_id: String,
    pub(super) live_terminal_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TerminalBindingRequest {
    pub(super) scope_id: String,
    pub(super) session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FindRequest {
    pub(super) scope_id: String,
    /// App 工具打开时携带的 PTY 实例编号；存在时必须精确附着。
    #[serde(default)]
    pub(super) session_id: Option<String>,
}

/// terminalFind 结果：会话不存在时 session_id 为 null（调用方据此新建）。
#[derive(Debug, Serialize)]
pub(super) struct FindResponse {
    pub(super) session_id: Option<String>,
    pub(super) history: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListByScopeRequest {
    pub(super) scope_id: String,
}

/// 宿主统一实例生命周期协议：`session_id` 为宿主会话，`instance_id` 为
/// 标签编号（终端即 PTY 编号）。与终端内部字段命名（session_id=PTY）不同，
/// 这里单独建模后映射到既有关闭逻辑。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstanceClosedRequest {
    pub(super) session_id: String,
    pub(super) instance_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListInstancesRequest {
    pub(super) session_id: String,
}

#[derive(Debug, Serialize)]
pub(super) struct InstanceEntry {
    pub(super) instance_id: String,
}

#[derive(Debug, Serialize)]
pub(super) struct ListInstancesResponse {
    pub(super) instances: Vec<InstanceEntry>,
}

/// terminalListByScope 条目：宿主会话下仍存活的终端（前端切换会话时
/// 恢复可见标签，历史输出由页面经 terminalFind/幂等 spawn 自行回放）。
#[derive(Debug, Serialize)]
pub(super) struct ScopeTerminalEntry {
    pub(super) session_id: String,
}

#[derive(Debug, Serialize)]
pub(super) struct ListByScopeResponse {
    pub(super) terminals: Vec<ScopeTerminalEntry>,
}

#[derive(Debug, Serialize)]
pub(super) struct OkResponse {
    pub(super) ok: bool,
}
