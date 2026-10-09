//! sidecar 请求分发与各平台接收连接的生命周期。
//!
//! 每个平台的数据位于 `<插件数据目录>/<平台>/`。常驻主进程为每个平台运行
//! 一个监管任务：平台已配置且 server 可用时建立接收连接，凭据变化时重建。
//! 只允许单连接的平台（QQ、微信）先取得 `<平台>/connection.lock` 文件锁，
//! 同机多个宿主（桌面内嵌 server 与 `tiangong server`）只有持锁者建连。

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tiangong_plugin_runtime::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, PROTOCOL_VERSION, Request, RequestInvocationContext, Response,
};
use tiangong_plugin_runtime::sidecar::{
    INSTANCE_ROLE_ENV, INSTANCE_ROLE_PRIMARY, PLUGIN_DATA_DIR_ENV, SERVER_TOKEN_ENV, SERVER_URL_ENV,
};
use tokio::sync::watch;

use crate::delivery::{self, DeliveryResult};
use crate::platform::{BotPlatform, ServerEndpoint, TargetView};
use crate::tools::{SendLocalMediaInput, SendTextInput};

const PLUGIN_ID: &str = "bot";
const CONNECTION_LOCK_FILE: &str = "connection.lock";
/// 连接任务异常结束后的重试间隔。
const RETRY_DELAY: Duration = Duration::from_secs(30);
/// 单连接平台等待文件锁的轮询间隔。
const LOCK_POLL: Duration = Duration::from_secs(15);

/// 推送工具名（与 plugin.json `tools` 一致）。
const TOOLS: &[&str] = &[
    "list_push_targets",
    "send_text_message",
    "send_image_message",
    "send_file_message",
];

/// 平台接收连接状态（供设置页展示）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConnectionState {
    /// 非主进程（验证/临时实例），不建立连接。
    Auxiliary,
    /// 尚未配置凭据。
    Unconfigured,
    /// 天工 server 未运行，无法转发消息。
    NoServer,
    /// 另一个天工宿主持有该平台的接收连接。
    Standby,
    /// 接收连接运行中（含断线重连）。
    Running,
}

struct PlatformSlot {
    platform: Arc<dyn BotPlatform>,
    dir: PathBuf,
    connection: Mutex<ConnectionState>,
    /// 凭据变化时通知接收连接重建。
    reload: watch::Sender<u64>,
}

impl PlatformSlot {
    fn set_connection(&self, state: ConnectionState) {
        if let Ok(mut current) = self.connection.lock() {
            *current = state;
        }
    }

    fn connection_state(&self) -> ConnectionState {
        self.connection
            .lock()
            .map(|state| state.clone())
            .unwrap_or(ConnectionState::Unconfigured)
    }
}

pub(crate) struct BotService {
    primary: bool,
    server: Option<ServerEndpoint>,
    slots: Vec<Arc<PlatformSlot>>,
}

#[derive(Deserialize)]
struct PlatformArgs {
    platform: String,
}

impl BotService {
    pub(crate) fn from_env() -> Result<Self> {
        let data_dir = std::env::var_os(PLUGIN_DATA_DIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .context("宿主未注入插件数据目录")?;
        let non_empty = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
        let server = non_empty(SERVER_URL_ENV).map(|url| ServerEndpoint {
            url,
            token: non_empty(SERVER_TOKEN_ENV),
        });
        Self::new(
            &data_dir,
            std::env::var(INSTANCE_ROLE_ENV).as_deref() == Ok(INSTANCE_ROLE_PRIMARY),
            server,
            crate::all_platforms(),
        )
    }

    pub(crate) fn new(
        data_dir: &Path,
        primary: bool,
        server: Option<ServerEndpoint>,
        platforms: Vec<Arc<dyn BotPlatform>>,
    ) -> Result<Self> {
        let mut slots = Vec::new();
        for platform in platforms {
            let dir = data_dir.join(platform.id());
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("创建 Bot 数据目录失败：{}", dir.display()))?;
            let (reload, _) = watch::channel(0);
            slots.push(Arc::new(PlatformSlot {
                platform,
                dir,
                connection: Mutex::new(if primary {
                    ConnectionState::Unconfigured
                } else {
                    ConnectionState::Auxiliary
                }),
                reload,
            }));
        }
        Ok(Self {
            primary,
            server,
            slots,
        })
    }

