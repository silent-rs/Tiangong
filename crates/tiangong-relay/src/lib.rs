//! 天工远程访问中继（Relay）。
//!
//! 纯中继：部署时不需要任何令牌或账号配置，同时承担两件事：
//! - **代理能力**：天工桌面端用自己生成的通道密钥反向连到 `/agent/ws`，中继由密钥
//!   派生出通道 ID（[`channel_id`]），每个通道同一时刻只接受一个桌面端；
//! - **H5 访问能力**：手机浏览器打开 `/?c=<通道 ID>` 拿到前端页面（由该通道的桌面端
//!   经隧道提供），再经 `/ws` 建立数据连接，消息原样转发给该桌面端。
//!
//! 通道 ID 是密钥的单向摘要：知道通道 ID 不能冒充桌面端。中继不解析业务、不持有
//! 会话数据：设备配对、单设备绑定与命令白名单全部由桌面端裁决，中继被攻破也无法
//! 越过桌面端的授权检查；不同通道之间的消息、资源响应严格隔离。

pub mod protocol;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use async_channel::Sender;
use async_lock::RwLock;
use base64::Engine;
use sha2::{Digest, Sha256};
use silent::prelude::*;
use silent::ws::{WSHandlerAppend, WebSocketHandler, WebSocketParts};
use tokio::sync::oneshot;
use tungstenite::protocol::WebSocketConfig;

use self::protocol::{AgentToRelay, CLOSE_CODE_AGENT_OFFLINE, CLOSE_CODE_REJECTED, RelayToAgent};

/// 静态资源请求等待桌面端响应的上限。
const ASSET_TIMEOUT: Duration = Duration::from_secs(30);
/// 单条 WebSocket 消息上限：附件以 data URL 传输（50MB 原文件 base64 后约 67MB）。
const MAX_MESSAGE_BYTES: usize = 128 * 1024 * 1024;
/// 通道密钥最短长度。
pub const MIN_SECRET_LEN: usize = 16;
/// 通道 ID 长度（十六进制字符）。
pub const CHANNEL_ID_LEN: usize = 32;
/// 手机端记住通道的 Cookie 名。
pub const CHANNEL_COOKIE: &str = "tg_channel";
/// 缺省最多同时接入的桌面端数量。
pub const DEFAULT_MAX_AGENTS: usize = 256;

/// 桌面端接入令牌无效。
pub const CLOSE_CODE_INVALID_SECRET: u16 = 4003;
/// 该通道已有桌面端在线。
pub const CLOSE_CODE_CHANNEL_BUSY: u16 = 4009;
/// 中继接入数已满。
pub const CLOSE_CODE_RELAY_FULL: u16 = 4029;

/// 由桌面端通道密钥派生通道 ID（桌面端与中继同一算法）。
pub fn channel_id(secret: &str) -> String {
    let digest = Sha256::digest(format!("tiangong-relay/v1:{}", secret.trim()).as_bytes());
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..CHANNEL_ID_LEN]
        .to_string()
}

/// 校验通道密钥强度。
pub fn validate_secret(secret: &str) -> Result<()> {
    if secret.trim().len() < MIN_SECRET_LEN {
        anyhow::bail!("通道密钥至少 {MIN_SECRET_LEN} 个字符");
    }
    Ok(())
}

