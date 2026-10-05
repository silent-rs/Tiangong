//! 远程访问的静态资源：前端页面（注入远程标记）与会话媒体文件。

use std::path::{Path, PathBuf};

use tauri::AppHandle;

/// 远程可读取的单个文件上限。
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

pub struct AssetReply {
    pub status: u16,
    pub mime: String,
    pub body: Vec<u8>,
    pub no_store: bool,
    pub sandbox: bool,
}

impl AssetReply {
    pub fn not_found() -> Self {
        Self::text(404, "Not Found")
    }

    pub fn text(status: u16, text: &str) -> Self {
        Self {
            status,
            mime: "text/plain; charset=utf-8".to_string(),
            body: text.as_bytes().to_vec(),
            no_store: true,
            sandbox: true,
        }
    }
}

/// 在页面 `<head>` 起始处注入远程模式标记（先于模块脚本执行）。
pub fn inject_remote_marker(html: &str) -> String {
    let script = "<script>window.__TIANGONG_REMOTE__={\"v\":1};</script>";
    match html.find("<head>") {
        Some(index) => {
            let at = index + "<head>".len();
            format!("{}{script}{}", &html[..at], &html[at..])
        }
        None => format!("{script}{html}"),
    }
}

/// 前端构建产物中允许远程读取的路径：主页面及其静态资源，不含配置页。
pub fn frontend_path_allowed(path: &str) -> bool {
    if path.is_empty() || path == "index.html" {
        return true;
    }
    if path.contains("..") || path.contains('\\') || path.starts_with('/') {
        return false;
    }
    if let Some(rest) = path.strip_prefix("assets/") {
        return !rest.is_empty() && !rest.starts_with("config");
    }
    // 根目录下的图标等单文件资源。
    !path.contains('/')
        && matches!(
            path.rsplit('.').next().unwrap_or_default(),
            "svg" | "png" | "ico" | "webp"
        )
}

pub fn serve_frontend(app: &AppHandle, path: &str) -> AssetReply {
    if !frontend_path_allowed(path) {
        return AssetReply::not_found();
    }
    let key = if path.is_empty() { "index.html" } else { path };
    let Some(asset) = app.asset_resolver().get(key.to_string()) else {
        return AssetReply::not_found();
    };
    if key == "index.html" {
        let html = inject_remote_marker(&String::from_utf8_lossy(asset.bytes()));
        return AssetReply {
            status: 200,
            mime: "text/html; charset=utf-8".to_string(),
            body: html.into_bytes(),
            no_store: true,
            sandbox: false,
        };
    }
    AssetReply {
        status: 200,
        mime: asset.mime_type().to_string(),
        body: asset.bytes().to_vec(),
        no_store: false,
        sandbox: false,
    }
}

/// 与桌面端 asset 协议一致的可读范围（`tauri.conf.json` 的 assetProtocol.scope）。
fn readable_roots() -> Vec<PathBuf> {
    let mut roots = vec![tiangong_config::io::storage_root()];
    if let Some(home) = dirs_home() {
        roots.push(home.join("Documents"));
        roots.push(home.join("Downloads"));
    }
    roots.push(PathBuf::from("/tmp"));
    roots
        .into_iter()
        .filter_map(|root| dunce_canonical(&root))
        .collect()
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn dunce_canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// 读取会话中的本地媒体文件（图片、音视频、附件），限定在桌面端同等可读范围内。
pub fn serve_file(raw_path: &str) -> AssetReply {
    let Some(path) = dunce_canonical(Path::new(raw_path)) else {
        return AssetReply::not_found();
    };
    if !readable_roots().iter().any(|root| path.starts_with(root)) {
        return AssetReply::text(403, "Forbidden");
    }
    let Ok(metadata) = std::fs::metadata(&path) else {
        return AssetReply::not_found();
    };
    if !metadata.is_file() {
        return AssetReply::not_found();
    }
    if metadata.len() > MAX_FILE_BYTES {
        return AssetReply::text(413, "文件过大，远程模式不支持预览");
    }
    let Ok(body) = std::fs::read(&path) else {
        return AssetReply::not_found();
    };
    AssetReply {
        status: 200,
        mime: mime_for(&path).to_string(),
        body,
        no_store: true,
        // 用户文件属于不可信内容：禁止在中继来源下执行脚本。
        sandbox: true,
    }
}

pub fn mime_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "pdf" => "application/pdf",
        "txt" | "md" | "csv" | "json" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// 解析 `a=1&b=2` 形式的查询串。
pub fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| percent_decode(value))
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_injection() {
        let html = inject_remote_marker("<html><head><title>x</title></head></html>");
        assert!(html.contains("<head><script>window.__TIANGONG_REMOTE__="));
    }

    #[test]
    fn frontend_paths() {
        assert!(frontend_path_allowed(""));
        assert!(frontend_path_allowed("index.html"));
        assert!(frontend_path_allowed("assets/main-abc.js"));
        assert!(frontend_path_allowed("tiangong.svg"));
        assert!(!frontend_path_allowed("config.html"));
        assert!(!frontend_path_allowed("assets/config-abc.js"));
        assert!(!frontend_path_allowed("assets/../config.html"));
        assert!(!frontend_path_allowed("a/b.svg"));
    }

    #[test]
    fn query_parsing() {
        assert_eq!(
            query_value("path=%2Ftmp%2Fa%20b.png&k=xyz", "path").as_deref(),
            Some("/tmp/a b.png")
        );
        assert_eq!(query_value("path=a&k=xyz", "k").as_deref(), Some("xyz"));
        assert_eq!(query_value("k=1", "path"), None);
        assert_eq!(query_value("p=%zz", "p").as_deref(), Some("%zz"));
    }

    #[test]
    fn file_scope_is_enforced() {
        assert_eq!(serve_file("/etc/hosts").status, 403);
        assert_eq!(serve_file("/definitely/missing/file").status, 404);
    }
}
