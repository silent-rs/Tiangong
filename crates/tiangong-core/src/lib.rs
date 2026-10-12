/// @提及候选（见 [`tiangong_types::MentionCandidate`]）。
pub use tiangong_types::MentionCandidate;
/// @提及候选分组（见 [`tiangong_types::MentionGroup`]）。
pub use tiangong_types::MentionGroup;

pub mod agent_input;
pub mod config;
pub mod context;
pub mod core;
pub(crate) mod formatting;
pub mod permission;
pub mod prompt;
pub mod react;
pub mod runtime;
pub mod session;
pub mod shared_runtime;
mod stream_throttle;
pub mod tools;
pub mod turn_context;
