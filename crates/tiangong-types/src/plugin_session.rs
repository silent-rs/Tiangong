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
    /// 思考强度。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// 本轮消息（剔除 Notice），不含本轮之前的历史。
    ///
    /// 只有 `on_turn_finished` 交付会话快照：首条为本轮用户输入，其后为运行中
    /// 引导消息、工具调用与结果、插件注入消息、最终回复。
    ///
    /// 以旧的扁平消息格式序列化，兼容已安装的旧版插件；反序列化两种格式都接受。
    #[serde(
        default,
        serialize_with = "crate::message::serialize_messages_flat",
        deserialize_with = "crate::message::deserialize_messages"
    )]
    pub messages: Vec<Message>,
    /// 上下文摘要。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_summary: Option<String>,
    /// 创建时间。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
}
