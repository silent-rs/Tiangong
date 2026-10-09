//! 飞书主动推送：文本卡片、图片与文件。

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use super::connection::{BotState, get_token, post_message, refresh_token};
use super::target_store::{self, AuthorizedTarget};
use crate::delivery::{
    self, DeliveryClaim, DeliveryResult, MAX_TEXT_CHARS, SendFailure, finish_delivery, rejected,
};
use crate::local_media::{LocalMedia, read_local_media};
use crate::tools::{SendLocalMediaInput, SendTextInput};

#[derive(Clone, Copy)]
enum UploadKind {
    Image,
    File { file_type: &'static str },
}

enum UploadAttemptError {
    TokenExpired,
    Failure(SendFailure),
}

fn resolve_target(data_dir: &Path, target_id: &str) -> Result<Option<AuthorizedTarget>> {
    target_store::find_authorized(data_dir, target_id)
}

pub(crate) async fn send_text_message(
    state: &Arc<BotState>,
    input: &SendTextInput,
) -> Result<DeliveryResult> {
    let text = input.text.trim();
    if text.is_empty() || text.chars().count() > MAX_TEXT_CHARS {
        return Ok(rejected("消息不能为空且不能超过 4000 个字符"));
    }
    let Some(target) = resolve_target(&state.data_dir, input.target_id.trim())? else {
        return Ok(rejected("推送目标不存在或尚未授权"));
    };
    let (path, record) = match delivery::claim_delivery(
        &state.data_dir,
        &target.target_id,
        input.idempotency_key.trim(),
    )? {
        DeliveryClaim::Existing(result) => return Ok(result),
        DeliveryClaim::New { path, delivery } => (path, delivery),
    };
    let card_content = serde_json::json!({
        "config": { "wide_screen_mode": true },
        "elements": [{ "tag": "markdown", "content": text }]
    })
    .to_string();
    let payload = serde_json::json!({
        "receive_id": target.chat_id,
        "msg_type": "interactive",
        "content": card_content,
    });
    let outcome = send_platform_payload(state, payload).await;
    Ok(finish_delivery(
        &path,
        record,
        outcome,
        "飞书已受理文本消息",
    ))
}

pub(crate) async fn send_media_message(
    state: &Arc<BotState>,
    input: &SendLocalMediaInput,
    workspace: Option<&Path>,
    image: bool,
) -> Result<DeliveryResult> {
    let Some(target) = resolve_target(&state.data_dir, input.target_id.trim())? else {
        return Ok(rejected("推送目标不存在或尚未授权"));
    };
    let media = match read_local_media(input.file_path.trim(), workspace, image) {
        Ok(media) => media,
        Err(message) => return Ok(rejected(&message)),
    };
    let (path, record) = match delivery::claim_delivery(
        &state.data_dir,
        &target.target_id,
        input.idempotency_key.trim(),
    )? {
        DeliveryClaim::Existing(result) => return Ok(result),
        DeliveryClaim::New { path, delivery } => (path, delivery),
    };
    let (kind, message_type, key_name, accepted) = if image {
        (
            UploadKind::Image,
            "image",
            "image_key",
            "飞书已受理图片消息",
        )
    } else {
        (
            UploadKind::File {
                file_type: feishu_file_type(&media.file_name),
            },
            "file",
            "file_key",
            "飞书已受理文件消息",
        )
    };
    let outcome = match upload_platform_media(state, &media, kind).await {
        Ok(key) => {
            let mut content = serde_json::Map::new();
            content.insert(key_name.to_string(), serde_json::Value::String(key));
            let payload = serde_json::json!({
                "receive_id": target.chat_id,
                "msg_type": message_type,
                "content": serde_json::Value::Object(content).to_string(),
            });
            send_platform_payload(state, payload).await
        }
        Err(error) => Err(error),
    };
    Ok(finish_delivery(&path, record, outcome, accepted))
}

fn feishu_file_type(file_name: &str) -> &'static str {
    let extension = Path::new(file_name)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("opus") => "opus",
        Some("mp4") => "mp4",
        Some("pdf") => "pdf",
        Some("doc" | "docx") => "doc",
        Some("xls" | "xlsx") => "xls",
        Some("ppt" | "pptx") => "ppt",
        _ => "stream",
    }
}

