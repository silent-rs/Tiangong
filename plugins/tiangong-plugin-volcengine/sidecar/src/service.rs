//! Volcengine sidecar 业务服务（Agent Plan）。
//!
//! 读取插件自有配置 → 组织火山方舟请求（见 [`crate::ark`]）或豆包语音请求
//!（见 [`crate::speech`]）→ 归档 / 落盘 → 返回结果。本机录音与播放见 [`crate::audio`]。
//! 模型名由用户在设置页手动填写（候选见套餐概览），插件不查询模型列表。

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::Value;
use tiangong_plugin_runtime::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request, Response,
    ServiceStatus,
};
use tiangong_plugin_volcengine_protocol::{
    Ack, Empty, GENERATE_IMAGE_OPERATION, GENERATE_VIDEO_OPERATION, GET_CONFIG_OPERATION,
    GeneratedImage, ImageRequest, ImageResponse, LIST_VOICES_OPERATION, ListVoicesResponse,
    PLAN_ARK_BASE_URL, PLAN_SPEECH_BASE_URL, PLAY_OPERATION, PLAY_STATUS_OPERATION, PLUGIN_ID,
    PLUGIN_VERSION, PlayRequest, PlayStatusResponse, RECORD_CANCEL_OPERATION,
    RECORD_START_OPERATION, RECORD_STOP_OPERATION, RecordControlRequest, RecordStartRequest,
    SET_CONFIG_OPERATION, STOP_OPERATION, SYNTHESIZE_OPERATION, SynthesizeRequest,
    SynthesizeResponse, TRANSCRIBE_OPERATION, TranscribeRequest, TranscribeResponse,
    VOLCENGINE_PROTOCOL_VERSION, VideoRequest, VideoResponse, VoiceInfo, VolcengineConfig,
};

use crate::ark::{self, Endpoint, VideoOptions};
use crate::speech::{self, SpeechEndpoint, TtsOptions};
use crate::{audio, config};

pub struct VolcengineService;

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for VolcengineService {
    async fn dispatch(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "Volcengine 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }
        match dispatch_operation(&request.operation, request.payload).await {
            Ok(payload) => Response::success(&request_id, payload),
            Err(error) => Response::error(
                &request_id,
                ErrorCode::ServiceError,
                format!("{error:#}"),
                false,
            ),
        }
    }
}

