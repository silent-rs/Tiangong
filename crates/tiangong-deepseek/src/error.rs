use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum DeepSeekError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("transport error: {0}")]
    Transport(String),

    #[error("authentication failed: {0}")]
    Authentication(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("rate limited: {0}")]
    RateLimited(String),

    #[error("request timeout: {0}")]
    Timeout(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("stream error: {0}")]
    Stream(String),

    #[error("API error: {0}")]
    Api(String),
}

/// 连接阶段失败（DNS、TCP 建连、代理握手）的 `Transport` 消息前缀。
pub(crate) const CONNECT_ERROR_PREFIX: &str = "connect: ";

impl DeepSeekError {
    /// 是否为连接阶段失败：请求尚未送达服务端，重发不会造成重复处理。
    pub fn is_connect(&self) -> bool {
        matches!(self, Self::Transport(message) if message.starts_with(CONNECT_ERROR_PREFIX))
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::RateLimited(_))
    }
}
