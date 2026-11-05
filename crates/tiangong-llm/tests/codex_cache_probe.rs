//! Codex 直连的块级缓存探针（手动运行，不参与常规测试）。
//!
//! 用于回答「对话里带图片时缓存是否还命中」这类问题：同一会话 append-only 四轮，
//! 打印每轮的 `input_tokens` / `cached_tokens`。命中判据是「从第二轮起 cached
//! 覆盖上一轮全部输入并随上下文按 128 token 块递增」。
//!
//! ```bash
//! TIANGONG_CODEX_AUTH=/tmp/tiangong-root/auth/codex.json \
//! PROBE_VARIANT=image \
//! cargo test -p tiangong-llm --test codex_cache_probe -- --ignored --nocapture
//! ```
//!
//! `PROBE_VARIANT`：`text`（纯文本基线）/ `image`（第 2 轮插入一张图）/
//! `image2`（第 2、4 轮插入同一张图）。凭据文件默认读 `TIANGONG_STORAGE_ROOT`
//! 下的 `auth/codex.json`，可用 `TIANGONG_CODEX_AUTH` 直接指定。
//!
//! 注意：本文件的握手头与传输形态必须与 `src/providers/codex/ws.rs` 保持
//! 一致，否则探针测到的就不是生产链路的形态。

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const MODEL: &str = "gpt-5.6-terra";
const ENDPOINT: &str = "wss://chatgpt.com/backend-api/codex/responses";
/// 与生产侧 [`crate::providers::codex::ws`] 的握手超时一致。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

struct Turn {
    text: String,
    cached: u64,
    input_tokens: u64,
}

/// 与生产链路同形态的 WS 连接：tokio-tungstenite（rustls）+ 相同握手头。
async fn connect(
    token: &str,
    account: &str,
    session: &str,
) -> Result<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, String> {
    let mut request = ENDPOINT
        .into_client_request()
        .map_err(|err| format!("url {err}"))?;
    let headers = request.headers_mut();
    for (key, value) in [
        ("Authorization", format!("Bearer {token}")),
        ("originator", "tiangong".to_string()),
        ("session-id", session.to_string()),
        ("session_id", session.to_string()),
        ("ChatGPT-Account-Id", account.to_string()),
        (
            "user-agent",
            format!("tiangong/{}", env!("CARGO_PKG_VERSION")),
        ),
    ] {
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|err| format!("header {err}"))?;
        let value = HeaderValue::from_str(&value).map_err(|err| format!("header {err}"))?;
        headers.insert(name, value);
    }
    let (ws, _response) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(request))
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|err| format!("connect {err}"))?;
    Ok(ws)
}

/// 读取测试账号凭据：`TIANGONG_CODEX_AUTH` 优先，其次 `TIANGONG_STORAGE_ROOT`。
fn credentials() -> (String, String) {
    let path = std::env::var("TIANGONG_CODEX_AUTH").unwrap_or_else(|_| {
        let root = std::env::var("TIANGONG_STORAGE_ROOT")
            .unwrap_or_else(|_| "/tmp/tiangong-root".to_string());
        format!("{root}/auth/codex.json")
    });
    let raw = std::fs::read_to_string(&path).expect("读取 Codex 凭据");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("解析 Codex 凭据");
    let token = value["access_token"]
        .as_str()
        .expect("凭据缺少 access_token")
        .to_string();
    let account = value["account_id"]
        .as_str()
        .expect("凭据缺少 account_id")
        .to_string();
    (token, account)
}

fn user_text(text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}]
    })
}

fn user_with_image(text: &str, data_url: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [
            {"type": "input_text", "text": text},
            {"type": "input_image", "image_url": data_url}
        ]
    })
}

