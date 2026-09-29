//! 豆包语音（Agent Plan）调用：语音合成与流式语音识别。
//!
//! - 语音合成：`POST {base}/api/v3/plan/tts/unidirectional`（HTTP Chunked 单向流式），
//!   响应为逐行 JSON：`{"code":0,"data":"<base64 音频>"}` 若干帧，
//!   `{"code":20000000,"message":"ok"}` 为结束帧，其余 code 为错误。
//! - 语音识别：`wss://{host}/api/v3/plan/sauc/bigmodel_nostream`（流式输入模式），
//!   二进制帧协议：full client request（gzip JSON）→ 分包 audio only request
//!   （gzip 音频，末包置负包标志）→ full server response（gzip JSON）。
//!
//! 鉴权：Agent Plan 专属 API Key 放在 `X-Api-Key`；`X-Api-Resource-Id` 按模型映射。
//!
//! 参考：<https://www.volcengine.com/docs/82379/2516286>（Agent Plan 接入语音模型）、
//! <https://www.volcengine.com/docs/6561/1354869>（大模型流式语音识别 API）。

use std::io::{Read, Write};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::ark::{http_client, preview};

/// 合成成功结束帧 / 识别成功状态码。
const SUCCESS_CODE: i64 = 20_000_000;
/// 识别：单包音频时长（毫秒），文档建议 100～200ms。
const ASR_CHUNK_MS: usize = 200;
/// 识别：整体超时。
const ASR_TIMEOUT: Duration = Duration::from_secs(300);

/// 模型 → `X-Api-Resource-Id`。未知模型原样透传（便于直接填写资源 ID）。
pub fn tts_resource_id(model: &str) -> &str {
    match model.trim() {
        "doubao-seed-tts-2.0" => "seed-tts-2.0",
        "doubao-seed-tts-1.0" => "seed-tts-1.0",
        other => other,
    }
}

/// 模型 → `X-Api-Resource-Id`（Agent Plan 流式识别为小时版资源）。
pub fn asr_resource_id(model: &str) -> &str {
    match model.trim() {
        "doubao-seed-asr-2.0" => "volc.seedasr.sauc.duration",
        "doubao-seed-asr-1.0" => "volc.bigasr.sauc.duration",
        other => other,
    }
}

/// 已解析的豆包语音端点。
pub struct SpeechEndpoint {
    /// `https://openspeech.bytedance.com`；识别时换成对应的 `wss://`。
    pub base_url: String,
    pub api_key: String,
}

impl SpeechEndpoint {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim().trim_end_matches('/'))
    }

    fn ws_url(&self, path: &str) -> String {
        let url = self.url(path);
        if let Some(rest) = url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            url
        }
    }
}

fn request_id() -> String {
    // 服务端只要求唯一性；scru128 已足够，避免额外引入 uuid 依赖。
    scru128::new().to_string()
}

// ── 语音合成 ──

/// 合成参数。
pub struct TtsOptions<'a> {
    /// 语音合成模型（如 `doubao-seed-tts-2.0`）。
    pub model: &'a str,
    pub speaker: &'a str,
    /// 语速倍率（1.0 正常），换算为服务端 `speech_rate` [-50, 100]。
    pub speed: Option<f64>,
}

/// 合成产物（mp3，24 kHz）。
pub struct TtsAudio {
    pub audio: Vec<u8>,
    pub mime_type: &'static str,
    pub extension: &'static str,
}

/// 倍率换算为服务端语速：1.0→0，2.0→100，0.5→-50，超界截断。
pub fn speech_rate(speed: f64) -> i64 {
    if !speed.is_finite() || speed <= 0.0 {
        return 0;
    }
    ((speed - 1.0) * 100.0).round().clamp(-50.0, 100.0) as i64
}

/// 组装语音合成请求体。
pub fn tts_body(uid: &str, text: &str, options: &TtsOptions<'_>) -> Value {
    let mut audio_params = json!({ "format": "mp3", "sample_rate": 24000 });
    if let Some(speed) = options.speed {
        audio_params["speech_rate"] = json!(speech_rate(speed));
    }
    json!({
        "user": { "uid": uid },
        "req_params": {
            "text": text,
            "speaker": options.speaker,
            "audio_params": audio_params,
        }
    })
}

