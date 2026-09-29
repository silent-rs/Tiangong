//! Volcengine（火山引擎）插件私有业务协议。
//!
//! 一个插件同时提供：
//! - 火山方舟 Ark：图片生成（Seedream）与视频生成（Seedance）；
//! - 豆包语音：语音合成（TTS，HTTP Chunked 单向流式 V3）与
//!   语音识别（ASR，录音文件识别极速版），以及本机录音与播放。
//!
//! 连接信息与模型由插件设置页独立配置，不依赖全局模型配置（models.json）。

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const PLUGIN_ID: &str = "volcengine";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const VOLCENGINE_PROTOCOL_VERSION: u32 = 1;

pub const TOOL_GENERATE_IMAGE: &str = "generate_image";
pub const TOOL_GENERATE_VIDEO: &str = "generate_video";
pub const TOOL_TEXT_TO_SPEECH: &str = "text_to_speech";
pub const TOOL_SPEECH_TO_TEXT: &str = "speech_to_text";

pub const GENERATE_IMAGE_OPERATION: &str = "generate_image";
pub const GENERATE_VIDEO_OPERATION: &str = "generate_video";
pub const GET_CONFIG_OPERATION: &str = "get_config";
pub const SET_CONFIG_OPERATION: &str = "set_config";
pub const SYNTHESIZE_OPERATION: &str = "synthesize";
pub const LIST_VOICES_OPERATION: &str = "list_voices";
pub const PLAY_OPERATION: &str = "play";
pub const PLAY_STATUS_OPERATION: &str = "play_status";
pub const STOP_OPERATION: &str = "stop";
pub const TRANSCRIBE_OPERATION: &str = "transcribe";
pub const RECORD_START_OPERATION: &str = "record_start";
pub const RECORD_STOP_OPERATION: &str = "record_stop";
pub const RECORD_CANCEL_OPERATION: &str = "record_cancel";

/// 火山方舟默认地址（华北 2 北京）。
pub const DEFAULT_BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/v3";
/// 视频任务默认轮询上限（秒）。
pub const DEFAULT_VIDEO_POLL_TIMEOUT_SECS: u64 = 300;
/// 豆包语音默认地址。
pub const DEFAULT_SPEECH_BASE_URL: &str = "https://openspeech.bytedance.com";
/// 语音合成默认资源 ID（豆包语音合成模型 2.0）。
pub const DEFAULT_TTS_RESOURCE_ID: &str = "seed-tts-2.0";
/// 语音合成默认音色（豆包 2.0 通用女声 Vivi）。
pub const DEFAULT_TTS_SPEAKER: &str = "zh_female_vv_uranus_bigtts";
/// 语音识别默认资源 ID（录音文件识别极速版）。
pub const DEFAULT_ASR_RESOURCE_ID: &str = "volc.bigasr.auc_turbo";

/// 一个类型化的业务操作。
pub trait VolcengineOperation {
    const NAME: &'static str;
    type Request: Serialize;
    type Response: DeserializeOwned;
}

pub struct GenerateImage;
pub struct GenerateVideo;
pub struct GetConfig;
pub struct SetConfig;

impl VolcengineOperation for GenerateImage {
    const NAME: &'static str = GENERATE_IMAGE_OPERATION;
    type Request = ImageRequest;
    type Response = ImageResponse;
}

impl VolcengineOperation for GenerateVideo {
    const NAME: &'static str = GENERATE_VIDEO_OPERATION;
    type Request = VideoRequest;
    type Response = VideoResponse;
}

impl VolcengineOperation for GetConfig {
    const NAME: &'static str = GET_CONFIG_OPERATION;
    type Request = Empty;
    type Response = VolcengineConfig;
}

impl VolcengineOperation for SetConfig {
    const NAME: &'static str = SET_CONFIG_OPERATION;
    type Request = VolcengineConfig;
    type Response = Ack;
}

pub struct Synthesize;
pub struct ListVoices;
pub struct Play;
pub struct PlayStatus;
pub struct Stop;
pub struct Transcribe;
pub struct RecordStart;
pub struct RecordStop;
pub struct RecordCancel;

impl VolcengineOperation for Synthesize {
    const NAME: &'static str = SYNTHESIZE_OPERATION;
    type Request = SynthesizeRequest;
    type Response = SynthesizeResponse;
}

impl VolcengineOperation for ListVoices {
    const NAME: &'static str = LIST_VOICES_OPERATION;
    type Request = Empty;
    type Response = ListVoicesResponse;
}

impl VolcengineOperation for Play {
    const NAME: &'static str = PLAY_OPERATION;
    type Request = PlayRequest;
    type Response = PlayResponse;
}

impl VolcengineOperation for PlayStatus {
    const NAME: &'static str = PLAY_STATUS_OPERATION;
    type Request = Empty;
    type Response = PlayStatusResponse;
}

impl VolcengineOperation for Stop {
    const NAME: &'static str = STOP_OPERATION;
    type Request = Empty;
    type Response = Empty;
}

