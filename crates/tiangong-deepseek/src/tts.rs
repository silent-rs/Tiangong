use bytes::Bytes;
use futures_util::{SinkExt, StreamExt, TryStreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::client::DeepSeekClient;
use crate::error::DeepSeekError;
use crate::types::{
    MAX_TTS_BILLING_CHARS, MAX_TTS_BODY_BYTES, TtsAudioFormat, TtsFinishEvent, TtsRequest,
    TtsStreamEvent, TtsVoice,
};

const TTS_PATH: &str = "/v1/tools/tts";

/// 语音合成（`POST /v1/tools/tts` 与 `wss://api.deepseek.com/v1/tools/tts`）。
///
/// 支持两种方式：
/// - 整段合成：文本一次性发送，音频在响应体中边合成边返回；
/// - WebSocket 流式：分段发送文本（例如模型仍在生成时），音频按帧返回。
pub struct Tts<'c> {
    client: &'c DeepSeekClient,
}

impl<'c> Tts<'c> {
    pub fn new(client: &'c DeepSeekClient) -> Self {
        Self { client }
    }

    /// 整段合成：返回音频字节流（`wav` 为 audio/wav 容器；
    /// `pcm` 为 24 kHz、16 位、单声道原始 PCM）。
    ///
    /// 音频边合成边下发，单次请求最多 30 分钟音频；如果音频已开始返回后
    /// 合成失败，流会中途结束（`Err`），请把提前结束的结果视为失败。
    pub async fn synthesize_stream(
        &self,
        request: TtsRequest,
    ) -> Result<impl futures_util::Stream<Item = Result<Bytes, DeepSeekError>> + 'c, DeepSeekError>
    {
        validate_tts_request(&request)?;
        let response = self
            .client
            .post_tools_stream_raw(TTS_PATH, &request)
            .await?;
        Ok(response
            .bytes_stream()
            .map_err(|err| DeepSeekError::Stream(err.to_string())))
    }

    /// 整段合成的便捷方法：聚合完整音频字节。
    pub async fn synthesize(&self, request: TtsRequest) -> Result<Vec<u8>, DeepSeekError> {
        let mut audio = Vec::new();
        let mut stream = self.synthesize_stream(request).await?;
        while let Some(chunk) = stream.next().await {
            audio.extend_from_slice(&chunk?);
        }
        Ok(audio)
    }

    /// 建立 WebSocket 流式合成会话。
    ///
    /// 鉴权使用 `Authorization: Bearer <API Key>` 请求头。
    /// 之后按序调用 [`TtsStreamSession::send_text`] 发送文本分片，
    /// 最后调用 [`TtsStreamSession::finish_text`]，再循环
    /// [`TtsStreamSession::next_event`] 读取音频帧直至 `Finish` 事件。
    ///
    /// 限制：60 秒内没有收到文字时会话结束（`input_timeout`）；
    /// 单次会话最长 30 分钟、最多 10,000 个计费字符、64 KiB 文本。
    pub async fn stream_session(
        &self,
        voice: TtsVoice,
        format: TtsAudioFormat,
        filter_markdown: Option<bool>,
    ) -> Result<TtsStreamSession, DeepSeekError> {
        let url = self.client.build_tools_ws_url(
            TTS_PATH,
            &[
                ("voice_id", voice.as_str()),
                ("format", format.as_str()),
                (
                    "filter_markdown",
                    match filter_markdown {
                        Some(true) => "true",
                        Some(false) => "false",
                        None => "", // 服务端默认 true，省略参数
                    },
                ),
            ],
        );
        let mut request = url
            .into_client_request()
            .map_err(|err| DeepSeekError::InvalidRequest(format!("invalid TTS ws url: {err}")))?;
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Bearer {}", self.client.bearer_token())
                .parse()
                .map_err(|_| DeepSeekError::InvalidRequest("invalid API key header".into()))?,
        );
        let (ws, _response) = connect_async(request)
            .await
            .map_err(|err| DeepSeekError::Transport(format!("TTS websocket connect: {err}")))?;
        Ok(TtsStreamSession {
            ws,
            finished: false,
        })
    }
}

/// WebSocket 流式合成会话。
pub struct TtsStreamSession {
    ws: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    finished: bool,
}

impl TtsStreamSession {
    /// 发送一段文本；可多次调用（模型边生成边朗读的场景）。
    pub async fn send_text(&mut self, text: &str) -> Result<(), DeepSeekError> {
        if self.finished {
            return Err(DeepSeekError::InvalidRequest(
                "TTS session already finished".into(),
            ));
        }
        let payload = serde_json::json!({ "event": "text", "text": text });
        self.ws
            .send(tokio_tungstenite::tungstenite::Message::Text(
                payload.to_string().into(),
            ))
            .await
            .map_err(map_ws_error)
    }

