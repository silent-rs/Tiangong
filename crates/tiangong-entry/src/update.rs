//! `tiangong update` —— 检查并安装天工更新。
//!
//! 纯 CLI 实现（不依赖 Tauri 运行时），可在无图形环境的 Linux 上运行。
//! 读取桌面端 `latest.json`，下载当前平台的安装包，用 minisign 公钥验证签名后
//! 原子替换自身（AppImage 经 `APPIMAGE` 环境变量定位真实文件）。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use semver::Version;
use serde::Deserialize;

use crate::args::UpdateArgs;

/// 更新源端点（与桌面端 tauri updater 共用 latest.json），优先 OSS，失败回退 GitHub Release。
const UPDATE_ENDPOINTS: &[&str] = &[
    "https://silent-tiangong.oss-cn-hangzhou.aliyuncs.com/latest.json",
    "https://github.com/silent-rs/silent-Tiangong/releases/latest/download/latest.json",
];

/// minisign 公钥（base64 编码，与 `src-tauri/tauri.conf.json` 的 updater.pubkey 一致）。
///
/// CLI 用它验证下载制品的签名，安全性等价于桌面端 tauri_plugin_updater 的验签。
const UPDATER_PUBKEY_B64: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IERBQUE2RDc2MjQwN0I5QzMKUldURHVRY2tkbTJxMmtTTnJnZk9GSVd3amFmb3VoYjVmdFdGbHZCRHZ1VVZCeS9JcS8rUHBVVDgK";

#[derive(Debug, Deserialize)]
struct ReleaseManifest {
    #[serde(alias = "name")]
    version: String,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    platforms: BTreeMap<String, ReleasePlatform>,
}

#[derive(Debug, Deserialize)]
struct ReleasePlatform {
    url: String,
    signature: String,
}

pub(crate) fn run_update_command(args: UpdateArgs) -> anyhow::Result<()> {
    let endpoints: Vec<&str> = if let Some(custom) = args
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        vec![custom]
    } else {
        UPDATE_ENDPOINTS.to_vec()
    };

    let mut manifest = None;
    let mut last_error = None;
    for endpoint in &endpoints {
        match fetch_release_manifest(endpoint) {
            Ok(Some(m)) => {
                manifest = Some(m);
                break;
            }
            Ok(None) => {
                // 404 — 没有发布，继续尝试下一个
                last_error = None;
            }
            Err(e) => {
                last_error = Some(e);
            }
        }
    }

    if let Some(err) = last_error
        && manifest.is_none()
    {
        return Err(err);
    }

    let Some(manifest) = manifest else {
        println!("当前没有可用的在线更新发布。");
        return Ok(());
    };
    let current_version = parse_version(env!("CARGO_PKG_VERSION"))?;
    let latest_version = parse_version(&manifest.version)?;

    println!("当前版本：{}", current_version);
    println!("最新版本：{}", latest_version);

    if latest_version <= current_version {
        println!("当前已是最新版本。");
        return Ok(());
    }

    println!("发现新版本：{}", manifest.version.trim());
    if let Some(notes) = manifest
        .notes
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        println!("\n更新说明：\n{}", notes.trim());
    }

    let platform_key =
        current_platform_key().ok_or_else(|| anyhow!("当前平台不支持自动更新，请手动下载安装"))?;
    let platform = manifest
        .platforms
        .get(&platform_key)
        .with_context(|| format!("更新源未提供当前平台的制品：{platform_key}"))?;

    println!("\n更新包：{}", platform.url);

    if args.check {
        return Ok(());
    }

    // 下载 + 签名验证 + 自替换。
    println!("\n开始下载并安装更新...");
    let downloaded = download_to_temp(&platform.url)?;
    println!("下载完成，正在验证签名...");
    verify_signature(&downloaded, &platform.signature).context("更新包签名验证失败，已拒绝安装")?;
    println!("签名验证通过，正在安装...");
    install_update(&downloaded)?;
    println!("\n✅ 更新已安装，请重新启动天工使新版本生效。");
    Ok(())
}

