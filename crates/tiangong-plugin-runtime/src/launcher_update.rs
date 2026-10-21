//! App 对 Sandbox 自管理安装能力的薄封装。
//!
//! App 不维护版本目录或 active/pending 指针。首次准备与手动更新都复用
//! Sandbox 的 SelfUpdater，把程序和签名直接安装到 storage_root/sandbox。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

#[cfg(windows)]
mod windows_cache;

pub const LAUNCHER_MANIFEST_ENDPOINT: &str = tiangong_sandbox::update::DEFAULT_MANIFEST_URL;
const MANIFEST_URL_OVERRIDE: &str = "TIANGONG_LAUNCHER_MANIFEST_URL";

fn launcher_update_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[derive(Clone, PartialEq, Eq)]
struct LauncherCacheKey {
    path: PathBuf,
    length: u64,
    modified: Option<SystemTime>,
}

fn launcher_check_cache() -> &'static std::sync::Mutex<Option<(LauncherCacheKey, String)>> {
    static CACHE: OnceLock<std::sync::Mutex<Option<(LauncherCacheKey, String)>>> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

fn invalidate_launcher_check() {
    if let Ok(mut cache) = launcher_check_cache().lock() {
        *cache = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxLauncherStatus {
    Missing,
    Preparing,
    Ready,
    Failed,
}
impl SandboxLauncherStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Preparing => "preparing",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
    pub fn sidecars_allowed(self) -> bool {
        self == Self::Ready
    }
}

fn startup_prepare_flags() -> &'static (
    std::sync::atomic::AtomicBool,
    std::sync::Mutex<Option<String>>,
) {
    static FLAGS: OnceLock<(
        std::sync::atomic::AtomicBool,
        std::sync::Mutex<Option<String>>,
    )> = OnceLock::new();
    FLAGS.get_or_init(|| {
        (
            std::sync::atomic::AtomicBool::new(false),
            std::sync::Mutex::new(None),
        )
    })
}
pub fn mark_launcher_preparing(value: bool) {
    startup_prepare_flags()
        .0
        .store(value, std::sync::atomic::Ordering::Release);
    if value {
        invalidate_launcher_check();
    }
}
pub fn record_startup_prepare_failure(value: Option<String>) {
    if let Ok(mut slot) = startup_prepare_flags().1.lock() {
        *slot = value;
    }
}
pub fn startup_prepare_failure_reason() -> Option<String> {
    startup_prepare_flags()
        .1
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
}

pub fn launcher_status(storage_root: &Path) -> (SandboxLauncherStatus, Option<String>) {
    if startup_prepare_flags()
        .0
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return (SandboxLauncherStatus::Preparing, None);
    }
    if let Ok(version) = verified_launcher_version(storage_root) {
        return (SandboxLauncherStatus::Ready, Some(version));
    }
    let status = if startup_prepare_failure_reason().is_some() {
        SandboxLauncherStatus::Failed
    } else {
        SandboxLauncherStatus::Missing
    };
    (status, None)
}

pub struct LauncherUpdater {
    manifest_url: String,
}
impl Default for LauncherUpdater {
    fn default() -> Self {
        Self::new()
    }
}
impl LauncherUpdater {
    pub fn new() -> Self {
        let manifest_url = std::env::var(MANIFEST_URL_OVERRIDE)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| LAUNCHER_MANIFEST_ENDPOINT.to_string());
        Self { manifest_url }
    }
    pub fn with_manifest_url(manifest_url: String) -> Self {
        Self { manifest_url }
    }
    pub async fn install_or_update(&self, storage_root: &Path) -> Result<String> {
        let _guard = launcher_update_lock().lock().await;
        invalidate_launcher_check();
        #[cfg(windows)]
        windows_cache::invalidate(storage_root)?;
        let directory = host_install_directory(storage_root);
        let status = tiangong_sandbox::update::SelfUpdater::new(self.manifest_url.clone())?
            .install_to(&directory)
            .await?;
        let version = match status {
            tiangong_sandbox::update::UpdateStatus::Installed { version, .. }
            | tiangong_sandbox::update::UpdateStatus::Updated { version, .. } => version,
            tiangong_sandbox::update::UpdateStatus::UpToDate { current } => current,
            tiangong_sandbox::update::UpdateStatus::Available { version, .. } => version,
            #[cfg(windows)]
            tiangong_sandbox::update::UpdateStatus::UpdateScheduled { version, .. } => version,
        };
        cleanup_legacy_layout(storage_root);
        invalidate_launcher_check();
        Ok(version)
    }

    /// 启动后台自动更新：仅当本地已有可用 Sandbox 且官方清单版本更新时
    /// 才下载替换。本地缺失或损坏交由启动准备页处理，这里不介入。
    pub async fn update_if_newer(&self, storage_root: &Path) -> Result<AutoUpdateOutcome> {
        let root = storage_root.to_path_buf();
        let installed = tokio::task::spawn_blocking(move || verified_launcher_version(&root))
            .await
            .context("检查本地 Sandbox 版本任务异常")?;
        let Ok(installed) = installed else {
            return Ok(AutoUpdateOutcome::NotInstalled);
        };
        let latest = tiangong_sandbox::update::SelfUpdater::new(self.manifest_url.clone())?
            .latest_version()
            .await?;
        if !is_newer_version(&installed, &latest)? {
            return Ok(AutoUpdateOutcome::UpToDate { current: installed });
        }
        let version = self.install_or_update(storage_root).await?;
        Ok(AutoUpdateOutcome::Updated {
            previous: installed,
            version,
        })
    }
}