    /// 主进程迁移旧版 Bot 数据后为每个平台启动接收连接监管任务；
    /// 非主进程只处理请求。
    pub(crate) fn start(self: &Arc<Self>) {
        if !self.primary {
            tracing::info!("Bot 插件以辅助实例运行，不建立接收连接");
            return;
        }
        self.migrate_legacy();
        for slot in &self.slots {
            let slot = Arc::clone(slot);
            let server = self.server.clone();
            tokio::spawn(async move { supervise(slot, server).await });
        }
    }

    /// 从旧版 Bot（`<存储根>/bots/`）复制凭据与推送目标，见 [`crate::legacy`]。
    fn migrate_legacy(&self) {
        let Some(storage_root) = crate::local_media::storage_root() else {
            return;
        };
        let Some(data_dir) = self.slots.first().and_then(|slot| slot.dir.parent()) else {
            return;
        };
        let platforms: Vec<&str> = self.slots.iter().map(|slot| slot.platform.id()).collect();
        crate::legacy::migrate_all(&storage_root, data_dir, &platforms);
    }

    fn slot(&self, platform: &str) -> Result<&Arc<PlatformSlot>> {
        self.slots
            .iter()
            .find(|slot| slot.platform.id() == platform)
            .with_context(|| format!("不支持的 Bot 平台：{platform}"))
    }

    fn slot_of_target(&self, target_id: &str) -> Result<Option<&Arc<PlatformSlot>>> {
        for slot in &self.slots {
            if slot.platform.owns_target(&slot.dir, target_id)? {
                return Ok(Some(slot));
            }
        }
        Ok(None)
    }

    fn all_targets(&self, enabled_only: bool) -> Result<Vec<TargetView>> {
        let mut targets = Vec::new();
        for slot in &self.slots {
            targets.extend(
                slot.platform
                    .list_targets(&slot.dir)?
                    .into_iter()
                    .filter(|target| !enabled_only || target.enabled),
            );
        }
        targets.sort_by(|left, right| right.last_seen_at.cmp(&left.last_seen_at));
        Ok(targets)
    }

    async fn credentials_changed(&self, slot: &PlatformSlot) {
        slot.platform.invalidate().await;
        slot.reload.send_modify(|value| *value += 1);
    }

    async fn send_text(&self, input: SendTextInput) -> Result<DeliveryResult> {
        if let Some(rejected) = delivery::check_idempotency_key(input.idempotency_key.trim()) {
            return Ok(rejected);
        }
        let Some(slot) = self.slot_of_target(input.target_id.trim())? else {
            return Ok(delivery::rejected("推送目标不存在或尚未授权"));
        };
        slot.platform.send_text(&slot.dir, &input).await
    }

    async fn send_media(
        &self,
        input: SendLocalMediaInput,
        workspace: Option<&Path>,
        image: bool,
    ) -> Result<DeliveryResult> {
        if let Some(rejected) = delivery::check_idempotency_key(input.idempotency_key.trim()) {
            return Ok(rejected);
        }
        let Some(slot) = self.slot_of_target(input.target_id.trim())? else {
            return Ok(delivery::rejected("推送目标不存在或尚未授权"));
        };
        slot.platform
            .send_media(&slot.dir, &input, workspace, image)
            .await
    }

