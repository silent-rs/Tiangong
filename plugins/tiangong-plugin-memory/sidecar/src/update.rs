//! 自更新：从天工官方插件目录拉取 memory 插件的最新 sidecar 并替换自身。
//!
//! 信任链与天工安装插件一致：
//! 1. `catalog.json` 给出最新版本、当前平台的 sidecar 下载地址与签名清单地址；
//! 2. `release.json` 必须通过内置官方公钥（minisign）验签，发布者为官方，
//!    插件 ID 与版本和目录一致；
//! 3. 下载的二进制 sha256 必须同时匹配目录与签名清单；
//! 4. 候选二进制 `--version` 自检通过后，才在同目录原子替换当前可执行文件。
//!
//! 由天工插件管理器安装的副本（同目录存在 `release.json`）拒绝自更新，
//! 以免破坏宿主的签名校验，应在天工中更新插件。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use futures_util::StreamExt as _;
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tiangong_plugin_runtime::artifacts::{PluginCatalog, PluginRelease, current_platform_key};
use tiangong_plugin_runtime::signature::{OFFICIAL_PUBKEY_B64, SignedPluginRelease};

const PLUGIN_ID: &str = tiangong_plugin_memory_protocol::PLUGIN_ID;
const CATALOG_URL_ENV: &str = "TIANGONG_PLUGIN_CATALOG_URL";
/// sidecar 静态链接了 SQLCipher/OpenSSL/LanceDB，体积较大，上限留足余量。
const MAX_BINARY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 8 * 1024 * 1024;
const TRANSACTION_PREFIX: &str = ".tiangong-memory-update-";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
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
}

pub struct SelfUpdater {
    client: reqwest::Client,
    catalog_url: String,
    pubkey_b64: String,
    current_version: Version,
}

