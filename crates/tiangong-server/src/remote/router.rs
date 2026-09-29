use std::sync::Arc;

use anyhow::Result;
use tiangong_app_state::app_state::TiangongState;
use tiangong_core::session::MessageRole;
use tiangong_types::event::{EventSource, RuntimeEvent, RuntimeEventType};
use tiangong_types::{IncomingMessage, MediaAsset, MediaKind, MessageContent, OutgoingMessage};
use tokio::sync::Mutex;

use super::backend::ServerCoreBackend;
use super::event::{EventBus, TiangongEvent};

pub struct MessageRouter {
    state: Arc<Mutex<TiangongState>>,
    event_bus: Arc<EventBus>,
    core_backend: Arc<dyn ServerCoreBackend>,
}

impl MessageRouter {
    pub fn new(
        state: Arc<Mutex<TiangongState>>,
        event_bus: Arc<EventBus>,
        core_backend: Arc<dyn ServerCoreBackend>,
    ) -> Self {
        Self {
            state,
            event_bus,
            core_backend,
        }
    }

    pub async fn handle_incoming(&self, msg: IncomingMessage) -> Result<OutgoingMessage> {
        let (_, outgoing) = self.handle_incoming_with_session(msg).await?;
        Ok(outgoing)
    }

    pub async fn handle_incoming_with_session(
        &self,
        msg: IncomingMessage,
    ) -> Result<(String, OutgoingMessage)> {
        if !msg.sender_role.can_send_message() {
            let denied_text = format!(
                "权限不足：{}角色不允许发送消息",
                msg.sender_role.display_name()
            );
            return Ok((
                msg.channel_id.clone(),
                OutgoingMessage {
                    content: MessageContent::Text(denied_text),
                    attachments: Vec::new(),
                    reply_to: Some(msg.id.clone()),
                },
            ));
        }

        self.event_bus
            .publish(TiangongEvent::MessageReceived(msg.clone()));

        let text = extract_text(&msg);
        let media = extract_media(&msg);
        let channel_id = if msg.channel_id.trim().is_empty() {
            let state = self.state.lock().await;
            state.active_session_id.as_str().to_string()
        } else {
            msg.channel_id.clone()
        };
        let event = RuntimeEvent::new(
            RuntimeEventType::UserMessage,
            channel_id,
            EventSource::Connector,
            serde_json::json!({
                "connector": msg.connector,
                "sender_id": msg.sender_id,
                "sender_role": msg.sender_role,
                "text": text,
            }),
        );

        let Some(outgoing) = self
            .handle_runtime_event_with_reply(
                event,
                Some(msg.id.clone()),
                Some(msg.id.clone()),
                media,
            )
            .await?
        else {
            return Ok((
                msg.channel_id,
                OutgoingMessage {
                    content: MessageContent::Text("处理完成".to_string()),
                    attachments: Vec::new(),
                    reply_to: Some(msg.id.clone()),
                },
            ));
        };

        Ok(outgoing)
    }

    pub async fn handle_runtime_event(
        &self,
        event: RuntimeEvent,
    ) -> Result<Option<OutgoingMessage>> {
        Ok(self
            .handle_runtime_event_with_reply(event, None, None, Vec::new())
            .await?
            .map(|(_, outgoing)| outgoing))
    }

    pub async fn handle_system_signal(
        &self,
        session_id: impl Into<String>,
        signal: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<Option<OutgoingMessage>> {
        let signal = signal.into();
        let mut envelope = serde_json::Map::new();
        envelope.insert("signal".to_string(), serde_json::Value::String(signal));
        envelope.insert("payload".to_string(), payload);
        let event = RuntimeEvent::new(
            RuntimeEventType::SystemSignal,
            session_id.into(),
            EventSource::System,
            serde_json::Value::Object(envelope),
        );
        self.handle_runtime_event(event).await
    }

    async fn handle_runtime_event_with_reply(
        &self,
        event: RuntimeEvent,
        reply_to: Option<String>,
        message_id: Option<String>,
        media: Vec<MediaAsset>,
    ) -> Result<Option<(String, OutgoingMessage)>> {
        match event.event_type {
            RuntimeEventType::UserMessage => {
                let requested_session_id = event.session_id.clone();
                let connector = event
                    .payload
                    .get("connector")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("server-api");
                let text = event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .unwrap_or_default()
                    .to_string();
                if text.is_empty() && media.is_empty() {
                    return Ok(None);
                }

                let (actual_session_id, mut outgoing) =
                    if matches!(connector, "server-api" | "server-ws") {
                        self.core_backend
                            .send_message_and_wait(&requested_session_id, text, message_id, media)
                            .await?
                    } else {
                        self.core_backend
                            .send_connector_message_and_wait(
                                connector,
                                &requested_session_id,
                                text,
                                message_id,
                                media,
                            )
                            .await?
                    };
                outgoing.reply_to = reply_to;
                self.event_bus.publish(TiangongEvent::MessageSent {
                    session_id: actual_session_id.clone(),
                    message: outgoing.clone(),
                });
                Ok(Some((actual_session_id, outgoing)))
            }
            RuntimeEventType::TaskCompleted
            | RuntimeEventType::TaskFailed
            | RuntimeEventType::Notification
            | RuntimeEventType::SystemSignal => {
                let Some(summary) = summarize_runtime_event(&event) else {
                    return Ok(None);
                };

                {
                    let state = self.state.lock().await;
                    // session 真相源归磁盘（issue #245）：load → 追加 System 消息 → 落盘。
                    let mut session = state
                        .core_manager
                        .load_session(&event.session_id)
                        .map_err(|error| anyhow::anyhow!("加载会话失败：{error}"))?;
                    session.append_message(MessageRole::System, summary.clone());
                    session
                        .try_persist_to_disk()
                        .map_err(|error| anyhow::anyhow!("持久化会话失败：{error}"))?;
                }

                let outgoing = OutgoingMessage {
                    content: MessageContent::Text(summary),
                    attachments: Vec::new(),
                    reply_to,
                };
                let session_id = event.session_id.clone();
                self.event_bus.publish(TiangongEvent::MessageSent {
                    session_id: session_id.clone(),
                    message: outgoing.clone(),
                });
                Ok(Some((session_id, outgoing)))
            }
            _ => Ok(None),
        }
    }
}

fn summarize_runtime_event(event: &RuntimeEvent) -> Option<String> {
    match event.event_type {
        RuntimeEventType::TaskCompleted => Some(
            event
                .payload
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "后台任务已完成".to_string()),
        ),
        RuntimeEventType::TaskFailed => Some(
            event
                .payload
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "后台任务执行失败".to_string()),
        ),
        RuntimeEventType::Notification => Some(
            event
                .payload
                .get("summary")
                .or_else(|| event.payload.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "收到系统通知".to_string()),
        ),
        RuntimeEventType::SystemSignal => Some(
            event
                .payload
                .get("signal")
                .and_then(serde_json::Value::as_str)
                .map(|signal| format!("系统信号：{signal}"))
                .unwrap_or_else(|| "收到系统信号".to_string()),
        ),
        _ => None,
    }
}

