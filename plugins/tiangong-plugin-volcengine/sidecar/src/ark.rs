//! 火山方舟（Ark）HTTP 调用：直接组织请求，不经 llm / media crate 转接。
//!
//! - 图片生成：`POST {base}/images/generations`（Seedream），同步返回图片 URL；
//! - 视频生成：`POST {base}/contents/generations/tasks`（Seedance）提交任务，
//!   `GET {base}/contents/generations/tasks/{id}` 轮询到终态或超时。
//!
//! 参考：<https://www.volcengine.com/docs/82379/1541523>（图片生成）、
//! <https://www.volcengine.com/docs/82379/1520757>（创建视频生成任务）。

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tiangong_plugin_volcengine_protocol::VideoStatus;

/// 单次 HTTP 请求超时（生图通常数十秒）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
/// 视频任务轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// 已解析的 Ark 端点。
pub struct Endpoint {
    pub base_url: String,
    pub api_key: String,
}

impl Endpoint {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim().trim_end_matches('/'))
    }
}

pub(crate) fn http_client() -> Result<reqwest::Client> {
    // 与其他 sidecar 一致：沙箱内不依赖系统凭据服务，使用 Mozilla 公共根证书校验。
    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|cert| reqwest::Certificate::from_der(cert.as_ref()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("加载 HTTPS 根证书失败")?;
    reqwest::Client::builder()
        .tls_backend_rustls()
        .tls_certs_only(roots)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("tiangong-volcengine/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("构造 HTTP 客户端失败")
}

async fn send(request: reqwest::RequestBuilder, label: &str) -> Result<Value> {
    let response = request
        .send()
        .await
        .with_context(|| format!("请求火山方舟{label}接口失败"))?;
    let status = response.status();
    let text = response.text().await.context("读取响应体失败")?;
    if !status.is_success() {
        let message = error_message(&text).unwrap_or_else(|| preview(&text));
        bail!("火山方舟{label}调用失败 ({status})：{message}");
    }
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("解析火山方舟{label}响应失败：{}", preview(&text)))?;
    // 部分错误以 200 + error 字段返回。
    if let Some(message) = value.get("error").and_then(error_from_value) {
        bail!("火山方舟{label}返回错误：{message}");
    }
    Ok(value)
}

/// 从错误响应体中提取可读信息。
fn error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    value.get("error").and_then(error_from_value).or_else(|| {
        value
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn error_from_value(error: &Value) -> Option<String> {
    match error {
        Value::Null => None,
        Value::String(text) if text.trim().is_empty() => None,
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => {
            let message = map.get("message").and_then(Value::as_str).unwrap_or("");
            let code = map.get("code").and_then(Value::as_str).unwrap_or("");
            match (code.is_empty(), message.is_empty()) {
                (true, true) => None,
                (true, false) => Some(message.to_string()),
                (false, true) => Some(code.to_string()),
                (false, false) => Some(format!("{code}: {message}")),
            }
        }
        other => Some(other.to_string()),
    }
}

pub(crate) fn preview(text: &str) -> String {
    let mut excerpt: String = text.chars().take(500).collect();
    if text.chars().count() > 500 {
        excerpt.push('…');
    }
    excerpt
}

// ── 图片生成 ──

/// 组装 Seedream 图片生成请求体。
pub fn image_body(
    model: &str,
    prompt: &str,
    size: Option<&str>,
    images: &[String],
    watermark: bool,
) -> Value {
    let mut body = json!({
        "model": model,
        "prompt": prompt,
        "response_format": "url",
        "watermark": watermark,
    });
    if let Some(size) = size.map(str::trim).filter(|size| !size.is_empty()) {
        body["size"] = json!(size);
    }
    match images {
        [] => {}
        [single] => body["image"] = json!(single),
        many => body["image"] = json!(many),
    }
    body
}

/// 提交图片生成，返回图片 URL（或 base64 data URL）列表。
pub async fn generate_image(endpoint: &Endpoint, body: Value) -> Result<Vec<String>> {
    let request = http_client()?
        .post(endpoint.url("/images/generations"))
        .bearer_auth(&endpoint.api_key)
        .json(&body);
    let value = send(request, "图片生成").await?;
    let images = extract_images(&value);
    if images.is_empty() {
        bail!(
            "火山方舟图片生成未返回图片：{}",
            preview(&value.to_string())
        );
    }
    Ok(images)
}

/// 从 `data[]` 中提取图片引用：优先 `url`，其次 `b64_json`。
pub fn extract_images(value: &Value) -> Vec<String> {
    value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            if let Some(url) = item.get("url").and_then(Value::as_str)
                && !url.trim().is_empty()
            {
                return Some(url.to_string());
            }
            item.get("b64_json")
                .and_then(Value::as_str)
                .filter(|data| !data.trim().is_empty())
                .map(|data| format!("data:image/jpeg;base64,{data}"))
        })
        .collect()
}

// ── 视频生成 ──

/// 视频任务可选参数。
pub struct VideoOptions<'a> {
    pub duration: Option<u32>,
    pub resolution: Option<&'a str>,
    pub ratio: Option<&'a str>,
    pub image: Option<&'a str>,
    pub watermark: bool,
}