fn valid_channel(channel: &str) -> bool {
    channel.len() == CHANNEL_ID_LEN
        && channel
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 中继运行参数。
#[derive(Debug, Clone)]
pub struct RelayOptions {
    /// 最多同时接入的桌面端数量。
    pub max_agents: usize,
}

impl Default for RelayOptions {
    fn default() -> Self {
        Self {
            max_agents: DEFAULT_MAX_AGENTS,
        }
    }
}

struct AgentLink {
    id: String,
    tx: Sender<Message>,
}

struct ClientLink {
    channel: String,
    tx: Sender<Message>,
}

struct AssetResponse {
    status: u16,
    mime: String,
    body: Vec<u8>,
    no_store: bool,
    sandbox: bool,
}

struct PendingAsset {
    channel: String,
    tx: oneshot::Sender<AssetResponse>,
}

/// 中继运行期状态。
pub struct Hub {
    options: RelayOptions,
    /// 通道 ID → 在线桌面端。
    agents: Mutex<HashMap<String, AgentLink>>,
    /// 手机端连接 ID → 所属通道与发送端。
    clients: Mutex<HashMap<String, ClientLink>>,
    pending: Mutex<HashMap<String, PendingAsset>>,
}

/// WebSocket 连接上记录的身份（桌面端或手机端）。
#[derive(Clone)]
struct Peer {
    id: String,
    channel: String,
}

impl Hub {
    pub fn new(options: RelayOptions) -> Arc<Self> {
        Arc::new(Self {
            options,
            agents: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// 某通道的桌面端是否在线。
    pub fn agent_online(&self, channel: &str) -> bool {
        self.agents
            .lock()
            .map(|agents| agents.contains_key(channel))
            .unwrap_or(false)
    }

    /// 在线桌面端数量。
    pub fn agent_count(&self) -> usize {
        self.agents.lock().map(|agents| agents.len()).unwrap_or(0)
    }

    /// 发送给某通道的桌面端；不在线时返回 false。
    fn send_agent(&self, channel: &str, frame: &RelayToAgent) -> bool {
        let Ok(text) = serde_json::to_string(frame) else {
            return false;
        };
        let tx = self
            .agents
            .lock()
            .ok()
            .and_then(|agents| agents.get(channel).map(|link| link.tx.clone()));
        match tx {
            Some(tx) => tx.try_send(Message::text(text)).is_ok(),
            None => false,
        }
    }

    /// 发给某条手机端连接；只投递到 `channel` 名下的连接（通道隔离）。
    fn send_client(&self, channel: &str, conn: &str, message: Message) {
        let tx = self.clients.lock().ok().and_then(|clients| {
            clients
                .get(conn)
                .filter(|link| link.channel == channel)
                .map(|link| link.tx.clone())
        });
        if let Some(tx) = tx {
            let _ = tx.try_send(message);
        }
    }

    /// 关闭某通道的全部手机端连接（桌面端离线时调用）。
    fn close_channel_clients(&self, channel: &str, code: u16, reason: &str) {
        let senders: Vec<_> = self
            .clients
            .lock()
            .map(|clients| {
                clients
                    .values()
                    .filter(|link| link.channel == channel)
                    .map(|link| link.tx.clone())
                    .collect()
            })
            .unwrap_or_default();
        for tx in senders {
            let _ = tx.try_send(Message::close_with(code, reason));
        }
    }

    fn handle_agent_frame(&self, channel: &str, frame: AgentToRelay) {
        match frame {
            AgentToRelay::ToClient { conn, data } => {
                self.send_client(channel, &conn, Message::text(data))
            }
            AgentToRelay::CloseClient { conn, reason } => {
                tracing::debug!(%conn, %reason, "桌面端关闭手机端连接");
                // 立即摘除：不等对端完成关闭握手，之后该连接的消息不再转发。
                let tx = self.clients.lock().ok().and_then(|mut clients| {
                    if clients
                        .get(&conn)
                        .is_some_and(|link| link.channel == channel)
                    {
                        clients.remove(&conn).map(|link| link.tx)
                    } else {
                        None
                    }
                });
                if let Some(tx) = tx {
                    let _ = tx.try_send(Message::close_with(CLOSE_CODE_REJECTED, "rejected"));
                }
            }
            AgentToRelay::AssetRes {
                req,
                status,
                mime,
                body,
                no_store,
                sandbox,
            } => {
                let waiter = self.pending.lock().ok().and_then(|mut map| {
                    if map.get(&req).is_some_and(|entry| entry.channel == channel) {
                        map.remove(&req)
                    } else {
                        None
                    }
                });
                if let Some(waiter) = waiter {
                    let body = base64::engine::general_purpose::STANDARD
                        .decode(body.as_bytes())
                        .unwrap_or_default();
                    let _ = waiter.tx.send(AssetResponse {
                        status,
                        mime,
                        body,
                        no_store,
                        sandbox,
                    });
                }
            }
            AgentToRelay::Ping => {}
        }
    }
}

fn peer_of(parts: &WebSocketParts) -> Option<Peer> {
    parts.extensions().get::<Peer>().cloned()
}

fn bearer_token(parts: &WebSocketParts) -> String {
    if let Some(token) = parts.params().get("token") {
        return token.clone();
    }
    parts
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// 从 Cookie 头中读取通道 ID。
fn cookie_channel(headers: &header::HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == CHANNEL_COOKIE)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| valid_channel(value))
}

/// 从查询串中读取通道 ID（`c=`）。
fn query_channel(query: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == "c")
        .map(|(_, value)| value.to_string())
        .filter(|value| valid_channel(value))
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES))
}

