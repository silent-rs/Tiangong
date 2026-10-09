//! QQ 开放平台 WebSocket Gateway 客户端。
//!
//! QQ Bot 通过 WebSocket 网关建立长连接（出站连接，无需公网 IP），
//! 与飞书 openlark 的接入模型同构。本模块负责 op 消息编解码、
//! 心跳维护、鉴权（Identify）、断线重连（Resume）。
//!
//! ## op 体系（QQ 官方约定）
//! - `op:10` Hello —— 连接后由服务端下发，携带 `d.heartbeat_interval`
//! - `op:1`  Heartbeat —— 客户端按间隔发送，`d` 为最近 `s`（seq）或 null
//! - `op:11` Heartbeat ACK —— 服务端确认心跳
//! - `op:0`  Dispatch —— 事件推送，含 `s`(seq)、`t`(事件类型)、`d`(数据)
//! - `op:2`  Identify —— 客户端鉴权（携带 token + intents）
//! - `op:6`  Resume —— 断线恢复（携带 token + session_id + seq）
//! - `op:7`  Reconnect —— 服务端要求重连
//! - `op:9`  Invalid Session —— 鉴权/恢复失败，需全新 Identify
//!
//! ## intents
//! C2C 与群 @ 消息对应 `GROUP_AND_C2C_EVENT = 1 << 25`。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use super::token::AccessTokenCache;

/// C2C / 群 @ 消息 intent（QQ 官方 `GROUP_AND_C2C_EVENT`）。
pub const INTENT_GROUP_AND_C2C_EVENT: u32 = 1 << 25;

const OPENAPI_BASE: &str = "https://api.sgroup.qq.com";
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Gateway 事件分派回调。所有未知事件类型由调用方自行降级处理。
pub type DispatchHandler = Arc<
    dyn Fn(DispatchEvent) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// 已解析的 Dispatch 事件（op:0）。
#[derive(Debug, Clone)]
pub struct DispatchEvent {
    /// 事件类型，如 `C2C_MESSAGE_CREATE` / `GROUP_AT_MESSAGE_CREATE` / `READY`。
    pub event_type: String,
    /// 事件序号，用于断线重连的 Resume。
    pub seq: Option<u64>,
    /// 原始 payload，由调用方按事件类型进一步解析。
    pub data: Value,
}

/// 会话恢复上下文。
#[derive(Debug, Clone, Default)]
struct SessionState {
    session_id: Option<String>,
    seq: Option<u64>,
}

/// Gateway 客户端运行参数。
pub struct GatewayRunner {
    http: Client,
    token: AccessTokenCache,
    intents: u32,
    handler: DispatchHandler,
    /// 外部停止信号（收到 SIGTERM/SIGINT 时触发）。
    shutdown: Arc<Notify>,
}

impl GatewayRunner {
    pub fn new(
        http: Client,
        token: AccessTokenCache,
        intents: u32,
        handler: DispatchHandler,
        shutdown: Arc<Notify>,
    ) -> Self {
        Self {
            http,
            token,
            intents,
            handler,
            shutdown,
        }
    }

    /// 运行 Gateway 主循环：连接 → 鉴权 → 心跳 → 事件分派 → 断线重连。
    /// 仅在收到外部停止信号时返回。
    pub async fn run(&self) {
        let mut backoff = INITIAL_BACKOFF;
        let mut session = SessionState::default();

        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.notified() => {
                    tracing::info!("QQ Gateway 收到停止信号，退出主循环");
                    return;
                }
                result = self.run_once(&mut session) => {
                    match result {
                        Ok(()) => {
                            tracing::info!("QQ Gateway 连接正常结束");
                            return;
                        }
                        Err(error) => {
                            tracing::warn!("QQ Gateway 连接异常，{}s 后重连: {error}", backoff.as_secs());
                        }
                    }
                }
            }

