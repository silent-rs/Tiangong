use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum AnthropicError {
    #[error("配置错误：{0}")]
    Config(String),

    #[error("传输错误：{0}")]
    Transport(String),

    #[error("认证失败：{0}")]
    Authentication(String),

    #[error("请求无效：{0}")]
    InvalidRequest(String),

    #[error("请求被限流：{0}")]
    RateLimited(String),

    #[error("序列化失败：{0}")]
    Serialization(String),

    #[error("流式处理失败：{0}")]
    Stream(String),

    #[error("请求超时：{0}")]
    Timeout(String),

    #[error("Anthropic API 错误：{0}")]
    Api(String),
}

/// 连接阶段失败（DNS、TCP 建连、代理握手）的 `Transport` 消息前缀。
pub(crate) const CONNECT_ERROR_PREFIX: &str = "connect: ";

impl AnthropicError {
    /// 是否为连接阶段失败：请求尚未送达服务端，重发不会造成重复处理。
    pub fn is_connect(&self) -> bool {
        matches!(self, Self::Transport(message) if message.starts_with(CONNECT_ERROR_PREFIX))
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::RateLimited(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_connect_仅识别连接阶段失败() {
        assert!(AnthropicError::Transport(format!("{CONNECT_ERROR_PREFIX}refused")).is_connect());
        assert!(!AnthropicError::Transport("timeout: read".into()).is_connect());
        assert!(!AnthropicError::Api("connect: x".into()).is_connect());
    }
}
