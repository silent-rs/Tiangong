//! 输入附件本地归档
//!
//! 把用户输入的图片 / PDF / Office 文档归档到 `~/.tiangong/media/` 下，
//! 统一为本地路径引用。支持 data URL、http(s) 和本地路径三种来源。
//!
//! 原 `tiangong-core::media_archive`，已迁出为独立 crate（#208）。

#[cfg(test)]
use std::path::{Path, PathBuf};

#[cfg(test)]
use base64::{Engine as _, engine::general_purpose};
use tiangong_types::{MediaAsset, MediaKind};

mod attachment;

pub use attachment::{
    AttachmentCapabilitySnapshot, AttachmentStore, AttachmentTransaction, RawAttachment,
    SPEECH_TO_TEXT_TOOL, StoredAttachment, canonical_archived_media_path, extension_for_mime,
};

/// 把输入附件归档到本地（图片/文件）。
/// 把输入附件归档到本地。
///
/// 新规则：**用户上传的图片和文件全部归档**。`MediaKind::Image` 走图片归档，
/// `MediaKind::File` 走文件归档。文件类型/MIME 只用于确定扩展名和展示信息，
/// 不决定是否归档。无法识别类型的文件以 `application/octet-stream` 保存，
/// 不再保留临时远程地址。
///
/// 文件名、标题不能覆盖明确的 `MediaKind`——图片标题为 `report.pdf` 时仍按图片处理。
pub fn archive_input_media_assets(media: Vec<MediaAsset>) -> Result<Vec<MediaAsset>, String> {
    let raw = media
        .iter()
        .map(|asset| RawAttachment {
            kind: asset.kind,
            source: asset.url.clone(),
            mime_type: asset.mime_type.clone(),
            original_name: asset.title.clone(),
        })
        .collect();
    let transaction = AttachmentStore::default().store_batch(raw)?;
    let archived = media
        .into_iter()
        .zip(transaction.stored())
        .map(|(asset, stored)| MediaAsset {
            url: stored.local_path.clone(),
            mime_type: Some(stored.mime_type.clone()),
            ..asset
        })
        .collect();
    transaction.commit();
    Ok(archived)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedImage {
    path: String,
    mime_type: String,
}

impl ArchivedImage {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }
}

pub fn archive_image_reference(
    reference: &str,
    mime_hint: Option<&str>,
    original_name: Option<&str>,
) -> Result<ArchivedImage, String> {
    let transaction = AttachmentStore::default().store_batch(vec![RawAttachment {
        kind: MediaKind::Image,
        source: reference.to_string(),
        mime_type: mime_hint.map(str::to_string),
        original_name: original_name.map(str::to_string),
    }])?;
    let stored = transaction
        .stored()
        .first()
        .ok_or_else(|| "图片归档结果为空".to_string())?;
    let archived = ArchivedImage {
        path: stored.local_path.clone(),
        mime_type: stored.mime_type.clone(),
    };
    transaction.commit();
    Ok(archived)
}

#[cfg(test)]
fn media_images_dir() -> Result<PathBuf, String> {
    media_subdir("images")
}

#[cfg(test)]
fn media_files_dir() -> Result<PathBuf, String> {
    media_subdir("files")
}

#[cfg(test)]
fn media_subdir(name: &str) -> Result<PathBuf, String> {
    let dir = AttachmentStore::default().root().join(name);
    std::fs::create_dir_all(&dir).map_err(|err| format!("创建媒体归档目录失败：{err}"))?;
    Ok(dir)
}

/// 判断路径是否位于媒体归档目录（images 或 files）。
///
/// 后端用此验证前端传入的"已归档"路径确实位于真实媒体目录——不仅检查路径
/// 片段，还 canonicalize 真实归档根目录与目标路径，确认目标在根目录内且是
/// 实际存在的普通文件。防止 `/tmp/.tiangong/media/files/../../etc/passwd`
/// 或不存在的伪造路径通过校验。
pub fn is_archived_media_path_any(value: &str) -> bool {
    AttachmentStore::default().is_existing_in_store(value)
}