            tokio::select! {
                biased;
                _ = self.shutdown.notified() => {
                    tracing::info!("QQ Gateway 重连等待期间收到停止信号，退出");
                    return;
                }
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// 单次连接生命周期。
    async fn run_once(&self, session: &mut SessionState) -> Result<()> {
        let gateway_url = resolve_gateway(&self.http, &self.token).await?;
        tracing::info!("QQ Gateway 地址: {gateway_url}");

        let url = Url::parse(&gateway_url).context("解析 QQ Gateway 地址失败")?;
        let mut ws = tokio::select! {
            biased;
            _ = self.shutdown.notified() => return Ok(()),
            result = connect_gateway(&url) => result?,
        };
        tracing::info!("QQ Gateway WebSocket 已连接");

        let heartbeat_interval = receive_hello(&mut ws).await?;
        tracing::info!("QQ Gateway 心跳间隔: {}ms", heartbeat_interval.as_millis());

        // 鉴权或恢复
        let can_resume = session.session_id.is_some() && session.seq.is_some();
        if can_resume {
            tracing::info!(
                "尝试恢复 QQ 会话 session_id={:?} seq={:?}",
                session.session_id,
                session.seq
            );
            send_resume(&mut ws, &self.token, session).await?;
        } else {
            send_identify(&mut ws, &self.token, self.intents).await?;
        }

        // 心跳通过 channel 由主循环代为发送，避免对 stream 的双重借用
        let (outgoing_tx, mut outgoing_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let last_seq = Arc::new(tokio::sync::Mutex::new(session.seq));
        let heartbeat_handle = tokio::spawn(heartbeat_loop(
            heartbeat_interval,
            last_seq.clone(),
            outgoing_tx.clone(),
            self.shutdown.clone(),
        ));

        let mut result_flow = ControlFlow::Continue;
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.notified() => {
                    tracing::info!("QQ Gateway 收到停止信号，正在关闭连接");
                    result_flow = ControlFlow::Stop;
                    break;
                }
                message = outgoing_rx.recv() => {
                    let Some(message) = message else { break; };
                    if let Err(error) = ws.send(message).await {
                        tracing::warn!("QQ Gateway 发送消息失败: {error}");
                        break;
                    }
                }
                message = ws.next() => {
                    let Some(message) = message else {
                        tracing::info!("QQ Gateway 连接已关闭");
                        break;
                    };
                    let text = match message {
                        Ok(Message::Text(text)) => text.to_string(),
                        Ok(Message::Binary(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
                        Ok(Message::Close(reason)) => {
                            tracing::info!("QQ Gateway 收到关闭帧: {reason:?}");
                            break;
                        }
                        Ok(Message::Ping(payload)) => {
                            let _ = ws.send(Message::Pong(payload)).await;
                            continue;
                        }
                        Ok(_) => continue,
                        Err(error) => {
                            tracing::warn!("QQ Gateway 读取消息失败: {error}");
                            break;
                        }
                    };
                    result_flow = handle_inbound_text(
                        &text,
                        last_seq.clone(),
                        &self.handler,
                        session,
                        &outgoing_tx,
                    )
                    .await;
                    if !matches!(result_flow, ControlFlow::Continue) {
                        break;
                    }
                }
            }
        }

        // 通知心跳任务退出
        drop(outgoing_tx);
        let _ = heartbeat_handle.await;
        let _ = ws.close(None).await;

        match result_flow {
            ControlFlow::Stop => Ok(()),
            ControlFlow::ResumeInvalid => {
                session.session_id = None;
                session.seq = None;
                Ok(())
            }
            ControlFlow::Continue | ControlFlow::Reconnect => Ok(()),
        }
    }
}

enum ControlFlow {
    Continue,
    Reconnect,
    ResumeInvalid,
    Stop,
}

/// 处理一条入站文本消息，更新 seq / session 并分派事件。
async fn handle_inbound_text(
    text: &str,
    last_seq: Arc<tokio::sync::Mutex<Option<u64>>>,
    handler: &DispatchHandler,
    session: &mut SessionState,
    outgoing: &tokio::sync::mpsc::UnboundedSender<Message>,
) -> ControlFlow {
    let payload: Payload = match serde_json::from_str(text) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(
                "无法解析 QQ Gateway 消息: {error}, 原文: {}",
                truncate(text, 256)
            );
            return ControlFlow::Continue;
        }
    };

