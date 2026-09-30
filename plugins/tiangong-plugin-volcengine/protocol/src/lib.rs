//! Volcengine（火山引擎）插件私有业务协议。
//!
//! 当前仅对接火山方舟 **Agent Plan** 订阅套餐：同一个专属 API Key 覆盖
//! - 图片生成（Seedream）与视频生成（Seedance），走 `/api/plan/v3`；
//! - 语音合成（TTS，HTTP Chunked 单向流式）与语音识别（ASR，WebSocket 流式输入），
//!   走豆包语音的 `/api/v3/plan/...` 路径；以及本机录音与播放。
//!
//! 只需 Agent Plan API Key。模型名由用户在设置页手动填写（候选取自官方套餐概览，
//! 随插件版本阶段性更新），不调用需 Access Key 签名的管控面模型列表接口。
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
/// 朗读文本：已有合成音频直接播放文件，否则边合成边流式播放（同时落盘缓存）。
pub const SPEAK_OPERATION: &str = "speak";
pub const PLAY_STATUS_OPERATION: &str = "play_status";
pub const STOP_OPERATION: &str = "stop";
pub const TRANSCRIBE_OPERATION: &str = "transcribe";
pub const RECORD_START_OPERATION: &str = "record_start";
pub const RECORD_STOP_OPERATION: &str = "record_stop";
pub const RECORD_CANCEL_OPERATION: &str = "record_cancel";
/// WASM `on_turn_finished` 生命周期钩子转发：本轮最终答复（自动朗读用）。
pub const TURN_FINISHED_OPERATION: &str = "turn_finished";
/// sidecar 通知通道：本轮最终答复（经宿主 `sidecar.event` 到达插件 UI）。
pub const REPLY_FINAL_CHANNEL: &str = "volcengine.reply_final";
/// sidecar 通知通道：边录边识别的中间文本（`{session_id, text}`，text 为累计全文）。
pub const ASR_PARTIAL_CHANNEL: &str = "volcengine.asr_partial";

/// Agent Plan 数据面地址（图片 / 视频生成）。
pub const PLAN_ARK_BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/plan/v3";
/// Agent Plan 文本模型地址（Anthropic 兼容），在天工「模型配置」中使用。
pub const PLAN_TEXT_BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/plan";
/// 豆包语音地址（Agent Plan 路径为 `/api/v3/plan/...`）。
pub const PLAN_SPEECH_BASE_URL: &str = "https://openspeech.bytedance.com";
/// 视频任务默认轮询上限（秒）。
pub const DEFAULT_VIDEO_POLL_TIMEOUT_SECS: u64 = 300;
/// 默认语音合成模型（豆包语音合成模型 2.0）。
pub const DEFAULT_TTS_MODEL: &str = "doubao-seed-tts-2.0";
/// 默认语音识别模型（豆包流式语音识别模型 2.0）。
pub const DEFAULT_ASR_MODEL: &str = "doubao-seed-asr-2.0";
/// 语音合成默认音色（豆包 2.0 通用女声 Vivi）。
pub const DEFAULT_TTS_SPEAKER: &str = "zh_female_vv_uranus_bigtts";

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
pub struct Speak;

impl VolcengineOperation for Speak {
    const NAME: &'static str = SPEAK_OPERATION;
    type Request = SpeakRequest;
    type Response = SpeakResponse;
}
pub struct TurnFinished;

