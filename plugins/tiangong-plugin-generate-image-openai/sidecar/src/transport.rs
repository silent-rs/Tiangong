//! 生图请求发送：按协议组装请求并返回统一的「Responses 形态」响应 JSON。
//!
//! - Responses：`POST {base}/responses`，非流式，直接返回响应体；
//! - Codex（ChatGPT 账号）：`POST https://chatgpt.com/backend-api/codex/responses`，
//!   后端强制 `stream=true`、`store=false`，这里消费 SSE 并把
//!   `response.output_item.done` 聚合回 `{ "output": [...] }`；
//! - Chat Completions：`POST {base}/chat/completions`，从 `choices[].message`
//!   中提取图片（`images[]` / 多模态 content / markdown data URL），转换为
//!   `image_generation_call` 输出项，复用同一套提取逻辑。

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tiangong_plugin_generate_image_openai_protocol::ImageApiProtocol;

/// 生图超时：Codex 后端生成一张图通常需要数十秒。
const IMAGE_TIMEOUT: Duration = Duration::from_secs(300);

/// 发送请求时需要的端点信息。
pub struct Target<'a> {
    pub protocol: ImageApiProtocol,
    pub base_url: &'a str,
    pub api_key: &'a str,
    /// Codex：ChatGPT 账号 ID。
    pub account_id: Option<&'a str>,
    /// Codex：数据驻留区域。
    pub residency: Option<&'a str>,
}

