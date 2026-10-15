use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::balance::Balance;
use crate::chat::Chat;
#[allow(deprecated)]
use crate::config::DeepSeekConfig;
use crate::config::{ClientOptions, DeepSeekClientBuilder};
use crate::error::DeepSeekError;
use crate::files::Files;
use crate::models::Models;
use crate::responses::Responses;

#[derive(Clone)]
pub struct DeepSeekClient {
    http_client: reqwest::Client,
    stream_http_client: reqwest::Client,
    config: ClientOptions,
}

impl DeepSeekClient {
    /// 以默认配置创建构造器（推荐用法）。
    pub fn builder(api_key: impl Into<String>) -> DeepSeekClientBuilder {
        DeepSeekClientBuilder::new(api_key)
    }

    #[deprecated(since = "0.1.5", note = "请改用 `DeepSeekClient::builder`")]
    #[allow(deprecated)]
    pub fn from_config(config: DeepSeekConfig) -> Result<Self, DeepSeekError> {
        Self::from_options(config.into(), false)
    }

    pub(crate) fn from_options(
        config: ClientOptions,
        no_proxy: bool,
    ) -> Result<Self, DeepSeekError> {
        let mut http_client = reqwest::Client::builder()
            .default_headers(config.headers.clone())
            .timeout(config.timeout);
        let mut stream_http_client =
            reqwest::Client::builder().default_headers(config.headers.clone());
        if no_proxy {
            http_client = http_client.no_proxy();
            stream_http_client = stream_http_client.no_proxy();
        }
        let http_client = http_client
            .build()
            .map_err(|err| DeepSeekError::Transport(err.to_string()))?;
        let stream_http_client = stream_http_client
            .build()
            .map_err(|err| DeepSeekError::Transport(err.to_string()))?;
        Ok(Self {
            http_client,
            stream_http_client,
            config,
        })
    }

    // ── 能力访问器 ──────────────────────────────────────────

    /// 流式请求读取一次性完整响应的总时限。
    pub(crate) fn stream_timeout(&self) -> std::time::Duration {
        self.config.timeout
    }

    pub fn chat(&self) -> Chat<'_> {
        Chat::new(self)
    }

    pub fn models(&self) -> Models<'_> {
        Models::new(self)
    }

    pub fn balance(&self) -> Balance<'_> {
        Balance::new(self)
    }

    pub fn responses(&self) -> Responses<'_> {
        Responses::new(self)
    }

    pub fn files(&self) -> Files<'_> {
        Files::new(self)
    }

    // ── 通用 HTTP 方法 ──────────────────────────────────────

    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, DeepSeekError> {
        let url = self.build_url(path);
        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .send()
            .await
            .map_err(map_reqwest_error)?;
        parse_json_response(response).await
    }

    pub(crate) async fn get_with_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, DeepSeekError> {
        let url = if query.is_empty() {
            self.build_url(path)
        } else {
            let query_string = query
                .iter()
                .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
                .collect::<Vec<_>>()
                .join("&");
            format!("{}?{}", self.build_url(path), query_string)
        };
        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .send()
            .await
            .map_err(map_reqwest_error)?;
        parse_json_response(response).await
    }

    pub(crate) async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T, DeepSeekError> {
        let url = self.build_url(path);
        let response = self
            .http_client
            .delete(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .send()
            .await
            .map_err(map_reqwest_error)?;
        parse_json_response(response).await
    }

    pub(crate) async fn post_multipart<T: DeserializeOwned>(
        &self,
        path: &str,
        form: &MultipartForm,
    ) -> Result<T, DeepSeekError> {
        let url = self.build_url(path);
        let (content_type, body) = form.encode();
        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", content_type)
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        parse_json_response(response).await
    }

    pub(crate) async fn post<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R, DeepSeekError> {
        let url = self.build_url(path);
        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        parse_json_response(response).await
    }

    pub(crate) async fn post_stream_raw(
        &self,
        path: &str,
        body: &(impl Serialize + ?Sized),
    ) -> Result<reqwest::Response, DeepSeekError> {
        let url = self.build_url(path);
        let response = self
            .stream_http_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        if !response.status().is_success() {
            return Err(parse_error_response(response).await);
        }

        Ok(response)
    }

    fn build_url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.config.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

fn map_reqwest_error(err: reqwest::Error) -> DeepSeekError {
    // 连接失败优先判定（建连超时同时满足 is_timeout），供上层识别可安全重发的错误。
    if err.is_connect() {
        DeepSeekError::Transport(format!("{}{err}", crate::error::CONNECT_ERROR_PREFIX))
    } else if err.is_timeout() {
        DeepSeekError::Transport(format!("timeout: {err}"))
    } else {
        DeepSeekError::Transport(err.to_string())
    }
}

async fn parse_error_response(response: reqwest::Response) -> DeepSeekError {
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    let body_text = String::from_utf8_lossy(&body).to_string();
    classify_http_error(status, body_text)
}

pub(crate) async fn parse_json_response<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, DeepSeekError> {
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|err| DeepSeekError::Transport(err.to_string()))?;

    if !status.is_success() {
        let body_text = String::from_utf8_lossy(&body).to_string();
        return Err(classify_http_error(status, body_text));
    }

    serde_json::from_slice(&body).map_err(|err| {
        DeepSeekError::Serialization(format!(
            "{err}: {}",
            String::from_utf8_lossy(&body[..body.len().min(512)])
        ))
    })
}

