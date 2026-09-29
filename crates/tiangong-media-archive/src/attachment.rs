use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Serialize};
use tiangong_types::{ContentBlock, MediaKind, StoredAsset};

pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

/// App 层收到的原始附件。该类型不得进入 Core。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawAttachment {
    pub kind: MediaKind,
    #[serde(alias = "url")]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, alias = "title", skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
}

/// 处理音频附件的工具名：插件提供该工具即视为具备音频处理能力。
pub const SPEECH_TO_TEXT_TOOL: &str = "speech_to_text";

/// 生成本轮附件处理方案时使用的能力快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachmentCapabilitySnapshot {
    pub audio_processor: bool,
    pub video_processor: bool,
}

/// 已保存到 Store 的稳定附件信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAttachment {
    pub asset_id: String,
    pub local_path: String,
    pub original_name: String,
    pub mime_type: String,
    pub size: u64,
    pub kind: MediaKind,
    pub reused: bool,
}

/// App 层共享附件 Store。
#[derive(Debug, Clone)]
pub struct AttachmentStore {
    root: PathBuf,
}

impl Default for AttachmentStore {
    fn default() -> Self {
        Self::with_root(default_media_root())
    }
}

impl AttachmentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self::new(root)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 原子保存一批附件。任意一个失败都会删除本批次此前创建的文件。
    pub fn store_batch(
        &self,
        attachments: Vec<RawAttachment>,
    ) -> Result<AttachmentTransaction, String> {
        let mut stored = Vec::with_capacity(attachments.len());
        let mut created_paths = Vec::new();

        for attachment in attachments {
            match self.store_one(attachment) {
                Ok(item) => {
                    if !item.reused {
                        created_paths.push(PathBuf::from(&item.local_path));
                    }
                    stored.push(item);
                }
                Err(error) => {
                    if let Err(cleanup_error) = cleanup_paths(&created_paths) {
                        return Err(format!("{error}；回滚本批次附件失败：{cleanup_error}"));
                    }
                    return Err(error);
                }
            }
        }

        Ok(AttachmentTransaction {
            stored,
            assets: Vec::new(),
            created_paths,
            finished: false,
        })
    }

    pub(crate) fn is_existing_in_store(&self, value: &str) -> bool {
        self.reusable_path(value).is_some()
    }

    fn store_one(&self, attachment: RawAttachment) -> Result<StoredAttachment, String> {
        let source = attachment.source.trim();
        if source.is_empty() {
            return Err("附件来源为空".to_string());
        }

        if let Some(path) = self.reusable_path(source) {
            return self.describe_reused(path, attachment);
        }

        let loaded = load_source(source)?;
        ensure_size(loaded.bytes.len() as u64, "附件")?;

        let mime_type = resolve_mime(
            attachment.kind,
            attachment.mime_type.as_deref(),
            loaded.mime_type.as_deref(),
            source,
        );
        let extension = extension_for_mime(&mime_type);
        let original_name = attachment
            .original_name
            .as_deref()
            .and_then(clean_original_name)
            .or_else(|| {
                loaded
                    .original_name
                    .as_deref()
                    .and_then(clean_original_name)
            })
            .unwrap_or_else(|| format!("attachment.{extension}"));
        let asset_id = scru128::new().to_string();
        let directory = self.root.join(subdir_for_kind(attachment.kind));
        fs::create_dir_all(&directory).map_err(|error| format!("创建附件归档目录失败：{error}"))?;
        let path = directory.join(stored_filename(&asset_id, &original_name, extension));

        if let Err(error) = fs::write(&path, &loaded.bytes) {
            let _ = fs::remove_file(&path);
            return Err(format!("写入附件归档失败：{error}"));
        }

        Ok(StoredAttachment {
            asset_id,
            local_path: local_path_text(&path),
            original_name,
            mime_type,
            size: loaded.bytes.len() as u64,
            kind: attachment.kind,
            reused: false,
        })
    }

    fn reusable_path(&self, value: &str) -> Option<PathBuf> {
        if value.starts_with("data:")
            || value.starts_with("http://")
            || value.starts_with("https://")
        {
            return None;
        }

        let path = local_path_from_source(value);
        let target = fs::canonicalize(path).ok()?;
        if !target.is_file() {
            return None;
        }
        let root = fs::canonicalize(&self.root).ok()?;
        target.starts_with(root).then_some(target)
    }

    fn describe_reused(
        &self,
        path: PathBuf,
        attachment: RawAttachment,
    ) -> Result<StoredAttachment, String> {
        let metadata =
            fs::metadata(&path).map_err(|error| format!("读取已归档附件信息失败：{error}"))?;
        ensure_size(metadata.len(), "附件")?;
        let path_text = local_path_text(&path);
        let mime_type = resolve_mime(
            attachment.kind,
            attachment.mime_type.as_deref(),
            None,
            &path_text,
        );
        let extension = extension_for_mime(&mime_type);
        let original_name = attachment
            .original_name
            .as_deref()
            .and_then(clean_original_name)
            .or_else(|| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(clean_original_name)
            })
            .unwrap_or_else(|| format!("attachment.{extension}"));

        Ok(StoredAttachment {
            asset_id: asset_id_from_path(&path).unwrap_or_else(|| scru128::new().to_string()),
            local_path: path_text,
            original_name,
            mime_type,
            size: metadata.len(),
            kind: attachment.kind,
            reused: true,
        })
    }
}

