use silent::prelude::*;

use super::AuthToken;
use super::SharedAppContext;
use super::types::{ChatRequest, ChatResponse};
use crate::auth::check_auth;
use tiangong_types::MessageContent;

/// POST /api/v1/chat — 发送消息并获取 AI 回复
pub async fn chat(mut req: Request) -> Result<Response> {
    let token = req.get_state::<AuthToken>()?.clone();
    check_auth(&req, token.0.as_deref())?;

    let app = req.get_state::<SharedAppContext>()?.clone();
    let body: ChatRequest = req.json_parse().await?;

    // 未指定会话时新建会话。
    let session_id = body
        .session_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| scru128::new().to_string());

    let text = body.message.trim().to_string();
    if text.is_empty() {
        return Ok(Response::json(&ChatResponse {
            session_id,
            response: "处理完成".to_string(),
        }));
    }
    let (_, outgoing) = app
        .core_backend
        .send_message_and_wait(
            &session_id,
            text,
            Some(scru128::new().to_string()),
            Vec::new(),
            Default::default(),
        )
        .await
        .map_err(|e| {
            SilentError::business_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("发送消息失败：{e}"),
            )
        })?;

    let response_text = match outgoing.content {
        MessageContent::Text(text) => text,
        _ => String::new(),
    };

    Ok(Response::json(&ChatResponse {
        session_id,
        response: response_text,
    }))
}
