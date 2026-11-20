use crate::error::LlmError;

pub fn map_openai_error(error: &async_openai::error::OpenAIError) -> LlmError {
    let text = format_error_with_sources(error);
    use async_openai::error::OpenAIError;
    match error {
        OpenAIError::JSONDeserialize(..) => {
            return LlmError::Serialization("上游返回了无效 JSON".into());
        }
        OpenAIError::InvalidArgument(_) => return LlmError::InvalidRequest(text),
        OpenAIError::Reqwest(error) if error.is_timeout() => return LlmError::Timeout(0),
        OpenAIError::Reqwest(error) if error.is_decode() || error.is_builder() => {
            return LlmError::Serialization("请求或响应编码无效".into());
        }
        OpenAIError::ApiError(response) => {
            let status = response.status_code;
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return LlmError::Authentication(text);
            }
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 529 {
                return LlmError::RateLimited(text);
            }
            if status == reqwest::StatusCode::REQUEST_TIMEOUT {
                return LlmError::Timeout(0);
            }
            if status.is_client_error() {
                return LlmError::InvalidRequest(text);
            }
        }
        _ => {}
    }
    if is_rate_limited_text(&text) {
        return LlmError::RateLimited(text);
    }
    LlmError::Transport(text)
}

/// 拼接错误与其 source 链：reqwest 的 Display 不含底层原因，
/// 需要手动展开才能把 "connection reset by peer" 这类信息透给用户。
fn format_error_with_sources(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(err) = source {
        text.push_str(": ");
        text.push_str(&err.to_string());
        source = err.source();
    }
    text
}

fn is_rate_limited_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    text.contains("429")
        || text.contains("529")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("overloaded_error")
}
