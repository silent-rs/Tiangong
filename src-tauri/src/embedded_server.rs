use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tauri::{AppHandle, Manager};
use tiangong_core::permission::TrustMode;
use tiangong_core::session::MessageRole;
use tiangong_server::remote::backend::{CoreBackendKind, ServerCoreBackend};
use tiangong_server::remote::core::resolve_or_create_connector_session;
use tiangong_server::remote::event::{EventBus, TiangongEvent};
use tiangong_types::{MediaAsset, OutgoingMessage, TurnStatus};
use tokio::sync::{mpsc, oneshot};

use crate::app::TiangongApp;

type HostResult<T> = std::result::Result<T, String>;
type MessageReply = HostResult<(String, OutgoingMessage)>;

struct RemoteTurnLease<'a> {
    state: &'a TiangongApp,
    session_id: String,
    message_id: String,
}

impl Drop for RemoteTurnLease<'_> {
    fn drop(&mut self) {
        self.state
            .finish_remote_turn(&self.session_id, &self.message_id);
    }
}

enum EmbeddedCoreRequest {
    SendConnector {
        connector: String,
        channel_id: String,
        content: String,
        message_id: Option<String>,
        media: Vec<MediaAsset>,
        reply: oneshot::Sender<MessageReply>,
    },
    SendSession {
        session_id: String,
        content: String,
        message_id: Option<String>,
        media: Vec<MediaAsset>,
        reply: oneshot::Sender<MessageReply>,
    },
    DeleteSession {
        session_id: String,
        reply: oneshot::Sender<HostResult<bool>>,
    },
    /// 取消会话当前执行中的轮次（快速停止：等待方立即收到取消错误）。
    CancelTurn {
        session_id: String,
        reply: oneshot::Sender<HostResult<bool>>,
    },
    SyncConfig {
        reply: oneshot::Sender<HostResult<()>>,
    },
}

/// 内嵌 HTTP 线程只持有请求通道，不持有 Core 或会话写入器。
pub(crate) struct DesktopServerCoreBridge {
    request_tx: mpsc::UnboundedSender<EmbeddedCoreRequest>,
}

impl DesktopServerCoreBridge {
    async fn request_message(
        &self,
        request: impl FnOnce(oneshot::Sender<MessageReply>) -> EmbeddedCoreRequest,
    ) -> Result<(String, OutgoingMessage)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(request(reply_tx))
            .map_err(|_| anyhow!("Desktop 内嵌 Server 桥接已关闭"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("Desktop 内嵌 Server 未返回处理结果"))?
            .map_err(anyhow::Error::msg)
    }
}

#[async_trait]
impl ServerCoreBackend for DesktopServerCoreBridge {
    fn kind(&self) -> CoreBackendKind {
        CoreBackendKind::EmbeddedHost
    }

    async fn send_connector_message_and_wait(
        &self,
        connector: &str,
        channel_id: &str,
        content: String,
        message_id: Option<String>,
        media: Vec<MediaAsset>,
    ) -> Result<(String, OutgoingMessage)> {
        self.request_message(|reply| EmbeddedCoreRequest::SendConnector {
            connector: connector.to_string(),
            channel_id: channel_id.to_string(),
            content,
            message_id,
            media,
            reply,
        })
        .await
    }

    async fn send_message_and_wait(
        &self,
        session_id: &str,
        content: String,
        message_id: Option<String>,
        media: Vec<MediaAsset>,
    ) -> Result<(String, OutgoingMessage)> {
        self.request_message(|reply| EmbeddedCoreRequest::SendSession {
            session_id: session_id.to_string(),
            content,
            message_id,
            media,
            reply,
        })
        .await
    }

