//! Codex（ChatGPT 账号）的 WebSocket 传输：`wss://…/backend-api/codex/responses`。
//!
//! 对齐 opencode 的传输形态（其 openai provider 默认 `transport: "websocket"`，
//! 上游 `/models` 目录对每个模型标注 `prefer_websockets: true`）：
//! - 请求体：与 HTTP Responses 相同，外加 `type: "response.create"` 包裹（无
//!   `stream` 字段）；
//! - 响应帧：与 SSE 完全相同的事件流（`response.created` → … →
//!   `response.completed`），每帧一个 JSON 文本消息；
//! - 鉴权与身份头随握手发送（含 `session-id` 会话亲和头）。
//!
//! 连接管理：**按会话持久化**（键 = 会话 id）。长连接把同一会话的请求钉在同一
//! 服务节点，是块级前缀缓存命中的路由基础。请求在连接上串行执行：发送
//! `response.create` → 事件流消费到 clean 终态（`response.completed`）→ 流
//! Drop 时把连接放回池中复用；流被提前放弃（取消/超时）、连接出错，或终态为
//! `error`/`failed`/`incomplete` 等非 clean 形态（服务端可能随即可关连接）时
//! 丢弃连接，下次请求重建；闲置 [`WS_IDLE_TTL`] 的池条目惰性回收。会话内
//! ReAct 循环天然串行，与单连接串行复用一致。
//!
//! 本传输是 Codex 的**唯一**请求通道：连接失败不降级 HTTP（保持传输形态一致，
//! 避免打断会话亲和与前缀缓存路由），重试与重建全在 WS 上完成，见
//! `provider::stream_via_ws`。无会话 id 时使用不入池的
//! 一次性连接，避免不同会话共享同一条连接。
//!
//! 传输层用 `tokio-tungstenite`（rustls）。此前曾用 `wreq`（BoringSSL）逐项伪装
//! Bun 1.4.4 的 TLS 指纹，依据是「rustls 指纹落入上游降载桶」的假设；后续实测
//! 证伪——不稳定与缓存 miss 的根因是重试不够完善（#766 已在消费层修复），
//! TLS 指纹并非决定性信号（见 `docs/codex-direct-cache-analysis.md` 勘误）。
//! wreq/BoringSSL 因此整体移除：它与 SQLCipher 的 vendored OpenSSL 存在
//! `BIO_*` 等公共符号冲突，阻塞 memory 插件（rusqlite）链接。身份头
//! （originator/UA/session-id）保持不变。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;
use std::time::{Duration, Instant};

use futures_util::SinkExt;
use futures_util::Stream;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderName;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::error::LlmError;

/// 空闲连接回收阈值。上游对闲置长连接有自己的超时（实测约几分钟），
/// 客户端主动回收可避免把请求发到半死连接上。
const WS_IDLE_TTL: Duration = Duration::from_secs(240);
/// WS 握手超时（连接 + TLS + upgrade 握手整体）。
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 事件流载荷与 HTTP SSE 路径同构（`Result<Value, OpenAIError>`），
/// 复用 `map_responses_stream` 的全部解析逻辑。
pub(crate) type WsEventStream =
    Pin<Box<dyn Stream<Item = Result<Value, async_openai::error::OpenAIError>> + Send>>;

/// 一次 WS 请求的返回：事件流在终态事件后自然结束，连接随之归还连接池。
pub(crate) struct WsResponse {
    pub events: WsEventStream,
}

struct PoolEntry {
    client: Arc<AsyncMutex<Option<WsConnection>>>,
    last_used: Instant,
}

/// 进程级连接池：`session` → 连接槽。provider 是按请求构建的轻对象，
/// 持久连接必须挂在模块级。
static WS_POOL: OnceLock<Mutex<HashMap<String, PoolEntry>>> = OnceLock::new();

fn pool() -> &'static Mutex<HashMap<String, PoolEntry>> {
    WS_POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 已建立的 WS 连接。请求在其上串行执行；不绑定单个请求。
pub(crate) struct WsConnection {
    ws: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
}

impl WsConnection {
    async fn connect(url: &str, headers: &[(String, String)]) -> Result<Self, LlmError> {
        let mut request = url
            .into_client_request()
            .map_err(|err| LlmError::Transport(format!("Codex WS URL 无效：{err}")))?;
        let request_headers = request.headers_mut();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|err| {
                LlmError::Transport(format!("Codex WS 握手头无效（{name}）：{err}"))
            })?;
            let value = HeaderValue::from_str(value).map_err(|err| {
                LlmError::Transport(format!("Codex WS 握手头值无效（{name}）：{err}"))
            })?;
            request_headers.insert(name, value);
        }
        let (ws, _response) = tokio::time::timeout(WS_CONNECT_TIMEOUT, connect_async(request))
            .await
            .map_err(|_| LlmError::Timeout(WS_CONNECT_TIMEOUT.as_millis() as u64))?
            .map_err(|err| LlmError::Transport(format!("Codex WS 连接失败：{err}")))?;
        tracing::info!(url, "Codex WS 连接已建立");
        Ok(Self { ws })
    }
}