    match payload.op {
        Op::Dispatch => {
            if let Some(seq) = payload.s {
                *last_seq.lock().await = Some(seq);
                session.seq = Some(seq);
            }
            let event_type = payload.t.clone().unwrap_or_default();
            match event_type.as_str() {
                "READY" => {
                    if let Some(session_id) = payload.d.get("session_id").and_then(Value::as_str) {
                        session.session_id = Some(session_id.to_string());
                        tracing::info!("QQ 鉴权成功 session_id={session_id}");
                    }
                    ControlFlow::Continue
                }
                "RESUMED" => {
                    tracing::info!("QQ 会话恢复成功");
                    ControlFlow::Continue
                }
                "" => ControlFlow::Continue,
                _ => {
                    tracing::info!("收到 QQ Gateway 事件: {event_type}");
                    let event = DispatchEvent {
                        event_type,
                        seq: payload.s,
                        data: payload.d,
                    };
                    (handler)(event).await;
                    ControlFlow::Continue
                }
            }
        }
        Op::HeartbeatAck => {
            tracing::debug!("QQ 心跳 ACK");
            ControlFlow::Continue
        }
        Op::Reconnect => {
            tracing::info!("QQ 服务端要求重连（op:7）");
            ControlFlow::Reconnect
        }
        Op::InvalidSession => {
            tracing::warn!("QQ 会话无效（op:9），需要重新鉴权");
            ControlFlow::ResumeInvalid
        }
        Op::Hello => {
            tracing::debug!("QQ Gateway 迟到的 Hello，忽略");
            ControlFlow::Continue
        }
        Op::Heartbeat | Op::Identify | Op::Resume | Op::Unknown(_) => {
            // op:1 在服务端发起时也要求立即回一次心跳
            if matches!(payload.op, Op::Heartbeat) {
                let seq = *last_seq.lock().await;
                let heartbeat = Payload::heartbeat(seq);
                if let Ok(text) = serde_json::to_string(&heartbeat) {
                    let _ = outgoing.send(Message::Text(text.into()));
                }
                ControlFlow::Continue
            } else {
                tracing::debug!("忽略非预期 QQ op 消息: {:?}", payload.op);
                ControlFlow::Continue
            }
        }
    }
}

async fn heartbeat_loop(
    interval: Duration,
    last_seq: Arc<tokio::sync::Mutex<Option<u64>>>,
    outgoing: tokio::sync::mpsc::UnboundedSender<Message>,
    shutdown: Arc<Notify>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 跳过首次立即触发
    ticker.tick().await;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => return,
            _ = ticker.tick() => {}
        }
        let seq = *last_seq.lock().await;
        let payload = Payload::heartbeat(seq);
        let text = match serde_json::to_string(&payload) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!("序列化 QQ 心跳失败: {error}");
                continue;
            }
        };
        if outgoing.send(Message::Text(text.into())).is_err() {
            tracing::debug!("QQ 心跳通道已关闭，结束心跳任务");
            return;
        }
    }
}

type WebSocketStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_gateway(url: &Url) -> Result<WebSocketStream> {
    let connect_result = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async(url.as_str()),
    )
    .await
    .map_err(|_| anyhow!("连接 QQ Gateway 超时"))?;
    let (ws, _) = connect_result.map_err(|error| anyhow!("连接 QQ Gateway 失败: {error}"))?;
    Ok(ws)
}

async fn resolve_gateway(http: &Client, token: &AccessTokenCache) -> Result<String> {
    let access_token = token.get().await?;
    let response = http
        .get(format!("{OPENAPI_BASE}/gateway"))
        .header("Authorization", format!("QQBot {access_token}"))
        .timeout(CONNECT_TIMEOUT)
        .send()
        .await
        .context("请求 QQ Gateway 地址失败")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "获取 QQ Gateway 地址失败（HTTP {status}）: {}",
            truncate(&body, 256)
        );
    }
    let value: Value = serde_json::from_str(&body)
        .with_context(|| format!("解析 QQ Gateway 响应失败: {}", truncate(&body, 256)))?;
    let url = value
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("QQ Gateway 响应缺少 url 字段: {}", truncate(&body, 256)))?;
    if !url.starts_with("wss://") && !url.starts_with("ws://") {
        bail!("QQ Gateway 地址协议无效: {url}");
    }
    Ok(url.to_string())
}