impl VolcengineOperation for TurnFinished {
    const NAME: &'static str = TURN_FINISHED_OPERATION;
    type Request = TurnFinishedRequest;
    type Response = Empty;
}

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
///
/// 仅对接 Agent Plan：接口地址固定为 Agent Plan 专属地址，不再单独配置。
/// API Key 支持 `${ENV_VAR}` 形式的环境变量引用；旧版的 Access Key、
/// 套餐版本等字段读取时忽略、保存时丢弃。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolcengineConfig {
    /// Agent Plan 专属 API Key（图片 / 视频 / 语音共用）。
    #[serde(default)]
    pub api_key: String,
    /// 图片生成模型，留空则不提供生图。
    #[serde(default)]
    pub image_model: String,
    /// 视频生成模型，留空则不提供生视频。
    #[serde(default)]
    pub video_model: String,
    /// 是否添加 AI 生成水印。
    #[serde(default)]
    pub watermark: bool,
    /// 视频任务轮询上限（秒）。
    #[serde(default = "default_video_poll_timeout_secs")]
    pub video_poll_timeout_secs: u64,
    /// 语音合成模型（如 `doubao-seed-tts-2.0`）。
    #[serde(default = "default_tts_model")]
    pub tts_model: String,
    /// 默认音色（发音人 ID）。
    #[serde(default = "default_tts_speaker")]
    pub tts_speaker: String,
    /// 语音识别模型（如 `doubao-seed-asr-2.0`）。
    #[serde(default = "default_asr_model")]
    pub asr_model: String,
}

fn default_video_poll_timeout_secs() -> u64 {
    DEFAULT_VIDEO_POLL_TIMEOUT_SECS
}

fn default_tts_model() -> String {
    DEFAULT_TTS_MODEL.to_string()
}

fn default_tts_speaker() -> String {
    DEFAULT_TTS_SPEAKER.to_string()
}

fn default_asr_model() -> String {
    DEFAULT_ASR_MODEL.to_string()
}

impl Default for VolcengineConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            image_model: String::new(),
            video_model: String::new(),
            watermark: false,
            video_poll_timeout_secs: DEFAULT_VIDEO_POLL_TIMEOUT_SECS,
            tts_model: default_tts_model(),
            tts_speaker: default_tts_speaker(),
            asr_model: default_asr_model(),
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

/// 朗读请求：同一文本 + 音色 + 语速只合成一次，之后复用生成的音频文件。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpeakRequest {
    pub text: String,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub speed: Option<f64>,
}

/// 朗读响应：请求返回即已开始播放，结束经 `play_status` 轮询。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpeakResponse {
    /// true：本次边合成边流式播放；false：播放已生成的音频文件。
    pub streamed: bool,
    /// 已有音频文件的路径（流式播放时为空，合成完成后写入缓存）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
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
    /// 最近一次流式朗读失败的原因（读取即清除）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
    /// 边录边识别：录音期间即连接语音识别并实时上传，停止时直接返回识别文本。
    #[serde(default)]
    pub transcribe: bool,
    /// 实时识别语种（可选，同 [`TranscribeRequest::language`]）。
    #[serde(default)]
    pub language: Option<String>,
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
    /// 开启边录边识别时的识别文本；未开启时为空，调用方可再走 `transcribe`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// 本轮最终答复（`on_turn_finished` 从会话快照提取后转发 sidecar）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnFinishedRequest {
    pub session_id: String,
    pub message_id: String,
    /// 最终答复正文（Markdown 原文，朗读前由 UI 转换）。
    pub text: String,
}

