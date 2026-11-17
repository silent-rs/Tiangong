use crate::client::{DeepSeekClient, MultipartForm};
use crate::error::DeepSeekError;
use crate::types::{AsrResponse, MAX_ASR_FILE_BYTES};

const ASR_PATH: &str = "/v1/tools/asr";

/// 语音识别（`POST /v1/tools/asr`）。
///
/// 一次请求发送完整音频，返回完整转写文本，不支持流式识别；
/// 语种根据音频自动识别。计费按音频时长（精确到秒，不足 1 秒按 1 秒计）。
pub struct Asr<'c> {
    client: &'c DeepSeekClient,
}

impl<'c> Asr<'c> {
    pub fn new(client: &'c DeepSeekClient) -> Self {
        Self { client }
    }

    /// 转写音频文件。
    ///
    /// - 格式（由文件内容判断）：16 位 PCM WAV（采样率 8000/16000/22050/
    ///   24000/32000/44100/48000 Hz）或 Ogg Opus。
    /// - 声道：单声道或双声道；双声道只识别左声道。
    /// - 大小：最大 8 MiB；时长：最长 120 秒。
    pub async fn transcribe(
        &self,
        filename: &str,
        data: Vec<u8>,
    ) -> Result<AsrResponse, DeepSeekError> {
        if data.len() > MAX_ASR_FILE_BYTES {
            return Err(DeepSeekError::InvalidRequest(format!(
                "file is too large: max {} MiB, got {} bytes",
                MAX_ASR_FILE_BYTES / (1024 * 1024),
                data.len()
            )));
        }
        validate_audio_magic(filename, &data)?;
        let form = MultipartForm::new().file("file", filename, data);
        self.client.post_tools_multipart(ASR_PATH, &form).await
    }
}

/// 轻量魔数检查：WAV（`RIFF....WAVE`）或 Ogg（`OggS`）。
/// 服务端同样会校验，这里提前失败并给出可读错误。
fn validate_audio_magic(filename: &str, data: &[u8]) -> Result<(), DeepSeekError> {
    let is_wav = data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE";
    let is_ogg = data.len() >= 4 && &data[0..4] == b"OggS";
    if !is_wav && !is_ogg {
        return Err(DeepSeekError::InvalidRequest(format!(
            "'{filename}' is not 16-bit PCM WAV or Ogg Opus audio"
        )));
    }
    Ok(())
}