    async fn dispatch_operation(
        &self,
        operation: &str,
        payload: Value,
        context: Option<&RequestInvocationContext>,
    ) -> Result<Value> {
        match operation {
            HANDSHAKE_OPERATION => Ok(serde_json::json!({
                "plugin_id": PLUGIN_ID,
                "plugin_version": env!("CARGO_PKG_VERSION"),
                "sidecar_version": env!("CARGO_PKG_VERSION"),
                "protocol_version": PROTOCOL_VERSION,
                "business_protocol": 1,
                "capabilities": TOOLS.iter().map(|tool| format!("tool:{tool}")).collect::<Vec<_>>(),
                "instance_id": format!("{PLUGIN_ID}-sidecar-{}", std::process::id()),
                "status": "ready",
            })),
            // ── 推送工具（返回 ToolOutcome 形状）──
            "list_push_targets" => {
                let targets = self.all_targets(true)?;
                tool_outcome(
                    true,
                    &format!("共 {} 个已授权推送目标", targets.len()),
                    &serde_json::json!({ "targets": targets }),
                )
            }
            "send_text_message" => {
                let input: SendTextInput =
                    serde_json::from_value(payload).context("send_text_message 参数无效")?;
                delivery_outcome(self.send_text(input).await?)
            }
            "send_image_message" | "send_file_message" => {
                let input: SendLocalMediaInput =
                    serde_json::from_value(payload).context("发送本地文件参数无效")?;
                let workspace = context
                    .map(|context| context.workspace.trim())
                    .filter(|workspace| !workspace.is_empty())
                    .map(Path::new);
                delivery_outcome(
                    self.send_media(input, workspace, operation == "send_image_message")
                        .await?,
                )
            }
            // ── 设置页操作 ──
            "status" => {
                let mut platforms = Vec::new();
                for slot in &self.slots {
                    let account = slot.platform.configured_account(&slot.dir)?;
                    platforms.push(serde_json::json!({
                        "id": slot.platform.id(),
                        "name": slot.platform.name(),
                        "configured": account.is_some(),
                        "account": account,
                        "connection": slot.connection_state(),
                        "migrated_from": crate::legacy::read_marker(&slot.dir),
                    }));
                }
                Ok(serde_json::json!({
                    "server_available": self.server.is_some(),
                    "platforms": platforms,
                    "targets": self.all_targets(false)?,
                }))
            }
            "provision_begin" => {
                let PlatformArgs { platform } =
                    serde_json::from_value(payload).context("缺少平台参数")?;
                let slot = self.slot(&platform)?;
                let mut session = slot.platform.provision_begin(&slot.dir).await?;
                let qr_url = session["qr_url"]
                    .as_str()
                    .context("扫码会话缺少二维码地址")?
                    .to_string();
                session["qr_svg"] = Value::String(qr_svg(&qr_url)?);
                Ok(session)
            }
            "provision_poll" => {
                #[derive(Deserialize)]
                struct Args {
                    platform: String,
                    session: Value,
                }
                let Args { platform, session } =
                    serde_json::from_value(payload).context("扫码轮询参数无效")?;
                let slot = self.slot(&platform)?;
                let status = slot.platform.provision_poll(&slot.dir, session).await?;
                if status["status"] == "success" {
                    self.credentials_changed(slot).await;
                }
                Ok(status)
            }
            "save_credentials" => {
                #[derive(Deserialize)]
                struct Args {
                    platform: String,
                    credentials: Value,
                }
                let Args {
                    platform,
                    credentials,
                } = serde_json::from_value(payload).context("凭据参数无效")?;
                let slot = self.slot(&platform)?;
                slot.platform.save_credentials(&slot.dir, credentials)?;
                self.credentials_changed(slot).await;
                Ok(serde_json::json!({ "saved": true }))
            }
            "clear_credentials" => {
                let PlatformArgs { platform } =
                    serde_json::from_value(payload).context("缺少平台参数")?;
                let slot = self.slot(&platform)?;
                slot.platform.clear_credentials(&slot.dir)?;
                self.credentials_changed(slot).await;
                Ok(serde_json::json!({ "cleared": true }))
            }
            "delete_push_target" => {
                #[derive(Deserialize)]
                struct Args {
                    target_id: String,
                }
                let Args { target_id } =
                    serde_json::from_value(payload).context("推送目标参数无效")?;
                let slot = self
                    .slot_of_target(&target_id)?
                    .with_context(|| format!("未找到推送目标：{target_id}"))?;
                slot.platform.delete_target(&slot.dir, &target_id)?;
                Ok(serde_json::json!({ "target_id": target_id, "deleted": true }))
            }
            other => bail!("Bot 插件不支持操作 {other}"),
        }
    }

