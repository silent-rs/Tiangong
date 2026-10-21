//! 插件发布清单数字签名与制品完整性验证。

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};

use crate::manifest::{MANIFEST_FILE, PluginManifest, SidecarRuntime};
use crate::sidecar::CONTENT_MANIFEST_FILE;

pub use tiangong_plugin_sidecar::release::{
    OFFICIAL_PUBKEY_B64, SIGNATURE_FILE, SIGNED_RELEASE_FILE, SIGNED_RELEASE_SCHEMA_VERSION,
    SignedArtifact, SignedPluginRelease,
};

/// 验证签名发布清单及其覆盖的当前平台制品。
///
/// 缺少签名文件返回 `Ok(None)`，表示普通未签名插件；任何签名或制品不一致均返回错误。
pub fn verify_signed_release(
    directory: &Path,
    plugin_manifest: &PluginManifest,
) -> Result<Option<SignedPluginRelease>> {
    let release_path = directory.join(SIGNED_RELEASE_FILE);
    let signature_path = directory.join(SIGNATURE_FILE);
    let release_exists = release_path.is_file();
    let signature_exists = signature_path.is_file();
    if !release_exists && !signature_exists {
        return Ok(None);
    }
    if !release_exists || !signature_exists {
        bail!("插件签名文件不完整");
    }

    let release_bytes = std::fs::read(&release_path)
        .with_context(|| format!("读取插件签名清单失败: {}", release_path.display()))?;
    // 先解析拿到发布者标识，再按信任根路由验签（官方内置 / 本机用户
    // 密钥 / 已导入第三方公钥——见 trust.rs）。
    let release: SignedPluginRelease =
        serde_json::from_slice(&release_bytes).context("解析插件签名清单失败")?;
    verify_minisign_for_publisher(
        &release.publisher,
        directory,
        &release_bytes,
        &std::fs::read_to_string(&signature_path)?,
    )?;
    release.validate(directory, plugin_manifest)?;
    Ok(Some(release))
}

trait ValidateSignedRelease {
    fn validate(&self, directory: &Path, plugin_manifest: &PluginManifest) -> Result<()>;
}