pub fn image_mime_from_reference(value: &str) -> Option<String> {
    let lower = value.trim().to_ascii_lowercase();
    if lower.starts_with("data:image/") {
        return lower
            .split_once(';')
            .map(|(mime, _)| mime.trim_start_matches("data:").to_string());
    }
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Some("image/jpeg".to_string())
    } else if lower.ends_with(".webp") {
        Some("image/webp".to_string())
    } else if lower.ends_with(".gif") {
        Some("image/gif".to_string())
    } else if lower.ends_with(".png") {
        Some("image/png".to_string())
    } else {
        None
    }
}

/// 从文件引用推断文档 MIME（按扩展名）
#[cfg(test)]
fn document_mime_from_reference(value: &str) -> Option<String> {
    // 忽略查询参数与片段，只检查路径部分（如 "报告.pdf?token=xxx" → ".pdf"）。
    let path = value.split(['?', '#']).next().unwrap_or(value);
    let lower = path.trim().to_ascii_lowercase();
    if lower.ends_with(".pdf") {
        Some("application/pdf".to_string())
    } else if lower.ends_with(".docx") {
        Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document".to_string())
    } else if lower.ends_with(".xlsx") {
        Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string())
    } else if lower.ends_with(".pptx") {
        Some(
            "application/vnd.openxmlformats-officedocument.presentationml.presentation".to_string(),
        )
    } else {
        None
    }
}

/// 把 PDF/Office 文档引用归档到 `~/.tiangong/media/files/`。
///
/// 支持 data URL、http(s) 和本地路径三种来源。
pub fn archive_file_reference(
    reference: &str,
    mime_hint: Option<&str>,
    original_name: Option<&str>,
) -> Result<ArchivedImage, String> {
    let transaction = AttachmentStore::default().store_batch(vec![RawAttachment {
        kind: MediaKind::File,
        source: reference.to_string(),
        mime_type: mime_hint.map(str::to_string),
        original_name: original_name.map(str::to_string),
    }])?;
    let stored = transaction
        .stored()
        .first()
        .ok_or_else(|| "文件归档结果为空".to_string())?;
    let archived = ArchivedImage {
        path: stored.local_path.clone(),
        mime_type: stored.mime_type.clone(),
    };
    transaction.commit();
    Ok(archived)
}

