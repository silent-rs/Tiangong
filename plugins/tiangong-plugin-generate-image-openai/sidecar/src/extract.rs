//! 从 Responses API 响应中提取图片。
//!
//! OpenAI Responses API 的生图结果在 `output[]` 数组里，
//! 元素 `type == "image_generation_call"` 的 `result` 字段是 base64 编码的图片。
//! 参考：https://developers.openai.com/api/docs/guides/tools-image-generation

use anyhow::{Result, anyhow};
use serde_json::Value;

/// 从 Responses API 响应 JSON 中提取所有图片引用。
///
/// 返回的每个元素是一个可以直接交给 `archive_image_reference` 的原始引用：
/// 形如 `data:image/png;base64,...`。
pub fn extract_images(payload: &Value) -> Result<Vec<String>> {
    let output = payload.get("output").and_then(Value::as_array);

    let mut images = Vec::new();
    for item in output.into_iter().flatten() {
        if item.get("type").and_then(Value::as_str) != Some("image_generation_call") {
            continue;
        }
        let result = item
            .get("result")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let Some(b64) = result else {
            // Chat Completions 兼容接口可能返回远程图片地址。
            if let Some(url) = item
                .get("url")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                images.push(url.to_string());
            }
            continue;
        };
        let format = item
            .get("output_format")
            .and_then(Value::as_str)
            .unwrap_or("png");
        images.push(format!("data:image/{format};base64,{b64}"));
    }

    if images.is_empty() {
        return Err(anyhow!(image_failure_message(payload)));
    }

    Ok(images)
}

/// 只展示上游的错误及面向用户的回复，不把推理内容或完整响应放入错误。
fn image_failure_message(payload: &Value) -> String {
    let mut details = Vec::new();
    if let Some(message) = payload.pointer("/error/message").and_then(Value::as_str) {
        details.push(message);
    }
    for item in payload
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("image_generation_call") => {
                if let Some(message) = item.pointer("/error/message").and_then(Value::as_str) {
                    details.push(message);
                }
            }
            Some("message") => {
                for content in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let field = match content.get("type").and_then(Value::as_str) {
                        Some("refusal") => "refusal",
                        Some("output_text") => "text",
                        _ => continue,
                    };
                    if let Some(text) = content.get(field).and_then(Value::as_str) {
                        details.push(text);
                    }
                }
            }
            _ => {}
        }
    }
    let explanation = details
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if !explanation.is_empty() {
        let mut excerpt: String = explanation.chars().take(2000).collect();
        if explanation.chars().count() > 2000 {
            excerpt.push('…');
        }
        return format!("上游服务未生成图片，返回说明：{excerpt}");
    }
    if let Some(reason) = payload
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
    {
        return format!("上游生图响应未完成：{reason}");
    }
    "上游服务未返回图片，也未提供失败原因，请检查所选模型及服务是否支持生图".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_from_image_generation_call() {
        let payload = json!({
            "output": [{
                "type": "image_generation_call",
                "status": "completed",
                "output_format": "png",
                "result": "iVBORw0KGgo="
            }]
        });
        let images = extract_images(&payload).unwrap();
        assert_eq!(images.len(), 1);
        assert!(images[0].starts_with("data:image/png;base64,iVBORw0KGgo="));
    }

    #[test]
    fn extract_multiple_images() {
        let payload = json!({
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "这是图"}]},
                {"type": "image_generation_call", "status": "completed", "result": "AAAA", "output_format": "png"},
                {"type": "image_generation_call", "status": "completed", "result": "BBBB", "output_format": "webp"}
            ]
        });
        let images = extract_images(&payload).unwrap();
        assert_eq!(images.len(), 2);
        assert!(images[0].contains("AAAA"));
        assert!(images[1].contains("image/webp"));
    }

    #[test]
    fn skip_empty_result() {
        let payload = json!({
            "output": [{
                "type": "image_generation_call",
                "status": "generating",
                "result": ""
            }]
        });
        assert!(extract_images(&payload).is_err());
    }

    #[test]
    fn no_output_returns_error() {
        let payload = json!({"output": [{"type": "message"}]});
        assert!(extract_images(&payload).is_err());
    }
}