/// 返回归档媒体文件的规范路径，用于跨 Windows 路径表示比较文件身份。
pub fn canonical_archived_media_path(value: &str) -> Option<PathBuf> {
    AttachmentStore::default().reusable_path(value)
}

/// 一批已保存附件的事务。未提交即离开作用域时会自动回滚新文件。
#[derive(Debug)]
pub struct AttachmentTransaction {
    stored: Vec<StoredAttachment>,
    assets: Vec<StoredAsset>,
    created_paths: Vec<PathBuf>,
    finished: bool,
}

impl AttachmentTransaction {
    pub fn stored(&self) -> &[StoredAttachment] {
        &self.stored
    }

    pub fn assets(&self) -> &[StoredAsset] {
        &self.assets
    }

    pub fn newly_created_paths(&self) -> &[PathBuf] {
        &self.created_paths
    }

    /// 根据发送开始时捕获的能力快照生成持久内容与本轮运行内容。
    pub fn prepare_message(
        &mut self,
        message_id: &str,
        text: impl Into<String>,
        capabilities: AttachmentCapabilitySnapshot,
    ) -> Result<Vec<ContentBlock>, String> {
        let result = self.build_prepared_message(message_id, text.into(), capabilities);
        match result {
            Ok((message, assets)) => {
                self.assets = assets;
                Ok(message)
            }
            Err(error) => {
                let cleanup = self.rollback_in_place();
                match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => {
                        Err(format!("{error}；回滚本批次附件失败：{cleanup_error}"))
                    }
                }
            }
        }
    }

    pub fn commit(mut self) {
        self.finished = true;
    }

    pub fn rollback(mut self) -> Result<(), String> {
        self.rollback_in_place()
    }

    fn build_prepared_message(
        &self,
        message_id: &str,
        text: String,
        capabilities: AttachmentCapabilitySnapshot,
    ) -> Result<(Vec<ContentBlock>, Vec<StoredAsset>), String> {
        if message_id.trim().is_empty() {
            return Err("准备用户消息时 message_id 不能为空".to_string());
        }
        let mut content = Vec::with_capacity(1 + self.stored.len() * 2);
        content.push(ContentBlock::Text { text });
        let mut assets = Vec::with_capacity(self.stored.len());

        for (index, stored) in self.stored.iter().enumerate() {
            let asset = StoredAsset {
                asset_id: stored.asset_id.clone(),
                local_path: stored.local_path.clone(),
                original_name: stored.original_name.clone(),
                mime_type: stored.mime_type.clone(),
                size: stored.size,
                kind: stored.kind,
            };

            match stored.kind {
                // SVG 矢量图不 inline 发送：主流模型 API 仅接受 png/jpeg/webp/gif，
                // 直发 base64 会被拒收导致整轮消息失败，统一走文件引用路径
                MediaKind::Image if stored.mime_type == "image/svg+xml" => {
                    content.push(ContentBlock::AssetReference {
                        asset: asset.clone(),
                    });
                    content.push(ContentBlock::ModelInstruction {
                        text: file_attachment_instruction(index, &asset),
                    });
                }
                // 图片归档后以本地路径引用注入；以原生图片（base64）还是路径
                // 发送给模型，由 CoreManager 投递时按会话模型能力决定。
                MediaKind::Image => {
                    content.push(ContentBlock::AssetReference {
                        asset: asset.clone(),
                    });
                    content.push(ContentBlock::ModelInstruction {
                        text: inline_image_instruction(index, &asset),
                    });
                }
                MediaKind::Audio => {
                    content.push(ContentBlock::AssetReference {
                        asset: asset.clone(),
                    });
                    content.push(ContentBlock::ModelInstruction {
                        text: audio_attachment_instruction(
                            index,
                            &asset,
                            capabilities.audio_processor,
                        ),
                    });
                }
                MediaKind::Video => {
                    content.push(ContentBlock::AssetReference {
                        asset: asset.clone(),
                    });
                    content.push(ContentBlock::ModelInstruction {
                        text: video_attachment_instruction(
                            index,
                            &asset,
                            capabilities.video_processor,
                        ),
                    });
                }
                MediaKind::File => {
                    content.push(ContentBlock::AssetReference {
                        asset: asset.clone(),
                    });
                    content.push(ContentBlock::ModelInstruction {
                        text: file_attachment_instruction(index, &asset),
                    });
                }
            }

            assets.push(asset);
        }

        Ok((content, assets))
    }

    fn rollback_in_place(&mut self) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        cleanup_paths(&self.created_paths)?;
        self.finished = true;
        Ok(())
    }
}

