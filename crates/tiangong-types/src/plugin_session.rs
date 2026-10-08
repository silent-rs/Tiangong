//! 插件专用只读会话快照。
//!
//! [`PluginSession`] 是提供给 WASM 插件的会话视图，不包含 Core 内部运行状态。
//! 由 Core 在生命周期钩子里从完整 `Session` 转换而来，经 JSON 序列化传给 WASM。
//!
//! 插件不应了解也不依赖 Core 的完整 `Session` 类型。

use serde::{Deserialize, Serialize};

use crate::message::Message;

/// 插件只读会话快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSession {
    /// 会话 ID。
    pub id: String,
    /// 会话标题。
    pub title: String,
    /// 工作目录。
    pub cwd: String,
    /// 工作区标识（由宿主生成的平台无关 ID，通常取 cwd 的末尾目录名）。
    pub workspace_id: String,
    /// 本轮起始用户消息 ID（on_turn_started / on_turn_finished 钩子填充）。
    ///
    /// 插件按 ID 定位本轮起点，不受快照消息增删（如剔除 Notice）影响；
    /// 其余钩子（会话就绪/结束）无本轮概念，不填充。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_start_message_id: Option<String>,
    /// 思考强度。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// 消息列表（完整对话历史）。
    pub messages: Vec<Message>,
    /// 上下文摘要。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_summary: Option<String>,
    /// 创建时间。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
}
