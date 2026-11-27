//! DeepSeek Files API 上传准备：把内联图片/文件转换为 Responses 可用的 file_id。
//!
//! 上传直接开启；远程图片 URL 仍走内联 image_url，不上传。
//! base64 内容 hash → DeepSeek file_id 表用于去重，并落盘到
//! `~/.tiangong/data/deepseek/file-ids.json`。

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tiangong_deepseek::DeepSeekClient;
use tiangong_deepseek::files::{MAX_EXPIRES_AFTER_SECONDS, MIN_EXPIRES_AFTER_SECONDS};
use tiangong_deepseek::types::{FileObject, FileParsePurpose, MAX_DOCUMENT_FILE_BYTES};

use super::error::map_deepseek_error;
use crate::error::LlmError;
use crate::message::{FileContent, MessageContent, MessageRole};
use crate::request::ProviderRequest;

const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_USER_DATA_BYTES: usize = 32 * 1024 * 1024;
const MAX_INLINE_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// DeepSeek 上传准备层内部使用的 file_id 标记；不写入会话或 Core 数据结构。
pub(super) const IMAGE_FILE_ID_PREFIX: &str = "deepseek-image-file-id:";
pub(super) const FILE_ID_PREFIX: &str = "deepseek-file-id:";
pub(super) const LOCAL_PATH_PREFIX: &str = "local-path:";

#[derive(Clone, Default)]
pub(super) struct DeepSeekFileIdTable {
    entries: Arc<Mutex<HashMap<String, FileIdEntry>>>,
    path: Option<Arc<PathBuf>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileIdEntry {
    file_id: String,
    kind: UploadKind,
    expires_at: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct PersistedFileIdTable {
    #[serde(default)]
    entries: HashMap<String, FileIdEntry>,
}

impl FileIdEntry {
    fn is_valid(&self) -> bool {
        self.expires_at.is_none_or(|expires_at| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            now.saturating_add(60) < expires_at
        })
    }
}

impl DeepSeekFileIdTable {
    pub(super) fn persistent() -> Self {
        Self::with_path(default_table_path())
    }

    fn with_path(path: PathBuf) -> Self {
        let entries = load_table(&path);
        Self {
            entries: Arc::new(Mutex::new(entries)),
            path: Some(Arc::new(path)),
        }
    }

    fn get(&self, key: &str, kind: UploadKind) -> Option<String> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .filter(|entry| entry.kind == kind && entry.is_valid())
            .map(|entry| entry.file_id.clone())
    }

    fn insert(&self, key: String, kind: UploadKind, file: &FileObject) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.insert(
            key,
            FileIdEntry {
                file_id: file.id.clone(),
                kind,
                expires_at: file.expires_at,
            },
        );
        if let Some(path) = &self.path
            && let Err(error) = persist_table(path, &entries)
        {
            tracing::warn!(
                %error,
                path = %path.display(),
                "DeepSeek file_id 表落盘失败；本次仍使用内存映射"
            );
        }
    }
}

fn default_table_path() -> PathBuf {
    storage_root()
        .join("data")
        .join("deepseek")
        .join("file-ids.json")
}

/// 与 `tiangong_config::io::storage_root` 保持一致（本 crate 不依赖 config）。
fn storage_root() -> PathBuf {
    if let Some(root) = std::env::var_os("TIANGONG_STORAGE_ROOT").filter(|value| !value.is_empty())
    {
        return PathBuf::from(root);
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(".tiangong")
}

fn load_table(path: &Path) -> HashMap<String, FileIdEntry> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(error) => {
            tracing::warn!(
                %error,
                path = %path.display(),
                "读取 DeepSeek file_id 表失败；从空表启动"
            );
            return HashMap::new();
        }
    };
    let mut table: PersistedFileIdTable = match serde_json::from_str(&raw) {
        Ok(table) => table,
        Err(error) => {
            tracing::warn!(
                %error,
                path = %path.display(),
                "解析 DeepSeek file_id 表失败；从空表启动"
            );
            return HashMap::new();
        }
    };
    let before = table.entries.len();
    table.entries.retain(|_, entry| entry.is_valid());
    let removed = before.saturating_sub(table.entries.len());
    if removed > 0 {
        tracing::debug!(removed, path = %path.display(), "已清理过期的 DeepSeek file_id 表项");
    }
    table.entries
}

