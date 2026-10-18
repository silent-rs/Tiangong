//! 工作区文件操作：路径约束、目录列举、读取、原子写入与增删改名。
//!
//! 所有路径都是相对工作区根的路径；绝对路径、`..`、以及经符号链接逃逸到
//! 根之外的目标一律拒绝。

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 文本预览/编辑上限。
pub const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;
/// 图片预览上限（base64 回传，体积约 4/3）。
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
/// 单个目录最多返回的条目数。
pub const MAX_DIR_ENTRIES: usize = 5000;

/// 已规范化的工作区根。
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn open(root: &Path) -> Result<Self> {
        if !root.is_absolute() {
            bail!("工作区必须是绝对路径: {}", root.display());
        }
        let root = root
            .canonicalize()
            .with_context(|| format!("工作区不存在或不可访问: {}", root.display()))?;
        if !root.is_dir() {
            bail!("工作区不是目录: {}", root.display());
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 解析已存在的目标（允许根本身），并确认真实路径仍在根内。
    fn existing(&self, relative: &str) -> Result<PathBuf> {
        let joined = self.join(relative)?;
        let real = joined
            .canonicalize()
            .with_context(|| format!("路径不存在: {relative}"))?;
        self.ensure_inside(&real, relative)?;
        Ok(real)
    }

    /// 解析待创建的目标：父目录必须存在且在根内，目标本身不能是根。
    fn creatable(&self, relative: &str) -> Result<PathBuf> {
        let joined = self.join(relative)?;
        if joined == self.root {
            bail!("不能对工作区根执行该操作");
        }
        let name = joined
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("路径缺少文件名: {relative}"))?
            .to_owned();
        let parent = joined.parent().unwrap_or(&self.root);
        let real_parent = parent
            .canonicalize()
            .with_context(|| format!("父目录不存在: {relative}"))?;
        self.ensure_inside(&real_parent, relative)?;
        if !real_parent.is_dir() {
            bail!("父路径不是目录: {relative}");
        }
        Ok(real_parent.join(name))
    }

    /// 解析已存在但不能是根的目标；符号链接本身作为操作对象（不跟随）。
    fn existing_entry(&self, relative: &str) -> Result<PathBuf> {
        let target = self.creatable(relative)?;
        if fs::symlink_metadata(&target).is_err() {
            bail!("路径不存在: {relative}");
        }
        Ok(target)
    }

    fn join(&self, relative: &str) -> Result<PathBuf> {
        let mut path = self.root.clone();
        for component in Path::new(relative.trim()).components() {
            match component {
                Component::Normal(part) => path.push(part),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    bail!("只允许工作区内的相对路径: {relative}");
                }
            }
        }
        Ok(path)
    }

    fn ensure_inside(&self, real: &Path, relative: &str) -> Result<()> {
        if !real.starts_with(&self.root) {
            bail!("路径越出工作区: {relative}");
        }
        Ok(())
    }

    fn relative_of(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
}

#[derive(Debug, Serialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct DirListing {
    pub path: String,
    pub entries: Vec<DirEntry>,
    pub truncated: bool,
}

