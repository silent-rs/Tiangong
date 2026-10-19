use silent::prelude::*;

use super::types::{ApiMessageContent, ConnectorMessageRequest, ConnectorMessageResponse};
use super::{AuthToken, SharedAppContext};
use crate::auth::check_auth;
use tiangong_types::{MediaAsset, MediaKind, MessageAnnotations, OutgoingMessage};

use super::types::ConnectorMessage;

/// POST /api/v1/messages — 外部 Bot / Connector 统一消息入口
///
/// 默认同步等待整轮 turn 完成后返回 AI 回复。
/// 请求带 `Prefer: respond-async` 头时改为异步：立即返回 202 Accepted，整轮 turn
/// 在后台执行。scheduler 等只需确认「消息已被接收」的 fire-and-forget 调用方使用此模式，
/// 避免长任务因 HTTP 超时被误判为失败。
pub async fn post_message(mut req: Request) -> Result<Response> {
    let token = req.get_state::<AuthToken>()?.clone();
    check_auth(&req, token.0.as_deref())?;

    let respond_async = req
        .headers()
        .get("prefer")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("respond-async"));

    let app = req.get_state::<SharedAppContext>()?.clone();
    let body: ConnectorMessageRequest = req.json_parse().await?;
    let connector = body
        .connector
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("external-bot")
        .to_string();
    let channel_id = body.channel_id.trim().to_string();
    if channel_id.is_empty() {
        return Err(SilentError::business_error(
            StatusCode::BAD_REQUEST,
            "channel_id 不能为空".to_string(),
        ));
    }

    #[allow(deprecated)]
    let (text, media) = resolve_body(body.message, body.content, body.media)?;
    // 入口即校验：异步模式下后台失败只能写日志，调用方应当场拿到 400。
    let annotations = MessageAnnotations {
        instruction: body.instruction,
        render: body.render,
        workspace: body.workspace,
    }
    .normalized()
    .map_err(|error| SilentError::business_error(StatusCode::BAD_REQUEST, error))?;
    // 指定工作区须为已存在的工作区（全局工作目录或现有会话目录），入口即
    // 拒绝：异步模式下后台失败只能写日志；且绝不据此新建目录。
    {
        let state = app.state.lock().await;
        crate::remote::core::resolve_known_workspace(&state, annotations.workspace.as_deref())
            .map_err(|error| SilentError::business_error(StatusCode::BAD_REQUEST, error))?;
    }
    let message_id = body
        .message_id
        .filter(|id| !id.trim().is_empty())
        .map(|id| id.trim().to_string())
        .unwrap_or_else(|| scru128::new().to_string());
    let message = ConnectorMessage {
        connector: connector.clone(),
        channel_id: channel_id.clone(),
        message_id,
        text,
        media,
        annotations,
    };

    if respond_async {
        // 异步模式：立即返回 202，整轮 turn 在后台执行。
        // session_id 此时未知（由 Core 后端解析），返回请求的 channel_id 作为占位，
        // 调用方（如 scheduler）已通过 channel_id 语义绑定了目标会话。
        tokio::spawn(async move {
            if let Err(error) = deliver(&app, message).await {
                tracing::error!(%error, "异步处理外部消息失败");
            }
        });
        return Ok(Response::json(&serde_json::json!({
            "session_id": channel_id,
            "connector": connector,
            "channel_id": channel_id,
            "status": "accepted",
        }))
        .with_status(StatusCode::ACCEPTED));
    }

    let reply_to = Some(message.message_id.clone());
    let (session_id, outgoing) = deliver(&app, message).await.map_err(|e| {
        SilentError::business_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("处理外部消息失败：{e}"),
        )
    })?;

    let content = ApiMessageContent::from(outgoing.content);
    let attachments = outgoing
        .attachments
        .into_iter()
        .map(ApiMessageContent::from)
        .collect();
    let message = content.text();
    Ok(Response::json(&ConnectorMessageResponse {
        session_id,
        connector,
        channel_id,
        reply_to,
        message,
        content,
        attachments,
    }))
}

/// 把消息交给 Core 后端：`server-api` 按 `channel_id` 直投会话，其余按
/// connector + channel 解析（或新建）会话。
async fn deliver(
    app: &SharedAppContext,
    message: ConnectorMessage,
) -> anyhow::Result<(String, OutgoingMessage)> {
    let ConnectorMessage {
        connector,
        channel_id,
        message_id,
        text,
        media,
        annotations,
    } = message;
    let (session_id, outgoing) = if connector == "server-api" {
        app.core_backend
            .send_message_and_wait(&channel_id, text, Some(message_id), media, annotations)
            .await?
    } else {
        app.core_backend
            .send_connector_message_and_wait(
                &connector,
                &channel_id,
                text,
                Some(message_id),
                media,
                annotations,
            )
            .await?
    };
    Ok((session_id, outgoing))
}