impl ValidateSignedRelease for SignedPluginRelease {
    fn validate(&self, directory: &Path, plugin_manifest: &PluginManifest) -> Result<()> {
        if self.schema_version != SIGNED_RELEASE_SCHEMA_VERSION {
            bail!("插件签名清单版本不支持: {}", self.schema_version);
        }
        // 发布者格式校验（合法标识由路由侧解析为对应信任根；保留标识
        // 与三方标识均在 trust.rs 路由，未导入即验签失败）。
        if self.publisher != crate::trust::OFFICIAL_PUBLISHER
            && self.publisher != crate::trust::LOCAL_PUBLISHER
        {
            crate::trust::validate_publisher_id(&self.publisher)
                .with_context(|| "插件签名发布者无效")?;
        }
        if self.id != plugin_manifest.id || self.version != plugin_manifest.version {
            bail!("插件签名清单与 plugin.json 的 ID 或版本不一致");
        }
        if self.permissions.iter().any(|item| item.trim().is_empty())
            || permission_set(&self.permissions)?.len() != self.permissions.len()
        {
            bail!("插件签名清单包含空权限或重复权限");
        }
        if plugin_manifest
            .permissions
            .iter()
            .any(|item| item.trim().is_empty())
            || permission_set(&plugin_manifest.permissions)?.len()
                != plugin_manifest.permissions.len()
        {
            bail!("plugin.json 包含空权限或重复权限");
        }
        if permission_set(&self.permissions)? != permission_set(&plugin_manifest.permissions)? {
            bail!("插件签名清单与 plugin.json 的权限声明不一致");
        }
        self.manifest.verify(directory, Path::new(MANIFEST_FILE))?;
        // wasm 条目与 plugin.json 声明一致才校验；纯 TS/sidecar 插件两边都缺省。
        match (&self.wasm, plugin_manifest.wasm_binary()) {
            (Some(signed_wasm), Some(wasm_binary)) => signed_wasm.verify(directory, wasm_binary)?,
            (None, None) => {}
            _ => bail!("插件签名清单与 plugin.json 的 wasm 声明不一致"),
        }
        let signed_ui = self
            .ui
            .iter()
            .map(|artifact| artifact.path.clone())
            .collect::<BTreeSet<_>>();
        if signed_ui.len() != self.ui.len() {
            bail!("插件签名清单包含重复的 UI 制品");
        }
        plugin_manifest.validate_ui_artifacts(&signed_ui)?;
        for artifact in &self.ui {
            artifact.verify(directory, &artifact.path)?;
        }
        // 插件声明 sidecar 时，官方签名必须同时授权调用（两种形态一致）。
        if plugin_manifest.sidecar.is_some() && !self.has_permission("sidecar.invoke") {
            bail!("插件签名清单未授权 sidecar.invoke");
        }
        match (&self.sidecar, &plugin_manifest.sidecar) {
            (Some(signed), Some(sidecar)) => match sidecar.runtime {
                SidecarRuntime::Native => {
                    if self.content_manifest.is_some() {
                        bail!("native sidecar 签名清单不得包含 content_manifest 条目");
                    }
                    let binary = sidecar.binary.as_ref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "插件 {} native sidecar 缺少 binary 声明",
                            plugin_manifest.id
                        )
                    })?;
                    signed.verify(directory, &sidecar_binary_path(binary)?)?;
                }
                SidecarRuntime::Node | SidecarRuntime::Python => {
                    bail!(
                        "解释器 sidecar 签名清单不得包含二进制条目（信任由 content_manifest 承载）"
                    );
                }
            },
            (None, None) => {
                // local 创作链（用户密钥）对全部模板形态锚定内容清单——
                // 签名锚定清单本身，再按清单双向校验全树，与解释器 sidecar
                // 形态同强度。官方/三方无 sidecar 形态不带该条目（下方
                // 发布约定检查兜底）。
                if let Some(signed_manifest) = self.content_manifest.as_ref() {
                    signed_manifest.verify(directory, Path::new(CONTENT_MANIFEST_FILE))?;
                    crate::sidecar::SidecarConfig::verify_integrity_manifest(
                        &directory.join(CONTENT_MANIFEST_FILE),
                        directory,
                    )?;
                }
            }
            // 解释器官方形态：签名无二进制条目，信任锚是完整内容清单——
            // 先校验清单文件哈希与签名一致，再按清单双向校验全树。
            (None, Some(sidecar)) if sidecar.runtime != SidecarRuntime::Native => {
                let signed_manifest = self.content_manifest.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("官方解释器插件缺少 content_manifest 签名条目")
                })?;
                signed_manifest.verify(directory, Path::new(CONTENT_MANIFEST_FILE))?;
                crate::sidecar::SidecarConfig::verify_integrity_manifest(
                    &directory.join(CONTENT_MANIFEST_FILE),
                    directory,
                )?;
            }
            _ => bail!("插件签名清单与 plugin.json 的 sidecar 声明不一致"),
        }
        // 内容清单条目约定：非 local 发布者仅允许解释器形态携带（官方/
        // 三方发布约定）；local 创作链（用户密钥）对全部模板形态锚定
        // 内容清单——自制插件的信任根就是这份全树锚定。
        if self.content_manifest.is_some()
            && self.publisher != crate::trust::LOCAL_PUBLISHER
            && plugin_manifest
                .sidecar
                .as_ref()
                .is_none_or(|sidecar| sidecar.runtime == SidecarRuntime::Native)
        {
            bail!("content_manifest 签名条目仅允许解释器 sidecar 形态使用");
        }
        Ok(())
    }
}

trait VerifySignedArtifact {
    fn verify(&self, directory: &Path, expected_path: &Path) -> Result<()>;
}

impl VerifySignedArtifact for SignedArtifact {
    fn verify(&self, directory: &Path, expected_path: &Path) -> Result<()> {
        validate_relative_path(&self.path)?;
        if self.path != expected_path {
            bail!(
                "签名制品路径不一致: expected={}, actual={}",
                expected_path.display(),
                self.path.display()
            );
        }
        let actual = sha256_file(&directory.join(&self.path))?;
        if !actual.eq_ignore_ascii_case(self.sha256.trim_start_matches("sha256:")) {
            bail!("签名制品校验失败: {}", self.path.display());
        }
        Ok(())
    }
}

