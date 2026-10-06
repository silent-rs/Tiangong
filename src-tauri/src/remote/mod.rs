//! 远程访问：天工桌面端向手机 H5 提供对话侧能力，两种方式共用同一套逻辑。
//!
//! - **中继**（缺省）：桌面端主动连到 `tiangong-relay`，缺省使用官方中继
//!   `https://relay.smart7.tech`，也可填写自部署地址，适合跨网络访问。中继是纯转发
//!   服务，部署时无需配置令牌：通道密钥由天工自动生成并只保存在本机，中继只看到
//!   它的单向摘要（通道 ID），据此把手机端路由到本桌面端；
//! - **局域网直连**：桌面端在本机局域网端口内嵌运行中继服务，手机与电脑
//!   在同一网络内扫码即用，不经过任何外部服务。
//!
//! 链路：手机浏览器 ⇄ 中继（内嵌或独立部署）⇄（桌面端发起的 WebSocket）⇄ 本模块。
//!
//! - 手机端页面就是桌面端同一份前端构建产物（远程标记下隐藏设置与拓展区）；
//! - 手机端的 invoke 经 [`policy::decide`] 裁决后，交给主 WebView 的 IPC 入口按
//!   **与桌面端完全相同**的命令处理；宿主事件按白名单转发回手机端；
//! - 扫码配对后只绑定一个设备，同一时刻只允许一条手机端连接在线。

pub mod config;
pub mod files;
pub mod policy;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tauri::{AppHandle, Listener, Manager, State};
use tiangong_relay::protocol::{AgentToRelay, RelayToAgent};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use self::config::{random_token, sha256_hex, Pairing, RemoteConfig, RemoteMode};
use self::policy::{Decision, PluginScope, REMOTE_SLOTS};

/// 单条消息上限（附件以 data URL 传输）。
const MAX_MESSAGE_BYTES: usize = 128 * 1024 * 1024;
/// 单个远程命令的最长等待时间。
const INVOKE_TIMEOUT: Duration = Duration::from_secs(600);
/// 保活间隔。
const PING_INTERVAL: Duration = Duration::from_secs(25);
/// 设备标识最长保留字符数。
const MAX_LABEL_CHARS: usize = 120;
/// 命令结果超过该大小时压缩传输（手机端支持时）。
const COMPRESS_THRESHOLD: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    #[default]
    Disabled,
    Connecting,
    Connected,
    Error,
}

/// 当前在线的已授权手机端连接。
struct Device {
    conn: String,
    /// 媒体文件访问密钥：随连接签发，设备下线或被取代即失效。
    media_key: String,
    /// 手机端支持 gzip 压缩的大结果（浏览器 `DecompressionStream`）。
    gzip: bool,
}

#[derive(Default)]
struct Inner {
    config: RemoteConfig,
    pairing: Option<Pairing>,
    state: LinkState,
    last_error: Option<String>,
    outbound: Option<mpsc::UnboundedSender<AgentToRelay>>,
    device: Option<Device>,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
}

struct Shared {
    root: PathBuf,
    inner: Mutex<Inner>,
    app: OnceLock<AppHandle>,
}

/// 远程访问服务（Tauri 托管状态）。
#[derive(Clone)]
pub struct RemoteService(Arc<Shared>);

/// 设置页展示的远程访问状态。
#[derive(Debug, Clone, Serialize)]
pub struct RemoteView {
    pub enabled: bool,
    pub mode: RemoteMode,
    /// 自定义中继地址（为空表示使用缺省中继）。
    pub host: String,
    /// 缺省中继地址。
    pub default_host: &'static str,
    /// 通道 ID（通道密钥的单向摘要，可公开展示；密钥本身不出桌面端）。
    pub channel: Option<String>,
    /// 局域网直连二维码地址（留空表示自动探测）。
    pub lan_host: String,
    pub lan_port: u16,
    /// 自动探测到的本机局域网 IP。
    pub detected_lan_ip: Option<String>,
    /// 手机端访问地址（扫码打开的根地址）。
    pub access_url: Option<String>,
    pub state: LinkState,
    pub last_error: Option<String>,
    pub device_bound: bool,
    pub device_label: Option<String>,
    pub device_bound_at: Option<String>,
    pub device_online: bool,
}

