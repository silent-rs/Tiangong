//! Responses API 错误映射。
//!
//! 与 Chat Completions 共用 `async-openai` 错误类型，直接复用 Chat Completions 模块的映射逻辑。

pub(super) use crate::providers::openai_chatcompletions::error::map_openai_error as map_responses_error;
