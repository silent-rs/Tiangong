use serde::{Deserialize, Serialize};

/// `rerank_score` 达到该值视为优质结果。
pub const HIGH_QUALITY_RERANK_SCORE: f64 = 1.5;
/// `authority` 达到该值视为极权威结果。
pub const HIGH_AUTHORITY: i32 = 3;

/// 网页搜索请求（`POST /v1/tools/search`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SearchRequest {
    /// 搜索词，1～128 个 Unicode 字符且不能全为空白；
    /// 支持 `site:` / `-site:` / `-词` / `"短语"` / `lang:` / `after:` / `before:` 等语法。
    pub query: String,
    /// 返回条数，1～50，服务端默认 10。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// 触发搜索的用户原始问题，1～128 个字符，帮助理解搜索意图。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ori_question: Option<String>,
    /// 用于优先返回本地化结果的 IP 地址。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub localization: Option<String>,
    /// 是否在 `content` 中返回网页正文，服务端默认 false。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub need_content: Option<bool>,
}

impl SearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..Default::default()
        }
    }

    pub fn count(mut self, count: u32) -> Self {
        self.count = Some(count);
        self
    }

    pub fn ori_question(mut self, question: impl Into<String>) -> Self {
        self.ori_question = Some(question.into());
        self
    }

    pub fn localization(mut self, ip: impl Into<String>) -> Self {
        self.localization = Some(ip.into());
        self
    }

    pub fn need_content(mut self, need_content: bool) -> Self {
        self.need_content = Some(need_content);
        self
    }
}

/// 网页搜索响应；无匹配结果时 `results` 为空数组。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SearchResponse {
    #[serde(default)]
    pub results: Vec<SearchResult>,
}

/// 单条搜索结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SearchResult {
    /// 排序分数 0～3，综合相关性、权威性、时效性。
    #[serde(default)]
    pub rerank_score: f64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    /// 短摘要（约 100～200 字符），适合前端展示。
    #[serde(default)]
    pub snippet: String,
    /// 长摘要（约 600～800 字符），适合给模型阅读。
    #[serde(default)]
    pub summary: String,
    /// 网页正文；`need_content` 为 false 或正文缺失时为空字符串。
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(default)]
    pub site_icon: Option<String>,
    /// 发布时间（UTC RFC 3339），未知时为 None。
    #[serde(default)]
    pub date: Option<String>,
    /// 网页权威性 0～4，-1 表示未知。
    #[serde(default = "unknown_authority")]
    pub authority: i32,
}

fn unknown_authority() -> i32 {
    -1
}

impl SearchResult {
    /// `rerank_score >= 1.5`。
    pub fn is_high_quality(&self) -> bool {
        self.rerank_score >= HIGH_QUALITY_RERANK_SCORE
    }

    /// `authority >= 3`。
    pub fn is_highly_authoritative(&self) -> bool {
        self.authority >= HIGH_AUTHORITY
    }
}

/// 网页抓取请求（`POST /v1/tools/fetch`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FetchRequest {
    /// 含 host 的 HTTP/HTTPS URL，≤8192 字符，不能携带用户名或密码。
    pub url: String,
}

impl FetchRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

/// 网页抓取响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FetchResponse {
    /// 页面标题，缺失时为空字符串。
    #[serde(default)]
    pub title: String,
    /// Markdown 正文。
    #[serde(default)]
    pub content: String,
}