async fn dispatch_operation(operation: &str, payload: Value) -> Result<Value> {
    match operation {
        HANDSHAKE_OPERATION => serde_json::to_value(HandshakeResponse {
            plugin_id: PLUGIN_ID.to_string(),
            plugin_version: PLUGIN_VERSION.to_string(),
            sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION.to_string(),
            business_protocol: VOLCENGINE_PROTOCOL_VERSION,
            capabilities: vec![
                "image_generation".to_string(),
                "video_generation".to_string(),
                "text_to_speech".to_string(),
                "speech_to_text".to_string(),
            ],
            instance_id: format!("volcengine-sidecar-{}", std::process::id()),
            status: ServiceStatus::Ready,
        })
        .context("序列化握手响应失败"),

        GENERATE_IMAGE_OPERATION => {
            let request: ImageRequest =
                serde_json::from_value(payload).context("解析 generate_image 请求失败")?;
            let response = generate_image(request).await?;
            serde_json::to_value(response).context("序列化 generate_image 响应失败")
        }

        GENERATE_VIDEO_OPERATION => {
            let request: VideoRequest =
                serde_json::from_value(payload).context("解析 generate_video 请求失败")?;
            let response = generate_video(request).await?;
            serde_json::to_value(response).context("序列化 generate_video 响应失败")
        }

        GET_CONFIG_OPERATION => {
            let _: Empty = serde_json::from_value(payload).unwrap_or_default();
            serde_json::to_value(config::load()?).context("序列化配置失败")
        }

        SET_CONFIG_OPERATION => {
            let config: VolcengineConfig =
                serde_json::from_value(payload).context("解析配置失败")?;
            config::save(&normalize_config(config))?;
            serde_json::to_value(Ack {}).context("序列化响应失败")
        }

        SYNTHESIZE_OPERATION => {
            let request: SynthesizeRequest =
                serde_json::from_value(payload).context("解析 synthesize 请求失败")?;
            serde_json::to_value(synthesize(request).await?).context("序列化 synthesize 响应失败")
        }

        LIST_VOICES_OPERATION => {
            let _: Empty = serde_json::from_value(payload).unwrap_or_default();
            serde_json::to_value(list_voices()?).context("序列化 list_voices 响应失败")
        }

        PLAY_OPERATION => {
            let request: PlayRequest =
                serde_json::from_value(payload).context("解析 play 请求失败")?;
            serde_json::to_value(audio::play(request)?).context("序列化 play 响应失败")
        }

        PLAY_STATUS_OPERATION => {
            let _: Empty = serde_json::from_value(payload).unwrap_or_default();
            serde_json::to_value(PlayStatusResponse {
                playing: audio::play_running(),
            })
            .context("序列化 play_status 响应失败")
        }

        STOP_OPERATION => {
            let _: Empty = serde_json::from_value(payload).unwrap_or_default();
            audio::stop_playback();
            serde_json::to_value(Empty {}).context("序列化 stop 响应失败")
        }

        TRANSCRIBE_OPERATION => {
            let request: TranscribeRequest =
                serde_json::from_value(payload).context("解析 transcribe 请求失败")?;
            serde_json::to_value(transcribe(request).await?).context("序列化 transcribe 响应失败")
        }

        RECORD_START_OPERATION => {
            let request: RecordStartRequest =
                serde_json::from_value(payload).context("解析 record_start 请求失败")?;
            serde_json::to_value(audio::record_start(request)?)
                .context("序列化 record_start 响应失败")
        }

        RECORD_STOP_OPERATION => {
            let request: RecordControlRequest =
                serde_json::from_value(payload).context("解析 record_stop 请求失败")?;
            serde_json::to_value(audio::record_stop(request)?)
                .context("序列化 record_stop 响应失败")
        }

        RECORD_CANCEL_OPERATION => {
            let request: RecordControlRequest =
                serde_json::from_value(payload).context("解析 record_cancel 请求失败")?;
            audio::record_cancel(request);
            serde_json::to_value(Empty {}).context("序列化 record_cancel 响应失败")
        }

        other => bail!("未知的 Volcengine 操作: {other}"),
    }
}

/// 保存前规范化：去除首尾空白，空值回落默认值，轮询上限至少 30 秒。
fn normalize_config(mut config: VolcengineConfig) -> VolcengineConfig {
    let defaults = VolcengineConfig::default();
    let or_default = |value: &str, fallback: &str| {
        let value = value.trim();
        if value.is_empty() {
            fallback.to_string()
        } else {
            value.to_string()
        }
    };
    config.api_key = config.api_key.trim().to_string();
    config.image_model = config.image_model.trim().to_string();
    config.video_model = config.video_model.trim().to_string();
    config.video_poll_timeout_secs = config.video_poll_timeout_secs.max(30);
    config.tts_model = or_default(&config.tts_model, &defaults.tts_model);
    config.tts_speaker = or_default(&config.tts_speaker, &defaults.tts_speaker);
    config.asr_model = or_default(&config.asr_model, &defaults.asr_model);
    config
}

/// 解析 Agent Plan 专属 API Key；未配置时给出指向设置页的提示。
fn plan_api_key(config: &VolcengineConfig) -> Result<String> {
    let api_key = config::resolve_api_key(&config.api_key);
    if api_key.is_empty() {
        bail!("未配置 Agent Plan API Key，请在「设置 → 火山引擎」中填写后保存");
    }
    Ok(api_key)
}