/// 把请求体正文统一为「文本 + 附件」。
///
/// 已废弃的 `content` 在入口转换：图片/视频的 caption 仅在 `message` 为空时
/// 作为正文，附件本身排在 `media` 之前；附件 `title` 只取真实文件名。
fn resolve_body(
    message: Option<String>,
    content: Option<ApiMessageContent>,
    media: Vec<MediaAsset>,
) -> Result<(String, Vec<MediaAsset>)> {
    let mut text = message.unwrap_or_default().trim().to_string();
    let mut assets = Vec::with_capacity(media.len() + 1);
    if let Some(content) = content {
        let (caption, asset) = legacy_content_parts(content);
        if text.is_empty() {
            text = caption
                .map(|caption| caption.trim().to_string())
                .unwrap_or_default();
        }
        assets.extend(asset);
    }
    assets.extend(media);
    if text.is_empty() && assets.is_empty() {
        return Err(SilentError::business_error(
            StatusCode::BAD_REQUEST,
            "message 与 media 不能同时为空".to_string(),
        ));
    }
    Ok((text, assets))
}

/// 旧 `content` → （可作正文的文字, 附件）。
fn legacy_content_parts(content: ApiMessageContent) -> (Option<String>, Option<MediaAsset>) {
    let asset = |kind, url, title| MediaAsset {
        kind,
        url,
        mime_type: None,
        title,
        capability: None,
    };
    match content {
        ApiMessageContent::Text { text } => (Some(text), None),
        ApiMessageContent::Image { url, caption } => {
            (caption, Some(asset(MediaKind::Image, url, None)))
        }
        ApiMessageContent::Video { url, caption } => {
            (caption, Some(asset(MediaKind::Video, url, None)))
        }
        ApiMessageContent::Audio { url, .. } => (None, Some(asset(MediaKind::Audio, url, None))),
        ApiMessageContent::File { url, name } => {
            (None, Some(asset(MediaKind::File, url, Some(name))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(url: &str) -> MediaAsset {
        MediaAsset {
            kind: MediaKind::Image,
            url: url.to_string(),
            mime_type: None,
            title: None,
            capability: Some("multimodal".to_string()),
        }
    }

    fn urls(media: &[MediaAsset]) -> Vec<&str> {
        media.iter().map(|asset| asset.url.as_str()).collect()
    }

    #[test]
    fn message_and_media_pass_through() {
        let (text, media) = resolve_body(
            Some("  看看这两张  ".into()),
            None,
            vec![image("first"), image("second")],
        )
        .unwrap();
        assert_eq!(text, "看看这两张");
        assert_eq!(urls(&media), ["first", "second"]);
    }

    #[test]
    fn media_only_message_has_empty_text() {
        let (text, media) = resolve_body(None, None, vec![image("only")]).unwrap();
        assert_eq!(text, "");
        assert_eq!(urls(&media), ["only"]);
    }

    #[test]
    fn empty_message_without_media_is_rejected() {
        assert!(resolve_body(Some("  ".into()), None, Vec::new()).is_err());
        assert!(resolve_body(None, None, Vec::new()).is_err());
    }

    #[test]
    fn legacy_text_content_becomes_text() {
        let (text, media) = resolve_body(
            None,
            Some(ApiMessageContent::Text {
                text: "你好".into(),
            }),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(text, "你好");
        assert!(media.is_empty());
    }

    #[test]
    fn legacy_image_content_goes_before_media_without_placeholder() {
        let (text, media) = resolve_body(
            None,
            Some(ApiMessageContent::Image {
                url: "first".into(),
                caption: None,
            }),
            vec![image("second"), image("third")],
        )
        .unwrap();
        assert_eq!(text, "");
        assert_eq!(urls(&media), ["first", "second", "third"]);
        assert_eq!(media[0].kind, MediaKind::Image);
        assert!(media[0].title.is_none());
    }

    #[test]
    fn legacy_caption_is_text_not_title() {
        let (text, media) = resolve_body(
            None,
            Some(ApiMessageContent::Image {
                url: "img".into(),
                caption: Some("看看这个".into()),
            }),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(text, "看看这个");
        assert!(media[0].title.is_none());
    }

    #[test]
    fn legacy_content_keeps_media_when_message_is_present() {
        let (text, media) = resolve_body(
            Some("正文".into()),
            Some(ApiMessageContent::Image {
                url: "img".into(),
                caption: Some("说明".into()),
            }),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(text, "正文");
        assert_eq!(urls(&media), ["img"]);
    }

    #[test]
    fn legacy_file_uses_its_name_as_title() {
        let (_, media) = resolve_body(
            None,
            Some(ApiMessageContent::File {
                url: "file".into(),
                name: "a.pdf".into(),
            }),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(media[0].kind, MediaKind::File);
        assert_eq!(media[0].title.as_deref(), Some("a.pdf"));
    }

    #[test]
    fn legacy_request_json_is_still_accepted() {
        let body: ConnectorMessageRequest = serde_json::from_value(serde_json::json!({
            "channel_id": "chat",
            "content": { "type": "image", "url": "data:image/png;base64,AA==" },
            "media": [{ "kind": "image", "url": "second" }]
        }))
        .unwrap();
        #[allow(deprecated)]
        let (text, media) = resolve_body(body.message, body.content, body.media).unwrap();
        assert_eq!(text, "");
        assert_eq!(urls(&media), ["data:image/png;base64,AA==", "second"]);
    }
}