    async fn delete_session(&self, session_id: &str) -> Result<bool> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(EmbeddedCoreRequest::DeleteSession {
                session_id: session_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("Desktop 内嵌 Server 桥接已关闭"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("Desktop 内嵌 Server 未返回删除结果"))?
            .map_err(anyhow::Error::msg)
    }

    async fn cancel_session_turn(&self, session_id: &str) -> Result<bool> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(EmbeddedCoreRequest::CancelTurn {
                session_id: session_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("Desktop 内嵌 Server 桥接已关闭"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("Desktop 内嵌 Server 未返回取消结果"))?
            .map_err(anyhow::Error::msg)
    }

    async fn sync_config_from_state(&self) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(EmbeddedCoreRequest::SyncConfig { reply: reply_tx })
            .map_err(|_| anyhow!("Desktop 内嵌 Server 桥接已关闭"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("Desktop 内嵌 Server 未返回配置同步结果"))?
            .map_err(anyhow::Error::msg)
    }
}

pub(crate) fn spawn_desktop_server_core_bridge(
    app: AppHandle,
    event_bus: Arc<EventBus>,
) -> Arc<DesktopServerCoreBridge> {
    let (request_tx, mut request_rx) = mpsc::unbounded_channel();
    tauri::async_runtime::spawn(async move {
        while let Some(request) = request_rx.recv().await {
            let request_app = app.clone();
            let request_event_bus = event_bus.clone();
            tauri::async_runtime::spawn(async move {
                match request {
                    EmbeddedCoreRequest::SendConnector {
                        connector,
                        channel_id,
                        content,
                        message_id,
                        media,
                        reply,
                    } => {
                        let state = request_app.state::<TiangongApp>();
                        let result =
                            match resolve_connector_session(state.inner(), &connector, &channel_id)
                                .await
                            {
                                Ok((session_id, creates_session)) => {
                                    let completion_session_id = session_id.clone();
                                    if creates_session {
                                        request_event_bus.publish(TiangongEvent::SessionCreated(
                                            completion_session_id.clone(),
                                        ));
                                    }
                                    let result = send_message_and_wait(
                                        request_app.clone(),
                                        state.inner(),
                                        session_id,
                                        content,
                                        message_id,
                                        media,
                                    )
                                    .await;
                                    request_event_bus.publish(TiangongEvent::TurnCompleted {
                                        session_id: completion_session_id,
                                        success: result.is_ok(),
                                    });
                                    result
                                }
                                Err(error) => Err(error),
                            };
                        let _ = reply.send(result);
                    }
                    EmbeddedCoreRequest::SendSession {
                        session_id,
                        content,
                        message_id,
                        media,
                        reply,
                    } => {
                        let state = request_app.state::<TiangongApp>();
                        let completion_session_id = session_id.clone();
                        let result = send_message_and_wait(
                            request_app.clone(),
                            state.inner(),
                            session_id,
                            content,
                            message_id,
                            media,
                        )
                        .await;
                        request_event_bus.publish(TiangongEvent::TurnCompleted {
                            session_id: completion_session_id,
                            success: result.is_ok(),
                        });
                        let _ = reply.send(result);
                    }
                    EmbeddedCoreRequest::DeleteSession { session_id, reply } => {
                        let state = request_app.state::<TiangongApp>();
                        let result = delete_session(&request_app, state.inner(), &session_id).await;
                        let _ = reply.send(result);
                    }
                    EmbeddedCoreRequest::CancelTurn { session_id, reply } => {
                        let state = request_app.state::<TiangongApp>();
                        // 只投取消信号：轮次终态由原投递路径的等待器链处理
                        // （等待方收到「执行已取消」错误并释放远端轮次）。
                        let cancelled = state
                            .with_state_read(|core_state| {
                                Ok::<_, anyhow::Error>(
                                    core_state.core_manager.cancel_core(&session_id),
                                )
                            })
                            .await
                            .map_err(|error| error.to_string());
                        let _ = reply.send(cancelled);
                    }
                    EmbeddedCoreRequest::SyncConfig { reply } => {
                        let state = request_app.state::<TiangongApp>();
                        let result = state.sync_core_config_from_state().await;
                        if result.is_ok() {
                            request_event_bus.publish(TiangongEvent::ConfigChanged);
                        }
                        let _ = reply.send(result);
                    }
                }
            });
        }
    });
    Arc::new(DesktopServerCoreBridge { request_tx })
}

async fn resolve_connector_session(
    state: &TiangongApp,
    connector: &str,
    channel_id: &str,
) -> HostResult<(String, bool)> {
    let channel_id = channel_id.trim();
    if channel_id.is_empty() {
        return state
            .with_state_read(|core_state| {
                Ok((core_state.active_session_id.as_str().to_string(), false))
            })
            .await;
    }

    state
        .with_state(|core_state| {
            let resolved = resolve_or_create_connector_session(
                &core_state.core_manager,
                connector,
                channel_id,
            )?;
            if resolved.1 && core_state.active_session_id.trim().is_empty() {
                core_state.active_session_id = resolved.0.clone();
            }
            Ok(resolved)
        })
        .await
}

async fn send_message_and_wait(
    app: AppHandle,
    state: &TiangongApp,
    session_id: String,
    content: String,
    message_id: Option<String>,
    media: Vec<MediaAsset>,
) -> MessageReply {
    use std::sync::mpsc as std_mpsc;
    use tiangong_types::StreamEvent;

    let session_id = session_id.trim().to_string();
    if session_id.is_empty() {
        return Err("目标会话 ID 不能为空".to_string());
    }
    let message_id = message_id
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| scru128::new().to_string());

    state.wait_for_remote_turn_release(&session_id).await;
    let send_guard = loop {
        let session_lock = state.session_send_lock(&session_id);
        let guard = session_lock.lock_owned().await;
        if state.remote_turn_owner(&session_id).is_none() {
            break guard;
        }
        drop(guard);
        state.wait_for_remote_turn_release(&session_id).await;
    };

    // 会话存在性用 metadata 判定；消息字段需完整 Session，从磁盘 load
    //（issue #245：真相源归磁盘）。
    let session_exists = state
        .with_state_read(|core_state| {
            Ok::<_, anyhow::Error>(
                core_state
                    .core_manager
                    .list_session_metadata()
                    .iter()
                    .any(|m| m.id == session_id),
            )
        })
        .await?;
    let existing = if session_exists {
        let session = state.core_manager.load_session(&session_id)?;
        session
            .messages
            .iter()
            .find(|message| message.id == message_id)
            .map(|message| {
                (
                    message.role,
                    session
                        .turn_status_of(&message_id)
                        .filter(|status| status.is_terminal()),
                )
            })
    } else {
        None
    };
    if let Some((role, turn_status)) = existing {
        if role != MessageRole::User {
            drop(send_guard);
            return Err("消息 ID 已被非用户消息占用".to_string());
        }
        if let Some(status) = turn_status {
            let result = completed_message_result(state, &session_id, &message_id, status)
                .await
                .map(|outgoing| (session_id, outgoing));
            drop(send_guard);
            return result;
        }
        let has_pending_turn = state
            .with_state_read(|core_state| {
                Ok(crate::state_ops::has_pending_turn(core_state, &session_id))
            })
            .await?;
        drop(send_guard);
        return Err(incomplete_existing_message_error(has_pending_turn));
    }

    let has_pending_turn = state
        .with_state_read(|core_state| {
            Ok(crate::state_ops::has_pending_turn(core_state, &session_id))
        })
        .await?;
    if has_pending_turn {
        drop(send_guard);
        return Err("目标会话已有执行中的轮次，请等待完成后重试".to_string());
    }
    state.begin_remote_turn(&session_id, &message_id)?;
    let _remote_turn_lease = RemoteTurnLease {
        state,
        session_id: session_id.clone(),
        message_id: message_id.clone(),
    };
    state.sync_core_config_from_state().await?;

    let raw = media
        .into_iter()
        .map(|asset| tiangong_media_archive::RawAttachment {
            kind: asset.kind,
            source: asset.url,
            mime_type: asset.mime_type,
            original_name: asset.title,
        })
        .collect::<Vec<_>>();
    let message_id_for_prepare = message_id.clone();
    let prepared_batch = tokio::task::spawn_blocking(move || {
        let store = tiangong_media_archive::AttachmentStore::default();
        let mut transaction = store.store_batch(raw)?;
        let prepared = transaction.prepare_message(&message_id_for_prepare, content)?;
        Ok::<_, String>((transaction, prepared))
    })
    .await
    .map_err(|error| format!("附件准备任务失败：{error}"))??;
    let (transaction, prepared) = prepared_batch;
    let created_paths = transaction
        .newly_created_paths()
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    transaction.commit();

    let (stream_tx, stream_rx) = std_mpsc::channel::<StreamEvent>();
    let workspace_dir = if session_exists {
        None
    } else {
        Some(
            state
                .with_state_read(|core_state| Ok(core_state.workspace_dir.clone()))
                .await?,
        )
    };
    let ensured = state
        .ensure_core(
            &session_id,
            workspace_dir,
            (!session_exists).then_some(TrustMode::FullTrust),
            None,
            None,
            stream_tx,
        )
        .await;
    // 远端投递不改变模型：沿用会话已记录的选择策略。
    let session_model_ref = state
        .core_manager
        .load_session(&session_id)
        .ok()
        .and_then(|session| session.model_ref);
    let ensured = match ensured {
        Ok(ensured) => ensured,
        Err(error) => {
            rollback_failed_delivery(state, &session_id, &message_id, created_paths).await;
            return Err(error);
        }
    };
    let waiter = state.register_remote_turn_waiter(&session_id, &message_id);
    if let Err(error) = state
        .deliver_prepared_if_live(
            &ensured.session_id,
            message_id.clone(),
            prepared,
            // 远端投递沿用会话已记录的模型选择。
            session_model_ref.as_deref(),
        )
        .await
    {
        rollback_failed_delivery(state, &session_id, &message_id, created_paths).await;
        return Err(format!("消息投递失败：{error}"));
    }

    if ensured.is_new {
        crate::commands::start_stream_consumer(app, ensured.session_id.clone(), stream_rx);
    }
    if !session_exists {
        let _ = state
            .with_state(|core_state| {
                if core_state.active_session_id.trim().is_empty() {
                    core_state.active_session_id = session_id.clone();
                }
                Ok(())
            })
            .await;
    }
    drop(send_guard);

    waiter
        .await
        .map_err(|_| "远程消息等待器意外关闭".to_string())?
        .map(|outgoing| (session_id, outgoing))
}

fn incomplete_existing_message_error(has_pending_turn: bool) -> String {
    if has_pending_turn {
        "该消息已存在于其他执行轮次，不能作为新的远端轮次重复投递".to_string()
    } else {
        "该消息是重启前遗留的未完成记录，当前没有可等待的执行实例".to_string()
    }
}

async fn rollback_failed_delivery(
    state: &TiangongApp,
    session_id: &str,
    message_id: &str,
    created_paths: Vec<String>,
) {
    state.complete_remote_turn_waiters(
        session_id,
        message_id,
        Err("消息稳定持久化失败".to_string()),
    );
    crate::commands::shutdown_join_core_if_current(state, session_id).await;
    if let Err(error) =
        crate::commands::restore_failed_user_message_state(state, session_id, message_id).await
    {
        tracing::warn!(%error, %session_id, %message_id, "内嵌 Server 消息回滚失败");
    }
    crate::commands::cleanup_unreferenced_input_attachments(
        state,
        crate::commands::raw_attachments_for_paths(created_paths),
    )
    .await;
}

async fn completed_message_result(
    state: &TiangongApp,
    session_id: &str,
    message_id: &str,
    status: TurnStatus,
) -> HostResult<OutgoingMessage> {
    // 会话存在性用 metadata 判定；消息遍历需完整 Session，从磁盘 load
    //（issue #245：真相源归磁盘）。
    let session_exists = state
        .with_state_read(|core_state| {
            Ok::<_, anyhow::Error>(
                core_state
                    .core_manager
                    .list_session_metadata()
                    .iter()
                    .any(|m| m.id == session_id),
            )
        })
        .await?;
    if !session_exists {
        return Err(format!("目标会话不存在：{session_id}"));
    }
    let session = state.core_manager.load_session(session_id)?;
    let outgoing =
        tiangong_server::remote::core::assistant_outgoing_after_user(&session, message_id);
    if status == TurnStatus::Success {
        Ok(outgoing)
    } else {
        Err(match status {
            TurnStatus::Cancelled => "执行已取消".to_string(),
            TurnStatus::Failed => "执行失败".to_string(),
            TurnStatus::Processing | TurnStatus::Success => unreachable!("仅以终态调用"),
        })
    }
}

pub(crate) async fn complete_remote_turn_from_stream(
    state: &TiangongApp,
    session_id: &str,
    message_id: &str,
) {
    // 消息终态需读 messages（完整 Session）；从磁盘 load（issue #245）。
    // 按消息所属轮次取状态：远程消息接续意外中断的轮次时是引导消息，自身无状态。
    let status = match state.core_manager.load_session(session_id) {
        Ok(session) => session
            .turn_status_of(message_id)
            .filter(|status| status.is_terminal())
            .ok_or_else(|| anyhow!("未找到远程消息的终态：{message_id}")),
        Err(error) => Err(anyhow!("加载会话失败：{error}")),
    };
    let result = match status {
        Ok(status) => completed_message_result(state, session_id, message_id, status).await,
        Err(error) => Err(error.to_string()),
    };
    state.complete_remote_turn_waiters(session_id, message_id, result);
}

async fn delete_session(
    _app: &AppHandle,
    state: &TiangongApp,
    session_id: &str,
) -> HostResult<bool> {
    let exists = state
        .with_state_read(|core_state| {
            Ok(core_state
                .core_manager
                .list_session_metadata()
                .iter()
                .any(|m| m.id == session_id))
        })
        .await?;
    if !exists {
        return Ok(false);
    }

    let _cache_guard = state.input_cache_update_lock(session_id).lock_owned().await;
    let _send_guard = state.session_send_lock(session_id).lock_owned().await;
    // 逻辑删除：原子移动到 trash + 取消 Core。
    state.core_manager.delete_session(session_id).await?;
    // 清理内存状态。
    state.fail_remote_session_waiters(session_id, "目标会话已删除");
    state
        .with_state(|core_state| {
            crate::session_ops::remove_session_state(core_state, &state.core_manager, session_id);
            Ok(())
        })
        .await?;
    let _ = state.release_any_input_send_claim(session_id);
    state.remove_session_send_lock(session_id);
    // 不 emit sessions_updated：前端已本地移除会话，不需要全量刷新。
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bridge_declares_embedded_host_backend() {
        let (request_tx, _request_rx) = mpsc::unbounded_channel();
        let bridge = DesktopServerCoreBridge { request_tx };
        assert_eq!(bridge.kind(), CoreBackendKind::EmbeddedHost);
    }

    #[tokio::test]
    async fn stable_message_ids_keep_waiters_separate() {
        let state = TiangongApp::new();
        let mut first = state.register_remote_turn_waiter("session", "message-1");
        let second = state.register_remote_turn_waiter("session", "message-2");
        state.complete_remote_turn_waiters("session", "message-2", Err("second".to_string()));
        assert_eq!(second.await.unwrap().unwrap_err(), "second");
        assert!(first.try_recv().is_err(), "其他轮次不能被错误终态唤醒");
    }

    #[test]
    fn remote_turn_owner_identifies_terminal_waiter() {
        let state = TiangongApp::new();
        state.begin_remote_turn("session", "message-1").unwrap();
        assert_eq!(
            state.remote_turn_owner("session").as_deref(),
            Some("message-1")
        );
        state.finish_remote_turn("session", "message-1");
        assert!(state.remote_turn_owner("session").is_none());
    }

    #[test]
    fn restart_leftover_message_is_rejected_instead_of_registering_a_waiter() {
        let error = incomplete_existing_message_error(false);
        assert!(error.contains("重启前遗留"));
        assert!(error.contains("没有可等待的执行实例"));
    }
}