/// 读取配置并解析出端点与目标模型。
fn prepare(
    pick_model: fn(&VolcengineConfig) -> &str,
    label: &str,
) -> Result<(VolcengineConfig, Endpoint, String)> {
    let config = config::load()?;
    let api_key = plan_api_key(&config)?;
    let model = pick_model(&config).trim().to_string();
    if model.is_empty() {
        bail!("未填写{label}模型，请在「设置 → 火山引擎」中填写模型名称后保存");
    }
    let endpoint = Endpoint {
        base_url: PLAN_ARK_BASE_URL.to_string(),
        api_key,
    };
    Ok((config, endpoint, model))
}

async fn generate_image(request: ImageRequest) -> Result<ImageResponse> {
    if request.prompt.trim().is_empty() {
        bail!("prompt 不能为空");
    }
    let (config, endpoint, model) = prepare(|config| &config.image_model, "图片生成")?;
    let images = request
        .images
        .iter()
        .map(|image| image_input(image))
        .collect::<Result<Vec<_>>>()?;
    let body = ark::image_body(
        &model,
        &request.prompt,
        request.size.as_deref(),
        &images,
        config.watermark,
    );
    let raw_images = ark::generate_image(&endpoint, body).await?;

    let images = raw_images
        .iter()
        .map(|raw| {
            let reference = match tiangong_media_archive::archive_image_reference(raw, None, None) {
                Ok(archived) => archived.path().to_string(),
                Err(error) => {
                    tracing::warn!(%error, "图片归档失败，保留原始引用");
                    raw.clone()
                }
            };
            GeneratedImage { reference }
        })
        .collect();
    Ok(ImageResponse { images, model })
}

async fn generate_video(request: VideoRequest) -> Result<VideoResponse> {
    if request.prompt.trim().is_empty() {
        bail!("prompt 不能为空");
    }
    let (config, endpoint, model) = prepare(|config| &config.video_model, "视频生成")?;
    let image = request.image.as_deref().map(image_input).transpose()?;
    let options = VideoOptions {
        duration: request.duration,
        resolution: request.resolution.as_deref(),
        ratio: request.ratio.as_deref(),
        image: image.as_deref(),
        watermark: config.watermark,
    };
    let task_id = ark::create_video_task(
        &endpoint,
        ark::video_body(&model, &request.prompt, &options),
    )
    .await?;
    tiangong_plugin_sidecar::emit_progress(format!("视频任务已提交（{task_id}），等待生成..."))
        .await;
    let timeout = std::time::Duration::from_secs(config.video_poll_timeout_secs);
    let status = ark::wait_video_task(&endpoint, &task_id, timeout).await?;
    Ok(VideoResponse {
        task_id,
        model,
        status,
    })
}

// ── 语音 ──

fn speech_endpoint(config: &VolcengineConfig) -> Result<SpeechEndpoint> {
    Ok(SpeechEndpoint {
        base_url: PLAN_SPEECH_BASE_URL.to_string(),
        api_key: plan_api_key(config)?,
    })
}

async fn synthesize(request: SynthesizeRequest) -> Result<SynthesizeResponse> {
    let text = request.text.trim();
    if text.is_empty() {
        bail!("text 不能为空");
    }
    let config = config::load()?;
    let endpoint = speech_endpoint(&config)?;
    let speaker = request
        .voice
        .as_deref()
        .map(str::trim)
        .filter(|voice| !voice.is_empty())
        .unwrap_or(&config.tts_speaker)
        .to_string();
    let options = TtsOptions {
        model: &config.tts_model,
        speaker: &speaker,
        speed: request.speed,
    };
    let output = speech::synthesize(&endpoint, text, &options).await?;
    let file_path = audio::media_file_path("tts", output.extension)?;
    std::fs::write(&file_path, &output.audio)
        .with_context(|| format!("写入音频文件失败：{}", file_path.display()))?;
    Ok(SynthesizeResponse {
        file_path: file_path.display().to_string(),
        mime_type: output.mime_type.to_string(),
        duration: None,
        model: format!("{} / {speaker}", config.tts_model),
    })
}

