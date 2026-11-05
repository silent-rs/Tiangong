//! sidecar 安装验证记录：宿主在导入、安装与升级阶段完成完整验证后
//! 生成的权威能力快照。
//!
//! 运行时 Handler 路由只消费这里保存的验证结果，不在工具调用热路径
//! 临时启动 sidecar 探测能力。记录由宿主创建和维护，存放在插件目录
//! 之外的宿主管理目录（`plugins/.verifications/`），插件自身无法通过
//! 修改发布内容伪造或篡改。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::manifest::PluginManifest;
use crate::protocol::PROTOCOL_VERSION;

/// 验证记录目录（位于插件根目录下，与插件安装目录平级）。
const VERIFICATIONS_DIR: &str = ".verifications";
/// 计算制品摘要时排除的宿主管理项：运行期目录、启停标记与遗留本地
/// 信任锚不属于插件发布内容。
const DIGEST_EXCLUDED: [&str; 5] = ["runtime", "logs", "data", ".disabled", "local-trust.json"];

/// 已安装插件 sidecar 的宿主验证记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SidecarVerification {
    pub plugin_id: String,
    pub plugin_version: String,
    /// 插件完整制品摘要（受管文件树），文件被替换但版本不变时失效。
    pub artifact_digest: String,
    /// 验证时 sidecar 声明的通用通信协议版本。
    pub protocol_version: String,
    /// 完整验证采集到的 sidecar 能力（`tool:<name>` / `tool:*` 等）。
    pub capabilities: Vec<String>,
    pub verified_at: String,
}

/// 验证记录文件路径（由插件安装目录推导，插件目录的父目录即插件根）。
pub(crate) fn verification_path(plugin_directory: &Path) -> Option<PathBuf> {
    let file_name = verification_file_name(plugin_directory)?;
    plugin_directory
        .parent()
        .map(|root| root.join(VERIFICATIONS_DIR).join(file_name))
}

fn verification_file_name(plugin_directory: &Path) -> Option<String> {
    plugin_directory
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("{name}.json"))
}

/// 计算插件目录受管文件树的聚合摘要。
///
/// 排除运行期目录与宿主标记后，按相对路径排序对每个文件的 sha256
/// 再次聚合哈希；符号链接一律拒绝。
pub(crate) fn artifact_digest(plugin_directory: &Path) -> Result<String> {
    let mut files = BTreeMap::new();
    let mut stack = vec![plugin_directory.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("读取插件目录失败: {}", directory.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if directory == plugin_directory && DIGEST_EXCLUDED.contains(&name.as_ref()) {
                continue;
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .with_context(|| format!("读取插件文件信息失败: {}", path.display()))?;
            if metadata.file_type().is_symlink() {
                bail!("插件目录包含符号链接，拒绝计算摘要: {}", path.display());
            }
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(plugin_directory)
                .context("插件文件相对路径推算失败")?
                .to_string_lossy()
                .replace('\\', "/");
            let raw = std::fs::read(&path)
                .with_context(|| format!("读取插件文件失败: {}", path.display()))?;
            files.insert(relative, hex::encode(Sha256::digest(raw)));
        }
    }
    let canonical = serde_json::to_string(&files).context("序列化插件制品清单失败")?;
    Ok(hex::encode(Sha256::digest(canonical)))
}

/// 读取并校验验证记录，有效时返回已验证能力列表。
///
/// 记录必须同时满足：插件 ID、插件版本、制品摘要与当前 Runtime 支持
/// 的协议版本全部一致；任一不满足即视为缺失（触发回退或补验证）。
pub(crate) fn load_valid_capabilities(
    plugin_directory: &Path,
    manifest: &PluginManifest,
) -> Option<Vec<String>> {
    let path = verification_path(plugin_directory)?;
    let raw = std::fs::read(&path).ok()?;
    let record: SidecarVerification = serde_json::from_slice(&raw).ok()?;
    if !verification_is_valid(&record, manifest, plugin_directory) {
        return None;
    }
    Some(record.capabilities)
}

/// 校验验证记录与当前制品是否匹配（不读文件系统时的快速路径除外，
/// 制品摘要始终现场重算，防止只比版本号漏掉同版本替换）。
fn verification_is_valid(
    record: &SidecarVerification,
    manifest: &PluginManifest,
    plugin_directory: &Path,
) -> bool {
    record.plugin_id == manifest.id
        && record.plugin_version == manifest.version
        && record.protocol_version == PROTOCOL_VERSION
        && artifact_digest(plugin_directory).is_ok_and(|digest| digest == record.artifact_digest)
}

/// 保存验证记录（宿主管理目录，随安装事务提交后调用）。
///
/// 原子落盘：同目录临时文件写全量内容并落盘后 rename 替换——升级
/// 保存中途失败（磁盘满/IO 错误/写入中断）不会截断既有记录，回滚到
/// 旧插件后旧记录依然完整有效。
pub(crate) fn save_verification(
    plugin_directory: &Path,
    record: &SidecarVerification,
) -> Result<()> {
    let path = verification_path(plugin_directory)
        .ok_or_else(|| anyhow::anyhow!("无法推导插件验证记录路径"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建验证记录目录失败: {}", parent.display()))?;
    }
    let raw = serde_json::to_vec_pretty(record).context("序列化验证记录失败")?;
    // 临时文件与正式文件同目录（rename 不跨文件系统），随机编号避免
    // 并发保存互相覆盖临时文件。
    let temp_path = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "verification".to_string()),
        scru128::new()
    ));
    let write_result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&temp_path)
            .with_context(|| format!("创建验证记录临时文件失败: {}", temp_path.display()))?;
        std::io::Write::write_all(&mut file, &raw)
            .with_context(|| format!("写入验证记录临时文件失败: {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("落盘验证记录临时文件失败: {}", temp_path.display()))?;
        // Windows ReplaceFileW 需要重新打开替换文件；先释放写入句柄。
        drop(file);
        replace_verification_file(&temp_path, &path).with_context(|| {
            format!(
                "替换验证记录失败: {} -> {}",
                temp_path.display(),
                path.display()
            )
        })
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    write_result
}

/// 用临时文件原子替换正式记录：Unix rename 即原子覆盖；Windows 走
/// ReplaceFileW（失败保留原文件，语义与项目内 Launcher 更新的替换
/// 模式一致）。
fn replace_verification_file(from: &Path, to: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::rename(from, to)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        replace_file_windows(from, to)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (from, to);
        bail!("当前平台不支持验证记录替换")
    }
}