/// 解析合成流：逐行 JSON，拼接 base64 音频帧；遇错误码或无结束帧报错。
pub fn parse_tts_stream(body: &str) -> Result<Vec<u8>> {
    let mut audio = Vec::new();
    let mut finished = false;
    for line in body.lines() {
        let line = line.trim();
        // 同一接口的 SSE 形式以 `data:` 前缀承载同样的 JSON，这里一并兼容。
        let line = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let frame: Value = serde_json::from_str(line)
            .with_context(|| format!("解析语音合成响应帧失败：{}", preview(line)))?;
        let code = frame.get("code").and_then(Value::as_i64).unwrap_or(0);
        match code {
            0 => {
                if let Some(data) = frame.get("data").and_then(Value::as_str)
                    && !data.is_empty()
                {
                    let chunk = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .context("语音合成音频帧 base64 解码失败")?;
                    audio.extend_from_slice(&chunk);
                }
            }
            SUCCESS_CODE => {
                finished = true;
                break;
            }
            other => {
                let message = frame
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误");
                bail!("豆包语音合成失败（{other}）：{message}");
            }
        }
    }
    if !finished && audio.is_empty() {
        bail!("豆包语音合成未返回音频：{}", preview(body));
    }
    if audio.is_empty() {
        bail!("豆包语音合成返回空音频，请检查音色与资源 ID 是否匹配");
    }
    Ok(audio)
}

/// 调用语音合成。
pub async fn synthesize(
    endpoint: &SpeechEndpoint,
    text: &str,
    options: &TtsOptions<'_>,
) -> Result<TtsAudio> {
    let request = http_client()?
        .post(endpoint.url("/api/v3/plan/tts/unidirectional"))
        .header("X-Api-Key", &endpoint.api_key)
        .header("X-Api-Resource-Id", tts_resource_id(options.model))
        .header("X-Api-Request-Id", request_id())
        .json(&tts_body("tiangong", text, options));
    let response = request.send().await.context("请求豆包语音合成接口失败")?;
    let status = response.status();
    let body = response.text().await.context("读取语音合成响应失败")?;
    if !status.is_success() {
        bail!(
            "豆包语音合成调用失败 ({status})：{}",
            error_message(&body).unwrap_or_else(|| preview(&body))
        );
    }
    let audio = parse_tts_stream(&body)?;
    Ok(TtsAudio {
        audio,
        mime_type: "audio/mpeg",
        extension: "mp3",
    })
}

// ── 语音识别 ──

/// 识别结果。
#[derive(Debug, Clone, PartialEq)]
pub struct AsrResult {
    pub text: String,
    /// 音频时长（秒）。
    pub duration: Option<f64>,
}

/// 识别输入音频的容器格式（流式识别支持 pcm / wav / ogg(opus) / mp3，采样率 16k）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrFormat {
    Wav,
    Mp3,
    Ogg,
}

impl AsrFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::Ogg => "ogg",
        }
    }

    fn codec(self) -> &'static str {
        match self {
            Self::Ogg => "opus",
            Self::Wav | Self::Mp3 => "raw",
        }
    }
}

/// 按扩展名推断识别格式。
pub fn asr_format(file_path: &str) -> Result<AsrFormat> {
    let extension = std::path::Path::new(file_path)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    Ok(match extension.as_str() {
        "wav" => AsrFormat::Wav,
        "mp3" => AsrFormat::Mp3,
        "ogg" | "oga" | "opus" => AsrFormat::Ogg,
        _ => bail!("不支持的音频格式（火山引擎语音识别仅支持 wav / mp3 / ogg）"),
    })
}

/// 组装 full client request 的 JSON 参数。
pub fn asr_request_json(format: AsrFormat, language: Option<&str>) -> Value {
    let mut audio = json!({
        "format": format.as_str(),
        "codec": format.codec(),
        "rate": 16000,
        "bits": 16,
        "channel": 1,
    });
    if let Some(language) = language.map(str::trim).filter(|value| !value.is_empty()) {
        audio["language"] = json!(language);
    }
    json!({
        "user": { "uid": "tiangong" },
        "audio": audio,
        "request": {
            "model_name": "bigmodel",
            "enable_itn": true,
            "enable_punc": true,
        }
    })
}