fn permission_set(permissions: &[String]) -> Result<BTreeSet<&str>> {
    Ok(permissions.iter().map(String::as_str).collect())
}

/// 按发布者路由信任根验签（官方内置 / 本机用户密钥 / 已导入第三方公钥）。
/// 官方信任根唯一且不可配置；本机与三方信任根按插件目录推导存储位置。
fn verify_minisign_for_publisher(
    publisher: &str,
    directory: &Path,
    content: &[u8],
    signature_b64: &str,
) -> Result<()> {
    // 官方形态直用内置公钥（唯一信任根，不依赖本地存储布局）。
    let pubkey_b64 = if publisher == crate::trust::OFFICIAL_PUBLISHER {
        crate::signature::OFFICIAL_PUBKEY_B64.to_string()
    } else {
        let storage_root = crate::trust::storage_root_of(directory)?;
        crate::trust::resolve_publisher_pubkey(storage_root, publisher)?
    };
    verify_minisign_with_pubkey(content, signature_b64, &pubkey_b64)
}

/// 以显式公钥验签（不读任何全局状态；测试与路由层共用）。
fn verify_minisign_with_pubkey(
    content: &[u8],
    signature_b64: &str,
    pubkey_b64: &str,
) -> Result<()> {
    let public_text = base64::engine::general_purpose::STANDARD
        .decode(pubkey_b64.trim())
        .context("解析插件信任公钥失败")?;
    let public_text = String::from_utf8(public_text).context("插件信任公钥非 UTF-8")?;
    let public_key = PublicKey::decode(&public_text).context("解析插件公钥失败")?;
    let signature_text = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .context("解析插件签名失败")?;
    let signature_text = String::from_utf8(signature_text).context("插件签名非 UTF-8")?;
    let signature = Signature::decode(&signature_text).context("解析 minisign 插件签名失败")?;
    public_key
        .verify(content, &signature, false)
        .context("插件签名验证不通过")
}

/// 以内置官方公钥验签（Launcher 在线更新激活门禁专用：在线制品必须
/// 官方根，不接受本机用户密钥根）。
pub fn verify_minisign_with_official_pubkey(content: &[u8], signature_b64: &str) -> Result<()> {
    verify_minisign_with_pubkey(content, signature_b64, OFFICIAL_PUBKEY_B64)
}

/// 沙箱 Launcher 的伴生签名文件（与 Launcher 同路径，内容为
/// base64 包装的 minisign 签名文本，与插件 `.sig` 格式一致）。
pub const LAUNCHER_SIGNATURE_SUFFIX: &str = ".sig";

