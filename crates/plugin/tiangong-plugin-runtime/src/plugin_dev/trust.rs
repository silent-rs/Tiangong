//! plugin-dev 的信任登记：受信安装方判定与受信构建指纹。
//!
//! install 的授权对象是「使用 Creator 开发的产物」：调用方插件必须是
//! 宿主注入策略判定的受信创作插件（缺失即 fail-closed），且目标项目
//! 存在经宿主进程内真实构建的指纹登记——堵住前端自报身份冒充安装任意
//! 目录内容的通道。对外路径经 `plugin_dev` 根模块重导出保持稳定。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

/// 处理一次 `plugin-dev.*` 桥接调用（权限校验由 bridge 层完成）。
/// 自动签名安装的调用方身份（判定依据由宿主注入的策略消费）。
pub struct InstallerIdentity {
    pub plugin_id: String,
    /// 调用方插件当前持有官方签名（发布者为官方保留标识，来自宿主侧
    /// 注册表数据，不由前端传入）。
    pub official_signed: bool,
}

/// 受信安装方判定：返回 true 才允许经桥接触发用户密钥自动签名安装。
/// 由宿主启动时注入（策略含具体插件身份，runtime 保持插件中立）。
pub type TrustedInstallerHandler = Arc<dyn Fn(&InstallerIdentity) -> bool + Send + Sync>;

static TRUSTED_INSTALLER: std::sync::RwLock<Option<TrustedInstallerHandler>> =
    std::sync::RwLock::new(None);

/// 注入受信安装方判定（覆盖语义；缺失时 install fail-closed）。
pub fn set_plugin_dev_trusted_installer(handler: TrustedInstallerHandler) {
    if let Ok(mut current) = TRUSTED_INSTALLER.write() {
        *current = Some(handler);
    }
}

/// 清除受信安装方判定（测试隔离用：恢复 fail-closed 初始态）。
#[cfg(test)]
pub(crate) fn clear_plugin_dev_trusted_installer_for_test() {
    if let Ok(mut current) = TRUSTED_INSTALLER.write() {
        *current = None;
    }
}

/// 受信构建登记：宿主观察者登记「受信插件的 sidecar 真实构建产出」的
/// 项目。install 只接受有登记的项目——自动签名的授权对象是「使用 Creator
/// 开发的产物」，产物必须经宿主进程内发起的真实构建，堵住前端自报身份
/// 冒充安装任意目录内容的通道。
type TrustedBuildKey = (String, String);
type TrustedBuildTable = std::collections::HashMap<TrustedBuildKey, String>;

static TRUSTED_BUILDS: std::sync::OnceLock<std::sync::Mutex<TrustedBuildTable>> =
    std::sync::OnceLock::new();

fn trusted_builds() -> &'static std::sync::Mutex<TrustedBuildTable> {
    TRUSTED_BUILDS.get_or_init(|| std::sync::Mutex::new(TrustedBuildTable::new()))
}

/// 计算构建产物内容清单指纹（`<目录>/content-manifest.json` 整体 sha256）。
/// 宿主观察者登记受信构建与 install 核验暂存副本使用同一算法。
pub fn content_manifest_fingerprint(directory: &Path) -> Result<String> {
    let raw = std::fs::read(directory.join(crate::sidecar::CONTENT_MANIFEST_FILE)).with_context(
        || {
            format!(
                "读取构建产物内容清单失败: {}",
                directory
                    .join(crate::sidecar::CONTENT_MANIFEST_FILE)
                    .display()
            )
        },
    )?;
    use sha2::Digest;
    Ok(hex::encode(sha2::Sha256::digest(&raw)))
}

/// 登记一次成功构建的产物指纹（插件 × 项目 → 内容清单整体 sha256）；
/// `manifest_sha256` 为 None 表示撤销登记（构建失败或安装消费后失效）。
///
/// 指纹在观察者侧由「release 目录的 content-manifest.json 整体哈希」计算，
/// install 时与暂存副本逐一比对——授权对象是真实构建出的那份内容，
/// 构建后替换 release/ 无法通过签名安装。
pub fn note_trusted_build(plugin_id: &str, project_id: &str, manifest_sha256: Option<String>) {
    if let Ok(mut builds) = trusted_builds().lock() {
        match manifest_sha256 {
            Some(fingerprint) => {
                builds.insert((plugin_id.to_string(), project_id.to_string()), fingerprint);
            }
            None => {
                builds.remove(&(plugin_id.to_string(), project_id.to_string()));
            }
        }
    }
}