// 二进制帧协议（整数大端）：4 字节头 + [sequence] + payload size + payload。
const PROTOCOL_VERSION_HEADER: u8 = 0x11; // version=1, header size=1(×4 字节)
const MSG_FULL_CLIENT_REQUEST: u8 = 0b0001;
const MSG_AUDIO_ONLY_REQUEST: u8 = 0b0010;
const MSG_FULL_SERVER_RESPONSE: u8 = 0b1001;
const MSG_SERVER_ERROR: u8 = 0b1111;
const FLAG_NONE: u8 = 0b0000;
const FLAG_LAST_PACKET: u8 = 0b0010;
const SERIALIZATION_NONE: u8 = 0b0000;
const SERIALIZATION_JSON: u8 = 0b0001;
const COMPRESSION_GZIP: u8 = 0b0001;

fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).context("gzip 压缩失败")?;
    encoder.finish().context("gzip 压缩失败")
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut decoded)
        .context("gzip 解压失败")?;
    Ok(decoded)
}

/// 编码客户端帧（无 sequence 字段，payload 使用 gzip）。
pub fn encode_client_frame(
    message_type: u8,
    flags: u8,
    serialization: u8,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let compressed = gzip(payload)?;
    let size = u32::try_from(compressed.len()).context("音频分包过大")?;
    let mut frame = Vec::with_capacity(8 + compressed.len());
    frame.push(PROTOCOL_VERSION_HEADER);
    frame.push((message_type << 4) | flags);
    frame.push((serialization << 4) | COMPRESSION_GZIP);
    frame.push(0);
    frame.extend_from_slice(&size.to_be_bytes());
    frame.extend_from_slice(&compressed);
    Ok(frame)
}

/// 解码后的服务端帧。
#[derive(Debug, PartialEq)]
pub enum ServerFrame {
    /// 识别结果；`last` 表示最后一包的结果。
    Response {
        last: bool,
        payload: Value,
    },
    Error {
        code: u32,
        message: String,
    },
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow!("服务端帧长度不足"))?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn decode_payload(bytes: &[u8], compression: u8, serialization: u8) -> Result<Value> {
    let raw = if compression == COMPRESSION_GZIP {
        gunzip(bytes)?
    } else {
        bytes.to_vec()
    };
    if raw.is_empty() {
        return Ok(Value::Null);
    }
    if serialization == SERIALIZATION_JSON {
        serde_json::from_slice(&raw).map_err(|_| {
            anyhow!(
                "解析识别结果失败：{}",
                preview(&String::from_utf8_lossy(&raw))
            )
        })
    } else {
        Ok(Value::String(String::from_utf8_lossy(&raw).to_string()))
    }
}

/// 解码服务端帧。
pub fn decode_server_frame(data: &[u8]) -> Result<ServerFrame> {
    if data.len() < 4 {
        bail!("服务端帧长度不足");
    }
    let header_len = usize::from(data[0] & 0x0f) * 4;
    let message_type = data[1] >> 4;
    let flags = data[1] & 0x0f;
    let serialization = data[2] >> 4;
    let compression = data[2] & 0x0f;
    let body = data
        .get(header_len..)
        .ok_or_else(|| anyhow!("服务端帧头长度无效"))?;
    match message_type {
        MSG_FULL_SERVER_RESPONSE => {
            // flags bit0 表示携带 sequence（负数为最后一包）。
            let (sequence, rest) = if flags & 0b0001 != 0 {
                (Some(read_u32(body, 0)? as i32), &body[4..])
            } else {
                (None, body)
            };
            let size = read_u32(rest, 0)? as usize;
            let payload = rest
                .get(4..4 + size)
                .ok_or_else(|| anyhow!("服务端帧载荷长度无效"))?;
            let last = flags & 0b0010 != 0 || sequence.is_some_and(|value| value < 0);
            Ok(ServerFrame::Response {
                last,
                payload: decode_payload(payload, compression, serialization)?,
            })
        }
        MSG_SERVER_ERROR => {
            let code = read_u32(body, 0)?;
            let size = read_u32(body, 4)? as usize;
            let raw = body
                .get(8..8 + size)
                .ok_or_else(|| anyhow!("服务端错误帧长度无效"))?;
            let message = match decode_payload(raw, compression, serialization) {
                Ok(Value::String(text)) => text,
                Ok(value) => ["/message", "/error", "/header/message"]
                    .iter()
                    .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string()),
                Err(_) => String::from_utf8_lossy(raw).to_string(),
            };
            Ok(ServerFrame::Error { code, message })
        }
        other => bail!("未知的服务端消息类型：{other}"),
    }
}

