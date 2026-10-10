use std::sync::Arc;

use async_channel::Sender as UnboundedSender;
use async_lock::RwLock;
use serde::Deserialize;
use silent::prelude::*;
use silent::ws::{WSHandlerAppend, WebSocketHandler, WebSocketParts};

use super::SharedAppContext;
use crate::api::AuthToken;
use crate::auth::{
    RemoteAccessContext, check_ws_auth, extract_remote_access_from_ws, resolve_visible_session_id,
};
use crate::remote::event::{EventBus, TiangongEvent};
use tiangong_core::session::now_text;
use tiangong_types::{IncomingMessage, MessageContent};

/// 共享的 EventBus 类型（注入到 State 中）
#[derive(Clone)]
pub struct SharedEventBus(pub Arc<EventBus>);

/// 构建 WebSocket 路由: GET /api/v1/ws
pub fn ws_route() -> Route {
    let handler = WebSocketHandler::new()
        .on_connect(on_connect)
        .on_send(on_send)
        .on_receive(on_receive)
        .on_close(on_close);

    Route::new("ws").ws(None, handler)
}

/// WebSocket 连接建立时：订阅 EventBus 并持续推送事件到客户端
async fn on_connect(
    parts: Arc<RwLock<WebSocketParts>>,
    sender: UnboundedSender<Message>,
) -> Result<()> {
    let (event_bus, app, token, mut access) = {
        let parts_read = parts.read().await;
        let token = parts_read
            .extensions()
            .get::<AuthToken>()
            .cloned()
            .map(|token| token.0)
            .unwrap_or(None);
        check_ws_auth(&parts_read, token.as_deref())?;
        let access = extract_remote_access_from_ws(&parts_read)?;
        let event_bus = parts_read
            .extensions()
            .get::<SharedEventBus>()
            .cloned()
            .map(|eb| eb.0);
        let app = parts_read.extensions().get::<SharedAppContext>().cloned();
        (event_bus, app, token, access)
    };

    let Some(event_bus) = event_bus else {
        tracing::warn!("WebSocket 连接未找到 EventBus，无法推送事件");
        return Ok(());
    };
    let _ = token;

    if !access.role.can_manage_sessions()
        && access.session_scope.is_none()
        && let Some(app) = &app
    {
        let state = app.state.lock().await;
        access.session_scope = Some(state.active_session_id.as_str().to_string());
    }

    {
        let mut parts_write = parts.write().await;
        parts_write.extensions_mut().insert(access.clone());
    }

    let mut receiver = event_bus.subscribe();
    let sender_clone = sender.clone();

    // 在后台任务中持续推送事件
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if !event_visible_to(&event, &access) {
                        continue;
                    }
                    let json = event_to_json(&event);
                    if sender_clone.send(Message::text(json)).await.is_err() {
                        // 客户端已断开
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("WebSocket 事件推送落后 {n} 条消息");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    tracing::info!("WebSocket 客户端已连接");
    Ok(())
}

/// 发送消息前的回调（直接透传）
async fn on_send(msg: Message, _parts: Arc<RwLock<WebSocketParts>>) -> Result<Message> {
    Ok(msg)
}

/// 收到客户端消息时：通过 MessageRouter 处理
async fn on_receive(msg: Message, parts: Arc<RwLock<WebSocketParts>>) -> Result<()> {
    let text = match msg.to_str() {
        Ok(t) => t.to_string(),
        Err(_) => {
            tracing::warn!("收到非文本 WebSocket 消息，忽略");
            return Ok(());
        }
    };

    let parts_read = parts.read().await;
    let app = parts_read.extensions().get::<SharedAppContext>().cloned();
    let access = parts_read
        .extensions()
        .get::<RemoteAccessContext>()
        .cloned();
    drop(parts_read);

    if let (Some(app), Some(access)) = (app, access) {
        let request = parse_ws_request(&text);
        tokio::spawn(async move {
            let mut session_id = {
                let state = app.state.lock().await;
                match resolve_visible_session_id(
                    &access,
                    state.active_session_id.as_str(),
                    request.session_id.as_deref(),
                ) {
                    Ok(session_id) => session_id,
                    Err(err) => {
                        tracing::warn!("WebSocket 消息权限校验失败: {err}");
                        return;
                    }
                }
            };
            if session_id.trim().is_empty() {
                session_id = scru128::new().to_string();
            }

            let incoming = IncomingMessage {
                id: scru128::new().to_string(),
                connector: "server-ws".to_string(),
                channel_id: session_id,
                sender_id: "ws-client".to_string(),
                sender_role: access.role,
                content: MessageContent::Text(request.message),
                media: Vec::new(),
                reply_to: None,
                timestamp: now_text(),
                annotations: Default::default(),
            };

            if let Err(e) = app.router.handle_incoming(incoming).await {
                tracing::error!("WebSocket 消息处理失败: {e}");
            }
        });
    } else {
        tracing::warn!("WebSocket 未找到 ServerAppContext，无法处理消息");
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
struct WsIncomingRequest {
    message: String,
    #[serde(default)]
    session_id: Option<String>,
}

fn parse_ws_request(raw: &str) -> WsIncomingRequest {
    serde_json::from_str::<WsIncomingRequest>(raw).unwrap_or_else(|_| WsIncomingRequest {
        message: raw.to_string(),
        session_id: None,
    })
}

/// WebSocket 连接关闭
async fn on_close(_parts: Arc<RwLock<WebSocketParts>>) {
    tracing::info!("WebSocket 客户端已断开");
}

/// 将 TiangongEvent 转为 JSON 字符串
fn event_to_json(event: &TiangongEvent) -> String {
    match event {
        TiangongEvent::MessageReceived(msg) => serde_json::json!({
            "type": "message_received",
            "data": {
                "id": msg.id,
                "connector": msg.connector,
                "channel_id": msg.channel_id,
                "sender_id": msg.sender_id,
            }
        })
        .to_string(),
        TiangongEvent::MessageSent {
            session_id,
            message,
        } => {
            let content_text = match &message.content {
                MessageContent::Text(t) => t.clone(),
                _ => "[非文本内容]".to_string(),
            };
            serde_json::json!({
                "type": "message_sent",
                "data": {
                    "session_id": session_id,
                    "content": content_text,
                    "reply_to": message.reply_to,
                }
            })
            .to_string()
        }
        TiangongEvent::SessionCreated(id) => serde_json::json!({
            "type": "session_created",
            "data": { "session_id": id }
        })
        .to_string(),
        TiangongEvent::TurnCompleted {
            session_id,
            success,
        } => serde_json::json!({
            "type": "turn_completed",
            "data": {
                "session_id": session_id,
                "success": success,
            }
        })
        .to_string(),
        TiangongEvent::ConfigChanged => serde_json::json!({ "type": "config_changed" }).to_string(),
        TiangongEvent::Shutdown => serde_json::json!({ "type": "shutdown" }).to_string(),
    }
}

fn event_visible_to(event: &TiangongEvent, access: &RemoteAccessContext) -> bool {
    if !access.role.can_observe() {
        return false;
    }
    if access.role.can_manage_sessions() {
        return true;
    }

    let Some(session_scope) = access.session_scope.as_deref() else {
        return false;
    };

    match event {
        TiangongEvent::MessageReceived(msg) => msg.channel_id == session_scope,
        TiangongEvent::MessageSent { session_id, .. } => session_id == session_scope,
        TiangongEvent::SessionCreated(session_id) => session_id == session_scope,
        TiangongEvent::TurnCompleted { session_id, .. } => session_id == session_scope,
        TiangongEvent::ConfigChanged | TiangongEvent::Shutdown => true,
    }
}
