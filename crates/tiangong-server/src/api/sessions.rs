use silent::prelude::*;

use super::AuthToken;
use super::SharedAppContext;
use super::types::{MessageSummary, SessionSummary};
use crate::auth::check_auth;

/// GET /api/v1/sessions — 会话列表
pub async fn list_sessions(req: Request) -> Result<Response> {
    let token = req.get_state::<AuthToken>()?.clone();
    check_auth(&req, token.0.as_deref())?;
    let app_ctx = req.get_state::<SharedAppContext>()?.clone();
    let app = app_ctx.state.lock().await;

    let sessions: Vec<SessionSummary> = app
        .core_manager
        .list_session_metadata()
        .iter()
        .map(|metadata| SessionSummary {
            id: metadata.id.clone(),
            title: metadata.title.clone(),
            message_count: metadata.message_count,
            created_at: metadata.created_at.clone(),
            updated_at: metadata.updated_at.clone(),
        })
        .collect();

    Ok(Response::json(&serde_json::json!({
        "total": sessions.len(),
        "items": sessions,
    })))
}

/// GET /api/v1/sessions/:id — 会话详情（消息列表）
pub async fn get_session(req: Request) -> Result<Response> {
    let token = req.get_state::<AuthToken>()?.clone();
    check_auth(&req, token.0.as_deref())?;

    let id: String = req.get_path_params("id")?;
    let app_ctx = req.get_state::<SharedAppContext>()?.clone();
    let (core_manager, session_exists) = {
        let app = app_ctx.state.lock().await;
        (
            app.core_manager.clone(),
            app.core_manager.session_exists(&id),
        )
    };
    if !session_exists {
        return Err(SilentError::business_error(
            StatusCode::NOT_FOUND,
            format!("会话 '{id}' 不存在"),
        ));
    }
    // 消息内容需完整 Session；从磁盘 load（issue #245：真相源归磁盘）。
    let session = core_manager.load_session(&id).map_err(|error| {
        SilentError::business_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("加载会话失败：{error}"),
        )
    })?;

    let messages: Vec<MessageSummary> = session
        .messages
        .iter()
        .map(|m| MessageSummary {
            id: m.id.clone(),
            role: format!("{:?}", m.role).to_lowercase(),
            content: m.text_content(),
            created_at: m.created_at.clone(),
        })
        .collect();

    Ok(Response::json(&serde_json::json!({
        "id": session.id,
        "title": session.title,
        "messages": messages,
        "created_at": session.created_at,
        "updated_at": session.updated_at,
    })))
}

/// DELETE /api/v1/sessions/:id — 删除会话
pub async fn delete_session(req: Request) -> Result<Response> {
    let token = req.get_state::<AuthToken>()?.clone();
    check_auth(&req, token.0.as_deref())?;

    let id: String = req.get_path_params("id")?;
    let app_ctx = req.get_state::<SharedAppContext>()?.clone();
    let deleted = app_ctx
        .core_backend
        .delete_session(&id)
        .await
        .map_err(|error| {
            SilentError::business_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("删除会话失败：{error}"),
            )
        })?;
    if !deleted {
        return Err(SilentError::business_error(
            StatusCode::NOT_FOUND,
            format!("会话 '{id}' 不存在"),
        ));
    }

    Ok(Response::json(&serde_json::json!({
        "status": "deleted",
        "id": id,
    })))
}

/// POST /api/v1/sessions/:id/cancel — 取消会话当前执行中的轮次
///
/// 快速停止：向活跃 turn 投递取消信号，Core 立即终止；该会话全部等待中的
/// 投递调用立即收到「已取消/执行已取消」错误返回（不排队）。返回 true 表示
/// 取消命令已投递到活跃执行，false 表示本次没有可接受取消命令的活跃执行；
/// false 不表示会话资源不存在。
pub async fn cancel_session_turn(req: Request) -> Result<Response> {
    let token = req.get_state::<AuthToken>()?.clone();
    check_auth(&req, token.0.as_deref())?;

    let id: String = req.get_path_params("id")?;
    let app_ctx = req.get_state::<SharedAppContext>()?.clone();
    let cancelled = app_ctx
        .core_backend
        .cancel_session_turn(&id)
        .await
        .map_err(|error| {
            SilentError::business_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("取消会话执行失败：{error}"),
            )
        })?;

    Ok(Response::json(&serde_json::json!({
        "cancelled": cancelled,
        "id": id,
    })))
}
