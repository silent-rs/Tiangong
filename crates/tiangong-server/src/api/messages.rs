use silent::prelude::*;

use super::types::{ApiMessageContent, ConnectorMessageRequest, ConnectorMessageResponse};
use super::{AuthToken, SharedAppContext};
use crate::auth::{check_auth, ensure_remote_action, extract_remote_access};
use tiangong_core::session::now_text;
use tiangong_types::{IncomingMessage, MessageAnnotations};

/// POST /api/v1/messages — 外部 Bot / Connector 统一消息入口
///
/// 默认同步等待整轮 turn 完成后返回 AI 回复。
/// 请求带 `Prefer: respond-async` 头时改为异步：立即返回 202 Accepted，整轮 turn
/// 在后台执行。scheduler 等只需确认「消息已被接收」的 fire-and-forget 调用方使用此模式，
/// 避免长任务因 HTTP 超时被误判为失败。
pub async fn post_message(mut req: Request) -> Result<Response> {
    let token = req.get_state::<AuthToken>()?.clone();
    check_auth(&req, token.0.as_deref())?;
    let access = extract_remote_access(&req)?;
    ensure_remote_action(&access, access.role.can_send_message(), "发送消息")?;

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

    let content = resolve_content(body.message, body.content)?;
    // 入口即校验：异步模式下后台失败只能写日志，调用方应当场拿到 400。
    let annotations = MessageAnnotations {
        instruction: body.instruction,
        render: body.render,
    }
    .normalized()
    .map_err(|error| SilentError::business_error(StatusCode::BAD_REQUEST, error))?;
    let media = body.media;
    let message_id = body
        .message_id
        .filter(|id| !id.trim().is_empty())
        .map(|id| id.trim().to_string())
        .unwrap_or_else(|| scru128::new().to_string());
    let sender_id = body
        .sender_id
        .filter(|id| !id.trim().is_empty())
        .map(|id| id.trim().to_string())
        .unwrap_or_else(|| "external-user".to_string());
    let incoming = IncomingMessage {
        id: message_id,
        connector: connector.clone(),
        channel_id: channel_id.clone(),
        sender_id,
        sender_role: access.role,
        content: content.into(),
        media,
        reply_to: body.reply_to,
        timestamp: now_text(),
        annotations,
    };

    if respond_async {
        // 异步模式：立即返回 202，整轮 turn 在后台执行。
        // session_id 此时未知（由 router 解析），返回请求的 channel_id 作为占位，
        // 调用方（如 scheduler）已通过 channel_id 语义绑定了目标会话。
        let router = app.router.clone();
        tokio::spawn(async move {
            if let Err(error) = router.handle_incoming_with_session(incoming).await {
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

    let (session_id, outgoing) = app
        .router
        .handle_incoming_with_session(incoming)
        .await
        .map_err(|e| {
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
        reply_to: outgoing.reply_to,
        message,
        content,
        attachments,
    }))
}

fn resolve_content(
    message: Option<String>,
    content: Option<ApiMessageContent>,
) -> Result<ApiMessageContent> {
    match (message, content) {
        (Some(message), _) if !message.trim().is_empty() => Ok(ApiMessageContent::Text {
            text: message.trim().to_string(),
        }),
        (_, Some(content)) => Ok(content),
        _ => Err(SilentError::business_error(
            StatusCode::BAD_REQUEST,
            "message 或 content 必须提供一个".to_string(),
        )),
    }
}