pub fn list_dir(workspace: &Workspace, relative: &str) -> Result<DirListing> {
    let dir = workspace.existing(relative)?;
    if !dir.is_dir() {
        bail!("不是目录: {relative}");
    }
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in fs::read_dir(&dir).with_context(|| format!("读取目录失败: {relative}"))? {
        let Ok(item) = item else { continue };
        if entries.len() >= MAX_DIR_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(meta) = item.metadata() else { continue };
        let kind = if meta.file_type().is_symlink() {
            // 指向目录的链接按目录展示，展开时仍经 existing() 做越界检查
            if item.path().is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::Symlink
            }
        } else if meta.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        let path = item.path();
        entries.push(DirEntry {
            name: item.file_name().to_string_lossy().into_owned(),
            path: workspace.relative_of(&path),
            kind,
            size: if meta.is_file() { meta.len() } else { 0 },
            modified_ms: modified_ms(&meta),
        });
    }
    entries.sort_by(|a, b| {
        (a.kind != EntryKind::Dir)
            .cmp(&(b.kind != EntryKind::Dir))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(DirListing {
        path: workspace.relative_of(&dir),
        entries,
        truncated,
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    Image,
    Binary,
}

#[derive(Debug, Serialize)]
pub struct FileContent {
    pub path: String,
    pub kind: ContentKind,
    pub size: u64,
    pub modified_ms: u64,
    /// 内容摘要（sha256），写回时作为乐观锁。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// 超过预览上限，未返回内容。
    pub too_large: bool,
}

pub fn read_file(workspace: &Workspace, relative: &str) -> Result<FileContent> {
    let path = workspace.existing(relative)?;
    let meta = fs::metadata(&path).with_context(|| format!("读取文件信息失败: {relative}"))?;
    if !meta.is_file() {
        bail!("不是文件: {relative}");
    }
    let size = meta.len();
    let mut result = FileContent {
        path: workspace.relative_of(&path),
        kind: ContentKind::Binary,
        size,
        modified_ms: modified_ms(&meta),
        hash: None,
        content: None,
        base64: None,
        mime: None,
        too_large: false,
    };
    if let Some(mime) = image_mime(&path) {
        result.kind = ContentKind::Image;
        result.mime = Some(mime.to_string());
        if size > MAX_IMAGE_BYTES {
            result.too_large = true;
            return Ok(result);
        }
        let bytes = fs::read(&path).with_context(|| format!("读取文件失败: {relative}"))?;
        result.hash = Some(digest(&bytes));
        result.base64 = Some(base64::engine::general_purpose::STANDARD.encode(&bytes));
        return Ok(result);
    }
    if size > MAX_TEXT_BYTES {
        result.too_large = true;
        return Ok(result);
    }
    let bytes = fs::read(&path).with_context(|| format!("读取文件失败: {relative}"))?;
    result.hash = Some(digest(&bytes));
    let probe = &bytes[..bytes.len().min(8192)];
    if !probe.contains(&0)
        && let Ok(text) = String::from_utf8(bytes)
    {
        result.kind = ContentKind::Text;
        result.content = Some(text);
    }
    Ok(result)
}

#[derive(Debug, Serialize)]
pub struct WriteResult {
    pub path: String,
    pub size: u64,
    pub modified_ms: u64,
    pub hash: String,
}

/// 写入文本文件。`expected_hash` 为读取时的摘要：文件已被他人修改时拒绝覆盖；
/// 为 None 时仅允许创建新文件或无条件覆盖（由 `overwrite` 决定）。
pub fn write_file(
    workspace: &Workspace,
    relative: &str,
    content: &str,
    expected_hash: Option<&str>,
    overwrite: bool,
) -> Result<WriteResult> {
    let target = workspace.creatable(relative)?;
    match fs::symlink_metadata(&target) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!("不支持直接写入符号链接: {relative}");
            }
            if !meta.is_file() {
                bail!("目标不是普通文件: {relative}");
            }
            if let Some(expected) = expected_hash {
                let current = digest(
                    &fs::read(&target).with_context(|| format!("读取文件失败: {relative}"))?,
                );
                if current != expected {
                    bail!("文件已在外部被修改，请重新加载后再保存: {relative}");
                }
            } else if !overwrite {
                bail!("文件已存在: {relative}");
            }
        }
        Err(_) => {
            if expected_hash.is_some() {
                bail!("文件已被删除或移动: {relative}");
            }
        }
    }
    let bytes = content.as_bytes();
    atomic_write(&target, bytes).with_context(|| format!("写入文件失败: {relative}"))?;
    let meta = fs::metadata(&target).with_context(|| format!("读取文件信息失败: {relative}"))?;
    Ok(WriteResult {
        path: workspace.relative_of(&target),
        size: meta.len(),
        modified_ms: modified_ms(&meta),
        hash: digest(bytes),
    })
}