async fn upload_platform_media(
    state: &Arc<BotState>,
    media: &LocalMedia,
    kind: UploadKind,
) -> std::result::Result<String, SendFailure> {
    let token = get_token(state)
        .await
        .map_err(|error| SendFailure::Unknown(format!("获取飞书令牌失败：{error}")))?;
    match upload_platform_media_request(state, &token, media, kind).await {
        Ok(key) => Ok(key),
        Err(UploadAttemptError::Failure(error)) => Err(error),
        Err(UploadAttemptError::TokenExpired) => {
            let token = refresh_token(state)
                .await
                .map_err(|error| SendFailure::Unknown(format!("刷新飞书令牌失败：{error}")))?;
            match upload_platform_media_request(state, &token, media, kind).await {
                Ok(key) => Ok(key),
                Err(UploadAttemptError::Failure(error)) => Err(error),
                Err(UploadAttemptError::TokenExpired) => Err(SendFailure::Rejected(
                    "飞书身份令牌刷新后仍然无效".to_string(),
                )),
            }
        }
    }
}

async fn upload_platform_media_request(
    state: &Arc<BotState>,
    token: &str,
    media: &LocalMedia,
    kind: UploadKind,
) -> std::result::Result<String, UploadAttemptError> {
    let label = match kind {
        UploadKind::Image => "图片",
        UploadKind::File { .. } => "文件",
    };
    let part = reqwest::multipart::Part::bytes(media.bytes.clone())
        .file_name(media.file_name.clone())
        .mime_str(&media.mime_type)
        .map_err(|error| {
            UploadAttemptError::Failure(SendFailure::Rejected(format!(
                "构造飞书{label}上传请求失败：{error}"
            )))
        })?;
    let (url, form, key_field) = match kind {
        UploadKind::Image => (
            "https://open.feishu.cn/open-apis/im/v1/images",
            reqwest::multipart::Form::new()
                .text("image_type", "message")
                .part("image", part),
            "image_key",
        ),
        UploadKind::File { file_type } => (
            "https://open.feishu.cn/open-apis/im/v1/files",
            reqwest::multipart::Form::new()
                .text("file_type", file_type)
                .text("file_name", media.file_name.clone())
                .part("file", part),
            "file_key",
        ),
    };
    let response = state
        .http
        .post(url)
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .map_err(|error| {
            UploadAttemptError::Failure(SendFailure::Unknown(format!(
                "飞书{label}上传结果未知：{error}"
            )))
        })?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.map_err(|error| {
        let failure = if status.is_client_error() {
            SendFailure::Rejected(format!("飞书拒绝{label}上传：HTTP {status}"))
        } else {
            SendFailure::Unknown(format!("解析飞书{label}上传响应失败：{error}"))
        };
        UploadAttemptError::Failure(failure)
    })?;
    let code = body["code"].as_i64().unwrap_or(-1);
    if code == 99991663 || code == 99991668 {
        return Err(UploadAttemptError::TokenExpired);
    }
    if code != 0 {
        let message = body["msg"].as_str().unwrap_or("未知错误");
        return Err(UploadAttemptError::Failure(SendFailure::Rejected(format!(
            "飞书拒绝{label}上传：code={code}, message={message}"
        ))));
    }
    body["data"][key_field]
        .as_str()
        .map(ToString::to_string)
        .ok_or_else(|| {
            UploadAttemptError::Failure(SendFailure::Unknown(format!(
                "飞书{label}上传成功但响应缺少结果标识"
            )))
        })
}

async fn send_platform_payload(
    state: &Arc<BotState>,
    payload: serde_json::Value,
) -> std::result::Result<Option<String>, SendFailure> {
    let body = post_message(state, payload)
        .await
        .map_err(|error| SendFailure::Unknown(format!("飞书请求结果未知：{error}")))?;
    let code = body["code"].as_i64().unwrap_or(-1);
    if code != 0 {
        let message = body["msg"].as_str().unwrap_or("未知错误");
        return Err(SendFailure::Rejected(format!(
            "飞书拒绝消息：code={code}, message={message}"
        )));
    }
    Ok(body["data"]["message_id"]
        .as_str()
        .or_else(|| body["data"]["message"]["message_id"].as_str())
        .map(ToString::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_types_follow_feishu_categories() {
        assert_eq!(feishu_file_type("a.PDF"), "pdf");
        assert_eq!(feishu_file_type("a.docx"), "doc");
        assert_eq!(feishu_file_type("a.bin"), "stream");
    }
}