impl VolcengineOperation for Transcribe {
    const NAME: &'static str = TRANSCRIBE_OPERATION;
    type Request = TranscribeRequest;
    type Response = TranscribeResponse;
}

impl VolcengineOperation for RecordStart {
    const NAME: &'static str = RECORD_START_OPERATION;
    type Request = RecordStartRequest;
    type Response = RecordStartResponse;
}

impl VolcengineOperation for RecordStop {
    const NAME: &'static str = RECORD_STOP_OPERATION;
    type Request = RecordControlRequest;
    type Response = RecordStopResponse;
}

impl VolcengineOperation for RecordCancel {
    const NAME: &'static str = RECORD_CANCEL_OPERATION;
    type Request = RecordControlRequest;
    type Response = Empty;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Empty {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ack {}

/// 插件持久化配置（存于插件 data 目录下 `config.json`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolcengineConfig {
    /// Ark API 地址，默认 [`DEFAULT_BASE_URL`]。
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// API Key，支持 `${ENV_VAR}` 形式的环境变量引用。
    #[serde(default)]
    pub api_key: String,
    /// 图片生成模型 ID 或推理接入点（`ep-xxx`），留空则不提供生图。
    #[serde(default)]
    pub image_model: String,
    /// 视频生成模型 ID 或推理接入点（`ep-xxx`），留空则不提供生视频。
    #[serde(default)]
    pub video_model: String,
    /// 是否添加 AI 生成水印。
    #[serde(default)]
    pub watermark: bool,
    /// 视频任务轮询上限（秒）。
    #[serde(default = "default_video_poll_timeout_secs")]
    pub video_poll_timeout_secs: u64,
    /// 豆包语音配置（语音合成 / 语音识别）。
    #[serde(default)]
    pub speech: SpeechConfig,
}

/// 豆包语音配置。
///
/// 豆包语音与火山方舟是两套独立鉴权：这里的 API Key 取自豆包语音控制台
/// 「API Key 管理」（新版控制台）；旧版控制台可改填 App ID + Access Token。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechConfig {
    /// 豆包语音地址，默认 [`DEFAULT_SPEECH_BASE_URL`]。
    #[serde(default = "default_speech_base_url")]
    pub base_url: String,
    /// 新版控制台 API Key（`X-Api-Key`），支持 `${ENV_VAR}`。
    #[serde(default)]
    pub api_key: String,
    /// 旧版控制台 App ID（`X-Api-App-Id` / `X-Api-App-Key`），与 access_token 成对使用。
    #[serde(default)]
    pub app_id: String,
    /// 旧版控制台 Access Token（`X-Api-Access-Key`），支持 `${ENV_VAR}`。
    #[serde(default)]
    pub access_token: String,
    /// 语音合成资源 ID（如 `seed-tts-2.0` / `seed-tts-1.0`）。
    #[serde(default = "default_tts_resource_id")]
    pub tts_resource_id: String,
    /// 默认音色（发音人 ID）。
    #[serde(default = "default_tts_speaker")]
    pub tts_speaker: String,
    /// 语音识别资源 ID（默认录音文件识别极速版）。
    #[serde(default = "default_asr_resource_id")]
    pub asr_resource_id: String,
}

fn default_speech_base_url() -> String {
    DEFAULT_SPEECH_BASE_URL.to_string()
}

fn default_tts_resource_id() -> String {
    DEFAULT_TTS_RESOURCE_ID.to_string()
}

fn default_tts_speaker() -> String {
    DEFAULT_TTS_SPEAKER.to_string()
}

fn default_asr_resource_id() -> String {
    DEFAULT_ASR_RESOURCE_ID.to_string()
}

impl Default for SpeechConfig {
    fn default() -> Self {
        Self {
            base_url: default_speech_base_url(),
            api_key: String::new(),
            app_id: String::new(),
            access_token: String::new(),
            tts_resource_id: default_tts_resource_id(),
            tts_speaker: default_tts_speaker(),
            asr_resource_id: default_asr_resource_id(),
        }
    }
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

fn default_video_poll_timeout_secs() -> u64 {
    DEFAULT_VIDEO_POLL_TIMEOUT_SECS
}

impl Default for VolcengineConfig {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            api_key: String::new(),
            image_model: String::new(),
            video_model: String::new(),
            watermark: false,
            video_poll_timeout_secs: DEFAULT_VIDEO_POLL_TIMEOUT_SECS,
            speech: SpeechConfig::default(),
        }
    }
}

/// 图片生成请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageRequest {
    pub prompt: String,
    /// 尺寸：`1K`/`2K`/`4K` 或 `宽x高`（如 `2048x2048`），留空用模型默认。
    #[serde(default)]
    pub size: Option<String>,
    /// 参考图（本地路径、http(s) URL 或 data URL），传入时为图生图。
    #[serde(default)]
    pub images: Vec<String>,
}