impl SelfUpdater {
    /// 官方目录 + 内置官方公钥。目录地址遵循天工既有的
    /// `TIANGONG_PLUGIN_CATALOG_URL` 覆盖约定；信任根不可覆盖。
    pub fn official() -> Result<Self> {
        let catalog_url = std::env::var(CATALOG_URL_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                tiangong_plugin_runtime::artifacts::PLUGIN_CATALOG_ENDPOINT.to_string()
            });
        Self::with_trust(catalog_url, OFFICIAL_PUBKEY_B64, env!("CARGO_PKG_VERSION"))
    }

    fn with_trust(
        catalog_url: impl Into<String>,
        pubkey_b64: impl Into<String>,
        current_version: &str,
    ) -> Result<Self> {
        let catalog_url = catalog_url.into();
        validate_url(&catalog_url, "插件目录")?;
        let client = reqwest::Client::builder()
            .user_agent(format!(
                "tiangong-memory-sidecar/{}",
                env!("CARGO_PKG_VERSION")
            ))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30 * 60))
            .build()
            .context("构建更新客户端失败")?;
        Ok(Self {
            client,
            catalog_url,
            pubkey_b64: pubkey_b64.into().trim().to_string(),
            current_version: Version::parse(current_version).context("当前版本号非法")?,
        })
    }

    /// 只检查，不写磁盘。
    pub async fn check(&self) -> Result<UpdateStatus> {
        let release = self.fetch_release().await?;
        let latest = Version::parse(&release.version).context("目录中的版本号非法")?;
        let current = self.current_version.to_string();
        if latest <= self.current_version {
            return Ok(UpdateStatus::UpToDate { current });
        }
        Ok(UpdateStatus::Available {
            current,
            version: latest.to_string(),
        })
    }

    /// 更新当前正在执行的二进制。
    pub async fn update_current(&self) -> Result<UpdateStatus> {
        let target = std::env::current_exe().context("定位当前可执行文件失败")?;
        let target = std::fs::canonicalize(&target).unwrap_or(target);
        self.update_target(&target).await
    }

    async fn update_target(&self, target: &Path) -> Result<UpdateStatus> {
        ensure_standalone_install(target)?;
        let parent = target.parent().context("可执行文件缺少父目录")?;
        let _lock = UpdateLock::acquire(parent)?;
        cleanup_stale_transactions(parent, target);

        let release = self.fetch_release().await?;
        let latest = Version::parse(&release.version).context("目录中的版本号非法")?;
        if latest <= self.current_version {
            return Ok(UpdateStatus::UpToDate {
                current: self.current_version.to_string(),
            });
        }

        let platform = current_platform_key();
        let binary = release
            .sidecars
            .get(&platform)
            .with_context(|| format!("最新版本 {latest} 未提供当前平台 {platform} 的 sidecar"))?;
        let signed = release
            .signed_releases
            .get(&platform)
            .with_context(|| format!("最新版本 {latest} 缺少当前平台 {platform} 的签名清单"))?;
        validate_url(&binary.url, "sidecar 制品")?;
        validate_url(&signed.url, "签名清单")?;
        validate_url(&signed.signature_url, "签名")?;

        // 先验签名清单：清单不可信则不下载大文件。
        let release_bytes = self.download_bytes(&signed.url).await?;
        let signature_text = String::from_utf8(self.download_bytes(&signed.signature_url).await?)
            .context("签名文件不是 UTF-8")?;
        let signed_release =
            verify_release(&release_bytes, &signature_text, &self.pubkey_b64, &latest)?;
        let signed_sidecar = signed_release
            .sidecar
            .as_ref()
            .context("签名清单缺少 sidecar 条目")?;
        let catalog_sha = parse_sha256(&binary.checksum)?;
        let signed_sha = parse_sha256(&signed_sidecar.sha256)?;
        if catalog_sha != signed_sha {
            bail!("目录与签名清单的 sidecar 校验和不一致");
        }

        let transaction = parent.join(format!("{TRANSACTION_PREFIX}{}", scru128::new()));
        std::fs::create_dir(&transaction)
            .with_context(|| format!("创建更新临时目录失败：{}", transaction.display()))?;
        let _cleanup = TransactionCleanup(transaction.clone());
        let candidate = transaction.join(target.file_name().context("可执行文件名无效")?);
        eprintln!("正在下载 {latest}（{platform}）...");
        let actual = self.download_to_file(&binary.url, &candidate).await?;
        if actual != signed_sha {
            bail!(
                "sidecar 校验和不匹配: expected={}, actual={}",
                hex::encode(signed_sha),
                hex::encode(actual)
            );
        }
        set_executable(&candidate)?;
        verify_candidate_version(&candidate, &latest)?;
        replace_executable(&candidate, target)?;
        Ok(UpdateStatus::Updated {
            previous: self.current_version.to_string(),
            version: latest.to_string(),
            path: target.to_path_buf(),
        })
    }

    async fn fetch_release(&self) -> Result<PluginRelease> {
        let body = self
            .download_bytes(&self.catalog_url)
            .await
            .context("拉取插件目录失败")?;
        let catalog: PluginCatalog = serde_json::from_slice(&body).context("解析插件目录失败")?;
        catalog
            .plugins
            .into_iter()
            .find(|plugin| plugin.id == PLUGIN_ID)
            .with_context(|| format!("插件目录中没有 {PLUGIN_ID}"))
    }

    async fn download_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("请求失败：{url}"))?;
        if !response.status().is_success() {
            bail!("下载响应异常：{} {url}", response.status());
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_METADATA_BYTES)
        {
            bail!("下载内容超过大小上限：{url}");
        }
        let bytes = response.bytes().await?;
        if bytes.len() as u64 > MAX_METADATA_BYTES {
            bail!("下载内容超过大小上限：{url}");
        }
        Ok(bytes.to_vec())
    }

    /// 流式下载并计算 sha256。
    async fn download_to_file(&self, url: &str, path: &Path) -> Result<Vec<u8>> {
        use std::io::Write as _;

        let response = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("请求失败：{url}"))?;
        if !response.status().is_success() {
            bail!("下载响应异常：{} {url}", response.status());
        }
        let total_hint = response.content_length();
        if total_hint.is_some_and(|length| length > MAX_BINARY_BYTES) {
            bail!("下载内容超过大小上限：{url}");
        }
        let mut file = std::fs::File::create(path)
            .with_context(|| format!("创建临时文件失败：{}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut last_report = 0_u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("下载中断")?;
            total = total.saturating_add(chunk.len() as u64);
            if total > MAX_BINARY_BYTES {
                bail!("下载内容超过大小上限：{url}");
            }
            hasher.update(&chunk);
            file.write_all(&chunk)?;
            if total - last_report >= 32 * 1024 * 1024 {
                last_report = total;
                match total_hint {
                    Some(size) if size > 0 => {
                        eprintln!("  已下载 {} / {} MiB", total >> 20, size >> 20)
                    }
                    _ => eprintln!("  已下载 {} MiB", total >> 20),
                }
            }
        }
        file.sync_all()?;
        Ok(hasher.finalize().to_vec())
    }
}