async fn receive_hello(ws: &mut WebSocketStream) -> Result<Duration> {
    let message = tokio::time::timeout(Duration::from_secs(15), ws.next())
        .await
        .map_err(|_| anyhow!("等待 QQ Hello 超时"))?
        .ok_or_else(|| anyhow!("QQ Gateway 在 Hello 前关闭连接"))?
        .map_err(|error| anyhow!("读取 QQ Hello 失败: {error}"))?;
    let text = match message {
        Message::Text(text) => text.to_string(),
        Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        other => bail!("QQ Gateway 首条消息不是文本: {other:?}"),
    };
    let payload: Payload = serde_json::from_str(&text)
        .with_context(|| format!("解析 QQ Hello 失败: {}", truncate(&text, 256)))?;
    if !matches!(payload.op, Op::Hello) {
        bail!("QQ Gateway 首条消息 op 不是 Hello（10）: {:?}", payload.op);
    }
    let interval_ms = payload
        .d
        .get("heartbeat_interval")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            tracing::warn!("QQ Hello 缺少 heartbeat_interval，使用默认值 30s");
            DEFAULT_HEARTBEAT_INTERVAL.as_millis() as u64
        });
    Ok(Duration::from_millis(interval_ms.max(1000)))
}

async fn send_identify(
    ws: &mut WebSocketStream,
    token: &AccessTokenCache,
    intents: u32,
) -> Result<()> {
    let access_token = token.get().await?;
    let payload = Payload::new(
        Op::Identify,
        serde_json::json!({
            "token": format!("QQBot {access_token}"),
            "intents": intents,
            "shard": [0, 1],
        }),
    );
    let text = serde_json::to_string(&payload).context("序列化 Identify 失败")?;
    ws.send(Message::Text(text.into()))
        .await
        .context("发送 QQ Identify 失败")?;
    Ok(())
}

async fn send_resume(
    ws: &mut WebSocketStream,
    token: &AccessTokenCache,
    session: &SessionState,
) -> Result<()> {
    let access_token = token.get().await?;
    let payload = Payload::new(
        Op::Resume,
        serde_json::json!({
            "token": format!("QQBot {access_token}"),
            "session_id": session.session_id.clone().unwrap_or_default(),
            "seq": session.seq.unwrap_or(0),
        }),
    );
    let text = serde_json::to_string(&payload).context("序列化 Resume 失败")?;
    ws.send(Message::Text(text.into()))
        .await
        .context("发送 QQ Resume 失败")?;
    Ok(())
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

// ── 协议数据结构 ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Dispatch,
    Heartbeat,
    Identify,
    Resume,
    InvalidSession,
    Hello,
    HeartbeatAck,
    Reconnect,
    Unknown(u8),
}

impl Op {
    fn from_u64(value: u64) -> Self {
        match value {
            0 => Self::Dispatch,
            1 => Self::Heartbeat,
            2 => Self::Identify,
            6 => Self::Resume,
            7 => Self::Reconnect,
            9 => Self::InvalidSession,
            10 => Self::Hello,
            11 => Self::HeartbeatAck,
            other => Self::Unknown(other as u8),
        }
    }

    fn as_u64(self) -> u64 {
        match self {
            Self::Dispatch => 0,
            Self::Heartbeat => 1,
            Self::Identify => 2,
            Self::Resume => 6,
            Self::Reconnect => 7,
            Self::InvalidSession => 9,
            Self::Hello => 10,
            Self::HeartbeatAck => 11,
            Self::Unknown(other) => other as u64,
        }
    }
}