/// 音色列表：已配置的默认音色置顶，其后为常用预设。
fn list_voices() -> Result<ListVoicesResponse> {
    let speaker = config::load()?.tts_speaker;
    let mut voices: Vec<VoiceInfo> = speech::VOICE_PRESETS
        .iter()
        .map(|(id, name, gender)| VoiceInfo {
            id: (*id).to_string(),
            name: (*name).to_string(),
            gender: Some((*gender).to_string()),
        })
        .collect();
    if !speaker.trim().is_empty() && !voices.iter().any(|voice| voice.id == speaker) {
        voices.insert(
            0,
            VoiceInfo {
                id: speaker.clone(),
                name: speaker,
                gender: None,
            },
        );
    }
    Ok(ListVoicesResponse { voices })
}

async fn transcribe(request: TranscribeRequest) -> Result<TranscribeResponse> {
    // 安全限制：仅允许读取媒体目录内的音频文件。
    let format = speech::asr_format(&request.file_path)?;
    let path = audio::resolve_media_audio(&request.file_path)?;
    let audio_data = std::fs::read(&path).context("读取音频文件失败")?;
    let config = config::load()?;
    let endpoint = speech_endpoint(&config)?;
    let result = speech::recognize(
        &endpoint,
        &config.asr_model,
        &audio_data,
        format,
        request.language.as_deref(),
    )
    .await?;
    Ok(TranscribeResponse {
        text: result.text,
        language: request.language,
        duration: result.duration,
        model: config.asr_model,
        audio_path: request.file_path,
    })
}

/// 参考图输入：http(s) / data URL 原样传递，本地文件读成 base64 data URL。
fn image_input(reference: &str) -> Result<String> {
    let trimmed = reference.trim();
    if trimmed.is_empty() {
        bail!("图片路径为空");
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("data:") {
        return Ok(trimmed.to_string());
    }
    let bytes = std::fs::read(trimmed).with_context(|| format!("读取参考图片失败：{trimmed}"))?;
    let mime = tiangong_media_archive::image_mime_from_reference(trimmed)
        .unwrap_or_else(|| "image/png".to_string());
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:{mime};base64,{encoded}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_config_trims_and_defaults() {
        let config = normalize_config(VolcengineConfig {
            api_key: " key ".to_string(),
            image_model: " doubao-seedream-5-0-pro ".to_string(),
            video_model: String::new(),
            watermark: true,
            video_poll_timeout_secs: 1,
            tts_model: " ".to_string(),
            tts_speaker: " my_voice ".to_string(),
            asr_model: String::new(),
        });
        let defaults = VolcengineConfig::default();
        assert_eq!(config.api_key, "key");
        assert_eq!(config.image_model, "doubao-seedream-5-0-pro");
        assert_eq!(config.video_poll_timeout_secs, 30);
        assert!(config.watermark);
        assert_eq!(config.tts_model, defaults.tts_model);
        assert_eq!(config.tts_speaker, "my_voice");
        assert_eq!(config.asr_model, defaults.asr_model);
    }

    #[test]
    fn plan_api_key_is_required() {
        let error = plan_api_key(&VolcengineConfig::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Agent Plan API Key"), "{error}");
        let endpoint = speech_endpoint(&VolcengineConfig {
            api_key: "k".to_string(),
            ..VolcengineConfig::default()
        })
        .unwrap();
        assert_eq!(endpoint.api_key, "k");
        assert_eq!(endpoint.base_url, PLAN_SPEECH_BASE_URL);
    }

    #[test]
    fn image_input_passes_urls_and_encodes_local_files() {
        assert_eq!(image_input(" https://a/b.png ").unwrap(), "https://a/b.png");
        assert_eq!(
            image_input("data:image/png;base64,AA").unwrap(),
            "data:image/png;base64,AA"
        );
        assert!(image_input(" ").is_err());
        assert!(image_input("/definitely/missing/volcengine.png").is_err());

        let path =
            std::env::temp_dir().join(format!("tiangong-volcengine-{}.jpg", std::process::id()));
        std::fs::write(&path, b"ABC").unwrap();
        let encoded = image_input(path.to_str().unwrap()).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(encoded, "data:image/jpeg;base64,QUJD");
    }
}