    pub(crate) async fn dispatch_with_context(
        &self,
        request: Request,
        context: Option<RequestInvocationContext>,
    ) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "Bot 插件协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }
        match self
            .dispatch_operation(&request.operation, request.payload, context.as_ref())
            .await
        {
            Ok(payload) => Response::success(&request_id, payload),
            Err(error) => {
                if TOOLS.contains(&request.operation.as_str()) {
                    // 工具业务失败以 ToolOutcome 返回，模型能读到原因。
                    return Response::success(
                        &request_id,
                        serde_json::json!({ "ok": false, "summary": format!("{error:#}") }),
                    );
                }
                Response::error(
                    &request_id,
                    ErrorCode::ServiceError,
                    format!("{error:#}"),
                    false,
                )
            }
        }
    }
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for BotService {
    async fn dispatch(&self, request: Request) -> Response {
        let context = tiangong_plugin_sidecar::invocation_context();
        self.dispatch_with_context(request, context).await
    }
}

/// 单个平台的接收连接监管循环。
async fn supervise(slot: Arc<PlatformSlot>, server: Option<ServerEndpoint>) {
    let mut reload = slot.reload.subscribe();
    let mut lock: Option<File> = None;
    loop {
        reload.borrow_and_update();
        let configured = match slot.platform.configured_account(&slot.dir) {
            Ok(account) => account.is_some(),
            Err(error) => {
                tracing::warn!(
                    platform = slot.platform.id(),
                    "读取 Bot 凭据失败: {error:#}"
                );
                false
            }
        };
        if !configured {
            lock = None;
            slot.set_connection(ConnectionState::Unconfigured);
        } else if let Some(server) = server.clone() {
            if slot.platform.exclusive_connection() && lock.is_none() {
                match try_lock_connection(&slot.dir) {
                    Ok(Some(file)) => lock = Some(file),
                    Ok(None) => {
                        slot.set_connection(ConnectionState::Standby);
                        tokio::select! {
                            changed = reload.changed() => if changed.is_err() { return },
                            _ = tokio::time::sleep(LOCK_POLL) => {}
                        }
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(platform = slot.platform.id(), "获取连接锁失败: {error:#}");
                    }
                }
            }
            slot.set_connection(ConnectionState::Running);
            let mut stop = reload.clone();
            let stopped = Box::pin(async move {
                let _ = stop.changed().await;
            });
            if let Err(error) = slot.platform.run(&slot.dir, server, stopped).await {
                tracing::error!(platform = slot.platform.id(), "Bot 接收连接失败: {error:#}");
            }
            if reload.has_changed().unwrap_or(false) {
                continue;
            }
            // 连接任务异常结束（凭据无效等）：稍后重试或等待凭据变化。
            tokio::select! {
                changed = reload.changed() => if changed.is_err() { return },
                _ = tokio::time::sleep(RETRY_DELAY) => {}
            }
            continue;
        } else {
            slot.set_connection(ConnectionState::NoServer);
        }
        if reload.changed().await.is_err() {
            return;
        }
    }
}

/// 尝试独占平台接收连接；锁随文件句柄释放（进程退出自动释放）。
fn try_lock_connection(dir: &Path) -> Result<Option<File>> {
    let path = dir.join(CONNECTION_LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("打开连接锁失败：{}", path.display()))?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(file)),
        Err(error) if error.kind() == fs2::lock_contended_error().kind() => Ok(None),
        Err(error) => Err(error).with_context(|| format!("锁定连接锁失败：{}", path.display())),
    }
}

/// 扫码地址渲染为 SVG 二维码。
fn qr_svg(url: &str) -> Result<String> {
    let code = qrcode::QrCode::new(url.as_bytes()).context("生成扫码二维码失败")?;
    Ok(code
        .render::<qrcode::render::svg::Color<'_>>()
        .min_dimensions(200, 200)
        .quiet_zone(true)
        .build())
}

fn tool_outcome(ok: bool, summary: &str, output: &impl Serialize) -> Result<Value> {
    Ok(serde_json::json!({
        "ok": ok,
        "summary": summary,
        "stdout": serde_json::to_string(output)?,
    }))
}

