//! tiangong-types：天工统一数据结构
//!
//! 所有 crate（core、cli、gui、server、connector）共用的数据类型。
//! 不包含业务逻辑，只有数据结构和序列化。

pub mod attachment;
pub mod event;
pub mod mention;
pub mod message;
pub mod plugin_session;
pub mod remote;
pub mod session;
pub mod status;
pub mod stream;
pub mod token;
pub mod trust_mode;

pub use attachment::{
    INJECTED_ASSETS_FIELD, InjectedAsset, StoredAsset, ToolResultInjection,
    content_blocks_are_empty, content_blocks_text, stable_content_blocks,
    validate_ready_content_blocks,
};
pub use event::{EventSource, RuntimeEvent, RuntimeEventType};
pub use mention::{
    MentionCandidate, MentionContext, MentionGroup, MentionQuery, MentionRequest, MentionTarget,
};
pub use message::{
    ContentBlock, DeferredToolInjection, MESSAGE_RENDER_MAX_BYTES, MediaAsset, MediaKind, Message,
    MessageMeta, MessageRender, MessageRole, MessageToolCall, Role, TurnStatus, UserSource,
    deserialize_messages, now_text, serialize_messages_flat,
};
pub use plugin_session::PluginSession;
pub use remote::{
    IncomingMessage, MessageAnnotations, MessageContent, OutgoingMessage, RemoteRole,
};
pub use session::Session;
pub use status::RunStatus;
pub use stream::{MemoryRecallHitSummary, SessionEvent, StreamEvent, StreamToolCall};
pub use token::TokenUsage;
pub use trust_mode::TrustMode;

#[cfg(test)]
mod tests;