/// 组装 Seedance 视频任务请求体。
pub fn video_body(model: &str, prompt: &str, options: &VideoOptions<'_>) -> Value {
    let mut content = vec![json!({ "type": "text", "text": prompt })];
    if let Some(image) = options.image.filter(|image| !image.trim().is_empty()) {
        content.push(json!({
            "type": "image_url",
            "image_url": { "url": image },
            "role": "first_frame",
        }));
    }
    let mut body = json!({
        "model": model,
        "content": content,
        "watermark": options.watermark,
    });
    if let Some(duration) = options.duration {
        body["duration"] = json!(duration);
    }
    if let Some(resolution) = options.resolution.map(str::trim).filter(|v| !v.is_empty()) {
        body["resolution"] = json!(resolution);
    }
    if let Some(ratio) = options.ratio.map(str::trim).filter(|v| !v.is_empty()) {
        body["ratio"] = json!(ratio);
    }
    body
}

/// 提交视频任务，返回任务 ID。
pub async fn create_video_task(endpoint: &Endpoint, body: Value) -> Result<String> {
    let request = http_client()?
        .post(endpoint.url("/contents/generations/tasks"))
        .bearer_auth(&endpoint.api_key)
        .json(&body);
    let value = send(request, "视频生成").await?;
    value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!(
                "火山方舟视频任务响应缺少 id：{}",
                preview(&value.to_string())
            )
        })
}

/// 查询一次视频任务状态。
pub async fn query_video_task(endpoint: &Endpoint, task_id: &str) -> Result<VideoStatus> {
    let request = http_client()?
        .get(endpoint.url(&format!("/contents/generations/tasks/{task_id}")))
        .bearer_auth(&endpoint.api_key);
    let value = send(request, "视频任务查询").await?;
    parse_video_status(&value)
}