fn fetch_release_manifest(endpoint: &str) -> anyhow::Result<Option<ReleaseManifest>> {
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("tiangong-cli-updater")
        .build()
        .context("初始化更新客户端失败")?
        .get(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .with_context(|| format!("请求更新源失败：{endpoint}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(anyhow!("更新源返回异常状态：{}", response.status()));
    }

    response.json().map(Some).context("解析更新信息失败")
}

fn parse_version(raw: &str) -> anyhow::Result<Version> {
    let value = raw.trim().trim_start_matches('v');
    Version::parse(value).with_context(|| format!("版本号格式无效：{raw}"))
}

fn current_platform_key() -> Option<String> {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        return None;
    };

    let arch = if cfg!(target_arch = "x86") {
        "i686"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "arm") {
        "armv7"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "riscv64") {
        "riscv64"
    } else {
        return None;
    };

    Some(format!("{os}-{arch}"))
}

/// 流式下载更新包到临时文件，返回临时文件路径（含同目录原子替换所需的就近路径）。
///
/// 进度按 10% 步长打印。临时文件创建在目标可执行文件同目录下（确保跨文件系统
/// `rename` 原子替换可行）。
fn download_to_temp(url: &str) -> anyhow::Result<PathBuf> {
    use std::io::Write;

    let target = resolve_target_path()?;
    let dir = target
        .parent()
        .context("无法定位目标文件所在目录")?
        .to_path_buf();
    std::fs::create_dir_all(&dir).with_context(|| format!("创建目录失败：{}", dir.display()))?;

    let temp_path = dir.join(format!(
        ".tiangong-update-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));

    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .user_agent("tiangong-cli-updater")
        .build()
        .context("初始化下载客户端失败")?
        .get(url)
        .send()
        .with_context(|| format!("请求更新包失败：{url}"))?;
    if !response.status().is_success() {
        bail!("更新包下载响应非 2xx：{} {url}", response.status());
    }
    let total = response.content_length();
    let mut file = std::fs::File::create(&temp_path)
        .with_context(|| format!("创建临时文件失败：{}", temp_path.display()))?;
    let mut downloaded: u64 = 0;
    let mut last_percent: u64 = 0;
    let mut stream = response;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = stream
            .read(&mut buf)
            .with_context(|| format!("读取更新包失败：{}", temp_path.display()))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .with_context(|| format!("写入临时文件失败：{}", temp_path.display()))?;
        downloaded = downloaded.saturating_add(n as u64);
        if let Some(total) = total.filter(|v| *v > 0) {
            let percent = downloaded.saturating_mul(100) / total;
            if percent >= last_percent.saturating_add(10) || percent == 100 {
                last_percent = percent;
                print!("\r下载进度：{percent}%");
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
            }
        }
    }
    file.sync_all()
        .with_context(|| format!("同步临时文件失败：{}", temp_path.display()))?;
    println!();
    Ok(temp_path)
}

/// 用 minisign 公钥验证下载文件签名（等价于 tauri_plugin_updater 的验签）。
///
/// `signature_b64` 是 latest.json 中 base64 编码的签名；解码后为标准 minisign 文本。
fn verify_signature(downloaded: &Path, signature_b64: &str) -> anyhow::Result<()> {
    use minisign_verify::{PublicKey, Signature};

    let pubkey_text = base64::engine::general_purpose::STANDARD
        .decode(UPDATER_PUBKEY_B64)
        .context("解析内置公钥失败")?;
    let pubkey_text = String::from_utf8(pubkey_text).context("内置公钥非 UTF-8")?;
    let public_key = PublicKey::decode(&pubkey_text).context("解析 minisign 公钥失败")?;

    let signature_text = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .context("解析更新包签名失败")?;
    let signature_text = String::from_utf8(signature_text).context("更新包签名非 UTF-8")?;
    let signature = Signature::decode(&signature_text).context("解析 minisign 签名失败")?;

    let mut verifier = public_key
        .verify_stream(&signature)
        .context("初始化签名验证器失败")?;
    let mut file = std::fs::File::open(downloaded)
        .with_context(|| format!("打开下载文件失败：{}", downloaded.display()))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("读取下载文件失败：{}", downloaded.display()))?;
        if n == 0 {
            break;
        }
        verifier.update(&buf[..n]);
    }
    verifier.finalize().context("签名验证不通过")?;
    Ok(())
}

/// 定位要被替换的目标路径。
///
/// AppImage 运行时 `current_exe` 指向只读挂载点，必须经 `APPIMAGE` 环境变量
/// 定位真实的 `.AppImage` 文件；其他场景直接用 `current_exe`。
fn resolve_target_path() -> anyhow::Result<PathBuf> {
    if let Ok(appimage) = std::env::var("APPIMAGE")
        && !appimage.trim().is_empty()
    {
        let path = PathBuf::from(appimage);
        if path.exists() {
            return Ok(path);
        }
    }
    std::env::current_exe().context("定位当前可执行文件失败")
}

/// 把已验证的更新包原子替换到目标路径。
///
/// - Unix：`rename` 原子替换（覆盖正在运行的二进制安全，内核持有旧 inode）。
/// - Windows：先把目标 rename 到 `.old`，再替换，最后尝试删除 `.old`（占用则忽略）。
fn install_update(temp_path: &Path) -> anyhow::Result<()> {
    let target = resolve_target_path()?;

    #[cfg(unix)]
    {
        // 确保新文件可执行。
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(temp_path)
            .with_context(|| format!("读取临时文件元数据失败：{}", temp_path.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(temp_path, perms)
            .with_context(|| format!("设置执行权限失败：{}", temp_path.display()))?;

        std::fs::rename(temp_path, &target).with_context(|| {
            format!(
                "替换目标文件失败：{} -> {}",
                temp_path.display(),
                target.display()
            )
        })?;
    }

    #[cfg(windows)]
    {
        let backup = target.with_extension("exe.old");
        let _ = std::fs::remove_file(&backup);
        if target.exists() {
            std::fs::rename(&target, &backup)
                .with_context(|| format!("备份旧文件失败：{}", target.display()))?;
        }
        std::fs::rename(temp_path, &target)
            .with_context(|| format!("替换目标文件失败：{}", target.display()))?;
        // 旧文件可能仍被占用，删除失败则忽略（下次启动可清理）。
        let _ = std::fs::remove_file(&backup);
    }

    Ok(())
}