/// 设置页提交的远程访问配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteConfigInput {
    pub enabled: bool,
    #[serde(default)]
    pub mode: RemoteMode,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub lan_host: String,
    #[serde(default)]
    pub lan_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PairingView {
    pub url: String,
    pub expires_in_secs: u64,
}

/// 手机端 → 桌面端消息（经中继透传）。
#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ClientMsg {
    Hello {
        #[serde(default)]
        pair: Option<String>,
        #[serde(default)]
        device: Option<String>,
        #[serde(default)]
        label: Option<String>,
        /// 是否支持 gzip 压缩的大结果。
        #[serde(default)]
        gzip: bool,
    },
    Invoke {
        id: u64,
        cmd: String,
        #[serde(default)]
        args: Value,
    },
}

/// 按插件清单判定对话侧挂载点。
struct ManifestScope;

impl ManifestScope {
    fn slots_of(plugin_id: &str) -> Vec<(String, String)> {
        tiangong_plugin_runtime::registry::plugin_manifest(plugin_id)
            .map(|manifest| {
                manifest
                    .ui_contributions()
                    .into_iter()
                    .map(|item| (item.id, item.slot))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl PluginScope for ManifestScope {
    fn plugin_allowed(&self, plugin_id: &str) -> bool {
        !plugin_id.is_empty()
            && Self::slots_of(plugin_id)
                .iter()
                .any(|(_, slot)| REMOTE_SLOTS.contains(&slot.as_str()))
    }

    fn contribution_allowed(&self, plugin_id: &str, contribution_id: &str) -> bool {
        !plugin_id.is_empty()
            && Self::slots_of(plugin_id)
                .iter()
                .any(|(id, slot)| id == contribution_id && REMOTE_SLOTS.contains(&slot.as_str()))
    }
}

fn rustls_config() -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    let (added, _) =
        roots.add_parsable_certificates(webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned());
    if added == 0 {
        anyhow::bail!("加载 TLS 根证书失败");
    }
    // 叠加系统信任的根证书：自签/企业 CA、本地 mkcert 等部署方式（https://localhost）。
    let native = rustls_native_certs::load_native_certs();
    let (native_added, _) = roots.add_parsable_certificates(native.certs);
    if !native.errors.is_empty() {
        tracing::debug!(errors = ?native.errors, "读取系统根证书时有部分失败");
    }
    tracing::debug!(native_added, "已加载系统根证书");
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("初始化 TLS 配置失败")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn close_reason(code: u16, reason: &str) -> String {
    match code {
        4003 => "中继拒绝接入：通道密钥无效".to_string(),
        4009 => "该通道已有其他天工桌面端接入（可在设置中重置通道）".to_string(),
        4029 => "中继接入数已满，请联系中继管理员".to_string(),
        _ if reason.is_empty() => format!("中继关闭了连接（{code}）"),
        _ => format!("中继关闭了连接（{code}）：{reason}"),
    }
}

/// 手机端只用于展示，不需要推理签名（仅回传模型续写时使用，长会话中可占数 MB）。
fn strip_reasoning_signatures(value: &mut Value) {
    if let Some(messages) = value.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if let Some(object) = message.as_object_mut() {
                object.remove("reasoning_signature");
            }
        }
    }
}

/// 构造命令结果帧：结果较大且手机端支持时以 gzip + base64 传输（`t: "result_z"`），
/// 长会话加载的数据量通常可降到原来的 1/4 以下。
fn result_frame(id: u64, value: Value, gzip: bool) -> Value {
    if gzip {
        let text = value.to_string();
        if text.len() >= COMPRESS_THRESHOLD {
            if let Some(packed) = gzip_base64(text.as_bytes()) {
                return json!({ "t": "result_z", "id": id, "ok": true, "z": packed });
            }
        }
        return json!({ "t": "result", "id": id, "ok": true, "value": value });
    }
    json!({ "t": "result", "id": id, "ok": true, "value": value })
}

/// 命令失败原因的日志文本：字符串错误原样输出，其他结构序列化后截断。
fn error_text(error: &Value) -> String {
    let text = match error {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    text.chars().take(300).collect()
}

fn gzip_base64(bytes: &[u8]) -> Option<String> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::with_capacity(bytes.len() / 4),
        flate2::Compression::fast(),
    );
    encoder.write_all(bytes).ok()?;
    let packed = encoder.finish().ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(packed))
}