/// 单帧的处置结论。
enum FrameOutcome {
    /// 可继续消费的帧；`terminal` 表示这是终态帧（此后流结束，连接可复用）。
    Event { value: Value, terminal: bool },
    /// 连接不可再用：丢弃连接，向上层产出一条流错误。
    Broken(async_openai::error::OpenAIError),
}

/// 把一帧 WS 消息映射为事件流载荷；顺带判定连接是否仍可复用。
fn classify_frame(message: Message) -> FrameOutcome {
    match message {
        Message::Text(text) => match serde_json::from_str::<Value>(text.as_str()) {
            Ok(value) => FrameOutcome::Event {
                terminal: is_terminal_event(&value),
                value,
            },
            Err(err) => {
                FrameOutcome::Broken(frame_error(format!("Codex WS 帧不是合法 JSON：{err}")))
            }
        },
        // 底层自动应答 Ping；Pong 与控制帧对上层不可见。
        Message::Ping(_) | Message::Pong(_) => FrameOutcome::Event {
            value: Value::Null,
            terminal: false,
        },
        Message::Close(frame) => {
            let detail = frame
                .map(|frame| format!("{:?} {}", frame.code, frame.reason.as_str()))
                .unwrap_or_else(|| "无".to_string());
            FrameOutcome::Broken(frame_error(format!("Codex WS 连接关闭：{detail}")))
        }
        Message::Binary(bytes) => FrameOutcome::Broken(frame_error(format!(
            "Codex WS 意外的二进制帧（{} 字节）",
            bytes.len()
        ))),
        // tungstenite 读路径不会产出裸帧，穷尽匹配兜底。
        Message::Frame(_) => FrameOutcome::Broken(frame_error("Codex WS 意外的裸帧".to_string())),
    }
}

/// 事件流：代理 WS 帧到 SSE 同构的 `Result<Value, OpenAIError>`。
///
/// 终态帧（`response.completed`/`failed`/`incomplete`/`error`）产出一帧后结束。
/// 只有 **clean 终态**（`response.completed`）消费完毕才在 Drop 中把连接放回
/// 池槽；提前放弃、连接出错或终态为 `error`/`failed`/`incomplete`（服务端可能
/// 随即可关连接）都丢弃连接。`key` 为 `None` 表示不入池的一次性连接（无会话
/// id 时）。
/// 帧级超时不在此实现——连接半开挂起的兜底由上层 turn 取消/超时控制。
struct WsFrameStream {
    conn: Option<WsConnection>,
    slot: Option<Arc<AsyncMutex<Option<WsConnection>>>>,
    key: Option<String>,
    /// 已产出终态帧；此后下一次 poll 返回 None。
    finished: bool,
    /// 终态为 `response.completed`：唯一可安全归还池中的结束形态。
    clean: bool,
}

impl WsFrameStream {
    /// 丢弃本条连接对应的池条目（连接状态未知时调用）。
    fn discard(&mut self) {
        if let Some(key) = self.key.as_deref() {
            discard_connection(key);
        }
    }
}

impl Stream for WsFrameStream {
    type Item = Result<Value, async_openai::error::OpenAIError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        let Some(conn) = this.conn.as_mut() else {
            return Poll::Ready(None);
        };
        match Stream::poll_next(Pin::new(&mut conn.ws), cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                this.finished = true;
                this.discard();
                Poll::Ready(Some(Err(frame_error(
                    "Codex WS 连接被服务端关闭".to_string(),
                ))))
            }
            Poll::Ready(Some(Err(err))) => {
                this.finished = true;
                this.discard();
                Poll::Ready(Some(Err(frame_error(format!("Codex WS 帧错误：{err}")))))
            }
            Poll::Ready(Some(Ok(message))) => match classify_frame(message) {
                FrameOutcome::Event { value, terminal } => {
                    this.finished = terminal;
                    if terminal {
                        this.clean = is_clean_terminal(&value);
                    }
                    Poll::Ready(Some(Ok(value)))
                }
                FrameOutcome::Broken(err) => {
                    this.finished = true;
                    this.discard();
                    Poll::Ready(Some(Err(err)))
                }
            },
        }
    }
}

impl Drop for WsFrameStream {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else {
            return;
        };
        // 仅 clean 终态（response.completed 消费完毕）归还；提前放弃（取消/
        // 超时）、连接出错（池条目已移除，重复移除是幂等 no-op）或
        // error/failed/incomplete 终态都视为连接不可信，随本流销毁。
        if !self.finished || !self.clean {
            self.discard();
            return;
        }
        let (Some(key), Some(slot)) = (self.key.as_deref(), self.slot.as_ref()) else {
            // 一次性连接：随流结束丢弃。
            return;
        };
        // 正常结束：连接放回池槽。
        if let Ok(mut guard) = slot.try_lock()
            && guard.is_none()
        {
            *guard = Some(conn);
            touch_connection(key);
            tracing::debug!(key, "Codex WS 连接已归还池中");
        } else {
            // 槽位被占或锁忙：放弃归还（池条目按需重建）。
            discard_connection(key);
        }
    }
}