/// 聚合首块与剩余响应体并解析为完整响应，整体受 timeout 约束。
/// 供 chat/responses 流式入口在服务端忽略 stream 参数返回一次性 JSON 时使用。
pub(crate) async fn read_complete_response<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    first: bytes::Bytes,
    timeout: std::time::Duration,
) -> Result<T, DeepSeekError> {
    let read = async {
        let mut buf = first.to_vec();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| DeepSeekError::Transport(err.to_string()))?
        {
            buf.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<T>(&buf).map_err(|err| {
            DeepSeekError::Serialization(format!(
                "{err}: {}",
                String::from_utf8_lossy(&buf[..buf.len().min(256)])
            ))
        })
    };
    tokio::time::timeout(timeout, read)
        .await
        .map_err(|_| DeepSeekError::Timeout(format!("{} ms", timeout.as_millis())))?
}

fn classify_http_error(status: reqwest::StatusCode, body_text: String) -> DeepSeekError {
    if is_rate_limited_status_or_body(status, &body_text) {
        return DeepSeekError::RateLimited(body_text);
    }

    match status.as_u16() {
        400 => DeepSeekError::InvalidRequest(body_text),
        401 | 403 => DeepSeekError::Authentication(body_text),
        _ if status.is_server_error() => DeepSeekError::Transport(body_text),
        _ => DeepSeekError::Api(body_text),
    }
}

fn is_rate_limited_status_or_body(status: reqwest::StatusCode, body_text: &str) -> bool {
    let lower = body_text.to_ascii_lowercase();
    matches!(status.as_u16(), 429 | 529)
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("overloaded_error")
}

// ── multipart/form-data 表单（手工构造，避免新增依赖） ────

#[derive(Default)]
pub(crate) struct MultipartForm {
    parts: Vec<MultipartPart>,
}

enum MultipartPart {
    Field {
        name: String,
        value: String,
    },
    File {
        name: String,
        filename: String,
        data: Vec<u8>,
    },
}

impl MultipartForm {
    pub(crate) fn new() -> Self {
        Self { parts: Vec::new() }
    }

    pub(crate) fn field(mut self, name: &str, value: &str) -> Self {
        self.parts.push(MultipartPart::Field {
            name: name.to_string(),
            value: value.to_string(),
        });
        self
    }

    pub(crate) fn file(mut self, name: &str, filename: &str, data: Vec<u8>) -> Self {
        self.parts.push(MultipartPart::File {
            name: name.to_string(),
            filename: filename.to_string(),
            data,
        });
        self
    }

    pub(crate) fn encode(&self) -> (String, Vec<u8>) {
        let boundary = generate_boundary();
        let mut body = Vec::new();
        for part in &self.parts {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match part {
                MultipartPart::Field { name, value } => {
                    body.extend_from_slice(
                        format!(
                            "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
                            escape_form_value(name)
                        )
                        .as_bytes(),
                    );
                    body.extend_from_slice(value.as_bytes());
                }
                MultipartPart::File {
                    name,
                    filename,
                    data,
                } => {
                    body.extend_from_slice(
                        format!(
                            "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                            escape_form_value(name),
                            escape_form_value(filename)
                        )
                        .as_bytes(),
                    );
                    body.extend_from_slice(
                        format!("Content-Type: {}\r\n\r\n", sniff_image_content_type(data))
                            .as_bytes(),
                    );
                    body.extend_from_slice(data);
                }
            }
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={boundary}"), body)
    }
}

/// 百分号编码（RFC 3986）：仅保留非保留字符 `A-Z a-z 0-9 - _ . ~`，
/// 其余字符（含 `&`、`=`、`#`、空格、`/`、非 ASCII）按 UTF-8 字节转义，
/// 适用于查询参数值与 URL 路径段。
pub(crate) fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 生成足够独特的边界串：时间戳纳秒 + 进程内递增序号。
fn generate_boundary() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("tiangong-{nanos:x}-{seq:x}")
}

/// 转义 Content-Disposition 头中的引号与反斜杠，换行会破坏头部结构，替换为空格。
fn escape_form_value(value: &str) -> String {
    value
        .replace(['\r', '\n'], " ")
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// 按文件实际内容判断图片格式（Files API 以内容而非扩展名判断格式）。
pub(crate) fn sniff_image_content_type(data: &[u8]) -> &'static str {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        "image/gif"
    } else if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}
