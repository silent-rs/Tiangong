//! 待发送本地文件的路径与内容校验（三平台共用）。
//!
//! 文件只允许来自当前会话工作区（宿主权威上下文提供）或天工媒体目录；
//! 相对路径必须有会话工作区才能解析。

use std::path::{Path, PathBuf};

pub(crate) const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
pub(crate) const MAX_FILE_BYTES: u64 = 30 * 1024 * 1024;
const MAX_FILE_NAME_CHARS: usize = 255;

pub(crate) struct LocalMedia {
    /// 规范化后的绝对路径。
    pub(crate) path: PathBuf,
    pub(crate) bytes: Vec<u8>,
    pub(crate) file_name: String,
    /// 图片按内容嗅探的 MIME；普通文件为 `application/octet-stream`。
    pub(crate) mime_type: String,
}

/// 读取并校验本地文件；失败返回可直接展示给模型的原因。
pub(crate) fn read_local_media(
    file_path: &str,
    workspace: Option<&Path>,
    image: bool,
) -> Result<LocalMedia, String> {
    let max_bytes = if image {
        MAX_IMAGE_BYTES
    } else {
        MAX_FILE_BYTES
    };
    if file_path.is_empty() {
        return Err("本地文件路径不能为空".to_string());
    }
    let workspace = workspace
        .filter(|path| !path.as_os_str().is_empty())
        .and_then(|path| std::fs::canonicalize(path).ok());
    let candidate = PathBuf::from(file_path);
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        match &workspace {
            Some(workspace) => workspace.join(candidate),
            None => return Err("当前调用没有会话工作目录，请使用绝对路径".to_string()),
        }
    };
    let canonical =
        std::fs::canonicalize(&candidate).map_err(|_| "本地文件不存在或无法访问".to_string())?;
    if !path_is_allowed(&canonical, workspace.as_deref()) {
        return Err("本地文件必须位于当前会话工作目录或 ~/.tiangong/media".to_string());
    }
    let metadata = std::fs::metadata(&canonical).map_err(|_| "无法读取本地文件信息".to_string())?;
    if !metadata.is_file() {
        return Err("本地路径必须指向普通文件".to_string());
    }
    if metadata.len() == 0 {
        return Err("不能发送空文件".to_string());
    }
    if metadata.len() > max_bytes {
        return Err(format!("本地文件超过 {} MiB 限制", max_bytes / 1024 / 1024));
    }
    let file_name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "本地文件名无效".to_string())?
        .to_string();
    if file_name.chars().count() > MAX_FILE_NAME_CHARS {
        return Err("本地文件名不能超过 255 个字符".to_string());
    }
    let bytes = std::fs::read(&canonical).map_err(|_| "读取本地文件失败".to_string())?;
    if bytes.is_empty() {
        return Err("不能发送空文件".to_string());
    }
    if bytes.len() as u64 > max_bytes {
        return Err(format!("本地文件超过 {} MiB 限制", max_bytes / 1024 / 1024));
    }
    let mime_type = if image {
        match (supported_image_extension(&canonical), image_mime(&bytes)) {
            (true, Some(mime)) => mime.to_string(),
            _ => return Err("图片格式不受支持，仅支持 PNG、JPEG、GIF 和 WebP".to_string()),
        }
    } else {
        "application/octet-stream".to_string()
    };
    Ok(LocalMedia {
        path: canonical,
        bytes,
        file_name,
        mime_type,
    })
}

fn supported_image_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp"
    )
}

/// 按 magic bytes 嗅探图片 MIME。
pub(crate) fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn path_is_allowed(path: &Path, workspace: Option<&Path>) -> bool {
    if workspace.is_some_and(|workspace| path.starts_with(workspace)) {
        return true;
    }
    media_directory()
        .and_then(|media| std::fs::canonicalize(media).ok())
        .is_some_and(|media| path.starts_with(media))
}

/// 天工媒体目录：`<存储根>/media`。
fn media_directory() -> Option<PathBuf> {
    storage_root().map(|root| root.join("media"))
}

/// 天工存储根：优先宿主注入的 `TIANGONG_STORAGE_ROOT`，否则 `~/.tiangong`。
pub(crate) fn storage_root() -> Option<PathBuf> {
    std::env::var_os(tiangong_plugin_runtime::sidecar::STORAGE_ROOT_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_directory().map(|home| home.join(".tiangong")))
}

fn home_directory() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(profile));
    }
    let mut drive = std::env::var_os("HOMEDRIVE").filter(|value| !value.is_empty())?;
    let path = std::env::var_os("HOMEPATH").filter(|value| !value.is_empty())?;
    drive.push(path);
    Some(PathBuf::from(drive))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0, 0, 0, 0];

    #[test]
    fn local_media_must_stay_in_workspace_and_relative_needs_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(workspace.path().join("p.png"), PNG).unwrap();
        std::fs::write(workspace.path().join("fake.png"), b"hello").unwrap();
        std::fs::write(workspace.path().join("empty.txt"), b"").unwrap();
        std::fs::write(outside.path().join("b.txt"), b"hello").unwrap();

        let media = read_local_media("a.txt", Some(workspace.path()), false).unwrap();
        assert_eq!(media.file_name, "a.txt");
        assert_eq!(media.mime_type, "application/octet-stream");
        assert!(read_local_media("a.txt", None, false).is_err());
        let outside_file = outside.path().join("b.txt");
        assert!(
            read_local_media(
                outside_file.to_str().unwrap(),
                Some(workspace.path()),
                false
            )
            .is_err()
        );
        assert!(read_local_media("empty.txt", Some(workspace.path()), false).is_err());
        assert!(read_local_media("a.txt", Some(workspace.path()), true).is_err());
        assert!(read_local_media("fake.png", Some(workspace.path()), true).is_err());
        let image = read_local_media("p.png", Some(workspace.path()), true).unwrap();
        assert_eq!(image.mime_type, "image/png");
    }

    #[test]
    fn image_mime_matches_common_formats() {
        assert_eq!(image_mime(PNG), Some("image/png"));
        assert_eq!(image_mime(&[0xff, 0xd8, 0xff, 0]), Some("image/jpeg"));
        assert_eq!(image_mime(b"GIF89a"), Some("image/gif"));
        assert_eq!(image_mime(b"RIFF\0\0\0\0WEBP"), Some("image/webp"));
        assert_eq!(image_mime(b"hello"), None);
    }
}
