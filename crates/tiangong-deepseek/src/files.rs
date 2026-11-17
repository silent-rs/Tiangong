use crate::client::{DeepSeekClient, MultipartForm, percent_encode};
use crate::error::DeepSeekError;
use crate::types::{
    DeleteFileResponse, FileObject, FileParsePurpose, ListFilesParams, ListFilesResponse,
    MAX_DOCUMENT_FILE_BYTES,
};

/// Files API 文件有效期：1 小时至 30 天。
pub const MIN_EXPIRES_AFTER_SECONDS: u32 = 3600;
pub const MAX_EXPIRES_AFTER_SECONDS: u32 = 2_592_000;

pub struct Files<'c> {
    client: &'c DeepSeekClient,
}

impl<'c> Files<'c> {
    pub fn new(client: &'c DeepSeekClient) -> Self {
        Self { client }
    }

    /// 上传图片文件（JPEG/PNG/GIF/WebP，按文件实际内容判断，单文件最大 64 MiB）。
    ///
    /// `expires_after_seconds` 取值 3600–2592000（1 小时至 30 天），
    /// `None` 表示永久有效。上传须在 10 分钟内完成。
    pub async fn upload(
        &self,
        filename: &str,
        data: Vec<u8>,
        expires_after_seconds: Option<u32>,
    ) -> Result<FileObject, DeepSeekError> {
        let mut form = MultipartForm::new()
            .field("purpose", "user_data")
            .file("file", filename, data);
        if let Some(seconds) = expires_after_seconds {
            form = form
                .field("expires_after[anchor]", "created_at")
                .field("expires_after[seconds]", &seconds.to_string());
        }
        self.client.post_multipart("/files", &form).await
    }

    /// 上传并解析文档（PDF、Word、PowerPoint、图片等），供对话中通过
    /// `file_id` 引用后基于文档作答。文件在上传时完成解析，解析成功后
    /// 才返回 `file_id`；调用方超时需覆盖上传加解析时间（各至多 120 秒）。
    ///
    /// - 单文件最大 100 MiB、1000 页；解析成功按页数一次性计费。
    /// - `purpose` 字段必须位于 `file` 之前，multipart 顺序由 SDK 保证。
    /// - 纯文本 / 表格 / 电子书（txt、md、csv、json、代码、XLSX、XLS、
    ///   EPUB、MOBI）不需要解析，请继续用 [`Files::upload`]（`user_data`）。
    pub async fn upload_document(
        &self,
        filename: &str,
        data: Vec<u8>,
        purpose: FileParsePurpose,
    ) -> Result<FileObject, DeepSeekError> {
        if data.len() > MAX_DOCUMENT_FILE_BYTES {
            return Err(DeepSeekError::InvalidRequest(format!(
                "file is too large: max {} MiB, got {} bytes",
                MAX_DOCUMENT_FILE_BYTES / (1024 * 1024),
                data.len()
            )));
        }
        let form = MultipartForm::new()
            .field("purpose", purpose.as_str())
            .file("file", filename, data);
        self.client.post_multipart("/files", &form).await
    }

    /// 列出当前 API key 下的文件，游标分页。
    pub async fn list(&self, params: ListFilesParams) -> Result<ListFilesResponse, DeepSeekError> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(after) = params.after {
            query.push(("after", after));
        }
        if let Some(limit) = params.limit {
            query.push(("limit", limit.to_string()));
        }
        if let Some(order) = params.order {
            query.push(("order", order.as_str().to_string()));
        }
        if let Some(purpose) = params.purpose {
            query.push(("purpose", purpose));
        }
        self.client.get_with_query("/files", &query).await
    }

    /// 查询单个文件信息。
    pub async fn retrieve(&self, file_id: &str) -> Result<FileObject, DeepSeekError> {
        self.client
            .get(&format!("/files/{}", percent_encode(file_id)))
            .await
    }

    /// 删除指定文件。
    pub async fn delete(&self, file_id: &str) -> Result<DeleteFileResponse, DeepSeekError> {
        self.client
            .delete(&format!("/files/{}", percent_encode(file_id)))
            .await
    }
}
