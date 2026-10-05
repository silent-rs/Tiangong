//! 天工远程访问中继（Relay）。
//!
//! 部署在公网可达的主机上，同时承担两件事：
//! - **代理能力**：天工桌面端以接入令牌反向连到 `/agent/ws`，中继只接受一个桌面端；
//! - **H5 访问能力**：手机浏览器访问中继根路径拿到前端页面（由桌面端经隧道提供），
//!   再经 `/ws` 建立数据连接，消息原样转发给桌面端。
//!
//! 中继不解析业务、不持有会话数据：设备配对、单设备绑定与命令白名单全部由
//! 桌面端裁决，中继被攻破也无法越过桌面端的授权检查。

pub mod protocol;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use async_channel::Sender;
use async_lock::RwLock;
use base64::Engine;
use silent::prelude::*;
use silent::ws::{WSHandlerAppend, WebSocketHandler, WebSocketParts};
use subtle::ConstantTimeEq;
use tokio::sync::oneshot;
use tungstenite::protocol::WebSocketConfig;

use self::protocol::{AgentToRelay, CLOSE_CODE_AGENT_OFFLINE, CLOSE_CODE_REJECTED, RelayToAgent};

/// 静态资源请求等待桌面端响应的上限。
const ASSET_TIMEOUT: Duration = Duration::from_secs(30);
/// 单条 WebSocket 消息上限：附件以 data URL 传输（50MB 原文件 base64 后约 67MB）。
const MAX_MESSAGE_BYTES: usize = 128 * 1024 * 1024;
/// 接入令牌最短长度。
pub const MIN_TOKEN_LEN: usize = 16;

struct AgentLink {
    id: String,
    tx: Sender<Message>,
}

struct AssetResponse {
    status: u16,
    mime: String,
    body: Vec<u8>,
    no_store: bool,
    sandbox: bool,
}

/// 中继运行期状态。
pub struct Hub {
    token: String,
    agent: Mutex<Option<AgentLink>>,
    clients: Mutex<HashMap<String, Sender<Message>>>,
    pending: Mutex<HashMap<String, oneshot::Sender<AssetResponse>>>,
}

#[derive(Clone)]
struct ConnId(String);