/// 读取项目当前登记的产物指纹（None 表示无有效登记）。
pub fn trusted_build_fingerprint(plugin_id: &str, project_id: &str) -> Option<String> {
    trusted_builds().lock().ok().and_then(|builds| {
        builds
            .get(&(plugin_id.to_string(), project_id.to_string()))
            .cloned()
    })
}

/// install 桥接入口的授权检查：调用方是宿主判定的受信创作插件，且目标
/// 项目存在受信构建登记。任一不满足即拒绝（fail-closed）。
pub(super) fn ensure_install_authorized(
    storage_root: &Path,
    plugin_id: &str,
    project_id: &str,
) -> Result<String> {
    ensure_trusted_installer(storage_root, plugin_id)?;
    trusted_build_fingerprint(plugin_id, project_id).ok_or_else(|| {
        anyhow::anyhow!(
            "项目 {project_id} 缺少受信构建登记（先经该插件的 sidecar 完成构建，再安装）"
        )
    })
}

/// 受信安装方判定（install 与 import 共用）：调用方必须是宿主注入策略
/// 判定的受信创作插件；缺失或拒绝即 fail-closed。
pub(super) fn ensure_trusted_installer(storage_root: &Path, plugin_id: &str) -> Result<()> {
    let official_signed = crate::registry::find_installed_plugin(storage_root, plugin_id)
        .ok()
        .and_then(|installed| installed.signed_release)
        .is_some_and(|release| release.publisher == crate::trust::OFFICIAL_PUBLISHER);
    let handler = TRUSTED_INSTALLER
        .read()
        .ok()
        .and_then(|current| current.clone())
        .ok_or_else(|| anyhow::anyhow!("宿主未接入受信安装方判定，拒绝签名安装（fail-closed）"))?;
    let identity = InstallerIdentity {
        plugin_id: plugin_id.to_string(),
        official_signed,
    };
    if !handler(&identity) {
        bail!("插件 {plugin_id} 无自动签名安装资格（用户密钥签名安装仅限受信创作插件）");
    }
    Ok(())
}

/// 待确认的导入摘要：呈现给用户做安装决策的信息。字段取自**暂存副本**，
/// 而非 inbox 原目录——inbox 是沙箱可写区，确认与导入必须作用于同一份
/// 不可变副本，防止「确认内容 ≠ 安装内容」。
#[derive(Debug, Clone)]
pub struct ImportSummary {
    pub plugin_id: String,
    pub version: String,
    pub name: Option<String>,
    /// 清单声明的权限（安装后即生效的授权面）。
    pub permissions: Vec<String>,
    pub file_count: u64,
    pub total_bytes: u64,
    /// 暂存副本对应的 inbox 来源目录（仅用于展示）。
    pub source: PathBuf,
}

/// 导入确认回调：宿主弹原生对话框展示摘要，返回值表示用户是否批准。
/// 由宿主启动时注入；缺失时 import fail-closed（对齐 TrustedInstallerHandler
/// 模式——runtime 保持插件中立，用户决策发生在宿主侧）。
pub type ImportConfirmHandler = Arc<dyn Fn(&ImportSummary) -> bool + Send + Sync>;

static IMPORT_CONFIRM: std::sync::RwLock<Option<ImportConfirmHandler>> =
    std::sync::RwLock::new(None);

/// 注入导入确认回调（覆盖语义；缺失时 import fail-closed）。
pub fn set_plugin_dev_import_confirm(handler: ImportConfirmHandler) {
    if let Ok(mut current) = IMPORT_CONFIRM.write() {
        *current = Some(handler);
    }
}

/// 清除导入确认回调（测试隔离用：恢复 fail-closed 初始态）。
#[cfg(test)]
pub(crate) fn clear_plugin_dev_import_confirm_for_test() {
    if let Ok(mut current) = IMPORT_CONFIRM.write() {
        *current = None;
    }
}

/// 请求用户确认导入：未注入回调、回调不可用或用户拒绝均返回 Err
/// （fail-closed）。授权依据是用户本人对摘要的批准，调用方插件身份
/// 不构成替代。
pub(super) fn confirm_import(summary: &ImportSummary) -> Result<()> {
    let handler = IMPORT_CONFIRM
        .read()
        .ok()
        .and_then(|current| current.clone())
        .ok_or_else(|| anyhow::anyhow!("宿主未接入导入确认回调，拒绝导入（fail-closed）"))?;
    if !handler(summary) {
        bail!(
            "用户未批准插件 {} 的导入（拒绝或超时均视为拒绝）",
            summary.plugin_id
        );
    }
    Ok(())
}