/// 验证沙箱 Launcher 签名：官方内置公钥与本机用户密钥任一通过即可。
///
/// Launcher 是安全边界的执行者，其信任锚必须高于沙箱自身（沙箱内进程
/// 够不到它），因此启动前逐次验签、失败即拒（fail-closed）；签名文件
/// 缺失同样拒绝。签名在发布时由 `xtask prepare-sandbox-release` 施加
/// （在线更新链携带），本地开发由 `xtask sign-sandbox` 施加，与插件
/// 发布共用同一把官方私钥。
pub fn verify_launcher_signature(launcher_path: &Path, storage_root: &Path) -> Result<()> {
    let signature_path = launcher_path.with_file_name(format!(
        "{}{LAUNCHER_SIGNATURE_SUFFIX}",
        launcher_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("Launcher 路径文件名无效")?
    ));
    let content = std::fs::read(launcher_path)
        .with_context(|| format!("读取沙箱 Launcher 失败: {}", launcher_path.display()))?;
    let signature_b64 = std::fs::read_to_string(&signature_path)
        .with_context(|| format!("沙箱 Launcher 缺少签名文件: {}", signature_path.display()))?;

    let official = verify_minisign_with_pubkey(&content, &signature_b64, OFFICIAL_PUBKEY_B64);
    if official.is_ok() {
        return Ok(());
    }
    let user = crate::trust::user_public_key_b64(storage_root)
        .and_then(|pubkey| verify_minisign_with_pubkey(&content, &signature_b64, &pubkey));
    match user {
        Ok(()) => Ok(()),
        Err(user_error) => {
            let official_error = official
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_else(|| "官方根已通过但本机根失败（不应发生）".to_string());
            Err(anyhow::anyhow!(
                "沙箱 Launcher 签名验证不通过（官方与本机信任根均拒绝）: 官方根: {official_error}; 本机根: {user_error:#}"
            ))
        }
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("读取签名制品失败: {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("签名制品路径必须是安全相对路径");
    }
    Ok(())
}

fn sidecar_binary_path(path: &Path) -> Result<PathBuf> {
    let mut path = path.to_path_buf();
    if !std::env::consts::EXE_SUFFIX.is_empty() {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .context("sidecar 文件名无效")?;
        if !name.ends_with(std::env::consts::EXE_SUFFIX) {
            path.set_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PluginManifest;

    /// Launcher 验签：用户密钥根通过、篡改拒绝、缺签名拒绝、官方根外的
    /// 签名拒绝。签名经 trust::sign_with_user_key 真实生成。
    #[test]
    fn launcher_signature_verification_paths() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        let launcher = root.path().join("tiangong-sandbox");
        std::fs::write(&launcher, b"launcher-bytes-v1").unwrap();

        // 无签名文件：拒绝。
        let error = verify_launcher_signature(&launcher, &storage).unwrap_err();
        assert!(error.to_string().contains("缺少签名文件"), "{error:#}");

        // 用户密钥签名：通过。
        crate::trust::sign_with_user_key(&storage, &launcher).unwrap();
        verify_launcher_signature(&launcher, &storage).unwrap();

        // 篡改 Launcher 内容：拒绝。
        std::fs::write(&launcher, b"launcher-bytes-tampered").unwrap();
        let error = verify_launcher_signature(&launcher, &storage).unwrap_err();
        assert!(error.to_string().contains("签名验证不通过"), "{error:#}");

        // 恢复内容并换一个不受信任密钥的签名：拒绝（两个根都不认）。
        std::fs::write(&launcher, b"launcher-bytes-v1").unwrap();
        let rogue = tempfile::tempdir().unwrap();
        std::fs::write(&launcher, b"launcher-bytes-v1").unwrap();
        crate::trust::sign_with_user_key(rogue.path(), &launcher).unwrap();
        let error = verify_launcher_signature(&launcher, &storage).unwrap_err();
        assert!(error.to_string().contains("签名验证不通过"), "{error:#}");
    }

    /// devkit 同款内容清单：受管全树（排除清单自身与签名文件）路径 + sha256。
    fn write_content_manifest(dir: &Path) {
        let mut files = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(directory) = stack.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                    continue;
                }
                let path = entry.path();
                let relative = path.strip_prefix(dir).unwrap().display().to_string();
                if relative == CONTENT_MANIFEST_FILE
                    || relative == SIGNED_RELEASE_FILE
                    || relative == SIGNATURE_FILE
                {
                    continue;
                }
                let raw = std::fs::read(&path).unwrap();
                files.push(serde_json::json!({
                    "path": relative.replace('\\', "/"),
                    "sha256": hex::encode(Sha256::digest(&raw)),
                }));
            }
        }
        std::fs::write(
            dir.join(CONTENT_MANIFEST_FILE),
            serde_json::to_vec(&serde_json::json!({"algorithm": "sha256", "files": files}))
                .unwrap(),
        )
        .unwrap();
    }

    /// 官方解释器插件目录：plugin.json + UI 入口 + node sidecar 入口 + 模板
    /// 资源 + 内容清单。
    fn setup_official_interpreter_plugin(root: &Path) -> (PathBuf, PluginManifest) {
        let dir = root.join("plugins").join("official-demo");
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::create_dir_all(dir.join("sidecar")).unwrap();
        std::fs::create_dir_all(dir.join("templates")).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            r#"{"schema_version":2,"id":"official-demo","version":"0.2.0","permissions":["sidecar.invoke"],"ui":{"contributions":[{"slot":"extension.tab","id":"app","entry":"dist/index.html"}]},"sidecar":{"runtime":"node","entry":"sidecar/main.mjs"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("dist/index.html"), "<html>demo</html>").unwrap();
        std::fs::write(dir.join("sidecar/main.mjs"), "// sidecar bundle").unwrap();
        std::fs::write(dir.join("templates/node-tool.txt"), "template").unwrap();
        write_content_manifest(&dir);
        let manifest = PluginManifest::load(&dir.join(MANIFEST_FILE)).expect("清单合法");
        (dir, manifest)
    }

    fn artifact(dir: &Path, path: &str) -> SignedArtifact {
        SignedArtifact {
            path: PathBuf::from(path),
            sha256: sha256_file(&dir.join(path)).unwrap(),
        }
    }

    #[test]
    fn static_ui_resources_are_verified_with_the_page() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let manifest: PluginManifest = serde_json::from_str(
            r#"{"schema_version":2,"id":"static-ui","version":"1.0.0","permissions":[],"resources":["ui"],"ui":{"contributions":[{"slot":"settings.plugin-page","id":"settings","entry":"ui/settings.html"}]}}"#,
        ).unwrap();
        manifest.validate().unwrap();
        std::fs::create_dir(dir.join("ui")).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        for name in ["ui/settings.html", "ui/settings.css", "ui/settings.js"] {
            std::fs::write(dir.join(name), name).unwrap();
        }
        let release = SignedPluginRelease {
            schema_version: SIGNED_RELEASE_SCHEMA_VERSION,
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            publisher: crate::trust::OFFICIAL_PUBLISHER.into(),
            permissions: vec![],
            manifest: artifact(dir, "plugin.json"),
            wasm: None,
            ui: ["ui/settings.html", "ui/settings.css", "ui/settings.js"]
                .iter()
                .map(|name| artifact(dir, name))
                .collect(),
            sidecar: None,
            content_manifest: None,
        };
        release.validate(dir, &manifest).unwrap();
        std::fs::write(dir.join("ui/settings.js"), "tampered").unwrap();
        assert!(
            release
                .validate(dir, &manifest)
                .unwrap_err()
                .to_string()
                .contains("校验失败")
        );
    }

    /// 生成密钥对、按指定发布者构造签名清单并落盘签名；返回测试公钥
    /// （base64 格式，供导入第三方信任根或做冒充官方的负面测试）。
    fn sign_release_for_publisher(
        dir: &Path,
        manifest: &PluginManifest,
        publisher: &str,
        mutate: impl FnOnce(&mut SignedPluginRelease),
    ) -> String {
        let mut release = SignedPluginRelease {
            schema_version: SIGNED_RELEASE_SCHEMA_VERSION,
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            publisher: publisher.to_string(),
            permissions: manifest.permissions.clone(),
            manifest: artifact(dir, MANIFEST_FILE),
            wasm: None,
            ui: manifest
                .ui_contributions()
                .into_iter()
                .map(|contribution| artifact(dir, &contribution.entry))
                .collect(),
            sidecar: None,
            content_manifest: Some(artifact(dir, CONTENT_MANIFEST_FILE)),
        };
        mutate(&mut release);
        let bytes = serde_json::to_vec_pretty(&release).unwrap();
        std::fs::write(dir.join(SIGNED_RELEASE_FILE), &bytes).unwrap();
        let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let signature =
            minisign::sign(Some(&keypair.pk), &keypair.sk, bytes.as_slice(), None, None).unwrap();
        // 与 tauri signer 输出一致：两行签名文本整体 base64 包装（verify_minisign 读取格式）。
        let signature_text = signature.into_string();
        std::fs::write(
            dir.join(SIGNATURE_FILE),
            base64::engine::general_purpose::STANDARD.encode(signature_text),
        )
        .unwrap();
        base64::engine::general_purpose::STANDARD.encode(keypair.pk.to_box().unwrap().into_string())
    }

    fn assert_verify_ok(dir: &Path, manifest: &PluginManifest) {
        verify_signed_release(dir, manifest).expect("官方签名验证应通过");
    }

    fn assert_verify_err(dir: &Path, manifest: &PluginManifest, needle: &str) {
        let error = verify_signed_release(dir, manifest).expect_err("官方签名验证应被拒绝");
        assert!(error.to_string().contains(needle), "{error:#}");
    }

    #[test]
    #[serial_test::serial]
    fn 官方信任根不可替换_测试密钥冒充官方被拒() {
        let root = tempfile::tempdir().unwrap();
        let (dir, manifest) = setup_official_interpreter_plugin(root.path());
        // 测试密钥签署 publisher=tiangong-official 的清单（签名格式合法、
        // 内容清单完备）：官方信任根是内置公钥，必须拒绝。
        let key = sign_release_for_publisher(&dir, &manifest, "tiangong-official", |_| {});
        assert!(
            verify_signed_release(&dir, &manifest).is_err(),
            "测试密钥不得被识别为官方密钥"
        );
        // 环境残留同名变量不影响结果（运行时已完全忽略该变量——此处保留
        // 攻击形态断言，证明覆盖通道已不存在）。
        let previous = std::env::var("TIANGONG_PLUGIN_PUBKEY_B64").ok();
        unsafe {
            std::env::set_var("TIANGONG_PLUGIN_PUBKEY_B64", &key);
        }
        let still_rejected = verify_signed_release(&dir, &manifest).is_err();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("TIANGONG_PLUGIN_PUBKEY_B64", value),
                None => std::env::remove_var("TIANGONG_PLUGIN_PUBKEY_B64"),
            }
        }
        assert!(still_rejected, "官方信任根不得被环境变量替换");
    }

    #[test]
    #[serial_test::serial]
    fn 本机用户签名_验签通过并覆盖全树() {
        let root = tempfile::tempdir().unwrap();
        let (dir, manifest) = setup_official_interpreter_plugin(root.path());
        // 创作链同款：用户密钥自动生成 + local 发布者签名清单。
        crate::trust::ensure_user_signing_key(root.path()).unwrap();
        let mut release = SignedPluginRelease {
            schema_version: SIGNED_RELEASE_SCHEMA_VERSION,
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            publisher: crate::trust::LOCAL_PUBLISHER.to_string(),
            permissions: manifest.permissions.clone(),
            manifest: artifact(&dir, MANIFEST_FILE),
            wasm: None,
            ui: manifest
                .ui_contributions()
                .into_iter()
                .map(|contribution| artifact(&dir, &contribution.entry))
                .collect(),
            sidecar: None,
            content_manifest: Some(artifact(&dir, CONTENT_MANIFEST_FILE)),
        };
        release.manifest = artifact(&dir, MANIFEST_FILE);
        let bytes = serde_json::to_vec_pretty(&release).unwrap();
        std::fs::write(dir.join(SIGNED_RELEASE_FILE), &bytes).unwrap();
        crate::trust::sign_with_user_key(root.path(), &dir.join(SIGNED_RELEASE_FILE)).unwrap();
        verify_signed_release(&dir, &manifest)
            .expect("用户签名（local 发布者）应验签通过")
            .expect("应返回签名清单");
        // 篡改 sidecar 后用户签名同样拒绝（全树校验与官方一致）。
        std::fs::write(dir.join("sidecar/main.mjs"), "// tampered").unwrap();
        assert!(verify_signed_release(&dir, &manifest).is_err());
    }

    #[test]
    #[serial_test::serial]
    fn 三方发布者_导入公钥后验签_未导入拒绝() {
        let root = tempfile::tempdir().unwrap();
        let (dir, manifest) = setup_official_interpreter_plugin(root.path());
        let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let release = SignedPluginRelease {
            schema_version: SIGNED_RELEASE_SCHEMA_VERSION,
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            publisher: "acme-dev".to_string(),
            permissions: manifest.permissions.clone(),
            manifest: artifact(&dir, MANIFEST_FILE),
            wasm: None,
            ui: manifest
                .ui_contributions()
                .into_iter()
                .map(|contribution| artifact(&dir, &contribution.entry))
                .collect(),
            sidecar: None,
            content_manifest: Some(artifact(&dir, CONTENT_MANIFEST_FILE)),
        };
        let bytes = serde_json::to_vec_pretty(&release).unwrap();
        std::fs::write(dir.join(SIGNED_RELEASE_FILE), &bytes).unwrap();
        let signature =
            minisign::sign(Some(&keypair.pk), &keypair.sk, bytes.as_slice(), None, None).unwrap();
        use base64::Engine;
        std::fs::write(
            dir.join(SIGNATURE_FILE),
            base64::engine::general_purpose::STANDARD.encode(signature.into_string()),
        )
        .unwrap();

        // 未导入公钥：拒绝并给出导入指引。
        let error = verify_signed_release(&dir, &manifest).expect_err("未导入三方公钥应拒绝");
        assert!(format!("{error:#}").contains("未导入"), "{error:#}");

        // 导入后验签通过。
        let public_b64 = base64::engine::general_purpose::STANDARD
            .encode(keypair.pk.to_box().unwrap().into_string());
        crate::trust::import_trusted_publisher(root.path(), "acme-dev", &public_b64).unwrap();
        verify_signed_release(&dir, &manifest)
            .expect("导入公钥后三方插件应验签通过")
            .expect("应返回签名清单");

        // 移除公钥后失效。
        crate::trust::remove_trusted_publisher(root.path(), "acme-dev").unwrap();
        assert!(verify_signed_release(&dir, &manifest).is_err());
    }

    #[test]
    #[serial_test::serial]
    fn 三方解释器插件_验签通过并覆盖全树() {
        let root = tempfile::tempdir().unwrap();
        let (dir, manifest) = setup_official_interpreter_plugin(root.path());
        let key = sign_release_for_publisher(&dir, &manifest, "acme-dev", |_| {});
        crate::trust::import_trusted_publisher(root.path(), "acme-dev", &key).unwrap();
        assert_verify_ok(&dir, &manifest);
    }

    #[test]
    #[serial_test::serial]
    fn 三方解释器插件_缺少内容清单条目拒绝() {
        let root = tempfile::tempdir().unwrap();
        let (dir, manifest) = setup_official_interpreter_plugin(root.path());
        let key = sign_release_for_publisher(&dir, &manifest, "acme-dev", |release| {
            release.content_manifest = None;
        });
        crate::trust::import_trusted_publisher(root.path(), "acme-dev", &key).unwrap();
        assert_verify_err(&dir, &manifest, "content_manifest");
    }

    #[test]
    #[serial_test::serial]
    fn 三方解释器插件_内容清单条目路径不是固定文件名拒绝() {
        let root = tempfile::tempdir().unwrap();
        let (dir, manifest) = setup_official_interpreter_plugin(root.path());
        let key = sign_release_for_publisher(&dir, &manifest, "acme-dev", |release| {
            release.content_manifest.as_mut().unwrap().path = PathBuf::from("sidecar/main.mjs");
        });
        crate::trust::import_trusted_publisher(root.path(), "acme-dev", &key).unwrap();
        assert_verify_err(&dir, &manifest, "签名制品路径不一致");
    }

    #[test]
    #[serial_test::serial]
    fn 三方解释器插件_篡改内容清单拒绝() {
        let root = tempfile::tempdir().unwrap();
        let (dir, manifest) = setup_official_interpreter_plugin(root.path());
        let key = sign_release_for_publisher(&dir, &manifest, "acme-dev", |_| {});
        std::fs::write(dir.join(CONTENT_MANIFEST_FILE), "{}").unwrap();
        crate::trust::import_trusted_publisher(root.path(), "acme-dev", &key).unwrap();
        assert_verify_err(&dir, &manifest, "校验失败");
    }

    #[test]
    #[serial_test::serial]
    fn 三方解释器插件_篡改sidecar入口拒绝() {
        let root = tempfile::tempdir().unwrap();
        let (dir, manifest) = setup_official_interpreter_plugin(root.path());
        let key = sign_release_for_publisher(&dir, &manifest, "acme-dev", |_| {});
        std::fs::write(dir.join("sidecar/main.mjs"), "// tampered").unwrap();
        crate::trust::import_trusted_publisher(root.path(), "acme-dev", &key).unwrap();
        assert_verify_err(&dir, &manifest, "不一致");
    }

    #[test]
    #[serial_test::serial]
    fn 三方解释器插件_篡改模板资源拒绝() {
        let root = tempfile::tempdir().unwrap();
        let (dir, manifest) = setup_official_interpreter_plugin(root.path());
        let key = sign_release_for_publisher(&dir, &manifest, "acme-dev", |_| {});
        std::fs::write(dir.join("templates/node-tool.txt"), "tampered").unwrap();
        crate::trust::import_trusted_publisher(root.path(), "acme-dev", &key).unwrap();
        assert_verify_err(&dir, &manifest, "不一致");
    }

    #[test]
    #[serial_test::serial]
    fn 三方解释器插件_添加未列出文件拒绝() {
        let root = tempfile::tempdir().unwrap();
        let (dir, manifest) = setup_official_interpreter_plugin(root.path());
        let key = sign_release_for_publisher(&dir, &manifest, "acme-dev", |_| {});
        std::fs::write(dir.join("extra-payload.mjs"), "unknown file").unwrap();
        crate::trust::import_trusted_publisher(root.path(), "acme-dev", &key).unwrap();
        assert_verify_err(&dir, &manifest, "未覆盖");
    }

    #[test]
    #[serial_test::serial]
    fn 三方解释器插件_签名携带sidecar二进制条目拒绝() {
        let root = tempfile::tempdir().unwrap();
        let (dir, manifest) = setup_official_interpreter_plugin(root.path());
        let key = sign_release_for_publisher(&dir, &manifest, "acme-dev", |release| {
            release.sidecar = Some(artifact(&dir, "sidecar/main.mjs"));
        });
        crate::trust::import_trusted_publisher(root.path(), "acme-dev", &key).unwrap();
        assert_verify_err(&dir, &manifest, "二进制条目");
    }

    #[test]
    #[serial_test::serial]
    fn 官方native插件_携带内容清单条目拒绝() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("plugins").join("native-demo");
        std::fs::create_dir_all(dir.join("sidecar")).unwrap();
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            r#"{"schema_version":2,"id":"native-demo","version":"0.2.0","permissions":["sidecar.invoke"],"ui":{"contributions":[{"slot":"extension.tab","id":"app","entry":"dist/index.html"}]},"sidecar":{"runtime":"native","binary":"sidecar/demo-bin"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("dist/index.html"), "<html></html>").unwrap();
        std::fs::write(dir.join("sidecar/demo-bin"), b"fake native binary").unwrap();
        write_content_manifest(&dir);
        let manifest = PluginManifest::load(&dir.join(MANIFEST_FILE)).unwrap();
        let key = sign_release_for_publisher(&dir, &manifest, "acme-dev", |release| {
            release.sidecar = Some(artifact(&dir, "sidecar/demo-bin"));
        });
        crate::trust::import_trusted_publisher(root.path(), "acme-dev", &key).unwrap();
        assert_verify_err(&dir, &manifest, "content_manifest 条目");
    }

    #[test]
    #[serial_test::serial]
    fn 纯ui插件_携带内容清单条目拒绝() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("plugins").join("ui-demo");
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            r#"{"schema_version":2,"id":"ui-demo","version":"0.2.0","permissions":[],"ui":{"contributions":[{"slot":"extension.tab","id":"app","entry":"dist/index.html"}]}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("dist/index.html"), "<html></html>").unwrap();
        write_content_manifest(&dir);
        let manifest = PluginManifest::load(&dir.join(MANIFEST_FILE)).unwrap();
        let key = sign_release_for_publisher(&dir, &manifest, "acme-dev", |_| {});
        crate::trust::import_trusted_publisher(root.path(), "acme-dev", &key).unwrap();
        assert_verify_err(&dir, &manifest, "仅允许解释器 sidecar 形态");
    }
}