impl Hub {
    pub fn new(token: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            token: token.into(),
            agent: Mutex::new(None),
            clients: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// 桌面端是否在线。
    pub fn agent_online(&self) -> bool {
        self.agent
            .lock()
            .map(|agent| agent.is_some())
            .unwrap_or(false)
    }

    fn token_matches(&self, candidate: &str) -> bool {
        !candidate.is_empty() && candidate.as_bytes().ct_eq(self.token.as_bytes()).into()
    }

    /// 发送给桌面端；不在线时返回 false。
    fn send_agent(&self, frame: &RelayToAgent) -> bool {
        let Ok(text) = serde_json::to_string(frame) else {
            return false;
        };
        let agent = self.agent.lock().ok();
        match agent.as_ref().and_then(|agent| agent.as_ref()) {
            Some(link) => link.tx.try_send(Message::text(text)).is_ok(),
            None => false,
        }
    }

    fn send_client(&self, conn: &str, message: Message) {
        let tx = self
            .clients
            .lock()
            .ok()
            .and_then(|clients| clients.get(conn).cloned());
        if let Some(tx) = tx {
            let _ = tx.try_send(message);
        }
    }

    /// 关闭全部手机端连接（桌面端离线时调用）。
    fn close_all_clients(&self, code: u16, reason: &str) {
        let senders: Vec<_> = self
            .clients
            .lock()
            .map(|clients| clients.values().cloned().collect())
            .unwrap_or_default();
        for tx in senders {
            let _ = tx.try_send(Message::close_with(code, reason));
        }
    }

    fn handle_agent_frame(&self, frame: AgentToRelay) {
        match frame {
            AgentToRelay::ToClient { conn, data } => self.send_client(&conn, Message::text(data)),
            AgentToRelay::CloseClient { conn, reason } => {
                tracing::info!(%conn, %reason, "桌面端关闭手机端连接");
                // 立即摘除：不等对端完成关闭握手，之后该连接的消息不再转发。
                let tx = self
                    .clients
                    .lock()
                    .ok()
                    .and_then(|mut clients| clients.remove(&conn));
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
                let waiter = self
                    .pending
                    .lock()
                    .ok()
                    .and_then(|mut map| map.remove(&req));
                if let Some(waiter) = waiter {
                    let body = base64::engine::general_purpose::STANDARD
                        .decode(body.as_bytes())
                        .unwrap_or_default();
                    let _ = waiter.send(AssetResponse {
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

fn conn_id_of(parts: &WebSocketParts) -> Option<String> {
    parts.extensions().get::<ConnId>().map(|id| id.0.clone())
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

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES))
}

/// 桌面端接入：`GET /agent/ws?token=<接入令牌>`。
fn agent_route(hub: Arc<Hub>) -> Route {
    let connect_hub = hub.clone();
    let receive_hub = hub.clone();
    let close_hub = hub;
    let handler = WebSocketHandler::new()
        .on_connect(
            move |parts: Arc<RwLock<WebSocketParts>>, tx: Sender<Message>| {
                let hub = connect_hub.clone();
                async move {
                    let token = bearer_token(&*parts.read().await);
                    if !hub.token_matches(&token) {
                        tracing::warn!("桌面端接入令牌无效，已拒绝");
                        let _ = tx.try_send(Message::close_with(4003u16, "invalid token"));
                        return Ok(());
                    }
                    let id = scru128::new().to_string();
                    {
                        let Ok(mut agent) = hub.agent.lock() else {
                            return Ok(());
                        };
                        if agent.is_some() {
                            // 同一时刻只服务一个桌面端：先到者保留，后来者拒绝。
                            tracing::warn!("已有桌面端在线，拒绝新的接入");
                            let _ =
                                tx.try_send(Message::close_with(4009u16, "agent already online"));
                            return Ok(());
                        }
                        *agent = Some(AgentLink {
                            id: id.clone(),
                            tx: tx.clone(),
                        });
                    }
                    parts.write().await.extensions_mut().insert(ConnId(id));
                    if let Ok(text) = serde_json::to_string(&RelayToAgent::Welcome) {
                        let _ = tx.try_send(Message::text(text));
                    }
                    tracing::info!("天工桌面端已接入");
                    Ok(())
                }
            },
        )
        .on_send(|message: Message, _parts: Arc<RwLock<WebSocketParts>>| async move { Ok(message) })
        .on_receive(
            move |message: Message, parts: Arc<RwLock<WebSocketParts>>| {
                let hub = receive_hub.clone();
                async move {
                    // 未登记为当前桌面端的连接（令牌错误/重复接入）不处理任何帧。
                    let Some(id) = conn_id_of(&*parts.read().await) else {
                        return Ok(());
                    };
                    let is_current = hub
                        .agent
                        .lock()
                        .map(|agent| agent.as_ref().is_some_and(|link| link.id == id))
                        .unwrap_or(false);
                    if !is_current {
                        return Ok(());
                    }
                    let Ok(text) = message.to_str() else {
                        return Ok(());
                    };
                    match serde_json::from_str::<AgentToRelay>(text) {
                        Ok(frame) => hub.handle_agent_frame(frame),
                        Err(error) => tracing::warn!(%error, "桌面端帧格式无效"),
                    }
                    Ok(())
                }
            },
        )
        .on_close(move |parts: Arc<RwLock<WebSocketParts>>| {
            let hub = close_hub.clone();
            async move {
                let Some(id) = conn_id_of(&*parts.read().await) else {
                    return;
                };
                let removed = hub
                    .agent
                    .lock()
                    .map(|mut agent| {
                        if agent.as_ref().is_some_and(|link| link.id == id) {
                            *agent = None;
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if removed {
                    tracing::info!("天工桌面端已断开");
                    hub.close_all_clients(CLOSE_CODE_AGENT_OFFLINE, "agent offline");
                    if let Ok(mut pending) = hub.pending.lock() {
                        pending.clear();
                    }
                }
            }
        });
    Route::new("agent/ws").ws(Some(ws_config()), handler)
}

/// 手机端数据连接：`GET /ws`，消息原样转发给桌面端。
fn client_route(hub: Arc<Hub>) -> Route {
    let connect_hub = hub.clone();
    let receive_hub = hub.clone();
    let close_hub = hub;
    let handler = WebSocketHandler::new()
        .on_connect(
            move |parts: Arc<RwLock<WebSocketParts>>, tx: Sender<Message>| {
                let hub = connect_hub.clone();
                async move {
                    let conn = scru128::new().to_string();
                    if let Ok(mut clients) = hub.clients.lock() {
                        clients.insert(conn.clone(), tx.clone());
                    }
                    parts
                        .write()
                        .await
                        .extensions_mut()
                        .insert(ConnId(conn.clone()));
                    if !hub.send_agent(&RelayToAgent::ClientOpen { conn: conn.clone() }) {
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
                    let Some(conn) = conn_id_of(&*parts.read().await) else {
                        return Ok(());
                    };
                    // 已被桌面端关闭的连接不再转发。
                    let registered = hub
                        .clients
                        .lock()
                        .map(|clients| clients.contains_key(&conn))
                        .unwrap_or(false);
                    if !registered {
                        return Ok(());
                    }
                    let Ok(text) = message.to_str() else {
                        return Ok(());
                    };
                    if !hub.send_agent(&RelayToAgent::ClientMsg {
                        conn: conn.clone(),
                        data: text.to_string(),
                    }) {
                        hub.send_client(
                            &conn,
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
                let Some(conn) = conn_id_of(&*parts.read().await) else {
                    return;
                };
                let removed = hub
                    .clients
                    .lock()
                    .map(|mut clients| clients.remove(&conn).is_some())
                    .unwrap_or(false);
                // 桌面端主动关闭的连接已摘除，无需再通知。
                if removed {
                    hub.send_agent(&RelayToAgent::ClientClose { conn });
                }
            }
        });
    Route::new("ws").ws(Some(ws_config()), handler)
}

fn offline_page() -> Response {
    let mut response = Response::html(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"UTF-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>天工远程</title></head>\
         <body style=\"font-family:sans-serif;padding:2rem;text-align:center\"><h3>天工桌面端未在线</h3><p>请确认桌面端已开启远程访问并连接到本中继。</p></body></html>",
    );
    response.set_status(StatusCode::SERVICE_UNAVAILABLE);
    response.set_header(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

/// 静态资源：转交桌面端读取（前端页面、会话媒体文件）。
async fn proxy_asset(hub: Arc<Hub>, req: Request, path: String) -> silent::Result<Response> {
    if path.split('/').any(|segment| segment == "..") {
        return Ok(Response::text("Not Found").with_status(StatusCode::NOT_FOUND));
    }
    if !hub.agent_online() {
        return Ok(offline_page());
    }
    let id = scru128::new().to_string();
    let (tx, rx) = oneshot::channel();
    if let Ok(mut pending) = hub.pending.lock() {
        pending.insert(id.clone(), tx);
    }
    let frame = RelayToAgent::AssetReq {
        req: id.clone(),
        path,
        query: req.uri().query().unwrap_or_default().to_string(),
    };
    if !hub.send_agent(&frame) {
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
        .append(Route::new("healthz").get(move |_req: Request| {
            let hub = health_hub.clone();
            async move {
                Ok(Response::json(&serde_json::json!({
                    "ok": true,
                    "agent_online": hub.agent_online(),
                })))
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

/// 校验接入令牌强度。
pub fn validate_token(token: &str) -> Result<()> {
    if token.trim().len() < MIN_TOKEN_LEN {
        anyhow::bail!("接入令牌至少 {MIN_TOKEN_LEN} 个字符");
    }
    Ok(())
}

/// 在给定监听器上运行中继，直到 `shutdown` 完成。
pub async fn serve_with_listener(
    listener: tokio::net::TcpListener,
    token: String,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    validate_token(&token)?;
    let hub = Hub::new(token.trim());
    let server = Server::new()
        .listen(Listener::from(listener))
        .with_shutdown(Duration::from_secs(2));
    tokio::select! {
        _ = server.serve(routes(hub)) => {}
        _ = shutdown => {}
    }
    Ok(())
}

/// 绑定地址并运行中继。
pub async fn serve(addr: SocketAddr, token: String) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("监听 {addr} 失败"))?;
    tracing::info!("天工远程中继已启动：http://{}", listener.local_addr()?);
    serve_with_listener(listener, token, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}