/// 单张生成图片的归档结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeneratedImage {
    /// 本地归档路径；归档失败时为原始 URL。
    pub reference: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageResponse {
    pub images: Vec<GeneratedImage>,
    pub model: String,
}

/// 视频生成请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VideoRequest {
    pub prompt: String,
    /// 时长（秒）。
    #[serde(default)]
    pub duration: Option<u32>,
    /// 分辨率：`480p` / `720p` / `1080p`。
    #[serde(default)]
    pub resolution: Option<String>,
    /// 宽高比：`16:9` / `9:16` / `1:1` 等。
    #[serde(default)]
    pub ratio: Option<String>,
    /// 首帧图（本地路径、http(s) URL 或 data URL），传入时为图生视频。
    #[serde(default)]
    pub image: Option<String>,
}

/// 视频任务状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VideoStatus {
    /// 生成完成。
    Succeeded {
        video_url: String,
        #[serde(default)]
        duration: Option<f64>,
    },
    /// 轮询超时时任务仍在排队或生成中。
    Running { status: String },
    /// 生成失败或被取消。
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoResponse {
    pub task_id: String,
    pub model: String,
    pub status: VideoStatus,
}

// ── 语音合成（TTS）──

/// 语音合成请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SynthesizeRequest {
    /// 待合成文本。
    pub text: String,
    /// 音色（发音人 ID，可选；未指定时用配置的默认音色）。
    #[serde(default)]
    pub voice: Option<String>,
    /// 语速倍率（可选，1.0 为正常语速，取值约 0.5～2.0）。
    #[serde(default)]
    pub speed: Option<f64>,
}

/// 语音合成响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SynthesizeResponse {
    /// 音频文件本地路径。
    pub file_path: String,
    /// 音频 MIME 类型。
    pub mime_type: String,
    /// 音频时长（秒，可能不返回）。
    #[serde(default)]
    pub duration: Option<f64>,
    /// 实际使用的资源 ID / 音色。
    pub model: String,
}

/// 播放音频请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayRequest {
    /// 音频文件本地路径。
    pub file_path: String,
}

/// 播放音频响应。
///
/// 播放为后台执行：请求返回只表示「已启动」，完成状态经
/// [`PLAY_STATUS_OPERATION`] 轮询获取。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayResponse {
    pub started: bool,
}

/// 播放状态响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayStatusResponse {
    pub playing: bool,
}

/// 音色信息（设置页 / 前端选择用）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub gender: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListVoicesResponse {
    pub voices: Vec<VoiceInfo>,
}

// ── 语音识别（ASR）──

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscribeRequest {
    /// 音频文件路径（仅允许 ~/.tiangong/media/ 目录下）。
    pub file_path: String,
    /// 语种（可选，如 `zh-CN` / `en-US`；留空自动识别中英文及方言）。
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscribeResponse {
    pub text: String,
    #[serde(default)]
    pub language: Option<String>,
    /// 音频时长（秒）。
    #[serde(default)]
    pub duration: Option<f64>,
    pub model: String,
    /// 转录音频文件路径（回传请求的 file_path，供前端关联语音消息回放）。
    pub audio_path: String,
}

/// 开始录音请求：会话 ID 由调用方生成（非空），停止/取消携带同一编号。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordStartRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordStartResponse {
    pub session_id: String,
}

/// 停止/取消录音请求：只作用于 `session_id` 匹配的录音会话，
/// 防止迟到的旧取消请求终止新录音。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordControlRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordStopResponse {
    pub file_path: String,
    pub mime_type: String,
    #[serde(default)]
    pub duration: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_fill_missing_fields() {
        let config: VolcengineConfig = serde_json::from_str(r#"{"api_key":"k"}"#).unwrap();
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.api_key, "k");
        assert_eq!(
            config.video_poll_timeout_secs,
            DEFAULT_VIDEO_POLL_TIMEOUT_SECS
        );
        assert!(!config.watermark);
        assert_eq!(config.speech, SpeechConfig::default());
    }

    #[test]
    fn speech_config_defaults_fill_missing_fields() {
        let config: VolcengineConfig =
            serde_json::from_str(r#"{"speech":{"api_key":"s","tts_speaker":"x"}}"#).unwrap();
        assert_eq!(config.speech.api_key, "s");
        assert_eq!(config.speech.tts_speaker, "x");
        assert_eq!(config.speech.base_url, DEFAULT_SPEECH_BASE_URL);
        assert_eq!(config.speech.tts_resource_id, DEFAULT_TTS_RESOURCE_ID);
        assert_eq!(config.speech.asr_resource_id, DEFAULT_ASR_RESOURCE_ID);
    }

    #[test]
    fn video_status_is_tagged() {
        let status = VideoStatus::Succeeded {
            video_url: "https://v".to_string(),
            duration: Some(5.0),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["kind"], "succeeded");
        assert_eq!(serde_json::from_value::<VideoStatus>(json).unwrap(), status);
    }
}
