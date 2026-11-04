use reqwest::header::HeaderMap;
use std::time::Duration;
/// Codex 后端配置。鉴权不在配置中：每次请求从 ChatGPT 登录态取令牌。
/// 重试不在配置中：打开阶段（连接/发送/探测）与流间重试的额度是代码常量
/// （`WS_MAX_ATTEMPTS` / 消费层 `MAX_STREAM_RETRY_ATTEMPTS`），各 10 次。
#[derive(Clone)]
pub struct CodexConfig {
    pub base_url: String,
    pub headers: HeaderMap,
    pub timeout: Duration,
}
impl CodexConfig {
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        Self {
            base_url: if base_url.trim().is_empty() {
                super::auth::CODEX_BASE_URL.to_string()
            } else {
                base_url
            },
            headers: HeaderMap::new(),
            timeout: Duration::from_secs(60),
        }
    }
}
impl std::fmt::Debug for CodexConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexConfig")
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .finish()
    }
}
