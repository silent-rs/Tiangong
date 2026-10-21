//! 已安装 Sandbox 的签名与自检验证。
//!
//! 生产 Sandbox 只信任本 crate 内置的独立官方公钥，不接受插件用户密钥、
//! 第三方插件公钥或任何运行时配置覆盖。

use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use minisign_verify::{PublicKey, Signature};

const OFFICIAL_PUBKEY_B64: &str = include_str!("official-pubkey.b64");

/// 验证固定路径程序的普通文件形态与官方 minisign 签名。
pub fn verify_official_signature(program: &Path) -> Result<()> {
    verify_signature_with_public_key(program, OFFICIAL_PUBKEY_B64)
}

/// 使用宿主显式提供的 base64 minisign 公钥验证程序。
pub fn verify_signature_with_public_key(program: &Path, public_key_b64: &str) -> Result<()> {
    ensure_regular_file(program, "Sandbox 程序")?;
    let signature_path = crate::launcher_manager::signature_path(program);
    ensure_regular_file(&signature_path, "Sandbox 签名")?;
    let public_text = base64::engine::general_purpose::STANDARD
        .decode(public_key_b64.trim())
        .context("解析 Sandbox 公钥失败")?;
    let public =
        PublicKey::decode(&String::from_utf8(public_text)?).context("解析 Sandbox 公钥失败")?;
    let signature_text = base64::engine::general_purpose::STANDARD
        .decode(std::fs::read_to_string(&signature_path)?.trim())?;
    let signature =
        Signature::decode(&String::from_utf8(signature_text)?).context("解析 Sandbox 签名失败")?;
    public
        .verify(&std::fs::read(program)?, &signature, false)
        .context("Sandbox 签名验证不通过")
}

/// 官方验签后执行真实自检，并返回产品版本。
pub fn verify_official_install(program: &Path) -> Result<String> {
    verify_official_signature(program)?;
    let mut command = std::process::Command::new(program);
    command.arg("--self-check");
    let output = crate::configure_no_window(&mut command)
        .output()
        .with_context(|| format!("运行 Sandbox 自检失败: {}", program.display()))?;
    if !output.status.success() && output.status.code() != Some(79) {
        bail!(
            "Sandbox 自检失败（退出码 {}）：{}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("解析 Sandbox 自检报告失败")?;
    report["product_version"]
        .as_str()
        .filter(|version| !version.is_empty())
        .map(ToOwned::to_owned)
        .context("Sandbox 自检报告缺少产品版本")
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("{label}不存在: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{label}必须是普通文件: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn symlink_program_is_rejected_before_signature_read() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let link = root.path().join("tiangong-sandbox");
        std::fs::write(&target, b"sandbox").unwrap();
        std::os::unix::fs::symlink(target, &link).unwrap();
        assert!(verify_official_signature(&link).is_err());
    }

    #[test]
    fn unsigned_program_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("tiangong-sandbox");
        std::fs::write(&program, b"sandbox").unwrap();
        assert!(verify_official_signature(&program).is_err());
    }
}