fn delivery_outcome(result: DeliveryResult) -> Result<Value> {
    let ok = matches!(result.status.as_str(), "accepted" | "queued");
    let summary = result.message.clone();
    tool_outcome(ok, &summary, &result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(operation: &str, payload: Value) -> Request {
        Request::new(operation, payload)
    }

    fn service(dir: &Path) -> BotService {
        BotService::new(dir, false, None, crate::all_platforms()).unwrap()
    }

    async fn call(service: &BotService, operation: &str, payload: Value) -> Response {
        service
            .dispatch_with_context(request(operation, payload), None)
            .await
    }

    #[tokio::test]
    async fn handshake_declares_every_tool() {
        let data_dir = tempfile::tempdir().unwrap();
        let response = call(
            &service(data_dir.path()),
            HANDSHAKE_OPERATION,
            serde_json::json!({}),
        )
        .await;
        assert!(response.success);
        let capabilities = response.payload.unwrap()["capabilities"].clone();
        for tool in TOOLS {
            assert!(
                capabilities
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!(format!("tool:{tool}")))
            );
        }
    }

    #[tokio::test]
    async fn status_lists_every_platform_and_send_rejects_unknown_target() {
        let data_dir = tempfile::tempdir().unwrap();
        let service = service(data_dir.path());
        let status = call(&service, "status", serde_json::json!({}))
            .await
            .payload
            .unwrap();
        let ids: Vec<_> = status["platforms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|platform| platform["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["feishu", "qq", "weixin"]);
        assert_eq!(status["platforms"][0]["connection"], "auxiliary");
        assert_eq!(status["server_available"], false);

        let response = call(
            &service,
            "send_text_message",
            serde_json::json!({"target_id":"nope","text":"hi","idempotency_key":"k"}),
        )
        .await
        .payload
        .unwrap();
        assert_eq!(response["ok"], false);
        assert!(response["summary"].as_str().unwrap().contains("不存在"));

        let response = call(
            &service,
            "send_text_message",
            serde_json::json!({"target_id":"nope","text":"hi","idempotency_key":""}),
        )
        .await
        .payload
        .unwrap();
        assert!(response["summary"].as_str().unwrap().contains("幂等键"));
    }

    #[tokio::test]
    async fn manual_credentials_are_saved_per_platform() {
        let data_dir = tempfile::tempdir().unwrap();
        let service = service(data_dir.path());
        for (platform, credentials) in [
            (
                "feishu",
                serde_json::json!({"app_id":"cli_x","app_secret":"s"}),
            ),
            ("qq", serde_json::json!({"app_id":"123","app_secret":"s"})),
            (
                "weixin",
                serde_json::json!({"bot_token":"token-abcdefghijkl"}),
            ),
        ] {
            let saved = call(
                &service,
                "save_credentials",
                serde_json::json!({"platform": platform, "credentials": credentials}),
            )
            .await;
            assert!(saved.success, "{platform}: {:?}", saved.error_message);
        }
        let status = call(&service, "status", serde_json::json!({}))
            .await
            .payload
            .unwrap();
        for platform in status["platforms"].as_array().unwrap() {
            assert_eq!(platform["configured"], true, "{platform}");
        }
        assert_eq!(status["platforms"][0]["account"], "cli_x");
        assert_eq!(status["platforms"][2]["account"], "toke****ijkl");

        let cleared = call(
            &service,
            "clear_credentials",
            serde_json::json!({"platform": "qq"}),
        )
        .await;
        assert!(cleared.success);
        let status = call(&service, "status", serde_json::json!({}))
            .await
            .payload
            .unwrap();
        assert_eq!(status["platforms"][1]["configured"], false);

        let invalid = call(
            &service,
            "save_credentials",
            serde_json::json!({"platform": "feishu", "credentials": {"app_id": " ", "app_secret": "x"}}),
        )
        .await;
        assert!(!invalid.success);
        let unknown = call(
            &service,
            "clear_credentials",
            serde_json::json!({"platform": "slack"}),
        )
        .await;
        assert!(!unknown.success);
    }

    #[test]
    fn exclusive_connection_lock_admits_single_holder() {
        let dir = tempfile::tempdir().unwrap();
        let first = try_lock_connection(dir.path()).unwrap();
        assert!(first.is_some());
        assert!(try_lock_connection(dir.path()).unwrap().is_none());
        drop(first);
        assert!(try_lock_connection(dir.path()).unwrap().is_some());
    }

    #[test]
    fn qr_svg_renders_svg_markup() {
        assert!(
            qr_svg("https://example.com/x?y=1")
                .unwrap()
                .contains("<svg")
        );
    }
}