fn asset_notice_item(index: usize, asset: &StoredAsset) -> String {
    format!(
        "index={index} asset_id={} kind={:?} name={} mime_type={} size={} path={}",
        asset.asset_id,
        asset.kind,
        asset.original_name,
        asset.mime_type,
        asset.size,
        asset.local_path,
    )
}

fn inline_image_instruction(index: usize, asset: &StoredAsset) -> String {
    format!(
        "本条消息中的图片已归档，本地工具需要读取时直接使用：index={index} path={}",
        asset.local_path
    )
}

fn file_attachment_instruction(index: usize, asset: &StoredAsset) -> String {
    format!(
        "本条用户消息包含文件引用，文件内容不会直接发送给模型。请使用文件工具、文档解析器或 Skill 按下列本地 path 处理。\n- {}",
        asset_notice_item(index, asset)
    )
}

fn audio_attachment_instruction(
    index: usize,
    asset: &StoredAsset,
    capability_available: bool,
) -> String {
    if capability_available {
        format!(
            "本条用户消息包含音频引用。需要获取音频内容时，请调用 speech_to_text 工具，并将下列本地 path 作为 file_path。\n- {}",
            asset_notice_item(index, asset)
        )
    } else {
        format!(
            "音频附件所需能力 speech_to_text 当前不可用；请保留附件引用并明确告知用户。\n- {}",
            asset_notice_item(index, asset)
        )
    }
}

fn video_attachment_instruction(
    index: usize,
    asset: &StoredAsset,
    capability_available: bool,
) -> String {
    if capability_available {
        format!(
            "本条用户消息包含视频引用。请使用已注册的视频处理能力按下列本地 path 处理。\n- {}",
            asset_notice_item(index, asset)
        )
    } else {
        format!(
            "视频附件所需内容分析能力当前不可用；请保留附件引用并明确告知用户。\n- {}",
            asset_notice_item(index, asset)
        )
    }
}

impl Drop for AttachmentTransaction {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Err(error) = cleanup_paths(&self.created_paths) {
            tracing::warn!(error = %error, "自动回滚附件事务失败");
        } else {
            self.finished = true;
        }
    }
}

struct LoadedSource {
    bytes: Vec<u8>,
    mime_type: Option<String>,
    original_name: Option<String>,
}

fn load_source(source: &str) -> Result<LoadedSource, String> {
    if source.starts_with("data:") {
        return load_data_url(source);
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        return load_remote(source);
    }
    load_local(source)
}