/// 同目录临时文件 + rename，避免写到一半留下截断内容；保留原文件权限。
fn atomic_write(target: &Path, bytes: &[u8]) -> Result<()> {
    let parent = target.parent().context("目标缺少父目录")?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = parent.join(format!(".{name}.{}.tmp", scru128::new_string()));
    let outcome = (|| -> Result<()> {
        let mut file = fs::File::create(&temp).context("创建临时文件失败")?;
        file.write_all(bytes).context("写入临时文件失败")?;
        file.sync_all().context("刷新临时文件失败")?;
        if let Ok(meta) = fs::metadata(target) {
            fs::set_permissions(&temp, meta.permissions()).context("复制文件权限失败")?;
        }
        fs::rename(&temp, target).context("替换目标文件失败")?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(&temp);
    }
    outcome
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CreateKind {
    File,
    Dir,
}

pub fn create(workspace: &Workspace, relative: &str, kind: CreateKind) -> Result<String> {
    let target = workspace.creatable(relative)?;
    if fs::symlink_metadata(&target).is_ok() {
        bail!("已存在同名文件或目录: {relative}");
    }
    match kind {
        CreateKind::Dir => {
            fs::create_dir(&target).with_context(|| format!("创建目录失败: {relative}"))?
        }
        CreateKind::File => {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .with_context(|| format!("创建文件失败: {relative}"))?;
        }
    }
    Ok(workspace.relative_of(&target))
}

pub fn rename(workspace: &Workspace, from: &str, to: &str) -> Result<String> {
    let source = workspace.existing_entry(from)?;
    let target = workspace.creatable(to)?;
    if fs::symlink_metadata(&target).is_ok() {
        bail!("目标已存在: {to}");
    }
    if source.is_dir() && target.starts_with(&source) {
        bail!("不能把目录移动到其自身内部: {from} -> {to}");
    }
    fs::rename(&source, &target).with_context(|| format!("重命名失败: {from} -> {to}"))?;
    Ok(workspace.relative_of(&target))
}

pub fn delete(workspace: &Workspace, relative: &str) -> Result<()> {
    let target = workspace.existing_entry(relative)?;
    let meta = fs::symlink_metadata(&target).with_context(|| format!("路径不存在: {relative}"))?;
    if meta.is_dir() {
        fs::remove_dir_all(&target).with_context(|| format!("删除目录失败: {relative}"))?;
    } else {
        fs::remove_file(&target).with_context(|| format!("删除文件失败: {relative}"))?;
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn modified_ms(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn image_mime(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().expect("创建临时目录");
        let workspace = Workspace::open(dir.path()).expect("打开工作区");
        (dir, workspace)
    }

    #[test]
    fn rejects_relative_root_and_escape() {
        assert!(Workspace::open(Path::new("relative")).is_err());
        let (_dir, ws) = workspace();
        assert!(list_dir(&ws, "../").is_err());
        assert!(read_file(&ws, "/etc/hosts").is_err());
        assert!(create(&ws, "a/../../x", CreateKind::File).is_err());
        assert!(delete(&ws, "").is_err());
        assert!(delete(&ws, ".").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let outside = tempfile::tempdir().expect("外部目录");
        fs::write(outside.path().join("secret.txt"), "s").expect("写外部文件");
        let (dir, ws) = workspace();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).expect("建链接");
        assert!(read_file(&ws, "link/secret.txt").is_err());
        assert!(list_dir(&ws, "link").is_err());
        assert!(write_file(&ws, "link/new.txt", "x", None, false).is_err());
        // 删除链接本身是允许的，且不影响外部目标
        delete(&ws, "link").expect("删除链接");
        assert!(outside.path().join("secret.txt").exists());
    }

    #[test]
    fn list_sorts_dirs_first() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("b.txt"), "b").expect("写文件");
        fs::create_dir(dir.path().join("z")).expect("建目录");
        fs::write(dir.path().join("A.md"), "a").expect("写文件");
        let listing = list_dir(&ws, "").expect("列目录");
        let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["z", "A.md", "b.txt"]);
        assert_eq!(listing.entries[0].kind, EntryKind::Dir);
        assert_eq!(listing.entries[1].path, "A.md");
        assert_eq!(listing.path, "");
    }

    #[test]
    fn read_classifies_text_binary_image() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("t.rs"), "fn main() {}").expect("写文本");
        fs::write(dir.path().join("b.bin"), [0u8, 1, 2]).expect("写二进制");
        fs::write(dir.path().join("p.png"), [0x89, b'P', b'N', b'G']).expect("写图片");
        let text = read_file(&ws, "t.rs").expect("读文本");
        assert!(matches!(text.kind, ContentKind::Text));
        assert_eq!(text.content.as_deref(), Some("fn main() {}"));
        let binary = read_file(&ws, "b.bin").expect("读二进制");
        assert!(matches!(binary.kind, ContentKind::Binary));
        assert!(binary.content.is_none());
        let image = read_file(&ws, "p.png").expect("读图片");
        assert!(matches!(image.kind, ContentKind::Image));
        assert_eq!(image.mime.as_deref(), Some("image/png"));
        assert!(image.base64.is_some());
    }

    #[test]
    fn large_text_is_not_returned() {
        let (dir, ws) = workspace();
        let file = fs::File::create(dir.path().join("big.log")).expect("建文件");
        file.set_len(MAX_TEXT_BYTES + 1).expect("扩容");
        let result = read_file(&ws, "big.log").expect("读大文件");
        assert!(result.too_large);
        assert!(result.content.is_none());
    }

    #[test]
    fn write_uses_optimistic_lock() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("a.txt"), "one").expect("写文件");
        let read = read_file(&ws, "a.txt").expect("读取");
        let hash = read.hash.expect("摘要");
        let written = write_file(&ws, "a.txt", "two", Some(&hash), false).expect("写回");
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).expect("读"),
            "two"
        );
        // 旧摘要已过期，拒绝覆盖
        assert!(write_file(&ws, "a.txt", "three", Some(&hash), false).is_err());
        write_file(&ws, "a.txt", "three", Some(&written.hash), false).expect("新摘要写回");
        // 无摘要且不允许覆盖时，已存在文件拒绝写入
        assert!(write_file(&ws, "a.txt", "x", None, false).is_err());
        write_file(&ws, "new.txt", "n", None, false).expect("新建写入");
        // 写入不留临时文件
        let leftovers = fs::read_dir(dir.path())
            .expect("列目录")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn create_rename_delete() {
        let (dir, ws) = workspace();
        create(&ws, "src", CreateKind::Dir).expect("建目录");
        create(&ws, "src/lib.rs", CreateKind::File).expect("建文件");
        assert!(create(&ws, "src/lib.rs", CreateKind::File).is_err());
        assert!(create(&ws, "missing/x.rs", CreateKind::File).is_err());
        let moved = rename(&ws, "src/lib.rs", "src/main.rs").expect("改名");
        assert_eq!(moved, "src/main.rs");
        assert!(rename(&ws, "src", "src/inner").is_err());
        create(&ws, "other", CreateKind::Dir).expect("建目录");
        assert!(rename(&ws, "src", "other").is_err());
        delete(&ws, "src").expect("删目录");
        assert!(!dir.path().join("src").exists());
        assert!(delete(&ws, "src").is_err());
    }
}