/// 提取消息文本。
///
/// 语音消息不在此处转写：音频随 `extract_media` 作为附件进入归档流水线，
/// 语音识别插件可用时由模型调用 `speech_to_text` 处理。
fn extract_text(msg: &IncomingMessage) -> String {
    match &msg.content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Audio { .. } => "[语音消息]".to_string(),
        MessageContent::Image { caption, .. } => {
            caption.clone().unwrap_or_else(|| "[图片消息]".to_string())
        }
        MessageContent::Video { caption, .. } => {
            caption.clone().unwrap_or_else(|| "[视频消息]".to_string())
        }
        MessageContent::File { name, .. } => format!("[文件: {name}]"),
    }
}

fn extract_media(msg: &IncomingMessage) -> Vec<MediaAsset> {
    let mut media = content_to_media(&msg.content);
    media.extend(msg.media.clone());
    media
}

fn content_to_media(content: &MessageContent) -> Vec<MediaAsset> {
    match content {
        MessageContent::Image { url, caption } => vec![MediaAsset {
            kind: MediaKind::Image,
            url: url.clone(),
            mime_type: None,
            title: caption.clone(),
            capability: None,
        }],
        MessageContent::Video { url, caption } => vec![MediaAsset {
            kind: MediaKind::Video,
            url: url.clone(),
            mime_type: None,
            title: caption.clone(),
            capability: None,
        }],
        MessageContent::Audio { url, .. } => vec![MediaAsset {
            kind: MediaKind::Audio,
            url: url.clone(),
            mime_type: None,
            title: None,
            capability: None,
        }],
        MessageContent::File { url, name } => vec![MediaAsset {
            kind: MediaKind::File,
            url: url.clone(),
            mime_type: None,
            title: Some(name.clone()),
            capability: None,
        }],
        MessageContent::Text(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_types::RemoteRole;

    fn image(url: &str) -> MediaAsset {
        MediaAsset {
            kind: MediaKind::Image,
            url: url.to_string(),
            mime_type: None,
            title: None,
            capability: Some("multimodal".to_string()),
        }
    }

    #[test]
    fn extract_media_keeps_content_image_before_additional_media() {
        let message = IncomingMessage {
            id: "message".to_string(),
            connector: "feishu-bot".to_string(),
            channel_id: "chat".to_string(),
            sender_id: "sender".to_string(),
            sender_role: RemoteRole::Controller,
            content: MessageContent::Image {
                url: "first".to_string(),
                caption: None,
            },
            media: vec![image("second"), image("third")],
            reply_to: None,
            timestamp: "now".to_string(),
        };

        let media = extract_media(&message);
        assert_eq!(media.len(), 3);
        assert_eq!(media[0].url, "first");
        assert_eq!(media[1].url, "second");
        assert_eq!(media[2].url, "third");
    }

    #[test]
    fn extract_media_does_not_duplicate_text_message_media() {
        let message = IncomingMessage {
            id: "message".to_string(),
            connector: "feishu-bot".to_string(),
            channel_id: "chat".to_string(),
            sender_id: "sender".to_string(),
            sender_role: RemoteRole::Controller,
            content: MessageContent::Text("正文".to_string()),
            media: vec![image("first"), image("second")],
            reply_to: None,
            timestamp: "now".to_string(),
        };

        let media = extract_media(&message);
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].url, "first");
        assert_eq!(media[1].url, "second");
    }
}