fn send_frame(tx: &Sender<Message>, frame: &RelayToAgent) {
    if let Ok(text) = serde_json::to_string(frame) {
        let _ = tx.try_send(Message::text(text));
    }
}

/// 桌面端接入：`GET /agent/ws`，`Authorization: Bearer <通道密钥>`。
fn agent_route(hub: Arc<Hub>) -> Route {
    let connect_hub = hub.clone();
    let receive_hub = hub.clone();
    let close_hub = hub;
    let handler = WebSocketHandler::new()
        .on_connect(
            move |parts: Arc<RwLock<WebSocketParts>>, tx: Sender<Message>| {
                let hub = connect_hub.clone();
                async move {
                    let secret = bearer_token(&*parts.read().await);
                    if validate_secret(&secret).is_err() {
                        tracing::warn!("桌面端通道密钥无效，已拒绝");
                        let _ = tx.try_send(Message::close_with(
                            CLOSE_CODE_INVALID_SECRET,
                            "invalid secret",
                        ));
                        return Ok(());
                    }
                    let channel = channel_id(&secret);
                    let id = scru128::new().to_string();
                    {
                        let Ok(mut agents) = hub.agents.lock() else {
                            return Ok(());
                        };
                        if agents.contains_key(&channel) {
                            // 同一通道只服务一个桌面端：先到者保留，后来者拒绝。
                            tracing::warn!(%channel, "该通道已有桌面端在线，拒绝新的接入");
                            let _ = tx.try_send(Message::close_with(
                                CLOSE_CODE_CHANNEL_BUSY,
                                "agent already online",
                            ));
                            return Ok(());
                        }
                        if agents.len() >= hub.options.max_agents {
                            tracing::warn!("接入数已达上限，拒绝新的桌面端");
                            let _ = tx
                                .try_send(Message::close_with(CLOSE_CODE_RELAY_FULL, "relay full"));
                            return Ok(());
                        }
                        agents.insert(
                            channel.clone(),
                            AgentLink {
                                id: id.clone(),
                                tx: tx.clone(),
                            },
                        );
                    }
                    parts.write().await.extensions_mut().insert(Peer {
                        id,
                        channel: channel.clone(),
                    });
                    send_frame(&tx, &RelayToAgent::Welcome);
                    tracing::info!(%channel, "天工桌面端已接入");
                    Ok(())
                }
            },
        )
        .on_send(|message: Message, _parts: Arc<RwLock<WebSocketParts>>| async move { Ok(message) })
        .on_receive(
            move |message: Message, parts: Arc<RwLock<WebSocketParts>>| {
                let hub = receive_hub.clone();
                async move {
                    // 未登记为当前桌面端的连接（密钥错误/重复接入）不处理任何帧。
                    let Some(peer) = peer_of(&*parts.read().await) else {
                        return Ok(());
                    };
                    let is_current = hub
                        .agents
                        .lock()
                        .map(|agents| {
                            agents
                                .get(&peer.channel)
                                .is_some_and(|link| link.id == peer.id)
                        })
                        .unwrap_or(false);
                    if !is_current {
                        return Ok(());
                    }
                    let Ok(text) = message.to_str() else {
                        return Ok(());
                    };
                    match serde_json::from_str::<AgentToRelay>(text) {
                        Ok(frame) => hub.handle_agent_frame(&peer.channel, frame),
                        Err(error) => tracing::warn!(%error, "桌面端帧格式无效"),
                    }
                    Ok(())
                }
            },
        )
        .on_close(move |parts: Arc<RwLock<WebSocketParts>>| {
            let hub = close_hub.clone();
            async move {
                let Some(peer) = peer_of(&*parts.read().await) else {
                    return;
                };
                let removed = hub
                    .agents
                    .lock()
                    .map(|mut agents| {
                        if agents
                            .get(&peer.channel)
                            .is_some_and(|link| link.id == peer.id)
                        {
                            agents.remove(&peer.channel);
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if removed {
                    tracing::info!(channel = %peer.channel, "天工桌面端已断开");
                    hub.close_channel_clients(
                        &peer.channel,
                        CLOSE_CODE_AGENT_OFFLINE,
                        "agent offline",
                    );
                    if let Ok(mut pending) = hub.pending.lock() {
                        pending.retain(|_, entry| entry.channel != peer.channel);
                    }
                }
            }
        });
    Route::new("agent/ws").ws(Some(ws_config()), handler)
}

/// 手机端数据连接：`GET /ws?c=<通道 ID>`（缺省取 Cookie），消息原样转发给该通道的桌面端。
fn client_route(hub: Arc<Hub>) -> Route {
    let connect_hub = hub.clone();
    let receive_hub = hub.clone();
    let close_hub = hub;
    let handler = WebSocketHandler::new()
        .on_connect(
            move |parts: Arc<RwLock<WebSocketParts>>, tx: Sender<Message>| {
                let hub = connect_hub.clone();
                async move {
                    let channel = {
                        let parts = parts.read().await;
                        parts
                            .params()
                            .get("c")
                            .cloned()
                            .filter(|value| valid_channel(value))
                            .or_else(|| cookie_channel(parts.headers()))
                    };
                    let Some(channel) = channel else {
                        let _ = tx.try_send(Message::close_with(
                            CLOSE_CODE_AGENT_OFFLINE,
                            "missing channel",
                        ));
                        return Ok(());
                    };
                    if !hub.agent_online(&channel) {
                        let _ = tx.try_send(Message::close_with(
                            CLOSE_CODE_AGENT_OFFLINE,
                            "agent offline",
                        ));
                        return Ok(());
                    }
                    let conn = scru128::new().to_string();
                    if let Ok(mut clients) = hub.clients.lock() {
                        clients.insert(
                            conn.clone(),
                            ClientLink {
                                channel: channel.clone(),
                                tx: tx.clone(),
                            },
                        );
                    }
                    parts.write().await.extensions_mut().insert(Peer {
                        id: conn.clone(),
                        channel: channel.clone(),
                    });
                    if !hub.send_agent(&channel, &RelayToAgent::ClientOpen { conn }) {
                        let _ = tx.try_send(Message::close_with(
                            CLOSE_CODE_AGENT_OFFLINE,
                            "agent offline",
                        ));
                    }
                    Ok(())
                }
            },
        )
        .on_send(|message: Message, _parts: Arc<RwLock<WebSocketParts>>| async move { Ok(message) })
        .on_receive(
            move |message: Message, parts: Arc<RwLock<WebSocketParts>>| {
                let hub = receive_hub.clone();
                async move {
                    let Some(peer) = peer_of(&*parts.read().await) else {
                        return Ok(());
                    };
                    // 已被桌面端关闭的连接不再转发。
                    let registered = hub
                        .clients
                        .lock()
                        .map(|clients| clients.contains_key(&peer.id))
                        .unwrap_or(false);
                    if !registered {
                        return Ok(());
                    }
                    let Ok(text) = message.to_str() else {
                        return Ok(());
                    };
                    if !hub.send_agent(
                        &peer.channel,
                        &RelayToAgent::ClientMsg {
                            conn: peer.id.clone(),
                            data: text.to_string(),
                        },
                    ) {
                        hub.send_client(
                            &peer.channel,
                            &peer.id,
                            Message::close_with(CLOSE_CODE_AGENT_OFFLINE, "agent offline"),
                        );
                    }
                    Ok(())
                }
            },
        )
        .on_close(move |parts: Arc<RwLock<WebSocketParts>>| {
            let hub = close_hub.clone();
            async move {
                let Some(peer) = peer_of(&*parts.read().await) else {
                    return;
                };
                let removed = hub
                    .clients
                    .lock()
                    .map(|mut clients| clients.remove(&peer.id).is_some())
                    .unwrap_or(false);
                // 桌面端主动关闭的连接已摘除，无需再通知。
                if removed {
                    hub.send_agent(&peer.channel, &RelayToAgent::ClientClose { conn: peer.id });
                }
            }
        });
    Route::new("ws").ws(Some(ws_config()), handler)
}

fn info_page(status: StatusCode, title: &str, detail: &str) -> Response {
    let mut response = Response::html(&format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"UTF-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>天工远程</title></head>\
         <body style=\"font-family:sans-serif;padding:2rem;text-align:center\"><h3>{title}</h3><p>{detail}</p></body></html>"
    ));
    response.set_status(status);
    response.set_header(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

fn offline_page() -> Response {
    info_page(
        StatusCode::SERVICE_UNAVAILABLE,
        "天工桌面端未在线",
        "请确认桌面端已开启远程访问并连接到本中继。",
    )
}

/// 静态资源：转交通道所属桌面端读取（前端页面、会话媒体文件）。
async fn proxy_asset(hub: Arc<Hub>, req: Request, path: String) -> silent::Result<Response> {
    if path.split('/').any(|segment| segment == "..") {
        return Ok(Response::text("Not Found").with_status(StatusCode::NOT_FOUND));
    }
    let query = req.uri().query().unwrap_or_default().to_string();
    let from_query = query_channel(&query);
    let Some(channel) = from_query.clone().or_else(|| cookie_channel(req.headers())) else {
        return Ok(info_page(
            StatusCode::NOT_FOUND,
            "天工远程中继",
            "请在天工桌面端「设置 → 远程访问」生成二维码后扫码打开。",
        ));
    };
    if !hub.agent_online(&channel) {
        return Ok(offline_page());
    }
    let id = scru128::new().to_string();
    let (tx, rx) = oneshot::channel();
    if let Ok(mut pending) = hub.pending.lock() {
        pending.insert(
            id.clone(),
            PendingAsset {
                channel: channel.clone(),
                tx,
            },
        );
    }
    let frame = RelayToAgent::AssetReq {
        req: id.clone(),
        path,
        query,
    };
    if !hub.send_agent(&channel, &frame) {
        if let Ok(mut pending) = hub.pending.lock() {
            pending.remove(&id);
        }
        return Ok(offline_page());
    }
    let result = tokio::time::timeout(ASSET_TIMEOUT, rx).await;
    if let Ok(mut pending) = hub.pending.lock() {
        pending.remove(&id);
    }
    let Ok(Ok(asset)) = result else {
        return Ok(Response::text("桌面端响应超时").with_status(StatusCode::GATEWAY_TIMEOUT));
    };
    let mut response = Response::empty();
    response.set_status(StatusCode::from_u16(asset.status).unwrap_or(StatusCode::OK));
    response.set_body(full(asset.body));
    if let Ok(mime) = header::HeaderValue::from_str(&asset.mime) {
        response.set_header(header::CONTENT_TYPE, mime);
    }
    if asset.no_store {
        response.set_header(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
    }
    if asset.sandbox {
        response.set_header(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static("sandbox"),
        );
    }
    // 记住通道：页面中的绝对路径资源请求（/assets/...）不带查询参数，靠 Cookie 路由。
    if let Some(channel) = from_query
        && let Ok(cookie) = header::HeaderValue::from_str(&format!(
            "{CHANNEL_COOKIE}={channel}; Path=/; Max-Age=31536000; SameSite=Lax"
        ))
    {
        response.set_header(header::SET_COOKIE, cookie);
    }
    response.set_header(
        header::HeaderName::from_static("x-content-type-options"),
        header::HeaderValue::from_static("nosniff"),
    );
    response.set_header(
        header::HeaderName::from_static("referrer-policy"),
        header::HeaderValue::from_static("no-referrer"),
    );
    Ok(response)
}

/// 构建中继路由。
pub fn routes(hub: Arc<Hub>) -> Route {
    let health_hub = hub.clone();
    let root_hub = hub.clone();
    let asset_hub = hub.clone();
    Route::new_root()
        .append(Route::new("healthz").get(move |req: Request| {
            let hub = health_hub.clone();
            async move {
                let channel = query_channel(req.uri().query().unwrap_or_default())
                    .or_else(|| cookie_channel(req.headers()));
                let mut body = serde_json::json!({
                    "ok": true,
                    "version": env!("CARGO_PKG_VERSION"),
                    "agents": hub.agent_count(),
                });
                if let Some(channel) = channel {
                    body["agent_online"] = serde_json::json!(hub.agent_online(&channel));
                }
                Ok(Response::json(&body))
            }
        }))
        .append(agent_route(hub.clone()))
        .append(client_route(hub))
        .append(Route::new("").get(move |req: Request| {
            let hub = root_hub.clone();
            async move { proxy_asset(hub, req, String::new()).await }
        }))
        .append(Route::new("<path:**>").get(move |req: Request| {
            let hub = asset_hub.clone();
            async move {
                let path: String = req.get_path_params("path")?;
                let path = path.trim_start_matches('/').to_string();
                proxy_asset(hub, req, path).await
            }
        }))
}

/// 在给定监听器上运行中继，直到 `shutdown` 完成。
pub async fn serve_with_listener(
    listener: tokio::net::TcpListener,
    options: RelayOptions,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let hub = Hub::new(options);
    let server = Server::new()
        .listen(Listener::from(listener))
        .with_shutdown(Duration::from_secs(2));
    tokio::select! {
        _ = server.serve(routes(hub)) => {}
        _ = shutdown => {}
    }
    Ok(())
}

/// 中继直接提供 HTTPS 时使用的证书（PEM）。
#[cfg(feature = "tls")]
#[derive(Debug, Clone)]
pub struct TlsFiles {
    pub cert: std::path::PathBuf,
    pub key: std::path::PathBuf,
}

/// 以 HTTPS 在给定监听器上运行中继（无反向代理时使用，如本地 `https://localhost`）。
#[cfg(feature = "tls")]
pub async fn serve_tls_with_listener(
    listener: tokio::net::TcpListener,
    tls: &TlsFiles,
    options: RelayOptions,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let store = silent::CertificateStore::builder()
        .cert_path(&tls.cert)
        .key_path(&tls.key)
        .build()
        .with_context(|| {
            format!(
                "加载 TLS 证书失败（cert={}，key={}）",
                tls.cert.display(),
                tls.key.display()
            )
        })?;
    // 仅 HTTP/1.1：WebSocket 升级依赖 HTTP/1.1。
    let acceptor = store
        .tls_acceptor(&[b"http/1.1"])
        .context("初始化 TLS 失败")?;
    let hub = Hub::new(options);
    let server = Server::new()
        .listen(Listener::from(listener).tls(acceptor))
        .with_shutdown(Duration::from_secs(2));
    tokio::select! {
        _ = server.serve(routes(hub)) => {}
        _ = shutdown => {}
    }
    Ok(())
}

/// 绑定地址并运行中继，直到 `shutdown` 完成。
pub async fn serve(
    addr: SocketAddr,
    options: RelayOptions,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let listener = bind(addr).await?;
    tracing::info!(
        "天工远程中继 v{} 已启动：http://{}",
        env!("CARGO_PKG_VERSION"),
        listener.local_addr()?
    );
    serve_with_listener(listener, options, shutdown).await
}

/// 绑定地址并以 HTTPS 运行中继，直到 `shutdown` 完成。
#[cfg(feature = "tls")]
pub async fn serve_tls(
    addr: SocketAddr,
    tls: TlsFiles,
    options: RelayOptions,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let listener = bind(addr).await?;
    tracing::info!(
        "天工远程中继 v{} 已启动：https://{}",
        env!("CARGO_PKG_VERSION"),
        listener.local_addr()?
    );
    serve_tls_with_listener(listener, &tls, options, shutdown).await
}

async fn bind(addr: SocketAddr) -> Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("监听 {addr} 失败"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_id_is_stable_hex() {
        let id = channel_id("secret-0123456789abcdef");
        assert_eq!(id.len(), CHANNEL_ID_LEN);
        assert!(valid_channel(&id));
        assert_eq!(id, channel_id(" secret-0123456789abcdef "));
        assert_ne!(id, channel_id("secret-0123456789abcdeg"));
    }

    #[test]
    fn channel_parsing() {
        let id = channel_id("secret-0123456789abcdef");
        assert_eq!(query_channel(&format!("v=1&c={id}")), Some(id.clone()));
        assert_eq!(query_channel("c=../../etc"), None);
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::COOKIE,
            header::HeaderValue::from_str(&format!("a=1; {CHANNEL_COOKIE}={id}")).unwrap(),
        );
        assert_eq!(cookie_channel(&headers), Some(id));
        assert!(validate_secret("short").is_err());
    }
}