fn load_data_url(source: &str) -> Result<LoadedSource, String> {
    let (header, payload) = source
        .split_once(',')
        .ok_or_else(|| "data URL 缺少内容".to_string())?;
    let metadata = header
        .strip_prefix("data:")
        .ok_or_else(|| "无效的 data URL".to_string())?;
    if !metadata
        .split(';')
        .skip(1)
        .any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return Err("data URL 仅支持 base64 编码".to_string());
    }

    let compact = payload.trim_end();
    let encoded_len = compact
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .count() as u64;
    let padding = compact
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count() as u64;
    let estimated_size = (encoded_len / 4 * 3).saturating_sub(padding.min(2));
    ensure_size(estimated_size, "附件")?;
    let compact_payload: String = payload
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    let bytes = general_purpose::STANDARD
        .decode(compact_payload)
        .map_err(|error| format!("解码 data URL 失败：{error}"))?;
    ensure_size(bytes.len() as u64, "附件")?;

    Ok(LoadedSource {
        bytes,
        mime_type: metadata.split(';').next().and_then(normalize_mime),
        original_name: None,
    })
}

fn load_remote(source: &str) -> Result<LoadedSource, String> {
    let url = source.to_string();
    std::thread::scope(|scope| {
        scope
            .spawn(move || load_remote_inner(&url))
            .join()
            .map_err(|_| "附件下载线程异常退出".to_string())?
    })
}

fn load_remote_inner(source: &str) -> Result<LoadedSource, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|error| format!("创建附件下载客户端失败：{error}"))?;
    let response = client
        .get(source)
        .send()
        .map_err(|error| format!("下载附件失败：{error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("下载附件失败：HTTP {status}"));
    }
    if let Some(size) = response.content_length() {
        ensure_size(size, "附件")?;
    }
    let mime_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(normalize_mime);
    let bytes = read_all_with_limit(response, "附件")?;

    Ok(LoadedSource {
        bytes,
        mime_type,
        original_name: inferred_name_from_source(source),
    })
}

fn load_local(source: &str) -> Result<LoadedSource, String> {
    let path = local_path_from_source(source);
    let metadata = fs::metadata(&path).map_err(|error| format!("读取本地附件失败：{error}"))?;
    if !metadata.is_file() {
        return Err(format!("本地附件不是普通文件：{}", path.display()));
    }
    ensure_size(metadata.len(), "附件")?;
    let bytes = fs::read(&path).map_err(|error| format!("读取本地附件失败：{error}"))?;

    Ok(LoadedSource {
        bytes,
        mime_type: mime_from_reference(&path.to_string_lossy()),
        original_name: path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string),
    })
}

fn read_all_with_limit(reader: impl Read, label: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_ATTACHMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取{label}失败：{error}"))?;
    ensure_size(bytes.len() as u64, label)?;
    Ok(bytes)
}

fn ensure_size(size: u64, label: &str) -> Result<(), String> {
    if size > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "{label}超过 {}MB 大小限制",
            MAX_ATTACHMENT_BYTES / 1024 / 1024
        ));
    }
    Ok(())
}

fn resolve_mime(
    kind: MediaKind,
    hint: Option<&str>,
    detected: Option<&str>,
    reference: &str,
) -> String {
    hint.and_then(normalize_mime)
        .filter(|mime| mime_matches_kind(kind, mime))
        .or_else(|| {
            detected
                .and_then(normalize_mime)
                .filter(|mime| mime_matches_kind(kind, mime))
        })
        .or_else(|| mime_from_reference(reference).filter(|mime| mime_matches_kind(kind, mime)))
        .unwrap_or_else(|| default_mime(kind).to_string())
}

fn normalize_mime(value: &str) -> Option<String> {
    let mime = value.split(';').next()?.trim().to_ascii_lowercase();
    (!mime.is_empty() && mime.contains('/')).then_some(mime)
}

fn mime_matches_kind(kind: MediaKind, mime: &str) -> bool {
    match kind {
        MediaKind::Image => mime.starts_with("image/"),
        MediaKind::Audio => mime.starts_with("audio/"),
        MediaKind::Video => mime.starts_with("video/"),
        MediaKind::File => true,
    }
}

fn default_mime(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image/png",
        MediaKind::Audio => "audio/mpeg",
        MediaKind::Video => "video/mp4",
        MediaKind::File => "application/octet-stream",
    }
}

fn mime_from_reference(value: &str) -> Option<String> {
    let path = value.split(['?', '#']).next().unwrap_or(value);
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())?
        .to_ascii_lowercase();
    let mime = match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "xml" => "application/xml",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        _ => return None,
    };
    Some(mime.to_string())
}

