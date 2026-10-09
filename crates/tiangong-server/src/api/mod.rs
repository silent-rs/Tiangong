mod chat;
mod health;
mod jobs;
mod messages;
mod server_ctrl;
mod sessions;
mod types;
mod webhook;
pub mod ws;

use std::sync::Arc;

use silent::prelude::*;
use tiangong_app_state::app_state::TiangongState;
use tiangong_config::CoreConfigProvider;
use tokio::sync::Mutex;

use crate::remote::backend::ServerCoreBackend;
use crate::remote::core::ServerCoreManager;
use crate::remote::event::EventBus;
use crate::remote::router::MessageRouter;

/// 共享应用状态类型
pub type SharedState = Arc<Mutex<TiangongState>>;

/// Server 共享上下文：统一持有应用状态、Core 运行时与消息路由器
#[derive(Clone)]
pub struct ServerAppContext {
    pub state: SharedState,
    pub config: CoreConfigProvider,
    pub core_backend: Arc<dyn ServerCoreBackend>,
    pub router: Arc<MessageRouter>,
}

impl ServerAppContext {
    pub fn new(
        state: SharedState,
        core_manager: tiangong_app_state::app_state::CoreManager,
        event_bus: Arc<EventBus>,
        _storage_root: std::path::PathBuf,
    ) -> Self {
        let config = core_manager.config().clone();
        let cores = Arc::new(ServerCoreManager::new(
            state.clone(),
            core_manager,
            event_bus.clone(),
        ));
        let core_backend: Arc<dyn ServerCoreBackend> = cores.clone();
        Self::with_backend(state, config, event_bus, core_backend)
    }

    /// 使用宿主提供的 Core 构建 Server。
    ///
    /// Desktop 内嵌模式必须走此入口，确保 HTTP 与桌面 UI 共用同一套 Core 生命周期。
    pub fn with_backend(
        state: SharedState,
        config: CoreConfigProvider,
        event_bus: Arc<EventBus>,
        core_backend: Arc<dyn ServerCoreBackend>,
    ) -> Self {
        let router = Arc::new(MessageRouter::new(
            state.clone(),
            event_bus,
            core_backend.clone(),
        ));
        Self {
            state,
            config,
            core_backend,
            router,
        }
    }

    pub async fn sync_core_config_from_state(&self) {
        if let Err(error) = self.core_backend.sync_config_from_state().await {
            tracing::warn!(%error, "同步 Core 配置失败");
        }
    }
}

/// 共享 Server 上下文
pub type SharedAppContext = Arc<ServerAppContext>;

/// 认证 Token 包装（用于注入到 State 中）
#[derive(Clone, Debug)]
pub struct AuthToken(pub Option<String>);

