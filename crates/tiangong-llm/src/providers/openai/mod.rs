//! OpenAI Responses API（`/responses`）适配层。
//!
//! 与 `providers/openai_chatcompletions`（Chat Completions）并列，二者共享 `async-openai`
//! 客户端与重试/错误映射基础能力，但请求/响应/流式结构各自独立实现。

mod client;
mod config;
mod error;
pub(crate) mod mapping;
mod provider;
mod stream;

pub(crate) use client::{ResponsesClient, ResponsesStreamResponse};
pub use config::OpenAiResponsesConfig;
pub(crate) use config::RetryNotifier;
pub use provider::OpenAiResponsesProvider;
pub(crate) use provider::map_responses_stream;
