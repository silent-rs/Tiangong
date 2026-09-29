//! 豆包语音（openspeech）HTTP 调用：语音合成与录音文件识别。
//!
//! - 语音合成：`POST {base}/api/v3/tts/unidirectional`（HTTP Chunked 单向流式 V3），
//!   响应为逐行 JSON：`{"code":0,"data":"<base64 音频>"}` 若干帧，
//!   `{"code":20000000,"message":"ok"}` 为结束帧，其余 code 为错误。
//! - 语音识别：`POST {base}/api/v3/auc/bigmodel/recognize/flash`（录音文件识别极速版），
//!   一次请求同步返回结果；状态码在响应头 `X-Api-Status-Code`。
//!
//! 鉴权：新版控制台只需 `X-Api-Key`；旧版控制台使用 App ID + Access Token。
//!
//! 参考：<https://www.volcengine.com/docs/6561/1598757>（HTTP Chunked 语音合成）、
//! <https://www.volcengine.com/docs/6561/1631584>（录音文件识别极速版）。

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::ark::{http_client, preview};

/// 合成成功结束帧 / 识别成功状态码。
const SUCCESS_CODE: i64 = 20_000_000;
/// 识别：静音音频（视为识别出空文本）。
const SILENT_AUDIO_CODE: &str = "20000003";

/// 已解析的豆包语音凭据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechAuth {
    /// 新版控制台 API Key。
    ApiKey(String),
    /// 旧版控制台 App ID + Access Token。
    AppToken { app_id: String, token: String },
}

/// 已解析的豆包语音端点。
pub struct SpeechEndpoint {
    pub base_url: String,
    pub auth: SpeechAuth,
}

