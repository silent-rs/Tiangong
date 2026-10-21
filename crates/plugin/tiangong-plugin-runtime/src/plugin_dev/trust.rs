//! plugin-dev 的信任登记：受信安装方判定与受信构建指纹。
//!
//! install 的授权对象是「使用 Creator 开发的产物」：调用方插件必须是
//! 宿主注入策略判定的受信创作插件（缺失即 fail-closed），且目标项目
//! 存在经宿主进程内真实构建的指纹登记——堵住前端自报身份冒充安装任意
//! 目录内容的通道。对外路径经 `plugin_dev` 根模块重导出保持稳定。

use std::path::Path;
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
    trusted_build_fingerprint(plugin_id, project_id).ok_or_else(|| {
        anyhow::anyhow!(
            "项目 {project_id} 缺少受信构建登记（先经该插件的 sidecar 完成构建，再安装）"
        )
    })
}
