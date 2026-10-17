use crate::client::DeepSeekClient;
use crate::error::DeepSeekError;
use crate::types::{FetchRequest, FetchResponse, SearchRequest, SearchResponse};

/// 搜索词与原始问题的最大字符数（Unicode 字符）。
pub const MAX_QUERY_CHARS: usize = 128;
/// 单次搜索最多返回条数。
pub const MAX_SEARCH_COUNT: u32 = 50;
/// 抓取 URL 最大长度。
pub const MAX_FETCH_URL_CHARS: usize = 8192;

const SEARCH_PATH: &str = "/v1/tools/search";
const FETCH_PATH: &str = "/v1/tools/fetch";

/// 网页搜索与抓取（Search / Fetch API）。
///
/// 两者为只读请求，按成功请求次数计费；本地先做参数校验，非法参数不会发出请求。
pub struct WebTools<'c> {
    client: &'c DeepSeekClient,
}

impl<'c> WebTools<'c> {
    pub fn new(client: &'c DeepSeekClient) -> Self {
        Self { client }
    }

    /// 按 query 搜索网页，返回结构化结果；无匹配时 `results` 为空。
    pub async fn search(&self, request: SearchRequest) -> Result<SearchResponse, DeepSeekError> {
        validate_search(&request)?;
        self.client.post_tools(SEARCH_PATH, &request).await
    }

    /// 抓取单个网页，返回标题与 Markdown 正文。
    pub async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, DeepSeekError> {
        validate_fetch_url(&request.url)?;
        self.client.post_tools(FETCH_PATH, &request).await
    }
}

pub(crate) fn validate_search(request: &SearchRequest) -> Result<(), DeepSeekError> {
    validate_text("query", &request.query)?;
    if let Some(question) = &request.ori_question {
        validate_text("ori_question", question)?;
    }
    if let Some(count) = request.count
        && !(1..=MAX_SEARCH_COUNT).contains(&count)
    {
        return Err(DeepSeekError::InvalidRequest(format!(
            "'count' must be between 1 and {MAX_SEARCH_COUNT}, got {count}"
        )));
    }
    Ok(())
}

fn validate_text(field: &str, value: &str) -> Result<(), DeepSeekError> {
    if value.trim().is_empty() {
        return Err(DeepSeekError::InvalidRequest(format!(
            "'{field}' must not be empty or whitespace"
        )));
    }
    let chars = value.chars().count();
    if chars > MAX_QUERY_CHARS {
        return Err(DeepSeekError::InvalidRequest(format!(
            "'{field}' must be at most {MAX_QUERY_CHARS} characters, got {chars}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_fetch_url(url: &str) -> Result<(), DeepSeekError> {
    let invalid = |reason: &str| DeepSeekError::InvalidRequest(format!("'url' {reason}"));
    if url.is_empty() {
        return Err(invalid("must not be empty"));
    }
    if url.chars().count() > MAX_FETCH_URL_CHARS {
        return Err(invalid(&format!(
            "must be at most {MAX_FETCH_URL_CHARS} characters"
        )));
    }
    let lower = url.to_ascii_lowercase();
    let rest = if lower.starts_with("https://") {
        &url["https://".len()..]
    } else if lower.starts_with("http://") {
        &url["http://".len()..]
    } else {
        return Err(invalid("must be an HTTP/HTTPS URL"));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err(invalid("must not contain username or password"));
    }
    let host = if authority.starts_with('[') {
        // IPv6 字面量：[::1]:8080
        authority
            .split_once(']')
            .map(|(host, _)| host.trim_start_matches('['))
            .unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    if host.is_empty() {
        return Err(invalid("must contain a host"));
    }
    Ok(())
}