/// 经主 WebView 的 IPC 入口执行命令：与桌面端前端调用走同一条处理链路
/// （命令表、ACL、状态注入完全一致）。
async fn invoke_via_ipc(app: &AppHandle, cmd: String, args: Value) -> Result<Value, Value> {
    use tauri::ipc::{CallbackFn, InvokeBody, InvokeError, InvokeResponse, InvokeResponseBody};

    // 按 webview 标签取主界面：主窗口内嵌了浏览器等子 webview 后不再是「单 webview
    // 窗口」，`get_webview_window("main")` 会返回 None，导致全部远程命令失败。
    let Some(webview) = app.get_webview("main") else {
        return Err(json!("天工主窗口不可用"));
    };
    let url = webview.url().map_err(|error| json!(error.to_string()))?;
    let (tx, rx) = oneshot::channel::<InvokeResponse>();
    let request = tauri::webview::InvokeRequest {
        cmd,
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url,
        body: InvokeBody::Json(args),
        headers: Default::default(),
        invoke_key: app.invoke_key().to_string(),
    };
    webview.on_message(
        request,
        Box::new(move |_webview, _cmd, response, _callback, _error| {
            let _ = tx.send(response);
        }),
    );
    match tokio::time::timeout(INVOKE_TIMEOUT, rx).await {
        Err(_) => Err(json!("执行超时")),
        Ok(Err(_)) => Err(json!("命令未返回结果")),
        Ok(Ok(InvokeResponse::Ok(InvokeResponseBody::Json(text)))) => {
            Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
        }
        Ok(Ok(InvokeResponse::Ok(InvokeResponseBody::Raw(bytes)))) => Ok(json!(
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )),
        Ok(Ok(InvokeResponse::Err(InvokeError(value)))) => Err(value),
    }
}

