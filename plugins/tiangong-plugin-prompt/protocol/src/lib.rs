//! Prompt 插件私有协议。
//!
//! - sidecar 操作名常量（设置页桥接与 sidecar dispatch 共用）；
//! - `get_prompt` / `set_prompt` 载荷形状。

use serde::{Deserialize, Serialize};

pub const PLUGIN_ID: &str = "prompt";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
/// sidecar business_protocol 版本（握手响应）。
pub const PROMPT_PROTOCOL_VERSION: u32 = 1;

/// 读取自定义指令的 sidecar 操作（设置页 `sidecar.get_prompt`）。
pub const GET_PROMPT_OPERATION: &str = "get_prompt";
/// 写入自定义指令的 sidecar 操作（设置页 `sidecar.set_prompt`）。
pub const SET_PROMPT_OPERATION: &str = "set_prompt";

/// get_prompt 响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetPromptResponse {
    /// custom-prompt.md 的完整内容（文件不存在时为空字符串）。
    pub content: String,
}

/// set_prompt 请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetPromptRequest {
    /// 新的 custom prompt 全文（空字符串表示清除）。
    pub content: String,
}