fn persist_table(path: &Path, entries: &HashMap<String, FileIdEntry>) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "DeepSeek file_id 表路径缺少父目录",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let body = serde_json::to_vec_pretty(&PersistedFileIdTable {
        entries: entries.clone(),
    })
    .map_err(std::io::Error::other)?;
    let tmp = path.with_extension(format!("json.tmp-{}", scru128::new()));
    if let Err(error) = write_table_file(&tmp, &body) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn write_table_file(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(body)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_table_file(path: &Path, body: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, body)
}

#[derive(Debug, Clone)]
struct UploadOptions {
    expires_after_seconds: Option<u32>,
    document_mode: FileParsePurpose,
}

impl UploadOptions {
    fn from_request(request: &ProviderRequest) -> Result<Self, LlmError> {
        let expires_after_seconds = request
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("deepseek_file_expires_after_seconds"))
            .map(|value| {
                let seconds = value
                    .as_u64()
                    .and_then(|seconds| u32::try_from(seconds).ok());
                match seconds {
                    Some(seconds)
                        if (MIN_EXPIRES_AFTER_SECONDS..=MAX_EXPIRES_AFTER_SECONDS)
                            .contains(&seconds) =>
                    {
                        Ok(Some(seconds))
                    }
                    _ => Err(LlmError::InvalidRequest(format!(
                        "deepseek_file_expires_after_seconds 必须在 {}..={} 之间",
                        MIN_EXPIRES_AFTER_SECONDS, MAX_EXPIRES_AFTER_SECONDS
                    ))),
                }
            })
            .transpose()?
            .flatten();
        let document_mode = match request
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("deepseek_document_mode"))
            .and_then(Value::as_str)
        {
            Some("rich_layout_recognition") => FileParsePurpose::RichLayoutRecognition,
            Some("text_recognition") | None => FileParsePurpose::TextRecognition,
            Some(other) => {
                tracing::warn!(
                    mode = other,
                    "未知的 deepseek_document_mode，已回退为 text_recognition"
                );
                FileParsePurpose::TextRecognition
            }
        };
        Ok(Self {
            expires_after_seconds,
            document_mode,
        })
    }
}