/// 从识别结果中提取文本与时长（毫秒 → 秒）。
///
/// `result` 可能是对象或数组（文档写作 list），两种形态都兼容。
pub fn parse_asr_body(body: &Value) -> AsrResult {
    let result = match body.get("result") {
        Some(Value::Array(items)) => items.first().cloned().unwrap_or(Value::Null),
        Some(other) => other.clone(),
        None => Value::Null,
    };
    let text = result
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let duration = body
        .pointer("/audio_info/duration")
        .and_then(Value::as_f64)
        .map(|millis| millis / 1000.0);
    AsrResult { text, duration }
}

/// 音频分包大小：wav 按 16k/16bit/单声道的 200ms 计，压缩格式按 200ms 近似码率。
fn chunk_size(format: AsrFormat) -> usize {
    match format {
        AsrFormat::Wav => 16_000 * 2 * ASR_CHUNK_MS / 1000,
        // mp3/ogg 为压缩流，按约 128kbps 估算 200ms 分包。
        AsrFormat::Mp3 | AsrFormat::Ogg => 128_000 / 8 * ASR_CHUNK_MS / 1000,
    }
}

/// 调用流式语音识别（流式输入模式），整段音频分包发送后取最终结果。
pub async fn recognize(
    endpoint: &SpeechEndpoint,
    model: &str,
    audio: &[u8],
    format: AsrFormat,
    language: Option<&str>,
) -> Result<AsrResult> {
    tokio::time::timeout(
        ASR_TIMEOUT,
        recognize_inner(endpoint, model, audio, format, language),
    )
    .await
    .map_err(|_| anyhow!("豆包语音识别超时"))?
}

