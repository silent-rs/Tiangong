//! Codex（ChatGPT 账号）后端适配层：`chatgpt.com/backend-api/codex`。
//!
//! 与 `providers/openai`（Responses API）并列。Codex 后端请求/响应结构与 Responses
//! 一致，因此复用其请求构建、SSE 事件解析与完整响应解析；本模块只负责 Codex 特有部分：
//! - 鉴权来自 ChatGPT OAuth 登录态（[`crate::codex_auth`]），401 时强制续期后重试一次；
//! - 请求体按后端约束改写（仅流式、`store=false`、不支持采样与输出上限参数）；
//! - `complete` 走流式后聚合（后端不接受非流式请求）；
//! - 模型目录走 `/models?client_version=`，保留上下文窗口等元信息。

mod client;
mod config;
mod provider;

pub use config::CodexConfig;
pub use provider::CodexProvider;
