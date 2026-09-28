//! Codex（ChatGPT 账号）后端适配层：`chatgpt.com/backend-api/codex`。
//!
//! 与 `providers/openai`（Responses API）并列。Codex 后端请求/响应结构与 Responses
//! 一致，因此复用其请求构建、SSE 事件解析与完整响应解析；本模块负责 Codex 特有部分：
//! - [`auth`]：ChatGPT OAuth 登录（浏览器 PKCE / 设备码）、凭据落盘与令牌续期；
//! - [`usage`]：账号用量额度查询；
//! - provider：请求体按后端约束改写（仅流式、`store=false`、不支持采样与输出上限参数），
//!   `complete` 走流式后聚合，401 时强制续期后重试一次；
//! - 模型目录走 `/models?client_version=`，保留上下文窗口等元信息。

pub mod auth;
mod client;
mod config;
mod provider;
pub mod usage;

pub use auth::{
    CODEX_BASE_URL, CodexAuthStatus, CodexLoginStart, access_readonly, cancel_login,
    load_credentials, logout, refresh_now, start_browser_login, start_device_login, status,
    wait_login,
};
pub use config::CodexConfig;
pub use provider::CodexProvider;
pub use usage::{CodexNamedLimit, CodexUsage, CodexUsageWindow, usage};