async fn recognize_inner(
    endpoint: &SpeechEndpoint,
    model: &str,
    audio: &[u8],
    format: AsrFormat,
    language: Option<&str>,
) -> Result<AsrResult> {
    let mut request = endpoint
        .ws_url("/api/v3/plan/sauc/bigmodel_nostream")
        .into_client_request()
        .context("构造语音识别请求失败")?;
    let headers = request.headers_mut();
    let header = |value: &str| value.parse().map_err(|_| anyhow!("请求头包含非法字符"));
    headers.insert("X-Api-Key", header(&endpoint.api_key)?);
    headers.insert("X-Api-Resource-Id", header(asr_resource_id(model))?);
    headers.insert("X-Api-Connect-Id", header(&request_id())?);
    headers.insert("X-Api-Request-Id", header(&request_id())?);
    headers.insert("X-Api-Sequence", header("-1")?);

    let (mut socket, _) = tokio_tungstenite::connect_async_tls_with_config(
        request,
        None,
        false,
        Some(tokio_tungstenite::Connector::Rustls(
            crate::ark::rustls_config()?,
        )),
    )
    .await
    .map_err(|error| anyhow!("连接豆包语音识别服务失败：{error}"))?;

    let params = serde_json::to_vec(&asr_request_json(format, language)).context("序列化失败")?;
    socket
        .send(Message::binary(encode_client_frame(
            MSG_FULL_CLIENT_REQUEST,
            FLAG_NONE,
            SERIALIZATION_JSON,
            &params,
        )?))
        .await
        .context("发送识别参数失败")?;

    // 读端与写端并行：服务端每收到一包就回一包结果，不读会阻塞发送。
    let (mut sink, mut stream) = socket.split();
    let chunks: Vec<Vec<u8>> = if audio.is_empty() {
        vec![Vec::new()]
    } else {
        audio
            .chunks(chunk_size(format))
            .map(<[u8]>::to_vec)
            .collect()
    };
    let writer = async move {
        let total = chunks.len();
        for (index, chunk) in chunks.into_iter().enumerate() {
            let flags = if index + 1 == total {
                FLAG_LAST_PACKET
            } else {
                FLAG_NONE
            };
            let frame =
                encode_client_frame(MSG_AUDIO_ONLY_REQUEST, flags, SERIALIZATION_NONE, &chunk)?;
            sink.send(Message::binary(frame))
                .await
                .context("发送音频分包失败")?;
        }
        anyhow::Ok(sink)
    };
    let reader = async {
        let mut latest = Value::Null;
        while let Some(message) = stream.next().await {
            let message = message.map_err(|error| anyhow!("读取识别结果失败：{error}"))?;
            let data = match message {
                Message::Binary(data) => data,
                Message::Close(frame) => {
                    let reason = frame
                        .map(|frame| frame.reason.to_string())
                        .unwrap_or_default();
                    if latest.is_null() {
                        bail!("豆包语音识别连接被关闭：{reason}");
                    }
                    break;
                }
                _ => continue,
            };
            match decode_server_frame(&data)? {
                ServerFrame::Response { last, payload } => {
                    if !payload.is_null() {
                        latest = payload;
                    }
                    if last {
                        break;
                    }
                }
                ServerFrame::Error { code, message } => {
                    bail!("豆包语音识别失败（{code}）：{message}")
                }
            }
        }
        Ok(latest)
    };
    let (sink, latest) = tokio::try_join!(writer, reader)?;
    let mut socket = sink
        .reunite(stream)
        .map_err(|_| anyhow!("识别连接状态异常"))?;
    let _ = socket.close(None).await;
    Ok(parse_asr_body(&latest))
}

/// 从错误响应体提取信息（兼容 `{message}`、`{header:{message}}` 等形态）。
fn error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    ["/message", "/header/message", "/error/message", "/error"]
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .filter(|message| !message.trim().is_empty())
        .map(str::to_string)
}