/// 构建完整的 API 路由树，通过 State 注入共享状态和 Token
pub fn build_routes(
    app: SharedAppContext,
    token: Option<String>,
    event_bus: Arc<EventBus>,
) -> (Route, State) {
    let mut configs = State::default();
    configs.insert(app);
    configs.insert(AuthToken(token));
    configs.insert(ws::SharedEventBus(event_bus));

    let route = Route::new("api/v1")
        .append(Route::new("health").get(health::health_check))
        .append(Route::new("chat").post(chat::chat))
        .append(Route::new("messages").post(messages::post_message))
        .append(
            Route::new("sessions").get(sessions::list_sessions).append(
                Route::new("<id>")
                    .get(sessions::get_session)
                    .append(Route::new("cancel").post(sessions::cancel_session_turn))
                    .delete(sessions::delete_session),
            ),
        )
        .append(
            Route::new("jobs")
                .get(jobs::list_jobs)
                .post(jobs::create_job)
                .append(
                    Route::new("<id>")
                        .get(jobs::get_job)
                        .put(jobs::update_job)
                        .delete(jobs::delete_job)
                        .append(Route::new("runs").get(jobs::list_job_runs))
                        .append(Route::new("trigger").post(jobs::trigger_job)),
                ),
        )
        .append(
            Route::new("webhooks")
                .get(webhook::list_webhooks)
                .post(webhook::create_webhook)
                .append(
                    Route::new("<id>")
                        .get(webhook::get_webhook)
                        .put(webhook::update_webhook)
                        .delete(webhook::delete_webhook)
                        .append(Route::new("runs").get(webhook::list_webhook_runs))
                        .append(Route::new("trigger").post(webhook::trigger_webhook))
                        .append(Route::new("invoke").post(webhook::invoke_webhook)),
                ),
        )
        .append(Route::new("server/shutdown").post(server_ctrl::shutdown))
        .append(ws::ws_route());

    (route, configs)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use tiangong_types::{
        IncomingMessage, MediaAsset, MessageContent, OutgoingMessage, RemoteRole,
    };

    use super::*;
    use crate::remote::backend::CoreBackendKind;

    struct EmbeddedBackend {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ServerCoreBackend for EmbeddedBackend {
        fn kind(&self) -> CoreBackendKind {
            CoreBackendKind::EmbeddedHost
        }

        async fn send_connector_message_and_wait(
            &self,
            _connector: &str,
            channel_id: &str,
            _content: String,
            _message_id: Option<String>,
            _media: Vec<MediaAsset>,
            _annotations: tiangong_types::MessageAnnotations,
        ) -> Result<(String, OutgoingMessage)> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok((
                channel_id.to_string(),
                OutgoingMessage {
                    content: MessageContent::Text("embedded reply".to_string()),
                    attachments: Vec::new(),
                    reply_to: None,
                },
            ))
        }

        async fn send_message_and_wait(
            &self,
            _session_id: &str,
            _content: String,
            _message_id: Option<String>,
            _media: Vec<MediaAsset>,
            _annotations: tiangong_types::MessageAnnotations,
        ) -> Result<(String, OutgoingMessage)> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(anyhow!("not used"))
        }

        async fn delete_session(&self, _session_id: &str) -> Result<bool> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(anyhow!("not used"))
        }

        async fn cancel_session_turn(&self, _session_id: &str) -> Result<bool> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(anyhow!("not used"))
        }

        async fn sync_config_from_state(&self) -> Result<()> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(anyhow!("not used"))
        }
    }

    #[tokio::test]
    async fn embedded_messages_use_host_backend_without_starting_another_scheduler() {
        let _storage_guard = crate::remote::core::test_support::STORAGE_TEST_LOCK
            .lock()
            .await;
        let temp = tempfile::tempdir().unwrap();
        let _home_guard =
            crate::remote::core::test_support::TestHomeGuard::new(&temp.path().join("home"));
        let config = CoreConfigProvider::new(tiangong_config::CoreConfig::default());
        let state = Arc::new(Mutex::new(
            tiangong_app_state::app_state::TiangongState::new(),
        ));
        let event_bus = Arc::new(EventBus::default());
        let backend_calls = Arc::new(AtomicUsize::new(0));
        let backend: Arc<dyn ServerCoreBackend> = Arc::new(EmbeddedBackend {
            calls: backend_calls.clone(),
        });

        let context = ServerAppContext::with_backend(state, config, event_bus, backend);

        assert_eq!(context.core_backend.kind(), CoreBackendKind::EmbeddedHost);
        assert_eq!(backend_calls.load(Ordering::Relaxed), 0);

        let outgoing = context
            .router
            .handle_incoming(IncomingMessage {
                id: "stable-message".to_string(),
                connector: "embedded-test".to_string(),
                channel_id: "desktop-session".to_string(),
                sender_id: "test".to_string(),
                sender_role: RemoteRole::Controller,
                content: MessageContent::Text("hello".to_string()),
                media: Vec::new(),
                reply_to: None,
                timestamp: tiangong_core::session::now_text(),
                annotations: Default::default(),
            })
            .await
            .unwrap();
        assert!(matches!(
            outgoing.content,
            MessageContent::Text(ref text) if text == "embedded reply"
        ));
        assert_eq!(backend_calls.load(Ordering::Relaxed), 1);
    }
}