impl<'de> Deserialize<'de> for Op {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self::from_u64(u64::deserialize(deserializer)?))
    }
}

impl Serialize for Op {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.as_u64())
    }
}

/// Gateway 协议载荷。`d` 统一使用 `Value`（服务端可能下发 null 或对象）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Payload {
    op: Op,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    t: Option<String>,
    #[serde(default)]
    d: Value,
}

impl Payload {
    fn new(op: Op, data: Value) -> Self {
        Self {
            op,
            s: None,
            t: None,
            d: data,
        }
    }

    /// 构造心跳载荷（op:1），`d` 为最近 seq 或 null。
    fn heartbeat(seq: Option<u64>) -> Self {
        let data = seq.map(Value::from).unwrap_or(Value::Null);
        Self::new(Op::Heartbeat, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_roundtrips_known_codes() {
        for code in [0u64, 1, 2, 6, 7, 9, 10, 11] {
            let value = serde_json::to_value(Op::from_u64(code)).unwrap();
            assert_eq!(value, serde_json::json!(code));
        }
    }

    #[test]
    fn unknown_op_is_preserved() {
        assert_eq!(Op::from_u64(99), Op::Unknown(99));
    }

    #[test]
    fn heartbeat_payload_carries_last_seq() {
        let value = serde_json::to_value(Payload::heartbeat(Some(42))).unwrap();
        assert_eq!(value["op"], 1);
        assert_eq!(value["d"], 42);
        assert!(value.get("s").is_none());
    }

    #[test]
    fn heartbeat_without_seq_sends_null() {
        let value = serde_json::to_value(Payload::heartbeat(None)).unwrap();
        assert_eq!(value["op"], 1);
        assert_eq!(value["d"], serde_json::Value::Null);
    }

    #[test]
    fn heartbeat_ack_without_data_is_parsed() {
        let payload: Payload = serde_json::from_str(r#"{"op":11}"#).unwrap();
        assert_eq!(payload.op, Op::HeartbeatAck);
        assert_eq!(payload.d, Value::Null);
    }

    #[test]
    fn hello_payload_decodes_heartbeat_interval() {
        let raw = r#"{"op":10,"d":{"heartbeat_interval":41250}}"#;
        let payload: Payload = serde_json::from_str(raw).unwrap();
        assert_eq!(payload.op, Op::Hello);
        let interval = payload.d.get("heartbeat_interval").and_then(Value::as_u64);
        assert_eq!(interval, Some(41250));
    }

    #[test]
    fn dispatch_ready_event_is_parsed() {
        let raw = r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","user":{"id":"102000"}}}"#;
        let payload: Payload = serde_json::from_str(raw).unwrap();
        assert_eq!(payload.op, Op::Dispatch);
        assert_eq!(payload.s, Some(1));
        assert_eq!(payload.t.as_deref(), Some("READY"));
        assert_eq!(
            payload.d.get("session_id").and_then(Value::as_str),
            Some("abc")
        );
    }

    #[test]
    fn invalid_session_message_is_recognized() {
        let raw = r#"{"op":9,"d":false}"#;
        let payload: Payload = serde_json::from_str(raw).unwrap();
        assert_eq!(payload.op, Op::InvalidSession);
    }

    #[test]
    fn truncation_keeps_chars_boundary() {
        assert_eq!(truncate("你好世界QQ", 4), "你好世界...");
        assert_eq!(truncate("QQ", 4), "QQ");
    }

    #[test]
    fn identify_payload_shape() {
        assert_eq!(INTENT_GROUP_AND_C2C_EVENT, 1 << 25);
        let value = serde_json::to_value(Payload::new(
            Op::Identify,
            serde_json::json!({
                "token": "QQBot xxx",
                "intents": INTENT_GROUP_AND_C2C_EVENT,
                "shard": [0, 1],
            }),
        ))
        .unwrap();
        assert_eq!(value["op"], 2);
        assert_eq!(value["d"]["intents"], INTENT_GROUP_AND_C2C_EVENT);
        assert_eq!(value["d"]["token"], "QQBot xxx");
    }
}