#[cfg(windows)]
fn replace_file_windows(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};
    if !to.exists() {
        std::fs::rename(from, to)?;
        return Ok(());
    }
    let from_wide = from
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let to_wide = to.as_os_str().encode_wide().chain([0]).collect::<Vec<_>>();
    if unsafe {
        ReplaceFileW(
            to_wide.as_ptr(),
            from_wide.as_ptr(),
            std::ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(anyhow::anyhow!(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// 删除验证记录（卸载与回滚时清理）。
pub(crate) fn remove_verification(plugin_directory: &Path) {
    if let Some(path) = verification_path(plugin_directory)
        && let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), %error, "删除插件验证记录失败");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str, version: &str) -> PluginManifest {
        PluginManifest {
            name: None,
            description: None,
            id: id.into(),
            version: version.into(),
            sidecar: None,
            permissions: vec![],
            entrypoints: None,
            model_requirements: None,
            storage_access: false,
            capabilities: None,
            ui: None,
            tools: None,
            prompt: None,
            resources: None,
            mention: None,
            tool_icons: None,
            tool_titles: None,
        }
    }

    #[test]
    fn artifact_digest_excludes_host_managed_entries() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("demo");
        std::fs::create_dir_all(plugin.join("sidecar")).unwrap();
        std::fs::write(plugin.join("plugin.json"), "{}").unwrap();
        std::fs::write(plugin.join("sidecar/main.mjs"), "entry").unwrap();
        std::fs::create_dir_all(plugin.join("logs")).unwrap();
        std::fs::write(plugin.join("logs/sidecar.log"), "noise").unwrap();
        std::fs::create_dir_all(plugin.join("runtime")).unwrap();
        std::fs::write(plugin.join("runtime/endpoint.json"), "noise").unwrap();
        std::fs::create_dir_all(plugin.join("data")).unwrap();
        std::fs::write(plugin.join(".disabled"), "").unwrap();

        let digest = artifact_digest(&plugin).unwrap();
        // 运行期目录与宿主标记变化不影响摘要。
        std::fs::write(plugin.join("logs/sidecar.log"), "changed").unwrap();
        std::fs::remove_file(plugin.join(".disabled")).unwrap();
        assert_eq!(artifact_digest(&plugin).unwrap(), digest);
        // 受管制品内容变化必须改变摘要（同版本替换检测）。
        std::fs::write(plugin.join("sidecar/main.mjs"), "tampered").unwrap();
        assert_ne!(artifact_digest(&plugin).unwrap(), digest);
    }

    #[test]
    fn verification_record_validity_follows_id_version_digest_and_protocol() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("demo");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("plugin.json"), "{}").unwrap();
        let record = SidecarVerification {
            plugin_id: "demo".into(),
            plugin_version: "0.1.0".into(),
            artifact_digest: artifact_digest(&plugin).unwrap(),
            protocol_version: PROTOCOL_VERSION.into(),
            capabilities: vec!["tool:demo".into()],
            verified_at: "2026-09-03 12:00:00".into(),
        };
        save_verification(&plugin, &record).unwrap();
        // 补验证必须能覆盖已有记录（Windows 不能持写入句柄替换文件）。
        save_verification(&plugin, &record).unwrap();
        let stored_path = verification_path(&plugin).unwrap();
        assert_eq!(
            stored_path,
            root.path().join(".verifications").join("demo.json")
        );

        assert_eq!(
            load_valid_capabilities(&plugin, &manifest("demo", "0.1.0")).as_deref(),
            Some(["tool:demo".to_string()].as_slice()),
            "记录与制品完全匹配时应有效"
        );
        assert!(
            load_valid_capabilities(&plugin, &manifest("demo", "0.2.0")).is_none(),
            "版本不匹配时记录失效"
        );
        assert!(
            load_valid_capabilities(&plugin, &manifest("other", "0.1.0")).is_none(),
            "插件 ID 不匹配时记录失效"
        );
        std::fs::write(plugin.join("plugin.json"), "{\"changed\":true}").unwrap();
        assert!(
            load_valid_capabilities(&plugin, &manifest("demo", "0.1.0")).is_none(),
            "制品摘要变化（同版本替换）时记录失效"
        );

        remove_verification(&plugin);
        assert!(
            load_valid_capabilities(&plugin, &manifest("demo", "0.1.0")).is_none(),
            "删除记录后应失效"
        );
        assert!(!stored_path.exists());
    }

    #[test]
    fn record_with_future_protocol_version_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("demo");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("plugin.json"), "{}").unwrap();
        let record = SidecarVerification {
            plugin_id: "demo".into(),
            plugin_version: "0.1.0".into(),
            artifact_digest: artifact_digest(&plugin).unwrap(),
            protocol_version: "999.0.0".into(),
            capabilities: vec![],
            verified_at: String::new(),
        };
        save_verification(&plugin, &record).unwrap();
        assert!(
            load_valid_capabilities(&plugin, &manifest("demo", "0.1.0")).is_none(),
            "当前 Runtime 不支持的协议版本应失效"
        );
    }

    /// 原子保存语义：保存新记录失败（目录只读）时旧记录必须原样完整，
    /// 不得被截断或半写——升级回滚后旧插件仍能按旧记录通过校验。
    #[test]
    #[cfg(unix)]
    fn failed_save_keeps_existing_record_intact() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let plugin = root.path().join("demo");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("plugin.json"), "{}").unwrap();
        let record = SidecarVerification {
            plugin_id: "demo".into(),
            plugin_version: "0.1.0".into(),
            artifact_digest: artifact_digest(&plugin).unwrap(),
            protocol_version: PROTOCOL_VERSION.into(),
            capabilities: vec!["tool:demo".into()],
            verified_at: "2026-09-03 12:00:00".into(),
        };
        save_verification(&plugin, &record).unwrap();
        let before = std::fs::read(verification_path(&plugin).unwrap()).unwrap();

        // 目录只读后保存新版本记录：临时文件创建失败，旧记录不得被碰。
        let verifications = root.path().join(".verifications");
        std::fs::set_permissions(&verifications, std::fs::Permissions::from_mode(0o555)).unwrap();
        let updated = SidecarVerification {
            plugin_version: "0.2.0".into(),
            ..record.clone()
        };
        assert!(save_verification(&plugin, &updated).is_err());
        std::fs::set_permissions(&verifications, std::fs::Permissions::from_mode(0o755)).unwrap();

        let after = std::fs::read(verification_path(&plugin).unwrap()).unwrap();
        assert_eq!(before, after, "保存失败不得改动既有记录");
        assert!(
            load_valid_capabilities(&plugin, &manifest("demo", "0.1.0")).is_some(),
            "旧记录在失败的保存后依然有效"
        );
        // 失败保存不留临时文件残骸。
        let leftovers: Vec<_> = std::fs::read_dir(&verifications)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(
            leftovers.is_empty(),
            "失败保存应清理临时文件: {leftovers:?}"
        );
    }
}