/// 启动后台自动更新的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoUpdateOutcome {
    /// 本地无可用 Sandbox，留给启动准备页安装。
    NotInstalled,
    UpToDate {
        current: String,
    },
    Updated {
        previous: String,
        version: String,
    },
}

fn is_newer_version(installed: &str, latest: &str) -> Result<bool> {
    let installed = semver::Version::parse(installed)
        .with_context(|| format!("本地 Sandbox 版本非法: {installed}"))?;
    let latest =
        semver::Version::parse(latest).with_context(|| format!("官方清单版本非法: {latest}"))?;
    Ok(latest > installed)
}

/// 天工宿主的 Sandbox 安装目录：直存布局 `<storage>/sandbox`。
/// P1 通用化后 crate 不再拼接存储根，目录由宿主决定。
fn host_install_directory(storage_root: &Path) -> PathBuf {
    storage_root.join("sandbox")
}
fn cleanup_legacy_layout(storage_root: &Path) {
    let directory = host_install_directory(storage_root);
    for file in ["active", "pending"] {
        let _ = std::fs::remove_file(directory.join(file));
    }
    for child in ["versions", ".transactions"] {
        let _ = std::fs::remove_dir_all(directory.join(child));
    }
}

pub fn launcher_available(storage_root: &Path) -> bool {
    verified_launcher_version(storage_root).is_ok()
}
pub fn installed_version(storage_root: &Path) -> Option<String> {
    verified_launcher_version(storage_root).ok()
}

fn verified_launcher_version(storage_root: &Path) -> Result<String> {
    let Some(launcher) = tiangong_sandbox::launcher_manager::resolve_installed_program(
        &host_install_directory(storage_root),
    ) else {
        bail!("Sandbox 程序不存在");
    };
    crate::signature::verify_launcher_signature(&launcher, storage_root)?;
    let key = launcher_cache_key(&launcher)?;
    let mut cache = launcher_check_cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("Sandbox 自检缓存锁已损坏"))?;
    if let Some((cached_key, version)) = cache.as_ref()
        && cached_key == &key
    {
        return Ok(version.clone());
    }
    #[cfg(windows)]
    let persistent_key = windows_cache::Key::new(&launcher).ok();
    #[cfg(windows)]
    if let Some(fingerprint) = &persistent_key
        && let Some(version) = windows_cache::read(storage_root, fingerprint)
        && launcher_cache_key(&launcher).is_ok_and(|current| current == key)
    {
        tracing::info!("复用本次系统启动内的 Sandbox 完整自检结果");
        *cache = Some((key, version.clone()));
        return Ok(version);
    }
    let (version, fully_checked) = verify_launcher_self_check(&launcher)?;
    #[cfg(not(windows))]
    let _ = fully_checked;
    #[cfg(windows)]
    if fully_checked
        && let Some(key) = persistent_key
        && windows_cache::Key::new(&launcher).is_ok_and(|current| current == key)
        && let Err(error) = windows_cache::save(storage_root, key, &version)
    {
        tracing::warn!(%error, "保存 Sandbox 自检复用记录失败，下次重新检查");
    }
    if launcher_cache_key(&launcher).is_ok_and(|current| current == key) {
        *cache = Some((key, version.clone()));
    }
    Ok(version)
}