fn http_client() -> Result<reqwest::Client> {
    // reqwest 0.13 的默认 Rustls 验证器仍依赖系统凭据服务；沙箱内使用
    // Mozilla 公共根证书完成证书链和域名校验，无需开放 Keychain 权限。
    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|cert| reqwest::Certificate::from_der(cert.as_ref()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("加载 HTTPS 根证书失败")?;
    reqwest::Client::builder()
        .tls_backend_rustls()
        .tls_certs_only(roots)
        .timeout(IMAGE_TIMEOUT)
        .user_agent(concat!(
            "tiangong-generate-image/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .context("构造 HTTP 客户端失败")
}

/// 拼接接口地址：兼容 base_url 末尾已带具体路径的写法。
pub fn endpoint_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with(path) {
        base.to_string()
    } else {
        format!("{base}{path}")
    }
}

/// 按协议发送请求，返回 Responses 形态的 JSON（含 `output[]`）。
pub async fn send(target: &Target<'_>, payload: Value) -> Result<Value> {
    match target.protocol {
        ImageApiProtocol::Responses => {
            let url = endpoint_url(target.base_url, "/responses");
            let request = http_client()?
                .post(&url)
                .bearer_auth(target.api_key)
                .json(&payload);
            let text = send_checked(request, "Responses API").await?;
            serde_json::from_str::<Value>(&text)
                .with_context(|| format!("解析 Responses API 响应失败：{}", preview(&text)))
        }
        ImageApiProtocol::Codex => {
            let url = endpoint_url(target.base_url, "/responses");
            let mut request = http_client()?
                .post(&url)
                .bearer_auth(target.api_key)
                .header("originator", "tiangong")
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .json(&payload);
            if let Some(account_id) = target.account_id {
                request = request.header("ChatGPT-Account-Id", account_id);
            }
            if let Some(residency) = target.residency {
                request = request.header("x-openai-internal-codex-residency", residency);
            }
            let text = send_checked(request, "ChatGPT 生图").await?;
            aggregate_sse(&text)
        }
        ImageApiProtocol::ChatCompletions => {
            let url = endpoint_url(target.base_url, "/chat/completions");
            let request = http_client()?
                .post(&url)
                .bearer_auth(target.api_key)
                .json(&payload);
            let text = send_checked(request, "Chat Completions").await?;
            let value = serde_json::from_str::<Value>(&text)
                .with_context(|| format!("解析 Chat Completions 响应失败：{}", preview(&text)))?;
            Ok(chat_to_responses_shape(&value))
        }
    }
}

async fn send_checked(request: reqwest::RequestBuilder, label: &str) -> Result<String> {
    let response = request
        .send()
        .await
        .with_context(|| format!("请求 {label} 失败"))?;
    let status = response.status();
    let text = response.text().await.context("读取响应体失败")?;
    if !status.is_success() {
        let message = extract_error_message(&text).unwrap_or_else(|| preview(&text));
        if status == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!("{label} 鉴权失败 ({status}): {message}");
        }
        anyhow::bail!("{label} 调用失败 ({status}): {message}");
    }
    Ok(text)
}

fn preview(text: &str) -> String {
    let mut excerpt: String = text.chars().take(500).collect();
    if text.chars().count() > 500 {
        excerpt.push('…');
    }
    excerpt
}

/// 从错误响应里提取可读的 message 字段。
pub fn extract_error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .or_else(|| value.get("detail"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 把 Responses SSE 聚合为完整响应：收集 `response.output_item.done` 的 item，
/// 并保留 `response.completed` / `response.failed` 中的元信息（model、error 等）。
pub fn aggregate_sse(body: &str) -> Result<Value> {
    let mut items = Vec::new();
    let mut response = serde_json::Map::new();
    let mut stream_error: Option<String> = None;
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    items.push(item.clone());
                }
            }
            Some("response.completed" | "response.failed" | "response.incomplete") => {
                if let Some(Value::Object(meta)) = event.get("response") {
                    for (key, value) in meta {
                        if key != "output" {
                            response.insert(key.clone(), value.clone());
                        }
                    }
                    // 后端未发 output_item.done 时回退使用终态中的 output。
                    if items.is_empty()
                        && let Some(Value::Array(output)) = meta.get("output")
                    {
                        items.extend(output.iter().cloned());
                    }
                }
            }
            Some("error") => {
                stream_error = event
                    .pointer("/error/message")
                    .or_else(|| event.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            _ => {}
        }
    }
    if items.is_empty()
        && response.is_empty()
        && let Some(message) = stream_error
    {
        return Err(anyhow!("ChatGPT 生图失败：{message}"));
    }
    response.insert("output".to_string(), Value::Array(items));
    Ok(Value::Object(response))
}

/// 把 Chat Completions 响应转换为 Responses 形态：图片转为 `image_generation_call`，
/// 文本转为 `message`（未出图时作为失败说明）。
pub fn chat_to_responses_shape(value: &Value) -> Value {
    let mut output = Vec::new();
    for choice in value
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(message) = choice.get("message") else {
            continue;
        };
        let mut images = Vec::new();
        let mut texts = Vec::new();
        // OpenRouter 等：message.images[].image_url.url
        for image in message
            .get("images")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(url) = image
                .pointer("/image_url/url")
                .or_else(|| image.get("url"))
                .and_then(Value::as_str)
            {
                images.push(url.to_string());
            }
        }
        match message.get("content") {
            Some(Value::String(text)) => {
                images.extend(markdown_image_urls(text));
                texts.push(text.clone());
            }
            Some(Value::Array(parts)) => {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("image_url") => {
                            if let Some(url) =
                                part.pointer("/image_url/url").and_then(Value::as_str)
                            {
                                images.push(url.to_string());
                            }
                        }
                        Some("text") => {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                images.extend(markdown_image_urls(text));
                                texts.push(text.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        for url in images {
            output.push(image_url_to_call(&url));
        }
        let text = texts.join("\n");
        if !text.trim().is_empty() {
            output.push(json!({
                "type": "message",
                "content": [{ "type": "output_text", "text": text }]
            }));
        }
    }
    let mut shaped = json!({ "output": output });
    if let Some(model) = value.get("model") {
        shaped["model"] = model.clone();
    }
    if let Some(error) = value.get("error") {
        shaped["error"] = error.clone();
    }
    shaped
}

/// data URL 拆为 base64 + 格式；远程 URL 原样保留（归档时再下载）。
fn image_url_to_call(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:image/")
        && let Some((format, b64)) = rest.split_once(";base64,")
    {
        return json!({
            "type": "image_generation_call",
            "status": "completed",
            "output_format": format,
            "result": b64,
        });
    }
    json!({
        "type": "image_generation_call",
        "status": "completed",
        "url": url,
    })
}

/// 提取 markdown `![..](url)` 中的图片地址。
fn markdown_image_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("](") {
        let after = &rest[start + 2..];
        let Some(end) = after.find(')') else {
            break;
        };
        let candidate = after[..end].trim();
        let is_image = rest[..start].rfind("![").is_some();
        if is_image
            && (candidate.starts_with("data:image/")
                || candidate.starts_with("http://")
                || candidate.starts_with("https://"))
        {
            urls.push(candidate.to_string());
        }
        rest = &after[end..];
    }
    urls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_url_handles_suffix() {
        assert_eq!(
            endpoint_url("https://api.openai.com/v1/", "/responses"),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            endpoint_url("https://x.com/v1/chat/completions", "/chat/completions"),
            "https://x.com/v1/chat/completions"
        );
    }

    #[test]
    fn aggregate_sse_collects_output_items() {
        let body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"image_generation_call\",\"status\":\"completed\",\"output_format\":\"png\",\"result\":\"AAAA\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"model\":\"gpt-5.5\",\"output\":[]}}\n\n",
        );
        let value = aggregate_sse(body).unwrap();
        assert_eq!(value["model"], "gpt-5.5");
        let output = value["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[1]["result"], "AAAA");
    }

    #[test]
    fn aggregate_sse_reports_stream_error() {
        let body = "data: {\"type\":\"error\",\"error\":{\"message\":\"quota exceeded\"}}\n\n";
        let error = aggregate_sse(body).unwrap_err().to_string();
        assert!(error.contains("quota exceeded"), "{error}");
    }

    #[test]
    fn chat_shape_extracts_images_from_all_forms() {
        let value = json!({
            "model": "img-model",
            "choices": [{
                "message": {
                    "content": "好的 ![图](data:image/jpeg;base64,BBBB)",
                    "images": [{ "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }]
                }
            }, {
                "message": {
                    "content": [
                        { "type": "image_url", "image_url": { "url": "https://cdn.example.com/a.png" } },
                        { "type": "text", "text": "说明" }
                    ]
                }
            }]
        });
        let shaped = chat_to_responses_shape(&value);
        let output = shaped["output"].as_array().unwrap();
        let calls: Vec<&Value> = output
            .iter()
            .filter(|item| item["type"] == "image_generation_call")
            .collect();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0]["result"], "AAAA");
        assert_eq!(calls[1]["output_format"], "jpeg");
        assert_eq!(calls[2]["url"], "https://cdn.example.com/a.png");
        assert_eq!(shaped["model"], "img-model");
    }

    #[test]
    fn chat_shape_keeps_text_when_no_image() {
        let value = json!({ "choices": [{ "message": { "content": "我无法生成图片" } }] });
        let shaped = chat_to_responses_shape(&value);
        assert_eq!(shaped["output"][0]["type"], "message");
    }
}