impl RemoteService {
    pub fn new(root: PathBuf) -> Self {
        let mut config = RemoteConfig::load(&root);
        // 通道密钥由天工自行生成并保存，用户无需配置。
        if config.ensure_token() {
            if let Err(error) = config.save(&root) {
                tracing::warn!(%error, "保存远程访问通道密钥失败");
            }
        }
        Self(Arc::new(Shared {
            root,
            inner: Mutex::new(Inner {
                config,
                ..Default::default()
            }),
            app: OnceLock::new(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn app(&self) -> Option<&AppHandle> {
        self.0.app.get()
    }

    /// setup 阶段调用：注册事件转发并按配置建立中继连接。
    pub fn start(&self, app: AppHandle) {
        if self.0.app.set(app.clone()).is_err() {
            return;
        }
        for event in policy::FORWARDED_EVENTS {
            let service = self.clone();
            app.listen_any(*event, move |payload| {
                service.forward_event(event, payload.payload());
            });
        }
        self.apply();
    }

    /// 按当前配置（重新）建立连接：中继模式直连中继；局域网模式在本机
    /// 局域网端口内嵌运行同一套中继服务，再经回环地址接入（链路逻辑完全复用）。
    fn apply(&self) {
        let mut inner = self.lock();
        if let Some(task) = inner.task.take() {
            task.abort();
        }
        inner.outbound = None;
        inner.device = None;
        inner.last_error = None;
        if !inner.config.enabled {
            inner.state = LinkState::Disabled;
            return;
        }
        inner.state = LinkState::Connecting;
        let service = self.clone();
        match inner.config.mode {
            RemoteMode::Relay => {
                let url = match config::agent_ws_url(inner.config.relay_host()) {
                    Ok(url) => url,
                    Err(error) => {
                        inner.state = LinkState::Error;
                        inner.last_error = Some(error.to_string());
                        return;
                    }
                };
                let token = inner.config.token.trim().to_string();
                inner.task = Some(tauri::async_runtime::spawn(async move {
                    service.run_link(url, token).await;
                }));
            }
            RemoteMode::Lan => {
                let port = inner.config.lan_port();
                let token = inner.config.token.trim().to_string();
                inner.task = Some(tauri::async_runtime::spawn(async move {
                    service.run_lan(port, token).await;
                }));
            }
        }
    }

    /// 局域网直连：监听 `0.0.0.0:<port>` 运行内嵌中继（仅接入一个桌面端），
    /// 桌面端经回环用同一通道密钥接入，手机端访问地址与中继模式同构。
    async fn run_lan(&self, port: u16, token: String) {
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(error) => {
                self.set_state(
                    LinkState::Error,
                    Some(format!(
                        "监听局域网端口 {port} 失败：{error}（端口可能被占用）"
                    )),
                );
                return;
            }
        };
        let agent_url = format!("ws://127.0.0.1:{port}/agent/ws");
        tracing::info!(port, "远程访问：局域网直连已监听");
        // 内嵌中继只服务本机：先占住唯一接入名额，局域网内其他设备无法接入。
        let options = tiangong_relay::RelayOptions { max_agents: 1 };
        tokio::select! {
            result = tiangong_relay::serve_with_listener(listener, options, std::future::pending()) => {
                if let Err(error) = result {
                    self.set_state(LinkState::Error, Some(format!("局域网服务异常退出：{error:#}")));
                }
            }
            _ = self.run_link(agent_url, token) => {}
        }
    }

    fn set_state(&self, state: LinkState, error: Option<String>) {
        let mut inner = self.lock();
        inner.state = state;
        inner.last_error = error;
    }

    async fn run_link(&self, url: String, token: String) {
        let mut backoff = Duration::from_secs(2);
        loop {
            self.set_state(LinkState::Connecting, None);
            let result = self.connect_once(&url, &token).await;
            {
                let mut inner = self.lock();
                inner.outbound = None;
                inner.device = None;
            }
            match result {
                Ok(true) => backoff = Duration::from_secs(2),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, "远程中继连接失败");
                    self.set_state(LinkState::Error, Some(format!("{error:#}")));
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    /// 单次连接；返回值表示是否曾成功接入（用于重置退避）。
    async fn connect_once(&self, url: &str, token: &str) -> Result<bool> {
        let mut request = url.into_client_request().context("中继地址无效")?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}")
                .parse()
                .map_err(|_| anyhow!("接入令牌包含非法字符"))?,
        );
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let connector = if url.starts_with("wss://") {
            Some(tokio_tungstenite::Connector::Rustls(rustls_config()?))
        } else {
            None
        };
        let (socket, _) = tokio_tungstenite::connect_async_tls_with_config(
            request,
            Some(ws_config),
            false,
            connector,
        )
        .await
        .map_err(|error| anyhow!("连接中继失败：{error}"))?;
        let (mut sink, mut stream) = socket.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentToRelay>();
        self.lock().outbound = Some(tx);
        let mut welcomed = false;
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        loop {
            tokio::select! {
                frame = rx.recv() => {
                    let Some(frame) = frame else { break };
                    let text = serde_json::to_string(&frame)?;
                    sink.send(WsMessage::text(text)).await.map_err(|error| anyhow!("发送失败：{error}"))?;
                }
                message = stream.next() => match message {
                    Some(Ok(WsMessage::Text(text))) => match serde_json::from_str::<RelayToAgent>(text.as_str()) {
                        Ok(RelayToAgent::Welcome) => {
                            welcomed = true;
                            self.set_state(LinkState::Connected, None);
                            tracing::info!("已接入远程中继");
                        }
                        Ok(frame) => self.handle_relay(frame),
                        Err(error) => tracing::warn!(%error, "中继帧格式无效"),
                    },
                    Some(Ok(WsMessage::Close(frame))) => {
                        let (code, reason) = frame
                            .map(|frame| (u16::from(frame.code), frame.reason.to_string()))
                            .unwrap_or((1000, String::new()));
                        if code == 1000 && welcomed {
                            break;
                        }
                        anyhow::bail!(close_reason(code, &reason));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => anyhow::bail!("中继连接中断：{error}"),
                    None => break,
                },
                _ = ping.tick() => {
                    let text = serde_json::to_string(&AgentToRelay::Ping)?;
                    sink.send(WsMessage::text(text)).await.map_err(|error| anyhow!("发送失败：{error}"))?;
                }
            }
        }
        Ok(welcomed)
    }

    fn send(&self, frame: AgentToRelay) {
        let tx = self.lock().outbound.clone();
        if let Some(tx) = tx {
            let _ = tx.send(frame);
        }
    }

    fn send_client(&self, conn: &str, value: Value) {
        self.send(AgentToRelay::ToClient {
            conn: conn.to_string(),
            data: value.to_string(),
        });
    }

    fn reject(&self, conn: &str, reason: &str) {
        self.send_client(conn, json!({ "t": "denied", "reason": reason }));
        self.send(AgentToRelay::CloseClient {
            conn: conn.to_string(),
            reason: reason.to_string(),
        });
    }

    fn is_active(&self, conn: &str) -> bool {
        self.lock()
            .device
            .as_ref()
            .is_some_and(|device| device.conn == conn)
    }

    fn device_gzip(&self, conn: &str) -> bool {
        self.lock()
            .device
            .as_ref()
            .is_some_and(|device| device.conn == conn && device.gzip)
    }

    fn handle_relay(&self, frame: RelayToAgent) {
        match frame {
            RelayToAgent::Welcome | RelayToAgent::ClientOpen { .. } => {}
            RelayToAgent::ClientClose { conn } => {
                let mut inner = self.lock();
                if inner
                    .device
                    .as_ref()
                    .is_some_and(|device| device.conn == conn)
                {
                    inner.device = None;
                }
            }
            RelayToAgent::ClientMsg { conn, data } => {
                match serde_json::from_str::<ClientMsg>(&data) {
                    Ok(ClientMsg::Hello {
                        pair,
                        device,
                        label,
                        gzip,
                    }) => self.authenticate(&conn, pair, device, label, gzip),
                    Ok(ClientMsg::Invoke { id, cmd, args }) => {
                        if self.is_active(&conn) {
                            self.dispatch(conn, id, cmd, args);
                        } else {
                            self.reject(&conn, "设备未授权或已在其他设备上登录");
                        }
                    }
                    Err(_) => self.reject(&conn, "无效的请求"),
                }
            }
            RelayToAgent::AssetReq { req, path, query } => self.serve_asset(req, path, query),
        }
    }

    fn authenticate(
        &self,
        conn: &str,
        pair: Option<String>,
        device: Option<String>,
        label: Option<String>,
        gzip: bool,
    ) {
        let label = label
            .map(|label| label.chars().take(MAX_LABEL_CHARS).collect::<String>())
            .filter(|label| !label.trim().is_empty());
        let outcome = {
            let mut inner = self.lock();
            let issued = match (pair.filter(|code| !code.is_empty()), device) {
                (Some(code), _) => {
                    let valid = inner
                        .pairing
                        .as_ref()
                        .is_some_and(|pairing| pairing.matches(&code));
                    if !valid {
                        Err("配对码无效或已过期，请在天工桌面端重新生成二维码")
                    } else {
                        // 配对码只能使用一次；新设备取代旧设备。
                        inner.pairing = None;
                        let token = random_token();
                        inner.config.device_token_sha256 = Some(sha256_hex(&token));
                        inner.config.device_bound_at = Some(
                            chrono::Local::now()
                                .naive_local()
                                .format("%Y-%m-%d %H:%M:%S")
                                .to_string(),
                        );
                        inner.config.device_label = label.clone();
                        if let Err(error) = inner.config.save(&self.0.root) {
                            tracing::warn!(%error, "保存远程设备绑定失败");
                        }
                        Ok(Some(token))
                    }
                }
                (None, Some(token)) if inner.config.device_matches(&token) => Ok(None),
                _ => Err("设备未授权或已被其他设备取代，请在天工桌面端重新扫码"),
            };
            issued.map(|token| {
                let previous = inner
                    .device
                    .take()
                    .map(|device| device.conn)
                    .filter(|previous| previous != conn);
                let media_key = random_token();
                inner.device = Some(Device {
                    conn: conn.to_string(),
                    media_key: media_key.clone(),
                    gzip,
                });
                (token, media_key, previous)
            })
        };
        match outcome {
            Err(reason) => self.reject(conn, reason),
            Ok((token, media_key, previous)) => {
                if let Some(previous) = previous {
                    self.send_client(
                        &previous,
                        json!({ "t": "kicked", "reason": "已在另一处打开远程会话" }),
                    );
                    self.send(AgentToRelay::CloseClient {
                        conn: previous,
                        reason: "replaced".to_string(),
                    });
                }
                self.send_client(
                    conn,
                    json!({ "t": "ready", "device": token, "media_key": media_key }),
                );
                tracing::info!("远程设备已连接");
            }
        }
    }

    fn dispatch(&self, conn: String, id: u64, cmd: String, args: Value) {
        match policy::decide(&cmd, &args, &ManifestScope) {
            Decision::Stub(value) => self.send_client(
                &conn,
                json!({ "t": "result", "id": id, "ok": true, "value": value }),
            ),
            Decision::Deny(reason) => {
                tracing::warn!(%cmd, reason, "远程命令被拒绝");
                self.send_client(
                    &conn,
                    json!({ "t": "result", "id": id, "ok": false, "error": reason }),
                )
            }
            Decision::Allow => {
                let Some(app) = self.app().cloned() else {
                    tracing::warn!(%cmd, "远程命令无法执行：应用句柄未就绪");
                    self.send_client(
                        &conn,
                        json!({ "t": "result", "id": id, "ok": false, "error": "天工桌面端尚未就绪，请稍后重试" }),
                    );
                    return;
                };
                let service = self.clone();
                tauri::async_runtime::spawn(async move {
                    let slim = matches!(cmd.as_str(), "load_session" | "load_session_messages");
                    let started = std::time::Instant::now();
                    let result = invoke_via_ipc(&app, cmd.clone(), args).await;
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    // 只记录命令名、结果、耗时与大小，不记录参数与内容（可能含对话与附件）。
                    match &result {
                        Ok(_) => tracing::info!(%cmd, elapsed_ms, "远程命令完成"),
                        Err(error) => {
                            let error = error_text(error);
                            tracing::warn!(%cmd, elapsed_ms, %error, "远程命令失败");
                        }
                    }
                    // 期间设备被取代则不再回复。
                    if !service.is_active(&conn) {
                        tracing::info!(%cmd, "远程设备已更换，丢弃命令结果");
                        return;
                    }
                    let gzip = service.device_gzip(&conn);
                    let reply = tauri::async_runtime::spawn_blocking(move || match result {
                        Ok(mut value) => {
                            if slim {
                                strip_reasoning_signatures(&mut value);
                            }
                            result_frame(id, value, gzip)
                        }
                        Err(error) => {
                            json!({ "t": "result", "id": id, "ok": false, "error": error })
                        }
                    })
                    .await;
                    match reply {
                        Ok(reply) => service.send_client(&conn, reply),
                        Err(error) => {
                            tracing::warn!(%error, "构造远程命令结果失败");
                            service.send_client(
                                &conn,
                                json!({ "t": "result", "id": id, "ok": false, "error": "构造命令结果失败" }),
                            );
                        }
                    }
                });
            }
        }
    }

    fn forward_event(&self, event: &str, payload: &str) {
        let conn = match self.lock().device.as_ref() {
            Some(device) => device.conn.clone(),
            None => return,
        };
        if !policy::event_forwardable(event, payload, &ManifestScope) {
            return;
        }
        let payload: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
        self.send_client(
            &conn,
            json!({ "t": "event", "event": event, "payload": payload }),
        );
    }

    fn media_key_matches(&self, candidate: &str) -> bool {
        self.lock().device.as_ref().is_some_and(|device| {
            !candidate.is_empty()
                && candidate
                    .as_bytes()
                    .ct_eq(device.media_key.as_bytes())
                    .into()
        })
    }

    fn serve_asset(&self, req: String, path: String, query: String) {
        let Some(app) = self.app().cloned() else {
            return;
        };
        let service = self.clone();
        tauri::async_runtime::spawn(async move {
            let path = path.trim_start_matches('/').to_string();
            let reply = if path == "remote/file" {
                let authorized = files::query_value(&query, "k")
                    .is_some_and(|key| service.media_key_matches(&key));
                match (authorized, files::query_value(&query, "path")) {
                    (true, Some(file)) => {
                        tauri::async_runtime::spawn_blocking(move || files::serve_file(&file))
                            .await
                            .unwrap_or_else(|_| files::AssetReply::not_found())
                    }
                    (true, None) => files::AssetReply::not_found(),
                    (false, _) => files::AssetReply::text(403, "Forbidden"),
                }
            } else {
                files::serve_frontend(&app, &path)
            };
            service.send(AgentToRelay::AssetRes {
                req,
                status: reply.status,
                mime: reply.mime,
                body: base64::engine::general_purpose::STANDARD.encode(reply.body),
                no_store: reply.no_store,
                sandbox: reply.sandbox,
            });
        });
    }

    pub fn view(&self) -> RemoteView {
        let inner = self.lock();
        RemoteView {
            enabled: inner.config.enabled,
            mode: inner.config.mode,
            host: inner.config.host.clone(),
            default_host: config::DEFAULT_RELAY_HOST,
            channel: inner.config.channel(),
            lan_host: inner.config.lan_host.clone(),
            lan_port: inner.config.lan_port(),
            detected_lan_ip: config::detect_lan_ip().map(|ip| ip.to_string()),
            access_url: inner.config.access_url().ok(),
            state: inner.state,
            last_error: inner.last_error.clone(),
            device_bound: inner.config.device_token_sha256.is_some(),
            device_label: inner.config.device_label.clone(),
            device_bound_at: inner.config.device_bound_at.clone(),
            device_online: inner.device.is_some(),
        }
    }

    pub fn set_config(&self, input: RemoteConfigInput) -> Result<RemoteView> {
        let RemoteConfigInput {
            enabled,
            mode,
            host,
            lan_host,
            lan_port,
        } = input;
        // 留空或与缺省一致时存为空：跟随缺省中继地址（后续版本调整缺省值时自动生效）。
        let host = host.trim().to_string();
        let host = if host.is_empty() {
            host
        } else {
            let host = config::normalize_host(&host)?;
            if host == config::DEFAULT_RELAY_HOST {
                String::new()
            } else {
                host
            }
        };
        let lan_host = config::normalize_lan_host(&lan_host)?.unwrap_or_default();
        if lan_port == Some(0) {
            anyhow::bail!("局域网端口不能为 0");
        }
        {
            let mut inner = self.lock();
            let changed_target = inner.config.mode != mode
                || inner.config.host != host
                || inner.config.lan_host != lan_host
                || inner.config.lan_port != lan_port;
            inner.config.enabled = enabled;
            inner.config.mode = mode;
            inner.config.host = host;
            inner.config.lan_host = lan_host;
            inner.config.lan_port = lan_port;
            inner.config.ensure_token();
            if changed_target {
                inner.pairing = None;
            }
            inner.config.save(&self.0.root)?;
        }
        self.apply();
        Ok(self.view())
    }

    /// 重置通道：换一个新的通道密钥（旧访问地址失效，已绑定设备需重新扫码）。
    /// 用于通道地址泄露或被他人占用时。
    pub fn reset_channel(&self) -> Result<RemoteView> {
        let previous = {
            let mut inner = self.lock();
            inner.config.token = random_token();
            inner.config.device_token_sha256 = None;
            inner.config.device_label = None;
            inner.config.device_bound_at = None;
            inner.pairing = None;
            inner.config.save(&self.0.root)?;
            inner.device.take().map(|device| device.conn)
        };
        if let Some(conn) = previous {
            self.reject(&conn, "桌面端已重置远程通道，请重新扫码");
        }
        self.apply();
        Ok(self.view())
    }

    pub fn create_pairing(&self) -> Result<PairingView> {
        let mut inner = self.lock();
        if !inner.config.enabled {
            anyhow::bail!("请先启用远程访问");
        }
        let access_url = inner.config.access_url()?;
        let pairing = Pairing::new();
        let url = config::pairing_url(&access_url, pairing.code());
        let expires_in_secs = pairing.remaining().as_secs();
        inner.pairing = Some(pairing);
        Ok(PairingView {
            url,
            expires_in_secs,
        })
    }

    /// 解除设备绑定：清除令牌摘要并断开在线设备。
    pub fn unbind(&self) -> Result<RemoteView> {
        let previous = {
            let mut inner = self.lock();
            inner.config.device_token_sha256 = None;
            inner.config.device_label = None;
            inner.config.device_bound_at = None;
            inner.pairing = None;
            inner.config.save(&self.0.root)?;
            inner.device.take().map(|device| device.conn)
        };
        if let Some(conn) = previous {
            self.reject(&conn, "桌面端已解除该设备的绑定");
        }
        Ok(self.view())
    }
}

#[tauri::command]
pub fn remote_get_config(state: State<'_, RemoteService>) -> RemoteView {
    state.view()
}

#[tauri::command]
pub fn remote_set_config(
    config: RemoteConfigInput,
    state: State<'_, RemoteService>,
) -> Result<RemoteView, String> {
    state.set_config(config).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn remote_create_pairing(state: State<'_, RemoteService>) -> Result<PairingView, String> {
    state.create_pairing().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn remote_unbind_device(state: State<'_, RemoteService>) -> Result<RemoteView, String> {
    state.unbind().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn remote_reset_channel(state: State<'_, RemoteService>) -> Result<RemoteView, String> {
    state.reset_channel().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn large_results_are_gzipped_for_capable_clients() {
        let big = json!({ "messages": [{ "content": "x".repeat(COMPRESS_THRESHOLD * 2) }] });
        let frame = result_frame(7, big.clone(), true);
        assert_eq!(frame["t"], "result_z");
        assert_eq!(frame["id"], 7);
        let packed = base64::engine::general_purpose::STANDARD
            .decode(frame["z"].as_str().unwrap())
            .unwrap();
        let mut text = String::new();
        flate2::read::GzDecoder::new(packed.as_slice())
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), big);
        assert!(packed.len() < text.len() / 10);

        // 不支持解压的手机端、或结果很小时按原样返回。
        assert_eq!(result_frame(7, big.clone(), false)["t"], "result");
        assert_eq!(
            result_frame(7, json!({ "ok": 1 }), true)["value"],
            json!({ "ok": 1 })
        );
    }

    #[test]
    fn reasoning_signatures_are_stripped() {
        let mut value = json!({
            "id": "s",
            "messages": [
                { "role": "assistant", "content": "a", "reasoning_signature": "sig" },
                { "role": "tool", "content": "b" }
            ]
        });
        strip_reasoning_signatures(&mut value);
        assert_eq!(
            value["messages"],
            json!([{ "role": "assistant", "content": "a" }, { "role": "tool", "content": "b" }])
        );
    }
}