/// 常见媒体类型的归档扩展名（未知类型统一 `bin`）。
///
/// 宿主附件归档与工具注入接管（core）共用的权威映射，避免两侧各持一份漂移。
pub fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "application/pdf" => "pdf",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "text/html" => "html",
        "application/json" => "json",
        "application/xml" => "xml",
        "audio/mpeg" => "mp3",
        "audio/wav" => "wav",
        "audio/ogg" => "ogg",
        "audio/mp4" => "m4a",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        _ => "bin",
    }
}

fn subdir_for_kind(kind: MediaKind) -> &'static str {
    if kind == MediaKind::Image {
        "images"
    } else {
        "files"
    }
}

fn stored_filename(asset_id: &str, original_name: &str, extension: &str) -> String {
    let normalized = original_name.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or(original_name);
    let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name);
    let safe_stem: String = stem
        .trim()
        .chars()
        .take(64)
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_' | '.' | ' ') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if safe_stem.is_empty() {
        format!("{asset_id}.{extension}")
    } else {
        format!("{asset_id}_{safe_stem}.{extension}")
    }
}

fn clean_original_name(value: &str) -> Option<String> {
    let normalized = value.replace('\\', "/");
    normalized
        .rsplit('/')
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .map(str::to_string)
}

fn inferred_name_from_source(value: &str) -> Option<String> {
    let path = value.split(['?', '#']).next().unwrap_or(value);
    clean_original_name(path)
}

fn asset_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let candidate = stem.split('_').next().unwrap_or(stem);
    candidate
        .parse::<scru128::Id>()
        .ok()
        .map(|id| id.to_string())
}

fn local_path_from_source(value: &str) -> PathBuf {
    let mut normalized = value.trim().to_string();
    if let Some(path) = normalized.strip_prefix("file://") {
        normalized = path.to_string();
        #[cfg(windows)]
        if normalized.starts_with('/') && normalized.as_bytes().get(2).copied() == Some(b':') {
            normalized.remove(0);
        }
    }
    #[cfg(windows)]
    let normalized = normalized.replace('\\', "/");
    PathBuf::from(normalized)
}

fn local_path_text(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    {
        let text = if let Some(path) = text.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{path}")
        } else if let Some(path) = text.strip_prefix(r"\\?\") {
            path.to_string()
        } else {
            text.into_owned()
        };
        text.replace('\\', "/")
    }
    #[cfg(not(windows))]
    text.into_owned()
}

fn cleanup_paths(paths: &[PathBuf]) -> Result<(), String> {
    let mut failures = Vec::new();
    for path in paths.iter().rev() {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("{}：{error}", path.display())),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("；"))
    }
}

