//! QQ 平台：开放平台 WebSocket 网关收消息，OpenAPI 发送。
//!
//! 网关同一机器人多会话的投递行为未核实，按单连接运行：同机多个天工宿主
//! 只有取得连接锁的一方建连。

mod connection;
mod gateway;
mod provision;
mod push;
mod target_store;
mod token;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::delivery::DeliveryResult;
use crate::platform::{BotPlatform, ServerEndpoint, StopSignal, TargetView};
use crate::tools::{SendLocalMediaInput, SendTextInput};
use connection::{BotState, RecentMessages};
use provision::{Credentials, QrSession};
use token::AccessTokenCache;

#[derive(Default)]
pub(crate) struct Qq {
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
    let http = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .context("构建 QQ HTTP 客户端失败")?;
    Ok(BotState {
        token: AccessTokenCache::new(http.clone(), credentials.app_id, credentials.app_secret),
        http,
        tiangong_http: Client::builder()
            .build()
            .context("构建天工 HTTP 客户端失败")?,
        tiangong_url: url,
        tiangong_token: token,
        recent_messages: tokio::sync::Mutex::new(RecentMessages::default()),
        data_dir: dir.to_path_buf(),
    })
}

impl Qq {
    async fn push_state(&self, dir: &Path) -> Result<Arc<BotState>> {
        let mut guard = self.state.lock().await;
        if let Some(state) = guard.as_ref() {
            return Ok(Arc::clone(state));
        }
        let credentials = provision::load_credentials(dir)?
            .context("QQ Bot 尚未配置，请先在插件设置中扫码或填写 AppID / ClientSecret")?;
        let state = Arc::new(build_state(dir, credentials, None)?);
        *guard = Some(Arc::clone(&state));
        Ok(state)
    }
}

#[async_trait::async_trait]
impl BotPlatform for Qq {
    fn id(&self) -> &'static str {
        "qq"
    }

    fn name(&self) -> &'static str {
        "QQ"
    }

    fn exclusive_connection(&self) -> bool {
        true
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
            serde_json::from_value(credentials).context("QQ 凭据需要 app_id 与 app_secret")?;
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
                platform: "qq",
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
        let credentials = provision::load_credentials(dir)?.context("QQ Bot 尚未配置")?;
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