/// 轮询视频任务直到终态或超时；超时返回 `Running`（任务仍在服务端继续）。
pub async fn wait_video_task(
    endpoint: &Endpoint,
    task_id: &str,
    timeout: Duration,
) -> Result<VideoStatus> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let status = query_video_task(endpoint, task_id).await?;
        if !matches!(status, VideoStatus::Running { .. })
            || tokio::time::Instant::now() + POLL_INTERVAL > deadline
        {
            return Ok(status);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 解析任务状态：queued / running → Running；succeeded → Succeeded；
/// failed / cancelled / expired → Failed。
pub fn parse_video_status(value: &Value) -> Result<VideoStatus> {
    let status = value.get("status").and_then(Value::as_str).ok_or_else(|| {
        anyhow!(
            "火山方舟视频任务响应缺少 status：{}",
            preview(&value.to_string())
        )
    })?;
    Ok(match status {
        "succeeded" => {
            let video_url = value
                .pointer("/content/video_url")
                .and_then(Value::as_str)
                .filter(|url| !url.trim().is_empty())
                .ok_or_else(|| anyhow!("视频任务已完成但响应缺少 content.video_url"))?;
            VideoStatus::Succeeded {
                video_url: video_url.to_string(),
                duration: value.get("duration").and_then(Value::as_f64),
            }
        }
        "failed" | "cancelled" | "expired" => VideoStatus::Failed {
            error: value
                .get("error")
                .and_then(error_from_value)
                .unwrap_or_else(|| format!("任务状态：{status}")),
        },
        other => VideoStatus::Running {
            status: other.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_body_single_and_multi_reference() {
        let body = image_body("seedream", "cat", Some("2K"), &[], false);
        assert_eq!(body["model"], "seedream");
        assert_eq!(body["size"], "2K");
        assert_eq!(body["response_format"], "url");
        assert_eq!(body["watermark"], false);
        assert!(body.get("image").is_none());

        let one = image_body("m", "p", None, &["a".to_string()], true);
        assert_eq!(one["image"], "a");
        assert!(one.get("size").is_none());

        let two = image_body(
            "m",
            "p",
            Some(" "),
            &["a".to_string(), "b".to_string()],
            true,
        );
        assert_eq!(two["image"], json!(["a", "b"]));
        assert!(two.get("size").is_none());
    }

    #[test]
    fn extract_images_prefers_url_then_base64() {
        let value = json!({
            "data": [
                { "url": "https://img/1.jpeg", "size": "2048x2048" },
                { "b64_json": "QUJD" },
                { "url": "" }
            ]
        });
        assert_eq!(
            extract_images(&value),
            vec![
                "https://img/1.jpeg".to_string(),
                "data:image/jpeg;base64,QUJD".to_string()
            ]
        );
        assert!(extract_images(&json!({})).is_empty());
    }

    #[test]
    fn video_body_carries_options_and_first_frame() {
        let options = VideoOptions {
            duration: Some(5),
            resolution: Some("720p"),
            ratio: Some("16:9"),
            image: Some("https://img/first.png"),
            watermark: false,
        };
        let body = video_body("seedance", "a cat", &options);
        assert_eq!(body["model"], "seedance");
        assert_eq!(body["duration"], 5);
        assert_eq!(body["resolution"], "720p");
        assert_eq!(body["ratio"], "16:9");
        assert_eq!(
            body["content"][0],
            json!({ "type": "text", "text": "a cat" })
        );
        assert_eq!(body["content"][1]["role"], "first_frame");
        assert_eq!(
            body["content"][1]["image_url"]["url"],
            "https://img/first.png"
        );

        let bare = video_body(
            "m",
            "p",
            &VideoOptions {
                duration: None,
                resolution: None,
                ratio: None,
                image: None,
                watermark: true,
            },
        );
        assert_eq!(bare["content"].as_array().unwrap().len(), 1);
        assert!(bare.get("duration").is_none());
        assert_eq!(bare["watermark"], true);
    }

    #[test]
    fn parse_video_status_covers_terminal_states() {
        let succeeded = json!({
            "id": "cgt-1",
            "status": "succeeded",
            "content": { "video_url": "https://v/1.mp4" },
            "duration": 5
        });
        assert_eq!(
            parse_video_status(&succeeded).unwrap(),
            VideoStatus::Succeeded {
                video_url: "https://v/1.mp4".to_string(),
                duration: Some(5.0)
            }
        );

        let failed = json!({
            "status": "failed",
            "error": { "code": "InputTextSensitiveContentDetected", "message": "blocked" }
        });
        assert_eq!(
            parse_video_status(&failed).unwrap(),
            VideoStatus::Failed {
                error: "InputTextSensitiveContentDetected: blocked".to_string()
            }
        );

        assert_eq!(
            parse_video_status(&json!({ "status": "queued" })).unwrap(),
            VideoStatus::Running {
                status: "queued".to_string()
            }
        );
        assert!(parse_video_status(&json!({ "status": "succeeded" })).is_err());
        assert!(parse_video_status(&json!({})).is_err());
    }

    #[test]
    fn error_message_reads_ark_error_shape() {
        assert_eq!(
            error_message(r#"{"error":{"code":"AuthenticationError","message":"bad key"}}"#),
            Some("AuthenticationError: bad key".to_string())
        );
        assert_eq!(
            error_message(r#"{"message":"oops"}"#),
            Some("oops".to_string())
        );
        assert_eq!(error_message("not json"), None);
    }

    /// 本地 HTTP 桩：按请求路径返回固定响应，验证请求路径、鉴权头与响应解析。
    async fn stub_server(
        responses: Vec<(&'static str, u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (expected_path, status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0u8; 16 * 1024];
                let read = socket.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let request_line = request.lines().next().unwrap_or_default().to_string();
                assert!(
                    request_line.contains(expected_path),
                    "请求路径不符：{request_line}"
                );
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer test-key"),
                    "缺少鉴权头"
                );
                seen.push(request);
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            seen
        });
        (format!("http://{address}/api/v3/"), handle)
    }

    #[tokio::test]
    async fn image_and_video_round_trip_against_stub() {
        let (base_url, server) = stub_server(vec![
            (
                "POST /api/v3/images/generations",
                200,
                r#"{"model":"seedream","data":[{"url":"https://img/1.jpeg"}]}"#,
            ),
            (
                "POST /api/v3/contents/generations/tasks",
                200,
                r#"{"id":"cgt-42"}"#,
            ),
            (
                "GET /api/v3/contents/generations/tasks/cgt-42",
                200,
                r#"{"id":"cgt-42","status":"succeeded","content":{"video_url":"https://v/42.mp4"}}"#,
            ),
            (
                "POST /api/v3/images/generations",
                401,
                r#"{"error":{"code":"AuthenticationError","message":"bad key"}}"#,
            ),
        ])
        .await;
        let endpoint = Endpoint {
            base_url,
            api_key: "test-key".to_string(),
        };

        let images = generate_image(&endpoint, image_body("seedream", "cat", None, &[], false))
            .await
            .unwrap();
        assert_eq!(images, vec!["https://img/1.jpeg".to_string()]);

        let options = VideoOptions {
            duration: Some(5),
            resolution: None,
            ratio: None,
            image: None,
            watermark: false,
        };
        let task_id = create_video_task(&endpoint, video_body("seedance", "cat", &options))
            .await
            .unwrap();
        assert_eq!(task_id, "cgt-42");
        let status = wait_video_task(&endpoint, &task_id, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(
            status,
            VideoStatus::Succeeded {
                video_url: "https://v/42.mp4".to_string(),
                duration: None
            }
        );

        let error = generate_image(&endpoint, image_body("seedream", "cat", None, &[], false))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("401") && error.contains("bad key"),
            "{error}"
        );

        let requests = server.await.unwrap();
        assert!(requests[0].contains(r#""prompt":"cat""#));
        assert!(requests[1].contains(r#""duration":5"#));
    }
}