    /// 声明文本发送完毕；之后服务端下发剩余音频帧并最终发送 Finish 事件。
    pub async fn finish_text(&mut self) -> Result<(), DeepSeekError> {
        if self.finished {
            return Ok(());
        }
        let payload = serde_json::json!({ "event": "finish_text" });
        self.ws
            .send(tokio_tungstenite::tungstenite::Message::Text(
                payload.to_string().into(),
            ))
            .await
            .map_err(map_ws_error)
    }

    /// 读取下一个事件：`Audio`（音频帧）、`Finish`（会话结束，此后返回 `None`）。
    ///
    /// 服务端每 25 秒发送一次 ping，由协议层自动应答，不会出现在事件里。
    pub async fn next_event(&mut self) -> Result<Option<TtsStreamEvent>, DeepSeekError> {
        if self.finished {
            return Ok(None);
        }
        loop {
            let message = match self.ws.next().await {
                Some(Ok(message)) => message,
                Some(Err(err)) => return Err(map_ws_error(err)),
                None => {
                    self.finished = true;
                    return Err(DeepSeekError::Stream(
                        "TTS websocket closed without finish event".into(),
                    ));
                }
            };
            match message {
                tokio_tungstenite::tungstenite::Message::Binary(frame) => {
                    return Ok(Some(TtsStreamEvent::Audio(frame.into())));
                }
                tokio_tungstenite::tungstenite::Message::Text(text) => {
                    let event: TtsFinishEvent = serde_json::from_str(&text).map_err(|err| {
                        DeepSeekError::Serialization(format!(
                            "{err}: {}",
                            &text[..text.len().min(256)]
                        ))
                    })?;
                    self.finished = true;
                    return Ok(Some(TtsStreamEvent::Finish(event)));
                }
                // ping 由 tungstenite 自动应答 pong；其余控制帧忽略。
                tokio_tungstenite::tungstenite::Message::Ping(_)
                | tokio_tungstenite::tungstenite::Message::Pong(_)
                | tokio_tungstenite::tungstenite::Message::Frame(_) => continue,
                tokio_tungstenite::tungstenite::Message::Close(frame) => {
                    self.finished = true;
                    let detail = frame
                        .map(|close| format!(": {} {}", close.code, close.reason))
                        .unwrap_or_default();
                    return Err(DeepSeekError::Stream(format!(
                        "TTS websocket closed unexpectedly{detail}"
                    )));
                }
            }
        }
    }

    /// 便捷方法：发送全部文本分片后收完音频，返回 (音频字节, 结束事件)。
    pub async fn run(
        mut self,
        chunks: impl IntoIterator<Item = String>,
    ) -> Result<(Vec<u8>, TtsFinishEvent), DeepSeekError> {
        for chunk in chunks {
            self.send_text(&chunk).await?;
        }
        self.finish_text().await?;
        let mut audio = Vec::new();
        while let Some(event) = self.next_event().await? {
            match event {
                TtsStreamEvent::Audio(frame) => audio.extend_from_slice(&frame),
                TtsStreamEvent::Finish(finish) => {
                    if !finish.is_success() {
                        return Err(DeepSeekError::Api(format!(
                            "{}: {}",
                            finish.code, finish.message
                        )));
                    }
                    return Ok((audio, finish));
                }
            }
        }
        Err(DeepSeekError::Stream(
            "TTS stream ended without finish event".into(),
        ))
    }
}

fn map_ws_error(err: tokio_tungstenite::tungstenite::Error) -> DeepSeekError {
    DeepSeekError::Stream(format!("TTS websocket: {err}"))
}

/// 本地校验 TTS 请求：文本非空、计费字符 ≤ 10,000（汉字算 2）、请求体 ≤ 64 KiB。
pub(crate) fn validate_tts_request(request: &TtsRequest) -> Result<(), DeepSeekError> {
    if request.text.trim().is_empty() {
        return Err(DeepSeekError::InvalidRequest(
            "'text' must not be empty or whitespace".into(),
        ));
    }
    let billed = request.text.chars().count()
        + request
            .text
            .chars()
            .filter(|ch| is_cjk_billed_double(*ch))
            .count();
    if billed > MAX_TTS_BILLING_CHARS {
        return Err(DeepSeekError::InvalidRequest(format!(
            "'text' exceeds {MAX_TTS_BILLING_CHARS} billing characters (CJK counts as 2), got {billed}"
        )));
    }
    if request.text.len() > MAX_TTS_BODY_BYTES {
        return Err(DeepSeekError::InvalidRequest(format!(
            "'text' exceeds {MAX_TTS_BODY_BYTES} KiB request body limit, got {} bytes",
            request.text.len()
        )));
    }
    Ok(())
}

/// TTS 计费规则：汉字算 2 个字符（CJK 统一表意文字及扩展 A），
/// 其余（英文字母、数字、空格等）算 1 个。
fn is_cjk_billed_double(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF   // CJK 扩展 A
        | 0x4E00..=0x9FFF // CJK 统一表意文字
        | 0xF900..=0xFAFF // CJK 兼容表意文字
    )
}
