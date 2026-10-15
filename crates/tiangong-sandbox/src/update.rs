//! Sandbox Launcher 自管理在线升级。
//!
//! - `check-update`：只比较当前进程版本与官方清单，不写磁盘；
//! - `update`：下载并替换当前正在执行的 Sandbox；
//! - `update --root <目录>`：把官方最新版安装为指定目录下的标准文件名。
//!
//! Unix 可通过同目录原子重命名替换正在运行的文件。Windows 不能覆盖
//! 运行中的 image：当前进程启动已验签的新候选作为完成器，完成器等待
//! 旧进程退出后复制到目标临时文件，再执行替换。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use futures_util::StreamExt as _;
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use sha2::{Digest, Sha256};

fn launcher_file_name() -> String {
    format!("tiangong-sandbox{}", std::env::consts::EXE_SUFFIX)
}

fn signature_path(launcher: &Path) -> PathBuf {
    launcher.with_file_name(format!(
        "{}.sig",
        launcher
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("tiangong-sandbox")
    ))
}

pub const DEFAULT_MANIFEST_URL: &str =
    "https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/sandbox/latest.json";
const MAX_ARTIFACT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 1024 * 1024;
const OFFICIAL_PUBKEY_B64: &str = include_str!("official-pubkey.b64");
const UPDATE_DIR_PREFIX: &str = ".tiangong-sandbox-update-";

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct UpdateManifest {
    pub version: String,
    pub protocol_version: u32,
    pub policy_schema_max: u32,
    pub platforms: std::collections::BTreeMap<String, PlatformArtifact>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PlatformArtifact {
    pub url: String,
    pub checksum: String,
    pub signature_url: String,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum UpdateStatus {
    UpToDate {
        current: String,
    },
    Available {
        current: String,
        version: String,
    },
    Updated {
        previous: String,
        version: String,
        path: PathBuf,
    },
    Installed {
        version: String,
        path: PathBuf,
    },
    #[cfg(windows)]
    UpdateScheduled {
        previous: String,
        version: String,
        path: PathBuf,
    },
}

/// 构建更新客户端；`no_proxy` 为 true 时禁用代理（含环境变量代理）。
fn update_client(no_proxy: bool) -> reqwest::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(format!("tiangong-sandbox/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300));
    if no_proxy {
        builder = builder.no_proxy();
    }
    builder.build()
}

pub struct SelfUpdater {
    client: reqwest::Client,
    /// 禁用代理的客户端：环境变量代理不可达（如 VPN 已关闭）时直连重试（#662）。
    direct_client: reqwest::Client,
    manifest_url: String,
    official_pubkey_b64: String,
}

impl SelfUpdater {
    pub fn new(manifest_url: impl Into<String>) -> Result<Self> {
        Self::with_public_key(manifest_url, OFFICIAL_PUBKEY_B64)
    }

    /// 使用调用方显式选择的更新端点和信任根。不会读取环境变量。
    pub fn with_public_key(
        manifest_url: impl Into<String>,
        public_key_b64: impl Into<String>,
    ) -> Result<Self> {
        let manifest_url = manifest_url.into();
        validate_https_url(&manifest_url, "更新清单")?;
        let client = update_client(false).context("构建 Sandbox 更新客户端失败")?;
        let direct_client = update_client(true).context("构建 Sandbox 更新客户端失败")?;
        Ok(Self {
            client,
            direct_client,
            manifest_url,
            official_pubkey_b64: public_key_b64.into().trim().to_string(),
        })
    }

    #[cfg(all(test, not(windows)))]
    fn with_test_pubkey(manifest_url: impl Into<String>, pubkey_b64: String) -> Result<Self> {
        Self::with_public_key(manifest_url, pubkey_b64)
    }

    /// 只检查官方更新，不读写安装目录。
    pub async fn check(&self) -> Result<UpdateStatus> {
        let manifest = self.fetch_valid_manifest().await?;
        let current = env!("CARGO_PKG_VERSION").to_string();
        if Version::parse(&manifest.version)? <= Version::parse(&current)? {
            return Ok(UpdateStatus::UpToDate { current });
        }
        Ok(UpdateStatus::Available {
            current,
            version: manifest.version,
        })
    }

    /// 拉取并校验官方清单，返回其中的最新版本号；不读写磁盘。
    pub async fn latest_version(&self) -> Result<String> {
        Ok(self.fetch_valid_manifest().await?.version)
    }

    /// 更新当前正在执行的 Sandbox 自身。
    pub async fn update_current(&self) -> Result<UpdateStatus> {
        let target = std::env::current_exe().context("定位当前 Sandbox 可执行文件失败")?;
        ensure_regular_file(&target, "当前 Sandbox")?;
        self.update_target(&target, UpdateMode::Current).await
    }

    /// 把官方最新版安装到指定目录；目标名固定为 tiangong-sandbox[.exe]。
    pub async fn install_to(&self, directory: &Path) -> Result<UpdateStatus> {
        ensure_install_directory(directory)?;
        let target = directory.join(launcher_file_name());
        self.update_target(&target, UpdateMode::Install).await
    }

    async fn update_target(&self, target: &Path, mode: UpdateMode) -> Result<UpdateStatus> {
        let parent = target.parent().context("Sandbox 目标缺少父目录")?;
        ensure_install_directory(parent)?;
        let _lock = UpdateLock::acquire(parent)?;
        cleanup_stale_transactions(parent);

        let manifest = self.fetch_valid_manifest().await?;
        let running = Version::parse(env!("CARGO_PKG_VERSION"))?;
        let candidate = Version::parse(&manifest.version)?;
        if mode == UpdateMode::Current && candidate <= running {
            return Ok(UpdateStatus::UpToDate {
                current: running.to_string(),
            });
        }

        let platform = current_platform_key();
        let artifact = manifest
            .platforms
            .get(&platform)
            .with_context(|| format!("更新清单缺少当前平台: {platform}"))?;
        validate_https_url(&artifact.url, "Sandbox 制品")?;
        validate_https_url(&artifact.signature_url, "Sandbox 签名")?;

        let transaction = parent.join(format!("{UPDATE_DIR_PREFIX}{}", scru128::new()));
        std::fs::create_dir(&transaction)
            .with_context(|| format!("创建更新事务目录失败: {}", transaction.display()))?;
        let cleanup = TransactionCleanup::new(transaction.clone());
        let candidate_binary = transaction.join(launcher_file_name());
        let candidate_signature = signature_path(&candidate_binary);
        self.download_verified(&artifact.url, &artifact.checksum, &candidate_binary)
            .await?;
        self.download_limited(
            &artifact.signature_url,
            &candidate_signature,
            MAX_SIGNATURE_BYTES,
        )
        .await?;
        set_executable(&candidate_binary)?;
        verify_signature(
            &candidate_binary,
            &candidate_signature,
            &self.official_pubkey_b64,
        )?;
        verify_candidate_self_check(&candidate_binary, &manifest)?;

        let target_signature = signature_path(target);
        if target.is_file()
            && target_signature.is_file()
            && let Ok(installed) =
                trusted_target_version(target, &target_signature, &self.official_pubkey_b64)
            && candidate < installed
        {
            bail!(
                "拒绝降级指定目录中的 Sandbox: installed={}, candidate={}",
                installed,
                candidate
            );
        }
        match mode {
            UpdateMode::Install => {
                install_verified_pair(
                    &candidate_binary,
                    &candidate_signature,
                    target,
                    &target_signature,
                )?;
                cleanup.disarm();
                Ok(UpdateStatus::Installed {
                    version: manifest.version,
                    path: target.to_path_buf(),
                })
            }
            UpdateMode::Current => {
                #[cfg(unix)]
                {
                    install_verified_pair(
                        &candidate_binary,
                        &candidate_signature,
                        target,
                        &target_signature,
                    )?;
                    cleanup.disarm();
                    Ok(UpdateStatus::Updated {
                        previous: running.to_string(),
                        version: manifest.version,
                        path: target.to_path_buf(),
                    })
                }
                #[cfg(windows)]
                {
                    spawn_windows_update_completer(
                        &candidate_binary,
                        &candidate_signature,
                        target,
                        &target_signature,
                        &transaction,
                    )?;
                    cleanup.disarm();
                    Ok(UpdateStatus::UpdateScheduled {
                        previous: running.to_string(),
                        version: manifest.version,
                        path: target.to_path_buf(),
                    })
                }
                #[cfg(not(any(unix, windows)))]
                {
                    let _ = (
                        candidate_binary,
                        candidate_signature,
                        target_signature,
                        cleanup,
                    );
                    bail!("当前平台不支持 Sandbox 自更新")
                }
            }
        }
    }

    async fn fetch_valid_manifest(&self) -> Result<UpdateManifest> {
        let response = self
            .get(&self.manifest_url)
            .await
            .with_context(|| format!("拉取 Sandbox 更新清单失败: {}", self.manifest_url))?;
        if !response.status().is_success() {
            bail!("Sandbox 更新清单响应异常: {}", response.status());
        }
        let manifest: UpdateManifest = response.json().await.context("解析更新清单失败")?;
        validate_manifest(&manifest)?;
        Ok(manifest)
    }

    /// GET 请求；连接阶段失败（如环境变量代理不可达）时以直连客户端重试一次（#662）。
    ///
    /// 连接失败时请求尚未送达服务端，重试不会造成重复下载；HTTP 状态等其他错误不重试。
    async fn get(&self, url: &str) -> reqwest::Result<reqwest::Response> {
        match self.client.get(url).send().await {
            Err(error) if error.is_connect() => self.direct_client.get(url).send().await,
            result => result,
        }
    }

    async fn download_verified(&self, url: &str, checksum: &str, path: &Path) -> Result<()> {
        let expected = parse_checksum(checksum)?;
        self.download_limited(url, path, MAX_ARTIFACT_BYTES).await?;
        let actual = Sha256::digest(std::fs::read(path)?);
        if actual.as_slice() != expected.as_slice() {
            bail!(
                "Sandbox 制品校验和不匹配: expected={}, actual={}",
                hex::encode(expected),
                hex::encode(actual)
            );
        }
        Ok(())
    }

    async fn download_limited(&self, url: &str, path: &Path, limit: u64) -> Result<()> {
        let response = self.get(url).await?;
        if !response.status().is_success() {
            bail!("下载响应异常: {} {url}", response.status());
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit)
        {
            bail!("下载内容超过大小上限: {url}");
        }
        let mut stream = response.bytes_stream();
        let mut file = std::fs::File::create(path)?;
        let mut total = 0_u64;
        use std::io::Write as _;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            total = total.saturating_add(chunk.len() as u64);
            if total > limit {
                bail!("下载内容超过大小上限: {url}");
            }
            file.write_all(&chunk)?;
        }
        file.sync_all()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateMode {
    Current,
    Install,
}

struct UpdateLock(std::fs::File);
impl UpdateLock {
    fn acquire(directory: &Path) -> Result<Self> {
        let path = directory.join(".tiangong-sandbox-update.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
            anyhow!("另一个 Sandbox 更新正在执行（{}）：{error}", path.display())
        })?;
        Ok(Self(file))
    }

    #[cfg(windows)]
    fn acquire_wait(directory: &Path) -> Result<Self> {
        let path = directory.join(".tiangong-sandbox-update.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(Self(file))
    }
}
impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

struct TransactionCleanup(Option<PathBuf>);
impl TransactionCleanup {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }
    fn disarm(mut self) {
        self.0 = None;
    }
}
impl Drop for TransactionCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn validate_manifest(manifest: &UpdateManifest) -> Result<()> {
    Version::parse(&manifest.version).context("更新清单版本非法")?;
    if manifest.protocol_version != crate::LAUNCHER_PROTOCOL_VERSION {
        bail!(
            "Sandbox 协议不兼容: expected={}, actual={}",
            crate::LAUNCHER_PROTOCOL_VERSION,
            manifest.protocol_version
        );
    }
    if manifest.policy_schema_max != crate::LAUNCHER_POLICY_SCHEMA {
        bail!(
            "Sandbox 策略 Schema 不兼容: expected={}, actual={}",
            crate::LAUNCHER_POLICY_SCHEMA,
            manifest.policy_schema_max
        );
    }
    if manifest.platforms.is_empty() {
        bail!("更新清单缺少平台制品");
    }
    Ok(())
}

fn verify_signature(binary: &Path, signature: &Path, pubkey_b64: &str) -> Result<()> {
    ensure_regular_file(binary, "Sandbox 制品")?;
    ensure_regular_file(signature, "Sandbox 签名")?;
    let public_text = base64::engine::general_purpose::STANDARD.decode(pubkey_b64.trim())?;
    let public_text = String::from_utf8(public_text)?;
    let public = PublicKey::decode(&public_text).context("解析 Sandbox 官方公钥失败")?;
    let signature_text = base64::engine::general_purpose::STANDARD
        .decode(std::fs::read_to_string(signature)?.trim())?;
    let signature_text = String::from_utf8(signature_text)?;
    let signature = Signature::decode(&signature_text).context("解析 Sandbox 签名失败")?;
    public
        .verify(&std::fs::read(binary)?, &signature, false)
        .context("Sandbox 官方签名验证不通过")
}

fn verify_candidate_self_check(binary: &Path, manifest: &UpdateManifest) -> Result<()> {
    let mut command = std::process::Command::new(binary);
    command.arg("--self-check");
    let output = tiangong_toolkit::configure_no_window(&mut command)
        .output()
        .with_context(|| format!("运行候选 Sandbox 自检失败: {}", binary.display()))?;
    if !output.status.success() && output.status.code() != Some(79) {
        bail!(
            "候选 Sandbox 自检失败（退出码 {}）: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    if report["product_version"].as_str() != Some(&manifest.version)
        || report["protocol_version"].as_u64() != Some(u64::from(manifest.protocol_version))
        || report["policy_schema"].as_u64() != Some(u64::from(manifest.policy_schema_max))
    {
        bail!("候选 Sandbox 自报版本、协议或策略 Schema 与清单不一致");
    }
    Ok(())
}

fn trusted_target_version(binary: &Path, signature: &Path, pubkey_b64: &str) -> Result<Version> {
    verify_signature(binary, signature, pubkey_b64)?;
    let mut command = std::process::Command::new(binary);
    command.arg("--self-check");
    let output = tiangong_toolkit::configure_no_window(&mut command).output()?;
    if !output.status.success() && output.status.code() != Some(79) {
        bail!("已安装 Sandbox 自检失败");
    }
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    if report["protocol_version"].as_u64() != Some(u64::from(crate::LAUNCHER_PROTOCOL_VERSION))
        || report["policy_schema"].as_u64() != Some(u64::from(crate::LAUNCHER_POLICY_SCHEMA))
    {
        bail!("已安装 Sandbox 协议或策略 Schema 不兼容");
    }
    Version::parse(
        report["product_version"]
            .as_str()
            .context("已安装 Sandbox 自检缺少版本")?,
    )
    .context("已安装 Sandbox 自报版本非法")
}

fn install_verified_pair(
    candidate_binary: &Path,
    candidate_signature: &Path,
    target: &Path,
    target_signature: &Path,
) -> Result<()> {
    let parent = target.parent().context("Sandbox 目标缺少父目录")?;
    ensure_install_directory(parent)?;
    let id = scru128::new().to_string();
    let binary_temp = parent.join(format!(".tiangong-sandbox.{id}.new"));
    let signature_temp = parent.join(format!(".tiangong-sandbox.{id}.sig.new"));
    std::fs::copy(candidate_binary, &binary_temp)?;
    std::fs::copy(candidate_signature, &signature_temp)?;
    set_executable(&binary_temp)?;

    let binary_backup = parent.join(format!(".tiangong-sandbox.{id}.old"));
    let signature_backup = parent.join(format!(".tiangong-sandbox.{id}.sig.old"));
    let target_metadata = std::fs::symlink_metadata(target).ok();
    let signature_metadata = std::fs::symlink_metadata(target_signature).ok();
    let had_binary = target_metadata.as_ref().is_some_and(|meta| meta.is_file());
    let had_signature = signature_metadata
        .as_ref()
        .is_some_and(|meta| meta.is_file());
    if target_metadata.is_some() && !had_binary {
        bail!("Sandbox 目标不是普通文件: {}", target.display());
    }
    if signature_metadata.is_some() && !had_signature {
        bail!(
            "Sandbox 目标签名不是普通文件: {}",
            target_signature.display()
        );
    }
    // 备份使用复制，不先移走现有目标：程序路径在最终提交前始终可用。
    if had_binary {
        std::fs::copy(target, &binary_backup)?;
    }
    if had_signature {
        std::fs::copy(target_signature, &signature_backup)?;
    }

    let result: Result<()> = (|| {
        // 签名先就位，程序是最终提交点。
        replace_or_rename(&signature_temp, target_signature)?;
        replace_or_rename(&binary_temp, target)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(target);
        let _ = std::fs::remove_file(target_signature);
        if had_signature {
            let _ = replace_or_rename(&signature_backup, target_signature);
        }
        if had_binary {
            let _ = replace_or_rename(&binary_backup, target);
        }
        let _ = std::fs::remove_file(&binary_temp);
        let _ = std::fs::remove_file(&signature_temp);
        return Err(error).context("替换 Sandbox 程序与签名失败，已尝试恢复旧版本");
    }
    let _ = std::fs::remove_file(binary_backup);
    let _ = std::fs::remove_file(signature_backup);
    Ok(())
}

fn replace_or_rename(from: &Path, to: &Path) -> Result<()> {
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
        bail!("当前平台不支持 Sandbox 文件替换")
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
        return Err(anyhow!(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(windows)]
fn spawn_windows_update_completer(
    candidate_binary: &Path,
    candidate_signature: &Path,
    target: &Path,
    target_signature: &Path,
    transaction: &Path,
) -> Result<()> {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(candidate_binary)
        .arg("complete-update")
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--source")
        .arg(candidate_binary)
        .arg("--signature-source")
        .arg(candidate_signature)
        .arg("--target")
        .arg(target)
        .arg("--signature-target")
        .arg(target_signature)
        .arg("--cleanup")
        .arg(transaction)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("启动 Windows Sandbox 更新完成器失败")?;
    Ok(())
}

#[cfg(windows)]
pub fn complete_windows_update(
    parent_pid: u32,
    source: &Path,
    signature_source: &Path,
    target: &Path,
    signature_target: &Path,
    cleanup: &Path,
) -> Result<()> {
    let current = std::fs::canonicalize(std::env::current_exe()?)?;
    let source = std::fs::canonicalize(source)?;
    let expected_name = launcher_file_name();
    if source != current {
        bail!("Windows 更新完成器 source 必须是当前已验签候选进程");
    }
    if !target.is_absolute()
        || target.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str())
        || signature_source != signature_path(&source)
        || signature_target != signature_path(target)
        || cleanup != source.parent().context("候选 Sandbox 缺少事务目录")?
    {
        bail!("Windows 更新完成器路径合同无效");
    }
    let parent = target.parent().context("Windows Sandbox 目标缺少父目录")?;
    // 父进程仍持锁时在此阻塞；父进程退出释放后完成器立即接管，
    // 中间没有其他 update 可进入的竞态窗口。
    let _lock = UpdateLock::acquire_wait(parent)?;
    wait_for_process_exit(parent_pid, Duration::from_secs(60))?;
    // 完成器不信任父进程传参：重新验证 source 的官方签名和兼容自报。
    trusted_target_version(&source, signature_source, OFFICIAL_PUBKEY_B64)?;
    install_verified_pair(&source, signature_source, target, signature_target)?;
    // 当前完成器正在 cleanup 下执行，Windows 不能立即删除自身目录；
    // 下次 update 会清理残留事务目录。
    let _ = cleanup;
    Ok(())
}

#[cfg(windows)]
fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Ok(());
    }
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    let result = unsafe { WaitForSingleObject(handle, millis) };
    unsafe { CloseHandle(handle) };
    if result != WAIT_OBJECT_0 {
        bail!("等待旧 Sandbox 进程退出超时")
    }
    Ok(())
}

fn validate_https_url(url: &str, label: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("{label} URL 非法"))?;
    let test_http = cfg!(test)
        && parsed.scheme() == "http"
        && parsed
            .host_str()
            .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "::1"));
    if parsed.scheme() != "https" && !test_http {
        bail!("{label} 必须使用 HTTPS");
    }
    Ok(())
}

fn parse_checksum(value: &str) -> Result<Vec<u8>> {
    let hex = value
        .strip_prefix("sha256:")
        .context("校验和缺少 sha256: 前缀")?;
    let bytes = hex::decode(hex)?;
    if bytes.len() != 32 {
        bail!("SHA-256 校验和长度非法");
    }
    Ok(bytes)
}

fn ensure_install_directory(directory: &Path) -> Result<()> {
    if !directory.is_absolute() {
        bail!("Sandbox 安装目录必须是绝对路径: {}", directory.display());
    }
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!("Sandbox 安装路径不是普通目录: {}", directory.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(directory)?;
            let metadata = std::fs::symlink_metadata(directory)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("Sandbox 安装路径不是普通目录: {}", directory.display());
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("{label}不存在: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{label}必须是普通文件: {}", path.display());
    }
    Ok(())
}

fn cleanup_stale_transactions(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(UPDATE_DIR_PREFIX)
            && entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn current_platform_key() -> String {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "unknown"
    };
    format!("{os}-{arch}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_rejects_incompatible_protocol() {
        let manifest = UpdateManifest {
            version: "0.2.0".into(),
            protocol_version: crate::LAUNCHER_PROTOCOL_VERSION + 1,
            policy_schema_max: crate::LAUNCHER_POLICY_SCHEMA,
            platforms: [(
                "linux-x86_64".into(),
                PlatformArtifact {
                    url: "https://example.com/a".into(),
                    checksum: format!("sha256:{}", "0".repeat(64)),
                    signature_url: "https://example.com/a.sig".into(),
                },
            )]
            .into(),
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn checksum_requires_sha256() {
        assert!(parse_checksum(&format!("sha256:{}", "a".repeat(64))).is_ok());
        assert!(parse_checksum("md5:abc").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn install_directory_rejects_symlink() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let install = root.path().join("install");
        symlink(outside.path(), &install).unwrap();
        assert!(ensure_install_directory(&install).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn install_pair_rejects_symlink_target() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        let candidate = root.path().join("candidate");
        let signature = root.path().join("candidate.sig");
        let target = root.path().join(launcher_file_name());
        std::fs::write(&outside, b"safe").unwrap();
        std::fs::write(&candidate, b"new").unwrap();
        std::fs::write(&signature, b"sig").unwrap();
        symlink(&outside, &target).unwrap();
        assert!(
            install_verified_pair(&candidate, &signature, &target, &signature_path(&target))
                .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"safe");
    }

    #[tokio::test]
    async fn 代理不可达时直连重试() {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 1024];
            let _ = stream.read(&mut buffer).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        // 端口 9（discard）通常无人监听，模拟 VPN 关闭后失效的代理。
        let dead_proxy = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        let updater = SelfUpdater {
            client: dead_proxy,
            direct_client: update_client(true).unwrap(),
            manifest_url: String::new(),
            official_pubkey_b64: String::new(),
        };
        let response = updater.get(&format!("http://{address}/")).await.unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn install_to_downloads_verifies_and_installs_standard_name() {
        use std::io::{Read as _, Write as _};
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("install");
        std::fs::create_dir(&install).unwrap();
        let candidate = root.path().join("candidate");
        std::fs::write(&candidate, format!(
            "#!/bin/sh\nprintf '%s\\n' '{{\"platform\":\"available\",\"product_version\":\"0.2.0\",\"protocol_version\":{},\"policy_schema\":{}}}'\n",
            crate::LAUNCHER_PROTOCOL_VERSION, crate::LAUNCHER_POLICY_SCHEMA,
        )).unwrap();
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).unwrap();
        let payload = std::fs::read(&candidate).unwrap();
        let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let signature = minisign::sign(
            Some(&keypair.pk),
            &keypair.sk,
            payload.as_slice(),
            None,
            None,
        )
        .unwrap();
        let signature_b64 =
            base64::engine::general_purpose::STANDARD.encode(signature.into_string());
        let pubkey_b64 = base64::engine::general_purpose::STANDARD
            .encode(keypair.pk.to_box().unwrap().into_string());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let manifest = serde_json::json!({
            "version": "0.2.0",
            "protocol_version": crate::LAUNCHER_PROTOCOL_VERSION,
            "policy_schema_max": crate::LAUNCHER_POLICY_SCHEMA,
            "platforms": { current_platform_key(): {
                "url": format!("http://{address}/artifact"),
                "checksum": format!("sha256:{}", hex::encode(Sha256::digest(&payload))),
                "signature_url": format!("http://{address}/artifact.sig"),
            }}
        })
        .to_string();
        let server_payload = payload.clone();
        let server_signature = signature_b64.clone();
        let server = std::thread::spawn(move || {
            for stream in listener.incoming().take(6) {
                let mut stream = stream.unwrap();
                let mut buffer = [0_u8; 4096];
                let read = stream.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..read]);
                let body: &[u8] = match request.split_whitespace().nth(1).unwrap_or("/") {
                    "/latest.json" => manifest.as_bytes(),
                    "/artifact" => server_payload.as_slice(),
                    "/artifact.sig" => server_signature.as_bytes(),
                    _ => b"",
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });
        let updater =
            SelfUpdater::with_test_pubkey(format!("http://{address}/latest.json"), pubkey_b64)
                .unwrap();
        let status = updater.install_to(&install).await.unwrap();
        let target = install.join(launcher_file_name());
        assert_eq!(
            status,
            UpdateStatus::Installed {
                version: "0.2.0".into(),
                path: target.clone()
            }
        );
        assert_eq!(std::fs::read(&target).unwrap(), payload);
        assert!(signature_path(&target).is_file());
        assert!(!install.join("sandbox").exists());

        // 直接自更新策略的核心文件行为：替换目标文件本身与伴生签名，
        // 不创建 versions/active 仓库。
        let current_dir = root.path().join("current");
        std::fs::create_dir(&current_dir).unwrap();
        let current = current_dir.join(launcher_file_name());
        std::fs::write(&current, b"old").unwrap();
        std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o755)).unwrap();
        let status = updater
            .update_target(&current, UpdateMode::Current)
            .await
            .unwrap();
        server.join().unwrap();
        assert_eq!(
            status,
            UpdateStatus::Updated {
                previous: env!("CARGO_PKG_VERSION").into(),
                version: "0.2.0".into(),
                path: current.clone(),
            }
        );
        assert_eq!(std::fs::read(&current).unwrap(), payload);
        assert!(signature_path(&current).is_file());
        assert!(!current_dir.join("sandbox").exists());
    }
}
