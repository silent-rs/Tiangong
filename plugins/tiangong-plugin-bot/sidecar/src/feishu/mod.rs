//! 飞书平台：openlark WebSocket 长连接收消息，开放平台 HTTP API 发送。
//!
//! 飞书长连接为集群模式（同一应用最多 50 条连接，每条消息只投给其中一条），
//! 因此允许多个天工宿主各自建连，不需要单连接互斥。

mod connection;
mod provision;
mod push;
mod target_store;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use crate::delivery::DeliveryResult;
use crate::platform::{BotPlatform, ServerEndpoint, StopSignal, TargetView};
use crate::tools::{SendLocalMediaInput, SendTextInput};
pub(crate) use connection::install_rustls_provider;
use connection::{BotState, RecentMessages};
use provision::{Credentials, QrSession};

#[derive(Default)]
pub(crate) struct Feishu {
    /// 推送工具使用的客户端状态（含令牌缓存）；凭据变化时清空。
    state: Mutex<Option<Arc<BotState>>>,
}

fn build_state(
    dir: &Path,
    credentials: Credentials,
    server: Option<ServerEndpoint>,
) -> Result<BotState> {
    let (url, token) = server
        .map(|server| (server.url, server.token))
        .unwrap_or_default();
    Ok(BotState {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?,
        tiangong_http: reqwest::Client::builder().build()?,
        app_id: credentials.app_id,
        app_secret: credentials.app_secret,
        access_token: RwLock::new(None),
        tiangong_url: url,
        tiangong_token: token,
        recent_messages: tokio::sync::Mutex::new(RecentMessages::default()),
        data_dir: dir.to_path_buf(),
    })
}

impl Feishu {
    async fn push_state(&self, dir: &Path) -> Result<Arc<BotState>> {
        let mut guard = self.state.lock().await;
        if let Some(state) = guard.as_ref() {
            return Ok(Arc::clone(state));
        }
        let credentials = provision::load_credentials(dir)?
            .context("飞书 Bot 尚未配置，请先在插件设置中扫码或填写 App ID / App Secret")?;
        let state = Arc::new(build_state(dir, credentials, None)?);
        *guard = Some(Arc::clone(&state));
        Ok(state)
    }
}

#[async_trait::async_trait]
impl BotPlatform for Feishu {
    fn id(&self) -> &'static str {
        "feishu"
    }

    fn name(&self) -> &'static str {
        "飞书"
    }

    fn exclusive_connection(&self) -> bool {
        false
    }

    fn configured_account(&self, dir: &Path) -> Result<Option<String>> {
        Ok(provision::load_credentials(dir)?.map(|credentials| credentials.app_id))
    }

    async fn provision_begin(&self, _dir: &Path) -> Result<Value> {
        Ok(serde_json::to_value(provision::begin().await?)?)
    }

    async fn provision_poll(&self, dir: &Path, session: Value) -> Result<Value> {
        let session: QrSession = serde_json::from_value(session).context("扫码会话参数无效")?;
        Ok(serde_json::to_value(provision::poll(dir, &session).await?)?)
    }

    fn save_credentials(&self, dir: &Path, credentials: Value) -> Result<()> {
        let credentials: Credentials =
            serde_json::from_value(credentials).context("飞书凭据需要 app_id 与 app_secret")?;
        provision::save_credentials(dir, &credentials)
    }

    fn clear_credentials(&self, dir: &Path) -> Result<()> {
        provision::clear_credentials(dir)
    }

    async fn invalidate(&self) {
        *self.state.lock().await = None;
    }

    fn list_targets(&self, dir: &Path) -> Result<Vec<TargetView>> {
        Ok(target_store::list_views(dir)?
            .into_iter()
            .map(|target| TargetView {
                target_id: target.target_id,
                platform: "feishu",
                label: target.label,
                kind: target.kind,
                enabled: target.enabled,
                availability: target.availability,
                last_seen_at: target.last_seen_at,
                limitation: target.limitation,
            })
            .collect())
    }

    fn delete_target(&self, dir: &Path, target_id: &str) -> Result<()> {
        target_store::delete(dir, target_id)
    }

    fn owns_target(&self, dir: &Path, target_id: &str) -> Result<bool> {
        Ok(target_store::list_views(dir)?
            .iter()
            .any(|target| target.target_id == target_id))
    }

    async fn run(&self, dir: &Path, server: ServerEndpoint, stop: StopSignal) -> Result<()> {
        let credentials = provision::load_credentials(dir)?.context("飞书 Bot 尚未配置")?;
        let state = Arc::new(build_state(dir, credentials, Some(server))?);
        connection::run_connection(state, stop).await
    }

    async fn send_text(&self, dir: &Path, input: &SendTextInput) -> Result<DeliveryResult> {
        let state = self.push_state(dir).await?;
        push::send_text_message(&state, input).await
    }

    async fn send_media(
        &self,
        dir: &Path,
        input: &SendLocalMediaInput,
        workspace: Option<&Path>,
        image: bool,
    ) -> Result<DeliveryResult> {
        let state = self.push_state(dir).await?;
        push::send_media_message(&state, input, workspace, image).await
    }
}