pub(super) fn is_terminal_event(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("response.completed")
            | Some("response.failed")
            | Some("response.incomplete")
            | Some("error")
    )
}

/// 终态帧中只有 `response.completed` 视为 clean：连接已按协议正常收尾。
/// `error`/`failed`/`incomplete` 之后服务端可能立即关闭连接，归还复用只会把
/// 下一次请求发到半死连接上。
fn is_clean_terminal(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("response.completed")
}

fn frame_error(message: String) -> async_openai::error::OpenAIError {
    async_openai::error::OpenAIError::StreamError(Box::new(
        async_openai::error::StreamError::EventStream(message),
    ))
}

/// 取（或建）连接槽。池条目与连接分离：请求周期内槽位为空，归还时填回。
fn connection_slot(key: &str) -> Arc<AsyncMutex<Option<WsConnection>>> {
    let mut pool = pool().lock().expect("WS 连接池锁");
    if let Some(entry) = pool.get(key) {
        return entry.client.clone();
    }
    let slot = Arc::new(AsyncMutex::new(None));
    pool.insert(
        key.to_string(),
        PoolEntry {
            client: slot.clone(),
            last_used: Instant::now(),
        },
    );
    slot
}

fn touch_connection(key: &str) {
    if let Some(entry) = pool().lock().expect("WS 连接池锁").get_mut(key) {
        entry.last_used = Instant::now();
    }
}

fn discard_connection(key: &str) {
    pool().lock().expect("WS 连接池锁").remove(key);
}

/// 在会话连接上发起一次请求（单次尝试，重试与重建由调用方掌握）。
///
/// `payload` 为完整请求体（含 `type: "response.create"` 包裹，由调用方改写）。
/// 连接级失败（握手/发送）返回 `Err`；请求级错误（流内 error 事件等）经事件流
/// 产出，与 SSE 路径的语义一致。`key` 为 `None` 时使用不入池的一次性连接。
pub(crate) async fn request(
    key: Option<&str>,
    url: &str,
    headers: &[(String, String)],
    payload: Value,
) -> Result<WsResponse, LlmError> {
    // 惰性回收超时条目。
    pool()
        .lock()
        .expect("WS 连接池锁")
        .retain(|_, entry| entry.last_used.elapsed() < WS_IDLE_TTL);

    let Some(key) = key else {
        let conn = WsConnection::connect(url, headers).await?;
        tracing::debug!("Codex WS 建立一次性连接（无会话 id，不入池）");
        return send_on(conn, None, None, payload).await;
    };
    let slot = connection_slot(key);
    let mut guard = slot.lock().await;
    let reused = guard.is_some();
    if !reused {
        *guard = Some(WsConnection::connect(url, headers).await?);
    }
    let conn = guard.take().expect("连接槽在锁内刚填充");
    drop(guard);
    tracing::debug!(key, reused, "Codex WS 连接就绪");
    send_on(conn, Some(slot), Some(key.to_string()), payload).await
}

/// 在已建立的连接上发送请求体并交出事件流；发送失败则丢弃池条目。
async fn send_on(
    mut conn: WsConnection,
    slot: Option<Arc<AsyncMutex<Option<WsConnection>>>>,
    key: Option<String>,
    payload: Value,
) -> Result<WsResponse, LlmError> {
    // 复用连接的握手头已随首次连接发送；池键含会话 id，会话内头一致。
    if let Err(err) = conn.ws.send(Message::text(payload.to_string())).await {
        if let Some(key) = key.as_deref() {
            discard_connection(key);
        }
        return Err(LlmError::Transport(format!("Codex WS 发送失败：{err}")));
    }
    Ok(WsResponse {
        events: Box::pin(WsFrameStream {
            conn: Some(conn),
            slot,
            key,
            finished: false,
            clean: false,
        }),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn terminal_and_clean_classification() {
        let completed = json!({ "type": "response.completed" });
        assert!(super::is_terminal_event(&completed));
        assert!(super::is_clean_terminal(&completed));
        for event_type in ["response.failed", "response.incomplete", "error"] {
            let value = json!({ "type": event_type });
            assert!(super::is_terminal_event(&value), "{event_type}");
            // 非 clean 终态：连接不归还池中
            assert!(!super::is_clean_terminal(&value), "{event_type}");
        }
        let delta = json!({ "type": "response.output_text.delta", "delta": "x" });
        assert!(!super::is_terminal_event(&delta));
        assert!(!super::is_clean_terminal(&delta));
    }
}
