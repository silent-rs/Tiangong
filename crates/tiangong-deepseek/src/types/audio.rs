use serde::{Deserialize, Serialize};

// ── ASR（`POST /v1/tools/asr`）────────────────────────────

/// ASR 单文件大小上限：8 MiB。
pub const MAX_ASR_FILE_BYTES: usize = 8 * 1024 * 1024;
/// ASR 单文件音频时长上限：120 秒。
pub const MAX_ASR_AUDIO_SECONDS: u64 = 120;

/// 语音识别响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AsrResponse {
    /// 请求 ID。
    #[serde(default)]
    pub request_id: String,
    /// 完整转写文本。
    #[serde(default)]
    pub text: String,
    /// 音频时长（毫秒）。
    #[serde(default)]
    pub duration_ms: u64,
}

// ── TTS ───────────────────────────────────────────────────

/// 单次 TTS 请求的计费字符上限（汉字算 2 个字符）。
pub const MAX_TTS_BILLING_CHARS: usize = 10_000;
/// 整段 TTS 请求体上限：64 KiB。
pub const MAX_TTS_BODY_BYTES: usize = 64 * 1024;
/// TTS PCM 输出的采样规格：24 kHz、16 位、单声道。
pub const TTS_PCM_SAMPLE_RATE: u32 = 24_000;

/// TTS 音色。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TtsVoice {
    /// 海星，女声（默认）。
    #[default]
    Stella,
    /// 暗潮，男声。
    Tide,
}

impl TtsVoice {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stella => "stella",
            Self::Tide => "tide",
        }
    }
}

/// TTS 音频格式。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TtsAudioFormat {
    /// WAV 容器（整段接口默认）。
    #[default]
    Wav,
    /// 24 kHz、16 位、单声道原始 PCM。
    Pcm,
    /// 不带容器的 Opus 数据包（仅 WebSocket 流式支持）。
    Opus,
}

impl TtsAudioFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Pcm => "pcm",
            Self::Opus => "opus",
        }
    }
}

/// 整段语音合成请求（`POST /v1/tools/tts`）。
///
/// 音频边合成边返回；`wav` 返回 `audio/wav`，`pcm` 返回
/// 24 kHz、16 位、单声道原始 PCM。单次请求最多 30 分钟音频。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TtsRequest {
    /// 要朗读的文本，不能为空，最多 10,000 个计费字符（汉字算 2 个）。
    pub text: String,
    /// 音色，默认 `stella`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_id: Option<TtsVoice>,
    /// 输出格式，默认 `wav`；整段接口支持 `wav`/`pcm`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<TtsAudioFormat>,
    /// 朗读前是否去掉 Markdown 格式，服务端默认 true。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_markdown: Option<bool>,
}

impl TtsRequest {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            voice_id: None,
            format: None,
            filter_markdown: None,
        }
    }

    pub fn voice(mut self, voice: TtsVoice) -> Self {
        self.voice_id = Some(voice);
        self
    }

    pub fn format(mut self, format: TtsAudioFormat) -> Self {
        self.format = Some(format);
        self
    }

    pub fn filter_markdown(mut self, filter: bool) -> Self {
        self.filter_markdown = Some(filter);
        self
    }
}

/// WebSocket 流式合成会话的结束事件。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TtsFinishEvent {
    /// `success` 或错误码：`invalid_input` / `content_filter` / `input_timeout` / `service_error`。
    pub code: String,
    #[serde(default)]
    pub message: String,
}

impl TtsFinishEvent {
    pub fn is_success(&self) -> bool {
        self.code == "success"
    }
}

/// WebSocket 流式合成过程中收到的事件：二进制音频帧或结束事件。
#[derive(Debug, Clone, PartialEq)]
pub enum TtsStreamEvent {
    /// 音频帧（格式由连接参数决定，PCM 为 24 kHz/16 bit/单声道）。
    Audio(Vec<u8>),
    /// 会话结束事件；`code == "success"` 表示正常完成。
    Finish(TtsFinishEvent),
}
