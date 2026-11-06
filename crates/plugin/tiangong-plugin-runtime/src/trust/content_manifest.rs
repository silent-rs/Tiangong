//! 插件内容清单：devkit 构建产物的逐文件哈希锚。
//!
//! 官方签名验签与本地信任 sidecar 的启动前复核共用同一份清单与同一套校验，
//! 因此归信任域（D2）而不是传输层：任何需要回答「这份目录内容是否与清单
//! 一致」的调用方都走 [`verify`]，传输层只负责在 spawn 前调用它。
use anyhow::{Context, Result, bail};
use std::path::Path;
pub use tiangong_plugin_update::CONTENT_MANIFEST_FILE;
/// 按内容清单（路径 + sha256）复核插件目录文件树，任一文件缺失、路径逃逸
/// 或哈希不一致即报错；清单未覆盖的受管文件同样视为篡改。
///
/// 官方签名验签（安装时）与本地信任解释器 sidecar 的启动前复核共用本函数。
pub fn verify(manifest_path: &Path, root: &Path) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct ContentFile {
        path: String,
        sha256: String,
    }
    #[derive(serde::Deserialize)]
    struct ContentManifest {
        files: Vec<ContentFile>,
    }
    use sha2::{Digest, Sha256};
    let raw = std::fs::read(manifest_path)
        .with_context(|| format!("读取内容清单失败: {}", manifest_path.display()))?;
    let content: ContentManifest = serde_json::from_slice(&raw)
        .with_context(|| format!("解析内容清单失败: {}", manifest_path.display()))?;
    // 路径唯一性：清单不允许重复条目。
    let mut listed = std::collections::BTreeSet::new();
    for file in &content.files {
        let relative = Path::new(&file.path);
        if relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        }) {
            bail!("内容清单包含不安全路径: {}", file.path);
        }
        if !listed.insert(file.path.clone()) {
            bail!("内容清单包含重复路径: {}", file.path);
        }
        let path = root.join(relative);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("内容清单文件缺失: {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("内容清单条目不是普通文件: {}", file.path);
        }
        let raw = std::fs::read(&path)
            .with_context(|| format!("内容清单文件缺失: {}", path.display()))?;
        let actual = hex::encode(Sha256::digest(&raw));
        if !actual.eq_ignore_ascii_case(&file.sha256) {
            bail!(
                "插件文件 {} 与内容清单不一致（可能被篡改），拒绝启动",
                file.path
            );
        }
    }
    // 反向遍历受管文件树：清单必须完整覆盖——未列出的受管文件视为
    // 篡改（绕过哈希锁定的替换/新增通道）。运行时自管目录与信任标记除外。
    // 运行时自管目录、信任标记与官方签名文件（验签产物，非内容清单
    // 管辖——签名锚定的是清单本身）。
    const UNMANAGED: [&str; 7] = [
        "runtime",
        "logs",
        "data",
        "local-trust.json",
        "content-manifest.json",
        "release.json",
        "release.json.sig",
    ];
    let mut actual_files = std::collections::BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("读取插件目录失败: {}", directory.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if directory == root && UNMANAGED.contains(&name.as_ref()) {
                continue;
            }
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .context("插件文件相对路径推算失败")?
                    .to_string_lossy()
                    .replace('\\', "/");
                actual_files.insert(relative);
            }
        }
    }
    let unexpected: Vec<&String> = actual_files.difference(&listed).collect();
    if !unexpected.is_empty() {
        bail!(
            "插件目录存在内容清单未覆盖的文件（可能被篡改），拒绝启动: {}",
            unexpected
                .iter()
                .map(|path| path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}
