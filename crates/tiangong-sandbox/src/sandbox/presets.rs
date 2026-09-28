//! 可选的宿主策略预设。
//!
//! 通用核心不会自动加入任何路径；宿主可采用这些预设，也可完全传入自己的
//! `protected_paths` 与 `denied_read_paths`。

use std::path::{Path, PathBuf};

use super::policy::SandboxPolicy;

/// 常见用户凭据位置。是否禁止读取由宿主决定。
pub fn common_credential_paths() -> Vec<PathBuf> {
    let Some(home) = user_home_dir() else {
        return Vec::new();
    };
    [
        ".ssh",
        ".aws",
        ".gnupg",
        ".kube",
        ".docker",
        ".azure",
        ".config/gcloud",
        ".config/gh",
        ".netrc",
    ]
    .iter()
    .map(|path| home.join(path))
    .collect()
}

/// 天工 App 的敏感路径预设。`storage_root` 与其中的 `sandbox` 目录均由 App
/// 选择；Sandbox 核心不推导该布局。
pub fn apply_tiangong(policy: &mut SandboxPolicy, storage_root: &Path) {
    policy.denied_read_paths.extend(common_credential_paths());
    policy.denied_read_paths.extend(
        [
            "keys",
            "trust.db",
            "mcp.json",
            "models.json",
            "auth",
            "server.json",
            "app.json",
            "sandbox",
        ]
        .iter()
        .map(|path| storage_root.join(path)),
    );
}

fn user_home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let value = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            Some(PathBuf::from(drive).join(path))
        });
    #[cfg(not(windows))]
    let value = std::env::var_os("HOME").map(PathBuf::from);
    value.filter(|path| path.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiangong_preset_is_explicit() {
        let root = tempfile::tempdir().unwrap();
        let mut policy = SandboxPolicy::workspace_write("/workspace");
        assert!(policy.denied_read_paths.is_empty());
        apply_tiangong(&mut policy, root.path());
        assert!(policy.denied_read_paths.contains(&root.path().join("keys")));
        // 账号登录凭据（如 ChatGPT OAuth 令牌）默认禁读。
        assert!(policy.denied_read_paths.contains(&root.path().join("auth")));
        assert!(
            policy
                .denied_read_paths
                .contains(&root.path().join("sandbox"))
        );
    }
}