fn launcher_cache_key(launcher: &Path) -> Result<LauncherCacheKey> {
    let metadata = std::fs::metadata(launcher)
        .with_context(|| format!("读取 Sandbox 程序元数据失败: {}", launcher.display()))?;
    Ok(LauncherCacheKey {
        path: launcher.to_path_buf(),
        length: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

type CapabilitySet = BTreeSet<tiangong_sandbox::SandboxCapability>;

fn launcher_capability_cache()
-> &'static std::sync::Mutex<Option<(LauncherCacheKey, CapabilitySet)>> {
    static CACHE: OnceLock<std::sync::Mutex<Option<(LauncherCacheKey, CapabilitySet)>>> =
        OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// 查询（已验签的）Launcher 支持的沙箱能力。按程序路径、大小与修改时间
/// 缓存，Launcher 更新后自动重新查询。
pub fn launcher_capabilities(launcher: &Path) -> Result<CapabilitySet> {
    let key = launcher_cache_key(launcher)?;
    if let Ok(cache) = launcher_capability_cache().lock()
        && let Some((cached_key, capabilities)) = cache.as_ref()
        && cached_key == &key
    {
        return Ok(capabilities.clone());
    }
    let mut command = std::process::Command::new(launcher);
    command.arg("--capabilities");
    let output = crate::platform::configure_no_window(&mut command)
        .output()
        .with_context(|| format!("查询 Sandbox 能力失败: {}", launcher.display()))?;
    // 不认识 `--capabilities` 的旧 Launcher 会把它当作启动请求处理并以
    // 非零码退出（或输出无法解析的内容），统一视为"未上报任何能力"。
    let capabilities = if output.status.success() {
        parse_reported_capabilities(&output.stdout)
    } else {
        CapabilitySet::new()
    };
    if let Ok(mut cache) = launcher_capability_cache().lock() {
        *cache = Some((key, capabilities.clone()));
    }
    Ok(capabilities)
}

/// 解析能力报告。未知能力名（更新的 Launcher）忽略：宿主不会授予它不认识的能力。
fn parse_reported_capabilities(stdout: &[u8]) -> CapabilitySet {
    let Ok(report) = serde_json::from_slice::<serde_json::Value>(stdout) else {
        return CapabilitySet::new();
    };
    report["capabilities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| serde_json::from_value(value.clone()).ok())
        .collect()
}

/// 授予但 Launcher 未上报支持的能力。
pub fn missing_capabilities(granted: &CapabilitySet, supported: &CapabilitySet) -> CapabilitySet {
    granted.difference(supported).copied().collect()
}

/// 能力缺失时给用户与日志的提示。
pub fn missing_capabilities_message(plugin_id: &str, missing: &CapabilitySet) -> String {
    let names = missing
        .iter()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "插件 {plugin_id} 需要的沙箱能力（{names}）当前沙箱组件不支持，需要更新沙箱组件：请在设置中更新 Sandbox 后重试"
    )
}
fn verify_launcher_self_check(binary: &Path) -> Result<(String, bool)> {
    let mut command = std::process::Command::new(binary);
    command.arg("--self-check");
    let output = crate::platform::configure_no_window(&mut command)
        .output()
        .with_context(|| format!("运行 Sandbox 自检失败: {}", binary.display()))?;
    if !output.status.success() && output.status.code() != Some(79) {
        bail!(
            "Sandbox 自检失败（退出码 {}）：{}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("解析 Sandbox 自检报告失败")?;
    if report["protocol_version"].as_u64()
        != Some(u64::from(tiangong_sandbox::LAUNCHER_PROTOCOL_VERSION))
        || report["policy_schema"].as_u64()
            != Some(u64::from(tiangong_sandbox::LAUNCHER_POLICY_SCHEMA))
    {
        bail!("Sandbox 自报协议或策略版本与宿主不兼容");
    }
    let version = report["product_version"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .context("Sandbox 自检报告缺少产品版本")?;
    Ok((version, output.status.success()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reported_capabilities_parse_and_diff() {
        use tiangong_sandbox::SandboxCapability as C;
        let supported = parse_reported_capabilities(
            br#"{"capabilities":["credential_services","browser_runtime","future_capability"]}"#,
        );
        // 未知能力名（更新的 Launcher）被忽略。
        assert_eq!(
            supported,
            [C::CredentialServices, C::BrowserRuntime]
                .into_iter()
                .collect()
        );
        // 旧 Launcher：缺字段或输出无法解析，视为不支持任何能力。
        assert!(parse_reported_capabilities(br#"{"policy_schema":2}"#).is_empty());
        assert!(parse_reported_capabilities(b"not json").is_empty());

        let granted: CapabilitySet = [C::AudioInput, C::BrowserRuntime].into_iter().collect();
        let missing = missing_capabilities(&granted, &supported);
        assert_eq!(missing, [C::AudioInput].into_iter().collect());
        assert!(missing_capabilities(&CapabilitySet::new(), &CapabilitySet::new()).is_empty());
        let message = missing_capabilities_message("volcengine", &missing);
        assert!(message.contains("audio_input") && message.contains("更新沙箱组件"));
    }

    #[test]
    fn newer_version_compares_semver() {
        assert!(is_newer_version("0.1.9", "0.1.10").unwrap());
        assert!(!is_newer_version("0.2.0", "0.2.0").unwrap());
        assert!(!is_newer_version("0.3.0", "0.2.9").unwrap());
        assert!(is_newer_version("not-a-version", "0.1.0").is_err());
    }

    #[test]
    fn cleanup_removes_legacy_layout_only() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = host_install_directory(root.path());
        std::fs::create_dir_all(sandbox.join("versions/0.1.0")).unwrap();
        std::fs::create_dir_all(sandbox.join(".transactions/a")).unwrap();
        std::fs::write(sandbox.join("active"), "0.1.0").unwrap();
        let program = tiangong_sandbox::launcher_manager::installed_program(
            &host_install_directory(root.path()),
        );
        std::fs::write(&program, b"program").unwrap();
        cleanup_legacy_layout(root.path());
        assert!(program.is_file());
        assert!(!sandbox.join("versions").exists());
        assert!(!sandbox.join("active").exists());
    }
}