fn default_media_root() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tiangong")
        .join("media")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("tiangong-media-archive-{}", scru128::new()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn store(&self) -> AttachmentStore {
            AttachmentStore::with_root(&self.0)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn data_attachment(kind: MediaKind, name: &str, mime: &str, data: &[u8]) -> RawAttachment {
        RawAttachment {
            kind,
            source: format!(
                "data:{mime};base64,{}",
                general_purpose::STANDARD.encode(data)
            ),
            mime_type: Some(mime.to_string()),
            original_name: Some(name.to_string()),
        }
    }

    fn file_count(path: &Path) -> usize {
        fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| {
                if entry.path().is_dir() {
                    file_count(&entry.path())
                } else {
                    1
                }
            })
            .sum()
    }

    #[test]
    fn batch_preserves_attachment_order() {
        let root = TestRoot::new();
        let transaction = root
            .store()
            .store_batch(vec![
                data_attachment(MediaKind::File, "first.txt", "text/plain", b"1"),
                data_attachment(MediaKind::Image, "second.png", "image/png", b"2"),
                data_attachment(MediaKind::Audio, "third.mp3", "audio/mpeg", b"3"),
            ])
            .unwrap();
        let names: Vec<_> = transaction
            .stored()
            .iter()
            .map(|item| item.original_name.as_str())
            .collect();
        assert_eq!(names, ["first.txt", "second.png", "third.mp3"]);
    }

    #[test]
    fn batch_failure_removes_previously_created_files() {
        let root = TestRoot::new();
        let missing = root.0.join("missing.bin");
        let result = root.store().store_batch(vec![
            data_attachment(MediaKind::File, "valid.txt", "text/plain", b"valid"),
            RawAttachment {
                kind: MediaKind::File,
                source: missing.to_string_lossy().to_string(),
                mime_type: None,
                original_name: None,
            },
        ]);
        assert!(result.is_err());
        assert_eq!(file_count(&root.0), 0);
    }

    #[test]
    fn rollback_never_deletes_reused_file() {
        let root = TestRoot::new();
        let first = root
            .store()
            .store_batch(vec![data_attachment(
                MediaKind::File,
                "keep.txt",
                "text/plain",
                b"keep",
            )])
            .unwrap();
        let path = first.stored()[0].local_path.clone();
        first.commit();

        let reused = root
            .store()
            .store_batch(vec![RawAttachment {
                kind: MediaKind::File,
                source: path.clone(),
                mime_type: Some("text/plain".to_string()),
                original_name: Some("keep.txt".to_string()),
            }])
            .unwrap();
        assert!(reused.stored()[0].reused);
        reused.rollback().unwrap();
        assert!(Path::new(&path).is_file());
    }

    #[test]
    fn dropping_uncommitted_transaction_removes_new_files() {
        let root = TestRoot::new();
        let path = {
            let transaction = root
                .store()
                .store_batch(vec![data_attachment(
                    MediaKind::File,
                    "temporary.txt",
                    "text/plain",
                    b"temporary",
                )])
                .unwrap();
            let path = transaction.stored()[0].local_path.clone();
            assert!(Path::new(&path).exists());
            path
        };
        assert!(!Path::new(&path).exists());
    }

    #[test]
    fn image_is_archived_and_injected_as_path_reference() {
        let root = TestRoot::new();
        let mut transaction = root
            .store()
            .store_batch(vec![data_attachment(
                MediaKind::Image,
                "photo.png",
                "image/png",
                b"png bytes",
            )])
            .unwrap();
        let message = transaction
            .prepare_message(
                "message-inline",
                "look",
                AttachmentCapabilitySnapshot::default(),
            )
            .unwrap();

        assert_eq!(transaction.assets().len(), 1);
        assert!(!transaction.assets()[0].local_path.starts_with("data:"));
        assert_eq!(message.len(), 3);
        match &message[1] {
            ContentBlock::AssetReference { asset } => {
                assert_eq!(asset, &transaction.assets()[0]);
                assert_eq!(fs::read(&asset.local_path).unwrap(), b"png bytes");
            }
            other => panic!("图片应以路径引用注入，实际：{other:?}"),
        }
        let expected_instruction = format!(
            "本条消息中的图片已归档，本地工具需要读取时直接使用：index=0 path={}",
            transaction.assets()[0].local_path
        );
        assert!(matches!(
            &message[2],
            ContentBlock::ModelInstruction { text } if text == &expected_instruction
        ));
    }

    #[test]
    fn svg_image_uses_file_reference() {
        let root = TestRoot::new();
        let mut transaction = root
            .store()
            .store_batch(vec![data_attachment(
                MediaKind::Image,
                "diagram.svg",
                "image/svg+xml",
                b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
            )])
            .unwrap();
        let message = transaction
            .prepare_message(
                "message-svg",
                "look",
                AttachmentCapabilitySnapshot::default(),
            )
            .unwrap();

        assert_eq!(transaction.assets().len(), 1);
        assert_eq!(transaction.assets()[0].mime_type, "image/svg+xml");
        assert_eq!(message.len(), 3);
        match &message[1] {
            ContentBlock::AssetReference { asset } => {
                assert_eq!(asset, &transaction.assets()[0]);
            }
            other => panic!("SVG 应走文件引用，实际：{other:?}"),
        }
        assert!(matches!(
            &message[2],
            ContentBlock::ModelInstruction { text } if text.contains("path=")
        ));
        assert!(
            !message
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. }))
        );
    }

    #[test]
    fn inline_images_expose_all_archived_paths_in_upload_order() {
        let root = TestRoot::new();
        let mut transaction = root
            .store()
            .store_batch(vec![
                data_attachment(MediaKind::Image, "person.png", "image/png", b"person"),
                data_attachment(MediaKind::Image, "dress.png", "image/png", b"dress"),
            ])
            .unwrap();
        let message = transaction
            .prepare_message(
                "message-two-images",
                "try on",
                AttachmentCapabilitySnapshot::default(),
            )
            .unwrap();

        assert_eq!(message.len(), 5);
        assert!(matches!(
            &message[1],
            ContentBlock::AssetReference { asset } if asset.original_name == "person.png"
        ));
        assert!(matches!(
            &message[2],
            ContentBlock::ModelInstruction { text }
                if text == &format!(
                    "本条消息中的图片已归档，本地工具需要读取时直接使用：index=0 path={}",
                    transaction.assets()[0].local_path
                )
        ));
        assert!(matches!(
            &message[3],
            ContentBlock::AssetReference { asset } if asset.original_name == "dress.png"
        ));
        assert!(matches!(
            &message[4],
            ContentBlock::ModelInstruction { text }
                if text == &format!(
                    "本条消息中的图片已归档，本地工具需要读取时直接使用：index=1 path={}",
                    transaction.assets()[1].local_path
                )
        ));
    }

    #[test]
    fn reference_instructions_preserve_asset_order_and_capability_state() {
        let root = TestRoot::new();
        let mut other = root
            .store()
            .store_batch(vec![
                data_attachment(MediaKind::File, "doc.txt", "text/plain", b"file"),
                data_attachment(MediaKind::Audio, "voice.mp3", "audio/mpeg", b"audio"),
                data_attachment(MediaKind::Video, "clip.mp4", "video/mp4", b"video"),
            ])
            .unwrap();
        let message = other
            .prepare_message(
                "message-references",
                "other",
                AttachmentCapabilitySnapshot {
                    audio_processor: true,
                    video_processor: false,
                },
            )
            .unwrap();

        assert_eq!(other.assets().len(), 3);
        for (index, expected_name) in ["doc.txt", "voice.mp3", "clip.mp4"].iter().enumerate() {
            let block_index = 1 + index * 2;
            assert!(matches!(
                &message[block_index],
                ContentBlock::AssetReference { asset }
                    if asset.original_name == *expected_name
            ));
        }
        assert!(matches!(
            &message[2],
            ContentBlock::ModelInstruction { text }
                if text.contains("index=0") && text.contains("文件引用")
        ));
        assert!(matches!(
            &message[4],
            ContentBlock::ModelInstruction { text }
                if text.contains("index=1") && text.contains("speech_to_text")
        ));
        assert!(matches!(
            &message[6],
            ContentBlock::ModelInstruction { text }
                if text.contains("index=2") && text.contains("当前不可用")
        ));
    }

    #[test]
    fn all_sources_enforce_fifty_megabyte_limit() {
        let root = TestRoot::new();
        let oversized = root.0.join("oversized.bin");
        let file = fs::File::create(&oversized).unwrap();
        file.set_len(MAX_ATTACHMENT_BYTES + 1).unwrap();
        drop(file);

        let result = root.store().store_batch(vec![RawAttachment {
            kind: MediaKind::File,
            source: oversized.to_string_lossy().to_string(),
            mime_type: None,
            original_name: None,
        }]);
        assert!(result.unwrap_err().contains("50MB"));
    }

    #[cfg(windows)]
    #[test]
    fn reuse_path_accepts_both_separator_styles() {
        let root = TestRoot::new();
        let first = root
            .store()
            .store_batch(vec![data_attachment(
                MediaKind::File,
                "portable.txt",
                "text/plain",
                b"portable",
            )])
            .unwrap();
        let path = first.stored()[0].local_path.clone();
        first.commit();
        let portable_path = path.replace('/', "\\");

        let reused = root
            .store()
            .store_batch(vec![RawAttachment {
                kind: MediaKind::File,
                source: portable_path,
                mime_type: Some("text/plain".to_string()),
                original_name: None,
            }])
            .unwrap();
        assert!(reused.stored()[0].reused);
        assert!(!reused.stored()[0].local_path.starts_with(r"\\?\"));
        let normal = path.replace('/', "\\");
        let extended = format!(r"\\?\{normal}");
        assert_eq!(
            root.store().reusable_path(&normal),
            root.store().reusable_path(&extended)
        );
        reused.rollback().unwrap();
        assert!(Path::new(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn reuse_path_preserves_literal_backslash_in_filename() {
        let root = TestRoot::new();
        let path = root.0.join(r"literal\backslash.txt");
        fs::write(&path, b"portable").unwrap();

        assert_eq!(
            root.store().reusable_path(path.to_string_lossy().as_ref()),
            fs::canonicalize(&path).ok()
        );
    }
}