async fn one_turn(
    token: &str,
    account: &str,
    session: &str,
    salt: &str,
    input: &serde_json::Value,
) -> Result<Turn, String> {
    let mut ws = connect(token, account, session).await?;
    let payload = serde_json::json!({
        "type": "response.create",
        "model": MODEL,
        "instructions": format!("{salt}你是一个严谨的助手，需要逐步推理并给出简洁结论。"),
        "input": input,
        "store": false,
        "tools": [],
        "prompt_cache_key": session,
        "reasoning": {"effort": "medium", "summary": "auto"},
        "include": ["reasoning.encrypted_content"]
    });
    ws.send(Message::text(payload.to_string()))
        .await
        .map_err(|err| err.to_string())?;

    let mut text = String::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(120), ws.next()).await {
            Err(_) => return Err("timeout".into()),
            Ok(None) => return Err("closed".into()),
            Ok(Some(Err(err))) => return Err(format!("frame {err}")),
            Ok(Some(Ok(message))) => {
                let raw = match &message {
                    Message::Text(text) => text.as_str().to_string(),
                    _ => continue,
                };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
                    continue;
                };
                match value["type"].as_str().unwrap_or("?") {
                    "error" => return Err("overload".into()),
                    "response.failed" => return Err("failed".into()),
                    "response.output_text.delta" => {
                        text.push_str(value["delta"].as_str().unwrap_or(""));
                    }
                    "response.completed" => {
                        let usage = &value["response"]["usage"];
                        return Ok(Turn {
                            text,
                            cached: usage["input_tokens_details"]["cached_tokens"]
                                .as_u64()
                                .unwrap_or(0),
                            input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                        });
                    }
                    _ => {}
                }
            }
        }
    }
}

/// 1×1 透明 PNG：用于验证「是否只要出现图片就失效」，与体积无关。
const TINY_PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==";

/// 同一会话跑四轮 append-only，打印每轮缓存命中；`image_turns` 指定哪几轮带图。
async fn run(label: &str, image_turns: &[u32], tiny: bool, nudge: bool) {
    let (token, account) = credentials();
    let session = format!(
        "ses_probe_{}_{}",
        label,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    // 运行内固定的唯一前缀：只有「刚写入就可见」才会在后面几轮命中
    let salt = vec![format!("本次会话唯一标记 {session} 号。"); 60].join("");
    let image = if tiny {
        TINY_PNG_DATA_URL.to_string()
    } else {
        let image_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/tiangong-plugin-computer-use/resources/virtual-cursor.png");
        let png = std::fs::read(&image_path)
            .unwrap_or_else(|err| panic!("读取测试图片 {} 失败：{err}", image_path.display()));
        format!(
            "data:image/png;base64,{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png)
        )
    };

    let mut history: Vec<serde_json::Value> = Vec::new();
    for turn in 1..=4u32 {
        let item = if image_turns.contains(&turn) {
            user_with_image(&format!("第 {turn} 轮：这张图里是什么形状？"), &image)
        } else {
            user_text(&format!("第 {turn} 轮：用一句话说明第 {turn} 个质数是什么"))
        };
        history.push(item);
        let input = serde_json::Value::Array(history.clone());
        let mut result = Err("init".to_string());
        for attempt in 0..8u64 {
            result = one_turn(&token, &account, &session, &salt, &input).await;
            if result.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2 + attempt * 2)).await;
        }
        match result {
            Ok(done) => {
                let ratio = if done.input_tokens == 0 {
                    0.0
                } else {
                    done.cached as f64 / done.input_tokens as f64 * 100.0
                };
                println!(
                    "[{label}] turn{turn} input={} cached={} ({ratio:.0}%) text={}",
                    done.input_tokens,
                    done.cached,
                    done.text.trim()
                );
                history.push(serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": done.text}]
                }));
            }
            Err(err) => panic!("[{label}] turn{turn} 失败 {err}"),
        }
        if nudge {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}

#[tokio::test]
#[ignore = "手动探针：需要联网 + 真实 Codex 凭据"]
async fn codex_cache_probe() {
    let variant = std::env::var("PROBE_VARIANT").unwrap_or_else(|_| "text".to_string());
    let nudge = std::env::var("PROBE_NUDGE").is_ok();
    match variant.as_str() {
        "text" => run("纯文本", &[], false, nudge).await,
        "image" => run("带图", &[2], false, nudge).await,
        "image2" => run("带图两轮", &[2, 4], false, nudge).await,
        "first" => run("首轮带图", &[1], false, nudge).await,
        "tiny" => run("极小图", &[2], true, nudge).await,
        other => panic!("未知 PROBE_VARIANT={other}"),
    }
}