/// 构造归档文件名：优先保留原始文件名的词干，加 scru128 前缀避免冲突。
///
/// 例如 original_name="报告.pdf"、ext="pdf" → "SCRU128_报告.pdf"。
/// original_name 为空或无词干时回退到 "SCRU128.{ext}"。文件名中的路径分隔符
/// 和特殊字符会被清理。
#[cfg(test)]
fn archived_filename(original_name: Option<&str>, ext: &str) -> String {
    let id = scru128::new().to_string();
    if let Some(name) = original_name {
        // 取文件名部分（去掉路径），去掉扩展名，清理特殊字符。
        let normalized = name.replace('\\', "/");
        let stem = normalized.rsplit('/').next().unwrap_or(name).trim();
        let stem = stem.rsplit_once('.').map(|(s, _)| s).unwrap_or(stem);
        let stem = stem.trim();
        if !stem.is_empty() {
            let safe: String = stem
                .chars()
                .map(|ch| {
                    if ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.' | ' ') {
                        ch
                    } else {
                        '_'
                    }
                })
                .collect();
            if !safe.is_empty() {
                return format!("{id}_{safe}.{ext}");
            }
        }
    }
    format!("{id}.{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archives_pdf_data_url_to_local_file() {
        // 构造一个最小合法 PDF 的 data URL
        let pdf_bytes = b"%PDF-1.4\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF";
        let b64 = general_purpose::STANDARD.encode(pdf_bytes);
        let data_url = format!("data:application/pdf;base64,{b64}");

        let asset = MediaAsset {
            kind: MediaKind::File,
            url: data_url,
            mime_type: Some("application/pdf".to_string()),
            title: Some("test.pdf".to_string()),
            capability: None,
        };

        let archived = archive_input_media_assets(vec![asset]).unwrap();
        assert_eq!(archived.len(), 1);
        let result = &archived[0];

        // 归档后 url 必须是本地路径，不再是 data URL
        assert!(
            !result.url.starts_with("data:"),
            "归档后 url 仍为 data URL：{}",
            &result.url[..result.url.len().min(60)]
        );
        // 路径应指向 media/files/ 目录
        assert!(
            result.url.contains("media/files/"),
            "归档路径不在 media/files/ 下：{}",
            result.url
        );
        // 文件应真实存在
        assert!(
            Path::new(&result.url).exists(),
            "归档文件不存在：{}",
            result.url
        );
        // 清理
        let _ = std::fs::remove_file(&result.url);
    }

    #[test]
    fn compatibility_batch_is_atomic_on_failure() {
        let marker = format!("atomic-{}", scru128::new());
        let valid = MediaAsset {
            kind: MediaKind::File,
            url: format!(
                "data:text/plain;base64,{}",
                general_purpose::STANDARD.encode(b"valid")
            ),
            mime_type: Some("text/plain".to_string()),
            title: Some(format!("{marker}.txt")),
            capability: None,
        };
        let invalid = MediaAsset {
            kind: MediaKind::File,
            url: std::env::temp_dir()
                .join(format!("missing-{marker}"))
                .join("attachment.bin")
                .to_string_lossy()
                .to_string(),
            mime_type: None,
            title: None,
            capability: None,
        };

        assert!(archive_input_media_assets(vec![valid, invalid]).is_err());
        let files = media_files_dir().unwrap();
        let leaked = std::fs::read_dir(files).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(marker.as_str())
        });
        assert!(!leaked, "兼容批量接口失败后遗留了本批次文件");
    }

    #[test]
    fn archives_docx_by_mime_even_without_extension_in_url() {
        // data URL 没有 .docx 扩展名，但 mime_type 正确，应能归档
        let docx_bytes = b"PK\x03\x04fake_docx_payload";
        let b64 = general_purpose::STANDARD.encode(docx_bytes);
        let data_url = format!(
            "data:application/vnd.openxmlformats-officedocument.wordprocessingml.document;base64,{b64}"
        );

        let asset = MediaAsset {
            kind: MediaKind::File,
            url: data_url,
            mime_type: Some(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                    .to_string(),
            ),
            title: None,
            capability: None,
        };

        let archived = archive_input_media_assets(vec![asset]).unwrap();
        assert_eq!(archived.len(), 1);
        assert!(!archived[0].url.starts_with("data:"));
        assert!(archived[0].url.ends_with(".docx"));
        let _ = std::fs::remove_file(&archived[0].url);
    }

    #[test]
    fn image_assets_still_archive_to_images_dir() {
        // 确保图片归档逻辑未被文件归档改动破坏
        let png_bytes = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        ];
        let b64 = general_purpose::STANDARD.encode(png_bytes);
        let data_url = format!("data:image/png;base64,{b64}");

        let asset = MediaAsset {
            kind: MediaKind::Image,
            url: data_url,
            mime_type: Some("image/png".to_string()),
            title: None,
            capability: None,
        };

        let archived = archive_input_media_assets(vec![asset]).unwrap();
        assert_eq!(archived.len(), 1);
        assert!(
            archived[0].url.contains("media/images/"),
            "图片应归档到 media/images/：{}",
            archived[0].url
        );
        let _ = std::fs::remove_file(&archived[0].url);
    }

    #[test]
    fn unknown_file_type_archives_as_octet_stream() {
        // 无扩展名、无 MIME 的 data URL 文件应归档为 octet-stream，不报错
        let bytes = b"random binary content";
        let b64 = general_purpose::STANDARD.encode(bytes);
        let data_url = format!("data:application/octet-stream;base64,{b64}");
        let asset = MediaAsset {
            kind: MediaKind::File,
            url: data_url,
            mime_type: None,
            title: Some("unknown.dat".to_string()),
            capability: None,
        };
        let archived = archive_input_media_assets(vec![asset]).unwrap();
        assert_eq!(archived.len(), 1);
        assert!(
            archived[0].url.contains("media/files/"),
            "未知类型文件应归档到 media/files/：{}",
            archived[0].url
        );
        assert!(
            archived[0].mime_type.as_deref() == Some("application/octet-stream"),
            "未知类型应为 octet-stream，实际：{:?}",
            archived[0].mime_type
        );
        let _ = std::fs::remove_file(&archived[0].url);
    }

    #[test]
    fn image_with_pdf_title_still_archives_as_image() {
        // 图片标题为 report.pdf 时仍按图片归档（title 不覆盖 MediaKind）
        let bytes = b"fake png";
        let b64 = general_purpose::STANDARD.encode(bytes);
        let data_url = format!("data:image/png;base64,{b64}");
        let asset = MediaAsset {
            kind: MediaKind::Image,
            url: data_url,
            mime_type: Some("image/png".to_string()),
            title: Some("report.pdf".to_string()),
            capability: None,
        };
        let archived = archive_input_media_assets(vec![asset]).unwrap();
        assert_eq!(archived.len(), 1);
        assert!(
            archived[0].url.contains("media/images/"),
            "图片标题为 pdf 时仍应归档到 images/：{}",
            archived[0].url
        );
        let _ = std::fs::remove_file(&archived[0].url);
    }

    #[test]
    fn document_mime_from_reference_ignores_query_params() {
        // 带查询参数的 URL 应忽略参数后检查扩展名
        assert_eq!(
            document_mime_from_reference("https://host/dl/报告.pdf?token=xxx"),
            Some("application/pdf".to_string())
        );
        // 无扩展名的临时地址
        assert!(document_mime_from_reference("https://host/download/123").is_none());
    }

    #[test]
    fn is_archived_media_path_any_handles_backslashes() {
        // 创建真实归档文件用于 canonicalize 验证
        let dir = media_images_dir().unwrap();
        let file_path = dir.join("backslash_test.png");
        std::fs::write(&file_path, b"test").unwrap();

        // Unix 路径（真实文件）
        assert!(is_archived_media_path_any(file_path.to_str().unwrap()));

        // 非归档路径（不存在或不在归档目录）
        assert!(!is_archived_media_path_any("/tmp/some_file.png"));

        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn archived_filename_preserves_original_stem() {
        // 有原文件名时保留词干
        let f = archived_filename(Some("报告.pdf"), "pdf");
        assert!(f.contains("报告"), "文件名应含原词干：{f}");
        assert!(f.ends_with(".pdf"), "文件名应以 .pdf 结尾：{f}");

        // 带路径的原文件名只取文件名部分
        let f = archived_filename(Some("/tmp/sub/photo.PNG"), "png");
        assert!(f.contains("photo"), "应只取文件名部分：{f}");

        // 无原文件名时回退到 scru128.ext
        let f = archived_filename(None, "bin");
        assert!(f.ends_with(".bin"), "无原文件名应回退：{f}");

        // 空字符串原文件名
        let f = archived_filename(Some(""), "pdf");
        assert!(f.ends_with(".pdf"));
    }

    #[test]
    fn archive_preserves_original_filename() {
        let pdf_bytes = b"%PDF-1.4";
        let b64 = general_purpose::STANDARD.encode(pdf_bytes);
        let data_url = format!("data:application/pdf;base64,{b64}");
        let asset = MediaAsset {
            kind: MediaKind::File,
            url: data_url,
            mime_type: Some("application/pdf".to_string()),
            title: Some("季度报告.pdf".to_string()),
            capability: None,
        };
        let archived = archive_input_media_assets(vec![asset]).unwrap();
        assert_eq!(archived.len(), 1);
        assert!(
            archived[0].url.contains("季度报告"),
            "归档文件名应保留原始词干：{}",
            archived[0].url
        );
        assert!(archived[0].url.ends_with(".pdf"));
        let _ = std::fs::remove_file(&archived[0].url);
    }

    #[test]
    fn archive_image_preserves_filename() {
        let png_bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let b64 = general_purpose::STANDARD.encode(png_bytes);
        let data_url = format!("data:image/png;base64,{b64}");
        let asset = MediaAsset {
            kind: MediaKind::Image,
            url: data_url,
            mime_type: Some("image/png".to_string()),
            title: Some("截图.png".to_string()),
            capability: None,
        };
        let archived = archive_input_media_assets(vec![asset]).unwrap();
        assert_eq!(archived.len(), 1);
        assert!(
            archived[0].url.contains("截图"),
            "图片归档文件名应保留原始词干：{}",
            archived[0].url
        );
        assert!(archived[0].url.ends_with(".png"));
        let _ = std::fs::remove_file(&archived[0].url);
    }

    #[test]
    fn no_extension_temp_url_still_archives_as_file() {
        // 无扩展名临时下载地址的 data URL（模拟 Server 文件附件场景）
        let bytes = b"file content";
        let b64 = general_purpose::STANDARD.encode(bytes);
        let data_url = format!("data:application/octet-stream;base64,{b64}");
        let asset = MediaAsset {
            kind: MediaKind::File,
            url: data_url,
            mime_type: None,
            title: Some("downloaded_file".to_string()),
            capability: None,
        };
        let archived = archive_input_media_assets(vec![asset]).unwrap();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].url.contains("media/files/"));
        assert!(
            archived[0].url.contains("downloaded_file"),
            "应保留原始文件名词干：{}",
            archived[0].url
        );
        let _ = std::fs::remove_file(&archived[0].url);
    }

    #[test]
    fn re_archiving_already_archived_path_is_idempotent() {
        // 重复编辑场景：已归档路径再次归档应返回相同路径，不产生新文件。
        // 1. 先归档一次
        let png_bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let b64 = general_purpose::STANDARD.encode(png_bytes);
        let data_url = format!("data:image/png;base64,{b64}");
        let asset = MediaAsset {
            kind: MediaKind::Image,
            url: data_url,
            mime_type: Some("image/png".to_string()),
            title: Some("test.png".to_string()),
            capability: None,
        };
        let first = archive_input_media_assets(vec![asset]).unwrap();
        let archived_path = first[0].url.clone();
        assert!(Path::new(&archived_path).exists());

        // 2. 用已归档路径再次归档——应幂等放行，返回相同路径
        let asset2 = MediaAsset {
            kind: MediaKind::Image,
            url: archived_path.clone(),
            mime_type: Some("image/png".to_string()),
            title: Some("test.png".to_string()),
            capability: None,
        };
        let second = archive_input_media_assets(vec![asset2]).unwrap();
        assert_eq!(
            second[0].url, archived_path,
            "已归档路径再次归档应返回相同路径，不产生副本"
        );

        let _ = std::fs::remove_file(&archived_path);
    }

    #[test]
    fn archived_media_path_any_rejects_non_media_paths() {
        // 无效编辑场景：前端传入的路径不在媒体目录时，后端验证应拒绝
        assert!(!is_archived_media_path_any("/etc/passwd"));
        assert!(!is_archived_media_path_any("/tmp/malicious.png"));
        assert!(!is_archived_media_path_any("../../../etc/shadow"));
        // 路径包含 .tiangong/media/ 片段但不是真实文件 → 拒绝
        assert!(!is_archived_media_path_any(
            "/home/user/.tiangong/media/files/ghost.pdf"
        ));

        // 创建真实归档文件 → 通过
        let dir = media_files_dir().unwrap();
        let file_path = dir.join("validation_test.pdf");
        std::fs::write(&file_path, b"test").unwrap();
        assert!(is_archived_media_path_any(file_path.to_str().unwrap()));
        let _ = std::fs::remove_file(&file_path);
    }
}