/// 验证签名清单：官方签名、发布者、插件 ID 与版本。
fn verify_release(
    content: &[u8],
    signature_b64: &str,
    pubkey_b64: &str,
    expected_version: &Version,
) -> Result<SignedPluginRelease> {
    let public_text = base64::engine::general_purpose::STANDARD
        .decode(pubkey_b64.trim())
        .context("解析官方公钥失败")?;
    let public = PublicKey::decode(&String::from_utf8(public_text)?).context("解析官方公钥失败")?;
    let signature_text = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .context("解析签名失败")?;
    let signature =
        Signature::decode(&String::from_utf8(signature_text)?).context("解析 minisign 签名失败")?;
    public
        .verify(content, &signature, false)
        .context("官方签名验证不通过")?;
    let release: SignedPluginRelease =
        serde_json::from_slice(content).context("解析签名清单失败")?;
    if release.publisher != tiangong_plugin_runtime::OFFICIAL_PUBLISHER {
        bail!("签名清单发布者不是官方：{}", release.publisher);
    }
    if release.id != PLUGIN_ID {
        bail!("签名清单插件 ID 不符：{}", release.id);
    }
    if Version::parse(&release.version).ok().as_ref() != Some(expected_version) {
        bail!(
            "签名清单版本与目录不一致：release={}, catalog={expected_version}",
            release.version
        );
    }
    Ok(release)
}

fn parse_sha256(value: &str) -> Result<Vec<u8>> {
    let hex_text = value.trim().trim_start_matches("sha256:");
    let bytes = hex::decode(hex_text).with_context(|| format!("校验和格式无效：{value}"))?;
    if bytes.len() != 32 {
        bail!("校验和长度无效：{value}");
    }
    Ok(bytes)
}

fn validate_url(url: &str, label: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("{label} 地址无效：{url}"))?;
    let loopback_http = parsed.scheme() == "http"
        && parsed
            .host_str()
            .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1"));
    if parsed.scheme() != "https" && !loopback_http {
        bail!("{label} 必须使用 HTTPS：{url}");
    }
    Ok(())
}

/// 天工插件管理器安装的副本受宿主签名校验约束，不能就地替换。
fn ensure_standalone_install(target: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(target)
        .with_context(|| format!("读取可执行文件失败：{}", target.display()))?;
    if !metadata.is_file() {
        bail!("当前可执行文件不是普通文件：{}", target.display());
    }
    let parent = target.parent().context("可执行文件缺少父目录")?;
    if parent
        .join(tiangong_plugin_runtime::signature::SIGNED_RELEASE_FILE)
        .exists()
    {
        bail!(
            "当前二进制由天工插件管理器安装（{}），请在天工中更新 Memory 插件",
            parent.display()
        );
    }
    Ok(())
}