/// 常用音色预设（豆包语音合成模型 2.0）。
///
/// 豆包语音没有面向 API Key 的音色列表接口，这里提供静态预设供前端选择；
/// 其他音色可在设置页直接填写音色 ID。
pub const VOICE_PRESETS: &[(&str, &str, &str)] = &[
    ("zh_female_vv_uranus_bigtts", "Vivi（通用女声）", "female"),
    (
        "zh_female_xiaohe_uranus_bigtts",
        "小何（通用女声）",
        "female",
    ),
    ("zh_male_m191_uranus_bigtts", "云舟（通用男声）", "male"),
    ("zh_male_taocheng_uranus_bigtts", "小天（通用男声）", "male"),
    ("zh_female_cancan_uranus_bigtts", "知性灿灿", "female"),
    (
        "zh_female_qingxinnvsheng_uranus_bigtts",
        "清新女声",
        "female",
    ),
    (
        "zh_female_shuangkuaisisi_uranus_bigtts",
        "爽快思思",
        "female",
    ),
    ("zh_male_shaonianzixin_uranus_bigtts", "少年梓辛", "male"),
    ("zh_male_ruyayichen_uranus_bigtts", "儒雅逸辰", "male"),
    (
        "zh_female_kefunvsheng_uranus_bigtts",
        "暖阳女声（客服）",
        "female",
    ),
    ("en_male_tim_uranus_bigtts", "Tim（英文男声）", "male"),
    (
        "en_female_dacey_uranus_bigtts",
        "Dacey（英文女声）",
        "female",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_rate_maps_multiplier_to_server_range() {
        assert_eq!(speech_rate(1.0), 0);
        assert_eq!(speech_rate(2.0), 100);
        assert_eq!(speech_rate(0.5), -50);
        assert_eq!(speech_rate(3.0), 100);
        assert_eq!(speech_rate(0.1), -50);
        assert_eq!(speech_rate(1.25), 25);
        assert_eq!(speech_rate(f64::NAN), 0);
        assert_eq!(speech_rate(-1.0), 0);
    }

    #[test]
    fn model_maps_to_plan_resource_id() {
        assert_eq!(tts_resource_id("doubao-seed-tts-2.0"), "seed-tts-2.0");
        assert_eq!(tts_resource_id(" seed-icl-2.0 "), "seed-icl-2.0");
        assert_eq!(
            asr_resource_id("doubao-seed-asr-2.0"),
            "volc.seedasr.sauc.duration"
        );
        assert_eq!(asr_resource_id("custom"), "custom");
    }

    #[test]
    fn tts_body_carries_speaker_and_optional_rate() {
        let options = TtsOptions {
            model: "doubao-seed-tts-2.0",
            speaker: "zh_female_vv_uranus_bigtts",
            speed: Some(1.5),
        };
        let body = tts_body("u", "你好", &options);
        assert_eq!(body["user"]["uid"], "u");
        assert_eq!(body["req_params"]["text"], "你好");
        assert_eq!(body["req_params"]["speaker"], "zh_female_vv_uranus_bigtts");
        assert_eq!(body["req_params"]["audio_params"]["format"], "mp3");
        assert_eq!(body["req_params"]["audio_params"]["speech_rate"], 50);

        let plain = tts_body(
            "u",
            "hi",
            &TtsOptions {
                speed: None,
                ..options
            },
        );
        assert!(
            plain["req_params"]["audio_params"]
                .get("speech_rate")
                .is_none()
        );
    }

    #[test]
    fn parse_tts_stream_concatenates_audio_frames() {
        let body = concat!(
            "{\"code\":0,\"message\":\"\",\"data\":\"QUJD\"}\n",
            "{\"code\":0,\"message\":\"\",\"data\":null,\"sentence\":{\"text\":\"x\"}}\n",
            "{\"code\":0,\"message\":\"\",\"data\":\"REVG\"}\n",
            "{\"code\":20000000,\"message\":\"ok\",\"data\":null}\n"
        );
        assert_eq!(parse_tts_stream(body).unwrap(), b"ABCDEF".to_vec());

        let sse = "data: {\"code\":0,\"data\":\"QUJD\"}\n\ndata: {\"code\":20000000,\"message\":\"ok\"}\n";
        assert_eq!(parse_tts_stream(sse).unwrap(), b"ABC".to_vec());
    }

    #[test]
    fn parse_tts_stream_reports_errors() {
        let error = parse_tts_stream(
            "{\"code\":45000000,\"message\":\"speaker permission denied\",\"data\":null}\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("45000000") && error.contains("speaker permission denied"));

        assert!(parse_tts_stream("{\"code\":20000000,\"message\":\"ok\"}").is_err());
        assert!(parse_tts_stream("").is_err());
    }

    #[test]
    fn asr_format_and_request_json() {
        assert_eq!(asr_format("/a/b.WAV").unwrap(), AsrFormat::Wav);
        assert_eq!(asr_format("/a/b.mp3").unwrap(), AsrFormat::Mp3);
        assert_eq!(asr_format("/a/b.ogg").unwrap(), AsrFormat::Ogg);
        assert!(asr_format("/a/b.m4a").is_err());

        let body = asr_request_json(AsrFormat::Ogg, Some(" zh-CN "));
        assert_eq!(body["audio"]["format"], "ogg");
        assert_eq!(body["audio"]["codec"], "opus");
        assert_eq!(body["audio"]["rate"], 16000);
        assert_eq!(body["audio"]["language"], "zh-CN");
        assert_eq!(body["request"]["model_name"], "bigmodel");
        assert!(
            asr_request_json(AsrFormat::Mp3, None)["audio"]
                .get("language")
                .is_none()
        );
    }

    /// 构造服务端响应帧（带 sequence）。
    fn server_frame(sequence: i32, last: bool, payload: &Value) -> Vec<u8> {
        let compressed = gzip(&serde_json::to_vec(payload).unwrap()).unwrap();
        let flags = if last { 0b0011 } else { 0b0001 };
        let mut frame = vec![
            PROTOCOL_VERSION_HEADER,
            (MSG_FULL_SERVER_RESPONSE << 4) | flags,
            (SERIALIZATION_JSON << 4) | COMPRESSION_GZIP,
            0,
        ];
        frame.extend_from_slice(&sequence.to_be_bytes());
        frame.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
        frame.extend_from_slice(&compressed);
        frame
    }

    fn error_frame(code: u32, message: &str) -> Vec<u8> {
        let mut frame = vec![
            PROTOCOL_VERSION_HEADER,
            MSG_SERVER_ERROR << 4,
            SERIALIZATION_JSON << 4,
            0,
        ];
        frame.extend_from_slice(&code.to_be_bytes());
        frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
        frame.extend_from_slice(message.as_bytes());
        frame
    }

    #[test]
    fn client_frame_layout_and_server_frame_decoding() {
        let frame = encode_client_frame(
            MSG_AUDIO_ONLY_REQUEST,
            FLAG_LAST_PACKET,
            SERIALIZATION_NONE,
            b"PCM",
        )
        .unwrap();
        assert_eq!(&frame[..4], &[0x11, 0x22, 0x01, 0x00]);
        let size = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;
        assert_eq!(size, frame.len() - 8);
        assert_eq!(gunzip(&frame[8..]).unwrap(), b"PCM");

        let payload = json!({ "result": { "text": "你好" } });
        assert_eq!(
            decode_server_frame(&server_frame(1, false, &payload)).unwrap(),
            ServerFrame::Response {
                last: false,
                payload: payload.clone()
            }
        );
        assert_eq!(
            decode_server_frame(&server_frame(-3, true, &payload)).unwrap(),
            ServerFrame::Response {
                last: true,
                payload
            }
        );
        assert_eq!(
            decode_server_frame(&error_frame(45000001, "{\"error\":\"invalid audio\"}")).unwrap(),
            ServerFrame::Error {
                code: 45000001,
                message: "invalid audio".to_string()
            }
        );
        assert!(decode_server_frame(&[0x11]).is_err());
    }

    #[test]
    fn parse_asr_body_reads_object_or_list_result() {
        let object = json!({
            "audio_info": { "duration": 2499 },
            "result": { "text": " 关闭透传。 ", "utterances": [] }
        });
        assert_eq!(
            parse_asr_body(&object),
            AsrResult {
                text: "关闭透传。".to_string(),
                duration: Some(2.499)
            }
        );
        let list = json!({ "result": [{ "text": "你好" }] });
        assert_eq!(parse_asr_body(&list).text, "你好");
        assert_eq!(parse_asr_body(&json!({})).text, "");
    }

    /// 本地 HTTP 桩：验证合成路径、鉴权头与响应解析。
    async fn http_stub(raw_response: String) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request).to_string();
                if let Some(split) = text.find("\r\n\r\n") {
                    let length = text[..split]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if request.len() >= split + 4 + length {
                        break;
                    }
                }
            }
            socket.write_all(raw_response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).to_ascii_lowercase()
        });
        (format!("http://{address}"), handle)
    }

    #[tokio::test]
    async fn synthesize_uses_plan_path_and_api_key() {
        let body = "{\"code\":0,\"data\":\"QUJD\"}\n{\"code\":20000000,\"message\":\"ok\"}\n";
        let (base_url, server) = http_stub(format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ))
        .await;
        let endpoint = SpeechEndpoint {
            base_url,
            api_key: "plan-key".to_string(),
        };
        let audio = synthesize(
            &endpoint,
            "你好",
            &TtsOptions {
                model: "doubao-seed-tts-2.0",
                speaker: "zh_female_vv_uranus_bigtts",
                speed: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(audio.audio, b"ABC".to_vec());
        let request = server.await.unwrap();
        assert!(request.starts_with("post /api/v3/plan/tts/unidirectional "));
        assert!(request.contains("x-api-key: plan-key"));
        assert!(request.contains("x-api-resource-id: seed-tts-2.0"));
        assert!(request.contains("x-api-request-id:"));
    }

    /// 本地 WebSocket 桩：校验握手头与帧序列，按协议回包。
    async fn ws_stub(
        fail_with: Option<(u32, &'static str)>,
    ) -> (
        String,
        tokio::task::JoinHandle<(Vec<(String, String)>, usize, Value)>,
    ) {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut seen_headers = Vec::new();
            let mut path = String::new();
            #[allow(clippy::result_large_err)] // 签名由 tungstenite 的 Callback 约定决定
            let callback = |request: &Request, response: Response| {
                path = request.uri().path().to_string();
                for name in ["x-api-key", "x-api-resource-id", "x-api-sequence"] {
                    let value = request
                        .headers()
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    seen_headers.push((name.to_string(), value));
                }
                Ok(response)
            };
            let mut socket = tokio_tungstenite::accept_hdr_async(tcp, callback)
                .await
                .unwrap();
            seen_headers.push(("path".to_string(), path));

            // 第一帧：参数。
            let first = match socket.next().await.unwrap().unwrap() {
                Message::Binary(data) => data,
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(first[1] >> 4, MSG_FULL_CLIENT_REQUEST);
            let params: Value = serde_json::from_slice(&gunzip(&first[8..]).unwrap()).unwrap();
            socket
                .send(Message::binary(server_frame(1, false, &json!({}))))
                .await
                .unwrap();
            if let Some((code, message)) = fail_with {
                socket
                    .send(Message::binary(error_frame(code, message)))
                    .await
                    .unwrap();
                return (seen_headers, 0, params);
            }

            let mut audio_packets = 0;
            let mut sequence = 1;
            loop {
                let frame = match socket.next().await.unwrap().unwrap() {
                    Message::Binary(data) => data,
                    _ => continue,
                };
                assert_eq!(frame[1] >> 4, MSG_AUDIO_ONLY_REQUEST);
                audio_packets += 1;
                sequence += 1;
                let last = frame[1] & 0x0f == FLAG_LAST_PACKET;
                let payload = if last {
                    json!({ "audio_info": { "duration": 1500 }, "result": { "text": "你好世界" } })
                } else {
                    json!({ "result": { "text": "你好" } })
                };
                let seq = if last { -sequence } else { sequence };
                socket
                    .send(Message::binary(server_frame(seq, last, &payload)))
                    .await
                    .unwrap();
                if last {
                    break;
                }
            }
            (seen_headers, audio_packets, params)
        });
        (format!("http://{address}"), handle)
    }

    #[tokio::test]
    async fn recognize_streams_chunks_and_returns_final_text() {
        let (base_url, server) = ws_stub(None).await;
        let endpoint = SpeechEndpoint {
            base_url,
            api_key: "plan-key".to_string(),
        };
        // 3 个 wav 分包（200ms = 6400 字节）。
        let audio = vec![0u8; 6400 * 2 + 10];
        let result = recognize(
            &endpoint,
            "doubao-seed-asr-2.0",
            &audio,
            AsrFormat::Wav,
            Some("zh-CN"),
        )
        .await
        .unwrap();
        assert_eq!(result.text, "你好世界");
        assert_eq!(result.duration, Some(1.5));

        let (headers, packets, params) = server.await.unwrap();
        assert_eq!(packets, 3);
        assert_eq!(params["audio"]["format"], "wav");
        assert_eq!(params["audio"]["language"], "zh-CN");
        let get = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("path"), "/api/v3/plan/sauc/bigmodel_nostream");
        assert_eq!(get("x-api-key"), "plan-key");
        assert_eq!(get("x-api-resource-id"), "volc.seedasr.sauc.duration");
        assert_eq!(get("x-api-sequence"), "-1");
    }

    #[tokio::test]
    async fn recognize_surfaces_server_error_frame() {
        let (base_url, server) =
            ws_stub(Some((45000151, "{\"error\":\"invalid audio format\"}"))).await;
        let endpoint = SpeechEndpoint {
            base_url,
            api_key: "k".to_string(),
        };
        let error = recognize(&endpoint, "doubao-seed-asr-2.0", b"x", AsrFormat::Mp3, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("45000151") && error.contains("invalid audio format"),
            "{error}"
        );
        server.await.unwrap();
    }
}
