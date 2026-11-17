use crate::client::{DeepSeekClient, MultipartForm};
use crate::error::DeepSeekError;
use crate::types::{MAX_OCR_FILE_BYTES, OcrPurpose, OcrResponse};

const OCR_PATH: &str = "/v1/tools/ocr";

/// OCR 文字识别（`POST /v1/tools/ocr`）。
///
/// 识别 PDF、Word、PowerPoint 文档或图片中的文字，按页返回结果；
/// 文件不会存入 Files API，也不会交给对话模型。
/// 识别过程中服务端持续发送换行符保持连接，SDK 按完整 JSON 解析。
pub struct Ocr<'c> {
    client: &'c DeepSeekClient,
}

impl<'c> Ocr<'c> {
    pub fn new(client: &'c DeepSeekClient) -> Self {
        Self { client }
    }

    /// 上传并识别文件。
    ///
    /// - 支持格式（由文件实际内容判断）：PDF、Word（DOC/DOCX）、
    ///   PowerPoint（PPT/PPTX），以及 PNG/JPEG/WebP/GIF/BMP/TIFF 图片。
    /// - 单文件最大 100 MiB、1000 页；上传须在 120 秒内完成，总时长 180 秒。
    /// - 按识别成功的页数（`page_count`）计费，失败或超时不计费。
    pub async fn recognize(
        &self,
        filename: &str,
        data: Vec<u8>,
        purpose: OcrPurpose,
    ) -> Result<OcrResponse, DeepSeekError> {
        if data.len() > MAX_OCR_FILE_BYTES {
            return Err(DeepSeekError::InvalidRequest(format!(
                "file is too large: max {} MiB, got {} bytes",
                MAX_OCR_FILE_BYTES / (1024 * 1024),
                data.len()
            )));
        }
        let form = MultipartForm::new()
            .field("purpose", purpose.as_str())
            .file("file", filename, data);
        self.client.post_tools_multipart(OCR_PATH, &form).await
    }
}