/// DeepSeek Provider 内部读取可选上传参数；上传本身始终开启。
pub(super) fn file_upload_defaults() -> Result<Map<String, Value>, LlmError> {
    let mut options = Map::new();
    if let Ok(raw) = std::env::var("DEEPSEEK_FILE_EXPIRES_AFTER_SECONDS") {
        let seconds = raw.trim().parse::<u32>().map_err(|error| {
            LlmError::InvalidRequest(format!(
                "DEEPSEEK_FILE_EXPIRES_AFTER_SECONDS 必须是整数：{error}"
            ))
        })?;
        if !(MIN_EXPIRES_AFTER_SECONDS..=MAX_EXPIRES_AFTER_SECONDS).contains(&seconds) {
            return Err(LlmError::InvalidRequest(format!(
                "DEEPSEEK_FILE_EXPIRES_AFTER_SECONDS 必须在 {}..={} 之间",
                MIN_EXPIRES_AFTER_SECONDS, MAX_EXPIRES_AFTER_SECONDS
            )));
        }
        options.insert(
            "deepseek_file_expires_after_seconds".to_string(),
            Value::from(seconds),
        );
    }
    if let Ok(mode) = std::env::var("DEEPSEEK_DOCUMENT_MODE") {
        options.insert(
            "deepseek_document_mode".to_string(),
            Value::String(mode.trim().to_string()),
        );
    }
    Ok(options)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UploadKind {
    UserData,
    Document(FileParsePurpose),
}

pub(super) async fn prepare_request(
    mut request: ProviderRequest,
    client: &DeepSeekClient,
    direct_client: &DeepSeekClient,
    file_ids: &DeepSeekFileIdTable,
) -> Result<ProviderRequest, LlmError> {
    let options = UploadOptions::from_request(&request)?;

    for message in &mut request.messages {
        if message.role != MessageRole::User {
            continue;
        }
        for content in &mut message.content {
            match content {
                MessageContent::Image(image) if !image.data.starts_with(IMAGE_FILE_ID_PREFIX) => {
                    let Some(bytes) = decode_image(&image.data, &image.mime_type)? else {
                        // 远程 URL 继续走 input_image.image_url。
                        continue;
                    };
                    validate_image(&bytes, &image.mime_type)?;
                    let filename = sanitize_filename(None, &image.mime_type, "image");
                    let file = upload_cached(
                        &bytes,
                        &filename,
                        UploadKind::UserData,
                        options.expires_after_seconds,
                        client,
                        direct_client,
                        file_ids,
                    )
                    .await?;
                    image.data = format!("{IMAGE_FILE_ID_PREFIX}{}", file.id);
                }
                MessageContent::File(file) if !file.data.starts_with(FILE_ID_PREFIX) => {
                    let bytes = read_file_bytes(file)?;
                    let filename =
                        sanitize_filename(file.title.as_deref(), &file.mime_type, "attachment");
                    let kind = if is_user_data_mime(&file.mime_type) {
                        if bytes.len() > MAX_USER_DATA_BYTES {
                            return Err(LlmError::InvalidRequest(format!(
                                "DeepSeek user_data 文件超过 32 MiB 上限：{}",
                                filename
                            )));
                        }
                        if is_text_mime(&file.mime_type) && bytes.len() > MAX_INLINE_TEXT_BYTES {
                            return Err(LlmError::InvalidRequest(format!(
                                "DeepSeek 文本文件超过 8 MiB 上限：{}",
                                filename
                            )));
                        }
                        UploadKind::UserData
                    } else {
                        if bytes.len() > MAX_DOCUMENT_FILE_BYTES {
                            return Err(LlmError::InvalidRequest(format!(
                                "DeepSeek 文档超过 100 MiB 上限：{}",
                                filename
                            )));
                        }
                        UploadKind::Document(options.document_mode)
                    };
                    let uploaded = upload_cached(
                        &bytes,
                        &filename,
                        kind,
                        options.expires_after_seconds,
                        client,
                        direct_client,
                        file_ids,
                    )
                    .await?;
                    file.data = format!("{FILE_ID_PREFIX}{}", uploaded.id);
                }
                _ => {}
            }
        }
    }
    Ok(request)
}

#[allow(clippy::too_many_arguments)]
async fn upload_cached(
    bytes: &[u8],
    filename: &str,
    kind: UploadKind,
    expires_after_seconds: Option<u32>,
    client: &DeepSeekClient,
    direct_client: &DeepSeekClient,
    file_ids: &DeepSeekFileIdTable,
) -> Result<FileObject, LlmError> {
    let key = base64_content_hash(bytes);
    if let Some(file_id) = file_ids.get(&key, kind) {
        return Ok(FileObject {
            id: file_id,
            ..Default::default()
        });
    }

    let data = bytes.to_vec();
    let filename = filename.to_string();
    let uploaded = match kind {
        UploadKind::UserData => {
            let expires = expires_after_seconds;
            upload_with_direct_retry(client, direct_client, move |client| {
                let filename = filename.clone();
                let data = data.clone();
                async move { client.files().upload(&filename, data, expires).await }
            })
            .await?
        }
        UploadKind::Document(mode) => {
            upload_with_direct_retry(client, direct_client, move |client| {
                let filename = filename.clone();
                let data = data.clone();
                async move { client.files().upload_document(&filename, data, mode).await }
            })
            .await?
        }
    };
    if uploaded.id.trim().is_empty() {
        return Err(LlmError::InvalidRequest(
            "DeepSeek Files API 返回空 file_id".to_string(),
        ));
    }
    file_ids.insert(key, kind, &uploaded);
    Ok(uploaded)
}

async fn upload_with_direct_retry<F, Fut>(
    client: &DeepSeekClient,
    direct_client: &DeepSeekClient,
    operation: F,
) -> Result<FileObject, LlmError>
where
    F: Fn(DeepSeekClient) -> Fut,
    Fut: Future<Output = Result<FileObject, tiangong_deepseek::DeepSeekError>>,
{
    match operation(client.clone()).await {
        Err(error) if error.is_connect() => {
            tracing::warn!(%error, "DeepSeek 文件上传连接失败，改用直连重试");
            operation(direct_client.clone())
                .await
                .map_err(map_deepseek_error)
        }
        result => result.map_err(map_deepseek_error),
    }
}

fn decode_image(data: &str, mime_type: &str) -> Result<Option<Vec<u8>>, LlmError> {
    if data.trim_start().starts_with("https://") || data.trim_start().starts_with("http://") {
        return Ok(None);
    }
    decode_inline(data, mime_type).map(Some)
}

fn read_file_bytes(file: &FileContent) -> Result<Vec<u8>, LlmError> {
    if let Some(path) = file.data.strip_prefix(LOCAL_PATH_PREFIX) {
        let metadata = std::fs::metadata(path).map_err(|error| {
            LlmError::InvalidRequest(format!("无法读取 DeepSeek 本地附件：{error}"))
        })?;
        if metadata.len() > MAX_DOCUMENT_FILE_BYTES as u64 {
            return Err(LlmError::InvalidRequest(
                "DeepSeek 本地附件超过 100 MiB 上限".to_string(),
            ));
        }
        return std::fs::read(path).map_err(|error| {
            LlmError::InvalidRequest(format!("无法读取 DeepSeek 本地附件：{error}"))
        });
    }
    decode_file(&file.data)
}

/// 对 base64 内容再做 SHA-256，并编码为 base64；作为 file_id 表的稳定键。
fn base64_content_hash(bytes: &[u8]) -> String {
    let encoded = general_purpose::STANDARD.encode(bytes);
    general_purpose::STANDARD.encode(Sha256::digest(encoded.as_bytes()))
}

fn decode_file(data: &str) -> Result<Vec<u8>, LlmError> {
    if data.starts_with("data:") {
        decode_inline(data, "application/octet-stream")
    } else {
        Ok(data.as_bytes().to_vec())
    }
}

fn decode_inline(data: &str, mime_type: &str) -> Result<Vec<u8>, LlmError> {
    if let Some(payload) = data.strip_prefix("data:") {
        let Some((metadata, payload)) = payload.split_once(',') else {
            return Err(LlmError::InvalidRequest(
                "DeepSeek 内联附件缺少 data URL payload".to_string(),
            ));
        };
        if !metadata.split(';').any(|part| part == "base64") {
            return Err(LlmError::InvalidRequest(
                "DeepSeek Files API 上传仅支持 base64 data URL".to_string(),
            ));
        }
        return general_purpose::STANDARD
            .decode(payload)
            .map_err(|error| LlmError::InvalidRequest(format!("附件 base64 解码失败：{error}")));
    }
    if mime_type.starts_with("image/") {
        return general_purpose::STANDARD
            .decode(data)
            .map_err(|error| LlmError::InvalidRequest(format!("图片 base64 解码失败：{error}")));
    }
    Ok(data.as_bytes().to_vec())
}

fn validate_image(bytes: &[u8], mime_type: &str) -> Result<(), LlmError> {
    let mime_type = mime_type.split(';').next().unwrap_or(mime_type).trim();
    if !matches!(
        mime_type,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    ) {
        return Err(LlmError::InvalidRequest(format!(
            "DeepSeek Files API 仅支持 JPEG/PNG/GIF/WebP 图片，收到 {mime_type}"
        )));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(LlmError::InvalidRequest(
            "DeepSeek 图片超过 64 MiB 上限".to_string(),
        ));
    }
    let valid_magic = match mime_type {
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/gif" => bytes.starts_with(b"GIF8"),
        "image/webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP",
        _ => false,
    };
    if !valid_magic {
        return Err(LlmError::InvalidRequest(format!(
            "图片内容与声明的 {mime_type} 不一致"
        )));
    }
    Ok(())
}

fn is_user_data_mime(mime_type: &str) -> bool {
    let mime_type = mime_type.split(';').next().unwrap_or(mime_type).trim();
    mime_type.starts_with("text/")
        || matches!(
            mime_type,
            "application/json"
                | "application/xml"
                | "application/csv"
                | "application/vnd.ms-excel"
                | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                | "application/epub+zip"
                | "application/x-mobipocket-ebook"
        )
}

fn is_text_mime(mime_type: &str) -> bool {
    let mime_type = mime_type.split(';').next().unwrap_or(mime_type).trim();
    mime_type.starts_with("text/")
        || matches!(
            mime_type,
            "application/json" | "application/xml" | "application/csv"
        )
}

fn sanitize_filename(title: Option<&str>, mime_type: &str, fallback: &str) -> String {
    let raw = title.unwrap_or(fallback).trim();
    let raw = raw.rsplit(['/', '\\']).next().unwrap_or(fallback);
    let mut name = raw
        .chars()
        .filter(|character| !character.is_control() && !matches!(character, '/' | '\\'))
        .collect::<String>();
    if name.trim().is_empty() {
        name = fallback.to_string();
    }
    if std::path::Path::new(&name).extension().is_none()
        && let Some(extension) = extension_for_mime(mime_type)
    {
        name.push('.');
        name.push_str(extension);
    }
    while name.len() > 512 {
        name.pop();
    }
    name
}

fn extension_for_mime(mime_type: &str) -> Option<&'static str> {
    match mime_type.split(';').next().unwrap_or(mime_type).trim() {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "application/pdf" => Some("pdf"),
        "text/plain" | "application/json" | "application/xml" => Some("txt"),
        "text/csv" | "application/csv" => Some("csv"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ChatMessage, FileContent, ImageContent};
    use serde_json::json;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn request(contents: Vec<MessageContent>) -> ProviderRequest {
        ProviderRequest {
            model: "deepseek-flash".to_string(),
            system: None,
            messages: vec![ChatMessage::new(MessageRole::User, contents)],
            session_id: None,
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            stop_sequences: Vec::new(),
            metadata: None,
            reasoning_effort: crate::request::ReasoningEffort::None,
        }
    }

    async fn build_client(server: &MockServer) -> DeepSeekClient {
        DeepSeekClient::builder("test-key")
            .base_url(server.uri())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn uploads_image_and_text_once_then_reuses_file_ids() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(move |request: &wiremock::Request| {
                let body = String::from_utf8_lossy(&request.body);
                if body.contains("image.png") {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "id": "file-image", "object": "file", "bytes": 20,
                        "created_at": 0, "filename": "image.png", "purpose": "user_data"
                    }))
                } else if body.contains("notes.txt") {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "id": "file-text", "object": "file", "bytes": 5,
                        "created_at": 0, "filename": "notes.txt", "purpose": "user_data"
                    }))
                } else {
                    ResponseTemplate::new(400)
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let primary = build_client(&server).await;
        let direct = build_client(&server).await;
        let file_ids = DeepSeekFileIdTable::default();
        let png = b"\x89PNG\r\n\x1a\nimage-bytes";
        let original = request(vec![
            MessageContent::Image(ImageContent {
                mime_type: "image/png".to_string(),
                data: format!(
                    "data:image/png;base64,{}",
                    general_purpose::STANDARD.encode(png)
                ),
            }),
            MessageContent::File(FileContent {
                mime_type: "text/plain".to_string(),
                data: "hello".to_string(),
                title: Some("notes.txt".to_string()),
            }),
        ]);

        let prepared = prepare_request(original.clone(), &primary, &direct, &file_ids)
            .await
            .unwrap();
        let MessageContent::Image(image) = &prepared.messages[0].content[0] else {
            panic!("应为图片");
        };
        assert_eq!(image.data, format!("{IMAGE_FILE_ID_PREFIX}file-image"));
        let MessageContent::File(file) = &prepared.messages[0].content[1] else {
            panic!("应为文本文件");
        };
        assert_eq!(file.data, format!("{FILE_ID_PREFIX}file-text"));

        let reused = prepare_request(original, &primary, &direct, &file_ids)
            .await
            .unwrap();
        assert_eq!(reused.messages[0].content, prepared.messages[0].content);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "相同内容第二次请求必须命中上传缓存");
        assert!(
            requests
                .iter()
                .all(|request| { String::from_utf8_lossy(&request.body).contains("user_data") })
        );
    }

    #[tokio::test]
    async fn document_upload_uses_selected_parser_before_file_part() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "file-pdf", "object": "file", "bytes": 8,
                "created_at": 0, "filename": "report.pdf", "purpose": "rich_layout_recognition"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let primary = build_client(&server).await;
        let direct = build_client(&server).await;
        let file_ids = DeepSeekFileIdTable::default();
        let mut req = request(vec![MessageContent::File(FileContent {
            mime_type: "application/pdf".to_string(),
            data: "data:application/pdf;base64,JVBERi0xLjQ=".to_string(),
            title: Some("report.pdf".to_string()),
        })]);
        req.metadata.get_or_insert_with(Default::default).insert(
            "deepseek_document_mode".to_string(),
            Value::String("rich_layout_recognition".to_string()),
        );

        let prepared = prepare_request(req, &primary, &direct, &file_ids)
            .await
            .unwrap();
        let MessageContent::File(file) = &prepared.messages[0].content[0] else {
            panic!("应为文档");
        };
        assert_eq!(file.data, format!("{FILE_ID_PREFIX}file-pdf"));
        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&requests[0].body);
        let purpose = body.find("rich_layout_recognition").unwrap();
        let file_part = body.find("name=\"file\"").unwrap();
        assert!(
            purpose < file_part,
            "purpose 必须位于 file multipart 字段之前"
        );
    }

    #[tokio::test]
    async fn local_file_is_read_inside_llm_and_uploaded() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "file-local", "object": "file", "bytes": 5,
                "created_at": 0, "filename": "local.txt", "purpose": "user_data"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let path = std::env::temp_dir().join(format!(
            "deepseek-local-{}-{}.txt",
            std::process::id(),
            scru128::new()
        ));
        std::fs::write(&path, b"hello").unwrap();
        let primary = build_client(&server).await;
        let direct = build_client(&server).await;
        let file_ids = DeepSeekFileIdTable::default();
        let req = request(vec![MessageContent::File(FileContent {
            mime_type: "text/plain".to_string(),
            data: format!("local-path:{}", path.to_string_lossy()),
            title: Some("local.txt".to_string()),
        })]);
        let prepared = prepare_request(req, &primary, &direct, &file_ids)
            .await
            .unwrap();
        std::fs::remove_file(path).unwrap();
        let MessageContent::File(file) = &prepared.messages[0].content[0] else {
            panic!("应为本地文件");
        };
        assert_eq!(file.data, format!("{FILE_ID_PREFIX}file-local"));
    }

    #[test]
    fn base64_hash_table_reuses_only_matching_upload_kind() {
        let bytes = b"same-content";
        let key = base64_content_hash(bytes);
        assert_eq!(key, base64_content_hash(bytes));
        assert_ne!(key, base64_content_hash(b"other-content"));
        let table = DeepSeekFileIdTable::default();
        table.insert(
            key.clone(),
            UploadKind::UserData,
            &FileObject {
                id: "file-user-data".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(
            table.get(&key, UploadKind::UserData).as_deref(),
            Some("file-user-data")
        );
        assert_eq!(
            table.get(
                &key,
                UploadKind::Document(FileParsePurpose::TextRecognition)
            ),
            None
        );
    }

    #[test]
    fn persistent_table_survives_reload_and_drops_expired_entries() {
        let dir = std::env::temp_dir().join(format!(
            "deepseek-file-table-{}-{}",
            std::process::id(),
            scru128::new()
        ));
        let path = dir.join("nested").join("file-ids.json");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let table = DeepSeekFileIdTable::with_path(path.clone());
        table.insert(
            "valid".to_string(),
            UploadKind::UserData,
            &FileObject {
                id: "file-valid".to_string(),
                expires_at: Some(now + 3600),
                ..Default::default()
            },
        );
        table.insert(
            "expired".to_string(),
            UploadKind::UserData,
            &FileObject {
                id: "file-expired".to_string(),
                expires_at: Some(now.saturating_sub(1)),
                ..Default::default()
            },
        );
        assert!(path.exists());

        let reloaded = DeepSeekFileIdTable::with_path(path);
        assert_eq!(
            reloaded.get("valid", UploadKind::UserData).as_deref(),
            Some("file-valid")
        );
        assert_eq!(reloaded.get("expired", UploadKind::UserData), None);
        assert_eq!(
            reloaded.get(
                "valid",
                UploadKind::Document(FileParsePurpose::TextRecognition)
            ),
            None
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remote_image_stays_inline_and_invalid_expiry_is_rejected() {
        assert!(
            decode_image("https://example.com/a.png", "image/png")
                .unwrap()
                .is_none()
        );
        let mut req = request(Vec::new());
        req.metadata.get_or_insert_with(Default::default).insert(
            "deepseek_file_expires_after_seconds".to_string(),
            Value::from(60),
        );
        assert!(UploadOptions::from_request(&req).is_err());
    }
}