fn verify_candidate_version(candidate: &Path, expected: &Version) -> Result<()> {
    let mut command = std::process::Command::new(candidate);
    command
        .arg("--version")
        .env_remove("TIANGONG_PLUGIN_TRANSPORT")
        .stdin(std::process::Stdio::null());
    let output = tiangong_toolkit::configure_no_window(&mut command)
        .output()
        .context("运行候选版本自检失败")?;
    if !output.status.success() {
        bail!("候选版本自检失败：{}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let reported = text
        .split_whitespace()
        .last()
        .and_then(|value| Version::parse(value).ok())
        .ok_or_else(|| anyhow!("候选版本自检输出无法识别：{}", text.trim()))?;
    if &reported != expected {
        bail!("候选版本号不符：expected={expected}, actual={reported}");
    }
    Ok(())
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

/// 同目录替换：Unix 直接 rename 覆盖（运行中的旧 inode 不受影响）；
/// Windows 允许重命名运行中的 exe，先把旧文件挪开再放入新文件。
fn replace_executable(candidate: &Path, target: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let backup = target.with_extension(format!("old-{}", scru128::new()));
        std::fs::rename(target, &backup)
            .with_context(|| format!("移开旧版本失败：{}", target.display()))?;
        if let Err(error) = std::fs::rename(candidate, target) {
            let _ = std::fs::rename(&backup, target);
            return Err(error).with_context(|| format!("放置新版本失败：{}", target.display()));
        }
        // 运行中的旧文件删不掉，留给下次更新时清理。
        let _ = std::fs::remove_file(&backup);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(candidate, target)
            .with_context(|| format!("替换可执行文件失败：{}", target.display()))
    }
}

/// 清理上次更新遗留：本程序的事务目录，以及 Windows 上本程序自己的旧版本备份。
///
/// 备份名形如 `<可执行文件主名>.old-<scru128>`（`with_extension` 会替换原扩展名），
/// 只清理严格符合该命名的文件——同目录可能放着其他程序的文件，
/// 按 `.old-` 子串匹配会误删无关文件。
fn cleanup_stale_transactions(directory: &Path, target: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let backup_prefix = target
        .file_stem()
        .map(|stem| format!("{}.old-", stem.to_string_lossy()));
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(TRANSACTION_PREFIX) {
            let _ = std::fs::remove_dir_all(entry.path());
        } else if cfg!(windows)
            && backup_prefix
                .as_deref()
                .is_some_and(|prefix| is_own_backup(&name, prefix))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 是否为本程序生成的旧版本备份：前缀匹配且后缀是 scru128（25 位 0-9a-z）。
fn is_own_backup(name: &str, backup_prefix: &str) -> bool {
    let Some(suffix) = name.strip_prefix(backup_prefix) else {
        return false;
    };
    suffix.len() == 25
        && suffix
            .chars()
            .all(|ch| ch.is_ascii_digit() || ch.is_ascii_lowercase())
}

struct UpdateLock(std::fs::File);

impl UpdateLock {
    fn acquire(directory: &Path) -> Result<Self> {
        let path = directory.join(".tiangong-memory-update.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("创建更新锁失败：{}", path.display()))?;
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|error| anyhow!("另一个更新正在执行（{}）：{error}", path.display()))?;
        Ok(Self(file))
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

struct TransactionCleanup(PathBuf);

impl Drop for TransactionCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{Read as _, Write as _};

    use super::*;

    struct Fixture {
        _root: tempfile::TempDir,
        install: PathBuf,
        target: PathBuf,
        pubkey_b64: String,
        catalog_url: String,
        server: std::thread::JoinHandle<()>,
    }

    /// 本地 HTTP 发布一个"新版本"：脚本候选、签名清单、目录。
    fn fixture(
        new_version: &str,
        tamper_binary: bool,
        requests: usize,
        sign_with_other_key: bool,
    ) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("bin");
        std::fs::create_dir(&install).unwrap();
        let target = install.join("tiangong-memory-sidecar");
        std::fs::write(&target, "#!/bin/sh\necho old\n").unwrap();
        set_executable(&target).unwrap();

        let payload =
            format!("#!/bin/sh\necho tiangong-memory-sidecar {new_version}\n").into_bytes();
        let payload_sha = hex::encode(Sha256::digest(&payload));
        let release = serde_json::json!({
            "schema_version": 1,
            "id": "memory",
            "version": new_version,
            "publisher": "tiangong-official",
            "permissions": ["sidecar.invoke"],
            "manifest": {"path": "plugin.json", "sha256": "00"},
            "sidecar": {"path": "tiangong-memory-sidecar", "sha256": payload_sha},
        })
        .to_string();
        let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let signer = if sign_with_other_key {
            minisign::KeyPair::generate_unencrypted_keypair().unwrap()
        } else {
            minisign::KeyPair {
                pk: keypair.pk.clone(),
                sk: keypair.sk.clone(),
            }
        };
        let signature =
            minisign::sign(Some(&signer.pk), &signer.sk, release.as_bytes(), None, None).unwrap();
        let signature_b64 =
            base64::engine::general_purpose::STANDARD.encode(signature.into_string());
        let pubkey_b64 = base64::engine::general_purpose::STANDARD
            .encode(keypair.pk.to_box().unwrap().into_string());

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let platform = current_platform_key();
        let catalog = serde_json::json!({
            "version": 1,
            "plugins": [{
                "id": "memory",
                "name": "Memory",
                "version": new_version,
                "manifest": {"url": format!("{base}/plugin.json"), "checksum": "sha256:00"},
                "sidecars": {platform.clone(): {
                    "url": format!("{base}/bin"),
                    "checksum": format!("sha256:{payload_sha}"),
                }},
                "signed_releases": {platform: {
                    "url": format!("{base}/release.json"),
                    "signature_url": format!("{base}/release.json.sig"),
                }},
            }],
        })
        .to_string();
        let mut served_binary = payload.clone();
        if tamper_binary {
            served_binary.extend_from_slice(b"# tampered\n");
        }
        let server = std::thread::spawn(move || {
            for stream in listener.incoming().take(requests) {
                let mut stream = stream.unwrap();
                let mut buffer = [0_u8; 4096];
                let read = stream.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let body: Vec<u8> = match request.split_whitespace().nth(1).unwrap_or("/") {
                    "/catalog.json" => catalog.clone().into_bytes(),
                    "/release.json" => release.clone().into_bytes(),
                    "/release.json.sig" => signature_b64.clone().into_bytes(),
                    "/bin" => served_binary.clone(),
                    _ => Vec::new(),
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Fixture {
            _root: root,
            install,
            target,
            pubkey_b64,
            catalog_url: format!("{base}/catalog.json"),
            server,
        }
    }

    #[tokio::test]
    async fn check_reports_available_and_up_to_date() {
        let fx = fixture("0.2.0", false, 2, false);
        let updater = SelfUpdater::with_trust(&fx.catalog_url, &fx.pubkey_b64, "0.1.0").unwrap();
        assert_eq!(
            updater.check().await.unwrap(),
            UpdateStatus::Available {
                current: "0.1.0".to_string(),
                version: "0.2.0".to_string()
            }
        );
        let updater = SelfUpdater::with_trust(&fx.catalog_url, &fx.pubkey_b64, "0.2.0").unwrap();
        assert!(matches!(
            updater.check().await.unwrap(),
            UpdateStatus::UpToDate { .. }
        ));
        fx.server.join().unwrap();
    }

    #[tokio::test]
    async fn update_downloads_verifies_and_replaces() {
        let fx = fixture("0.2.0", false, 4, false);
        let updater = SelfUpdater::with_trust(&fx.catalog_url, &fx.pubkey_b64, "0.1.0").unwrap();
        let status = updater.update_target(&fx.target).await.unwrap();
        assert!(matches!(status, UpdateStatus::Updated { ref version, .. } if version == "0.2.0"));
        let content = std::fs::read_to_string(&fx.target).unwrap();
        assert!(content.contains("tiangong-memory-sidecar 0.2.0"));
        let leftovers = std::fs::read_dir(&fx.install)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(TRANSACTION_PREFIX)
            })
            .count();
        assert_eq!(leftovers, 0);
        fx.server.join().unwrap();
    }

    #[tokio::test]
    async fn tampered_binary_is_rejected_and_target_kept() {
        let fx = fixture("0.2.0", true, 4, false);
        let updater = SelfUpdater::with_trust(&fx.catalog_url, &fx.pubkey_b64, "0.1.0").unwrap();
        let error = updater.update_target(&fx.target).await.unwrap_err();
        assert!(error.to_string().contains("校验和不匹配"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(&fx.target).unwrap(),
            "#!/bin/sh\necho old\n"
        );
        fx.server.join().unwrap();
    }

    #[tokio::test]
    async fn untrusted_signature_is_rejected_before_download() {
        let fx = fixture("0.2.0", false, 3, true);
        let updater = SelfUpdater::with_trust(&fx.catalog_url, &fx.pubkey_b64, "0.1.0").unwrap();
        let error = updater.update_target(&fx.target).await.unwrap_err();
        assert!(format!("{error:#}").contains("签名验证不通过"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(&fx.target).unwrap(),
            "#!/bin/sh\necho old\n"
        );
        fx.server.join().unwrap();
    }

    #[tokio::test]
    async fn host_managed_install_is_refused() {
        let fx = fixture("0.2.0", false, 0, false);
        std::fs::write(fx.install.join("release.json"), "{}").unwrap();
        let updater = SelfUpdater::with_trust(&fx.catalog_url, &fx.pubkey_b64, "0.1.0").unwrap();
        let error = updater.update_target(&fx.target).await.unwrap_err();
        assert!(error.to_string().contains("插件管理器"), "{error:#}");
        fx.server.join().unwrap();
    }

    #[test]
    fn own_backup_matching_is_strict() {
        let prefix = "tiangong-memory.old-";
        let id = scru128::new().to_string();
        assert_eq!(id.len(), 25, "scru128 文本长度应为 25：{id}");
        assert!(is_own_backup(&format!("{prefix}{id}"), prefix));
        // 其他程序的备份、手工备份和非法后缀都不该被清理。
        assert!(!is_own_backup(&format!("other.old-{id}"), prefix));
        assert!(!is_own_backup(&format!("{prefix}manual"), prefix));
        assert!(!is_own_backup(
            &format!("{prefix}{}", id.to_uppercase()),
            prefix
        ));
        assert!(!is_own_backup(&format!("{prefix}{id}.zip"), prefix));
        assert!(!is_own_backup("notes.old-backup.txt", prefix));
    }

    #[test]
    fn url_and_checksum_validation() {
        assert!(validate_url("https://example.com/a", "x").is_ok());
        assert!(validate_url("http://127.0.0.1:1/a", "x").is_ok());
        assert!(validate_url("http://example.com/a", "x").is_err());
        assert!(parse_sha256(&format!("sha256:{}", "ab".repeat(32))).is_ok());
        assert!(parse_sha256("sha256:abcd").is_err());
    }
}