impl SpeechEndpoint {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim().trim_end_matches('/'))
    }

    /// 附加鉴权与资源头。旧版控制台 TTS 用 `X-Api-App-Id`，ASR 用 `X-Api-App-Key`。
    fn authorize(
        &self,
        request: reqwest::RequestBuilder,
        resource_id: &str,
        app_header: &str,
    ) -> reqwest::RequestBuilder {
        let request = request
            .header("X-Api-Resource-Id", resource_id)
            .header("X-Api-Request-Id", request_id());
        match &self.auth {
            SpeechAuth::ApiKey(key) => request.header("X-Api-Key", key),
            SpeechAuth::AppToken { app_id, token } => request
                .header(app_header, app_id)
                .header("X-Api-Access-Key", token),
        }
    }

    fn uid(&self) -> String {
        match &self.auth {
            SpeechAuth::ApiKey(_) => "tiangong".to_string(),
            SpeechAuth::AppToken { app_id, .. } => app_id.clone(),
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
    pub resource_id: &'a str,
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
    let request = endpoint
        .authorize(
            http_client()?.post(endpoint.url("/api/v3/tts/unidirectional")),
            options.resource_id,
            "X-Api-App-Id",
        )
        .json(&tts_body(&endpoint.uid(), text, options));
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

/// 按扩展名推断识别接口的 `audio.format`（极速版支持 wav / mp3 / ogg opus）。
pub fn asr_format(file_path: &str) -> Result<&'static str> {
    let extension = std::path::Path::new(file_path)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    Ok(match extension.as_str() {
        "wav" => "wav",
        "mp3" => "mp3",
        "ogg" | "oga" | "opus" => "ogg",
        _ => bail!("不支持的音频格式（火山引擎语音识别仅支持 wav / mp3 / ogg）"),
    })
}

/// 组装录音文件识别请求体（音频以 base64 直传）。
pub fn asr_body(uid: &str, audio: &[u8], format: &str, language: Option<&str>) -> Value {
    let mut audio_json = json!({
        "data": base64::engine::general_purpose::STANDARD.encode(audio),
        "format": format,
    });
    if let Some(language) = language.map(str::trim).filter(|value| !value.is_empty()) {
        audio_json["language"] = json!(language);
    }
    json!({
        "user": { "uid": uid },
        "audio": audio_json,
        "request": {
            "model_name": "bigmodel",
            "enable_itn": true,
            "enable_punc": true,
        }
    })
}

/// 从识别响应体中提取文本与时长（毫秒 → 秒）。
pub fn parse_asr_body(body: &Value) -> AsrResult {
    let text = body
        .pointer("/result/text")
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

/// 调用录音文件识别（极速版）。
pub async fn recognize(
    endpoint: &SpeechEndpoint,
    resource_id: &str,
    audio: &[u8],
    format: &str,
    language: Option<&str>,
) -> Result<AsrResult> {
    let request = endpoint
        .authorize(
            http_client()?
                .post(endpoint.url("/api/v3/auc/bigmodel/recognize/flash"))
                .timeout(Duration::from_secs(300)),
            resource_id,
            "X-Api-App-Key",
        )
        .header("X-Api-Sequence", "-1")
        .json(&asr_body(&endpoint.uid(), audio, format, language));
    let response = request.send().await.context("请求豆包语音识别接口失败")?;
    let status = response.status();
    let api_status = header_value(&response, "X-Api-Status-Code");
    let api_message = header_value(&response, "X-Api-Message");
    let body = response.text().await.context("读取语音识别响应失败")?;

    if let Some(code) = api_status.as_deref() {
        if code == SILENT_AUDIO_CODE {
            return Ok(AsrResult {
                text: String::new(),
                duration: None,
            });
        }
        if code != SUCCESS_CODE.to_string() {
            let message = api_message
                .filter(|value| !value.trim().is_empty())
                .or_else(|| error_message(&body))
                .unwrap_or_else(|| preview(&body));
            bail!("豆包语音识别失败（{code}）：{message}");
        }
    } else if !status.is_success() {
        bail!(
            "豆包语音识别调用失败 ({status})：{}",
            error_message(&body).unwrap_or_else(|| preview(&body))
        );
    }

    let value: Value = serde_json::from_str(&body)
        .map_err(|_| anyhow!("解析豆包语音识别响应失败：{}", preview(&body)))?;
    Ok(parse_asr_body(&value))
}

fn header_value(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
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

/// 常用音色预设（豆包语音合成模型 2.0，`seed-tts-2.0` 资源）。
///
/// 豆包语音没有面向 API Key 的音色列表接口（音色列表属控制台 OpenAPI，需 AK/SK 签名），
/// 这里提供静态预设供前端选择；其他音色可在设置页直接填写音色 ID。
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
    fn tts_body_carries_speaker_and_optional_rate() {
        let options = TtsOptions {
            resource_id: "seed-tts-2.0",
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
    fn asr_format_and_body() {
        assert_eq!(asr_format("/a/b.WAV").unwrap(), "wav");
        assert_eq!(asr_format("/a/b.mp3").unwrap(), "mp3");
        assert_eq!(asr_format("/a/b.ogg").unwrap(), "ogg");
        assert!(asr_format("/a/b.m4a").is_err());

        let body = asr_body("u", b"ABC", "wav", Some(" zh-CN "));
        assert_eq!(body["audio"]["data"], "QUJD");
        assert_eq!(body["audio"]["format"], "wav");
        assert_eq!(body["audio"]["language"], "zh-CN");
        assert_eq!(body["request"]["model_name"], "bigmodel");
        assert!(
            asr_body("u", b"", "mp3", None)["audio"]
                .get("language")
                .is_none()
        );
    }

    #[test]
    fn parse_asr_body_reads_text_and_duration() {
        let value = json!({
            "audio_info": { "duration": 2499 },
            "result": { "text": " 关闭透传。 ", "utterances": [] }
        });
        assert_eq!(
            parse_asr_body(&value),
            AsrResult {
                text: "关闭透传。".to_string(),
                duration: Some(2.499)
            }
        );
        assert_eq!(parse_asr_body(&json!({})).text, "");
    }

    /// 本地 HTTP 桩：验证路径、鉴权头与响应解析。
    async fn stub(
        responses: Vec<(&'static str, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (expected_path, raw_response) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = vec![0u8; 64 * 1024];
                // 读到请求头结束且 body 长度满足 content-length 为止。
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
                let request = String::from_utf8_lossy(&request).to_string();
                assert!(
                    request
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .contains(expected_path),
                    "请求路径不符：{request}"
                );
                seen.push(request);
                socket.write_all(raw_response.as_bytes()).await.unwrap();
            }
            seen
        });
        (format!("http://{address}/"), handle)
    }

    fn http_response(headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn synthesize_and_recognize_round_trip_against_stub() {
        let tts_body = "{\"code\":0,\"data\":\"QUJD\"}\n{\"code\":20000000,\"message\":\"ok\"}\n";
        let asr_ok = r#"{"audio_info":{"duration":1500},"result":{"text":"你好"}}"#;
        let (base_url, server) = stub(vec![
            (
                "POST /api/v3/tts/unidirectional",
                http_response("", tts_body),
            ),
            (
                "POST /api/v3/auc/bigmodel/recognize/flash",
                http_response(
                    "x-api-status-code: 20000000\r\nx-api-message: OK\r\n",
                    asr_ok,
                ),
            ),
            (
                "POST /api/v3/auc/bigmodel/recognize/flash",
                http_response(
                    "x-api-status-code: 45000151\r\nx-api-message: invalid audio format\r\n",
                    "{}",
                ),
            ),
        ])
        .await;

        let api_key = SpeechEndpoint {
            base_url: base_url.clone(),
            auth: SpeechAuth::ApiKey("speech-key".to_string()),
        };
        let audio = synthesize(
            &api_key,
            "你好",
            &TtsOptions {
                resource_id: "seed-tts-2.0",
                speaker: "zh_female_vv_uranus_bigtts",
                speed: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(audio.audio, b"ABC".to_vec());
        assert_eq!(audio.extension, "mp3");

        let legacy = SpeechEndpoint {
            base_url,
            auth: SpeechAuth::AppToken {
                app_id: "app-1".to_string(),
                token: "tok".to_string(),
            },
        };
        let result = recognize(&legacy, "volc.bigasr.auc_turbo", b"RIFF", "wav", None)
            .await
            .unwrap();
        assert_eq!(result.text, "你好");
        assert_eq!(result.duration, Some(1.5));

        let error = recognize(&api_key, "volc.bigasr.auc_turbo", b"x", "mp3", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("45000151") && error.contains("invalid audio format"));

        let requests: Vec<String> = server
            .await
            .unwrap()
            .into_iter()
            .map(|request| request.to_ascii_lowercase())
            .collect();
        assert!(requests[0].contains("x-api-key: speech-key"));
        assert!(requests[0].contains("x-api-resource-id: seed-tts-2.0"));
        assert!(requests[0].contains("x-api-request-id:"));
        assert!(requests[1].contains("x-api-app-key: app-1"));
        assert!(requests[1].contains("x-api-access-key: tok"));
        assert!(requests[1].contains("x-api-sequence: -1"));
        assert!(requests[1].contains("\"data\":\"uklgrg==\""));
    }
}