/// 从 `on_turn_finished` 的会话快照中提取本轮最终答复。
///
/// 只认本轮（用户锚点之后）**Summary 相位**的 assistant 消息——Core 只在
/// 轮次成功收尾时把最终答复定格为 Summary（失败会回收为过程相位），
/// 工具执行期间的过程文本（React 相位）与思考过程都不会被选中。
/// 锚点优先按 `turn_start_message_id` 定位，缺失时回退 `turn_start_idx`。
pub fn final_reply(session_json: &str, turn_start_idx: u32) -> Option<TurnFinishedRequest> {
    let session: serde_json::Value = serde_json::from_str(session_json).ok()?;
    let session_id = session.get("id")?.as_str()?.to_string();
    let messages = session.get("messages")?.as_array()?;
    let anchor = session
        .get("turn_start_message_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|id| {
            messages.iter().position(|message| {
                message.get("id").and_then(serde_json::Value::as_str) == Some(id)
            })
        })
        .unwrap_or(turn_start_idx as usize);
    let str_field = |message: &serde_json::Value, key: &str| {
        message
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    messages
        .get(anchor + 1..)?
        .iter()
        .rev()
        .filter(|message| {
            str_field(message, "role").as_deref() == Some("assistant")
                && str_field(message, "phase").as_deref() == Some("summary")
        })
        .find_map(|message| {
            let text = message
                .get("content")
                .and_then(serde_json::Value::as_array)?
                .iter()
                .filter(|block| {
                    block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                })
                .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
                .collect::<String>();
            (!text.trim().is_empty()).then(|| TurnFinishedRequest {
                session_id: session_id.clone(),
                message_id: str_field(message, "id").unwrap_or_default(),
                text,
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, role: &str, phase: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "role": role,
            "phase": phase,
            "content": [{ "type": "text", "text": text }],
        })
    }

    #[test]
    fn final_reply_picks_summary_of_current_turn_only() {
        let session = serde_json::json!({
            "id": "s1",
            "turn_start_message_id": "u2",
            "messages": [
                message("u1", "user", "normal", "上一轮"),
                message("a1", "assistant", "summary", "上一轮答复"),
                message("u2", "user", "normal", "本轮"),
                message("a2", "assistant", "react", "我先查一下"),
                message("t1", "tool", "react", "工具结果"),
                message("a3", "assistant", "summary", "最终答复"),
            ],
        })
        .to_string();
        let reply = final_reply(&session, 0).unwrap();
        assert_eq!(reply.session_id, "s1");
        assert_eq!(reply.message_id, "a3");
        assert_eq!(reply.text, "最终答复");
    }

    #[test]
    fn final_reply_ignores_turns_without_summary() {
        // 失败 / 取消的轮次没有 Summary 相位答复：只有过程文本时不朗读。
        let session = serde_json::json!({
            "id": "s1",
            "messages": [
                message("a1", "assistant", "summary", "上一轮答复"),
                message("u2", "user", "normal", "本轮"),
                message("a2", "assistant", "react", "过程文本"),
                message("a3", "assistant", "summary", "  "),
            ],
        })
        .to_string();
        // 无锚点 id 时回退 turn_start_idx。
        assert!(final_reply(&session, 1).is_none());
        assert!(final_reply("not json", 0).is_none());
    }

    #[test]
    fn config_defaults_fill_missing_fields() {
        let config: VolcengineConfig = serde_json::from_str(r#"{"api_key":"k"}"#).unwrap();
        assert_eq!(config.api_key, "k");
        assert_eq!(
            config.video_poll_timeout_secs,
            DEFAULT_VIDEO_POLL_TIMEOUT_SECS
        );
        assert!(!config.watermark);
        assert_eq!(config.tts_model, DEFAULT_TTS_MODEL);
        assert_eq!(config.tts_speaker, DEFAULT_TTS_SPEAKER);
        assert_eq!(config.asr_model, DEFAULT_ASR_MODEL);
        assert!(config.image_model.is_empty() && config.video_model.is_empty());
    }

    #[test]
    fn legacy_config_fields_are_ignored() {
        // 旧版配置中的 base_url / speech（按量付费）与 access_key_id /
        // secret_access_key / edition（模型列表查询）字段不影响解析，且保存时丢弃。
        let config: VolcengineConfig = serde_json::from_str(
            r#"{"base_url":"https://x","api_key":"k","speech":{"api_key":"s"},
                "access_key_id":"ak","secret_access_key":"sk","edition":"enterprise",
                "image_model":"doubao-seedream-5-0-pro"}"#,
        )
        .unwrap();
        assert_eq!(config.api_key, "k");
        assert_eq!(config.image_model, "doubao-seedream-5-0-pro");
        assert_eq!(config.tts_model, DEFAULT_TTS_MODEL);
        let saved = serde_json::to_value(&config).unwrap();
        for legacy in [
            "access_key_id",
            "secret_access_key",
            "edition",
            "base_url",
            "speech",
        ] {
            assert!(saved.get(legacy).is_none(), "{legacy} 不应再被保存");
        }
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
