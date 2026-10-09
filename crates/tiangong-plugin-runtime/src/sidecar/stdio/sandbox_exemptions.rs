//! 沙箱策略的宿主授权豁免。

use crate::sidecar::SensitiveStorageAccess;

/// 按宿主授权从禁读清单移除对应配置文件（仅读开放；写保护不动）。
pub(super) fn exempt_authorized_reads(
    policy: &mut tiangong_sandbox::SandboxPolicy,
    access: SensitiveStorageAccess,
    storage_root: &std::path::Path,
) {
    let mut exemptions: Vec<std::path::PathBuf> = Vec::new();
    if access.model_config {
        exemptions.push(storage_root.join("models.json"));
        // 模型配置中的 OAuth 供应商（ChatGPT）凭据存于 auth/，随模型配置读取授权开放。
        exemptions.push(storage_root.join("auth"));
    }
    if access.mcp_config {
        exemptions.push(storage_root.join("mcp.json"));
    }
    if access.server_config {
        exemptions.push(storage_root.join("server.json"));
    }
    if access.app_config {
        exemptions.push(storage_root.join("app.json"));
    }
    if exemptions.is_empty() {
        return;
    }
    policy
        .denied_read_paths
        .retain(|path| !exemptions.contains(path));
}

/// mcp.json 的写豁免：它是 MCP 配置的权威存储，插件中唯一合法写者是
/// mcp 插件自身（宿主已验证官方签名身份；宿主进程为 Bot 登记 MCP 时也
/// 直接写入，不受沙箱约束）。从写保护清单移除后，写权限随存储根整体
/// 可写恢复；其他插件对 mcp.json 的写保护不变。
pub(super) fn exempt_mcp_config_write(
    policy: &mut tiangong_sandbox::SandboxPolicy,
    storage_root: &std::path::Path,
) {
    let target = storage_root.join("mcp.json");
    policy.protected_paths.retain(|path| *path != target);
}

/// 天工宿主的 Launcher 解析：存储目录直存优先，宿主程序同目录（开发与
/// 测试布局）兜底。P1 通用化后组合策略由宿主决定，crate 只提供原语。
pub(super) fn resolve_launcher(storage_root: &std::path::Path) -> Option<std::path::PathBuf> {
    tiangong_sandbox::launcher_manager::resolve_installed_program(&storage_root.join("sandbox"))
        .or_else(tiangong_sandbox::launcher_manager::sibling_program)
}

/// 天工宿主的 `protected_paths`（读写双禁）组合：存储配置信任件 + 家目录
/// 凭据 + 宿主验证记录目录（sidecar 能力快照由宿主维护，插件不得读写
/// 伪造）。P1 通用化后预设组合归宿主，crate 只提供通用原语。
pub(super) fn tiangong_protected_paths(storage_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<std::path::PathBuf> = [
        "keys",
        "trust.db",
        "mcp.json",
        "models.json",
        // 账号登录凭据（ChatGPT OAuth 令牌）：只由宿主写入与续期。
        "auth",
        "server.json",
        "app.json",
        "sandbox",
        "plugins/.verifications",
    ]
    .iter()
    .map(|path| storage_root.join(path))
    .collect();
    paths.extend(tiangong_sandbox::sandbox::presets::common_credential_paths());
    paths
}

/// 按宿主验证过的插件身份移除用户凭据目录禁读；写保护清单保持不变。
pub(super) fn exempt_authorized_user_credentials(
    policy: &mut tiangong_sandbox::SandboxPolicy,
    access: crate::host_policy::UserCredentialReadAccess,
) {
    // 网络证书验证由 Launcher 根据 allow_network 独立开放，不授予凭据访问。
    policy.allow_credential_services = access.ssh || access.github_cli;
    let Some(home) = crate::interpreter_env::user_home_dir() else {
        return;
    };
    let mut exemptions = Vec::new();
    if access.ssh {
        exemptions.push(home.join(".ssh"));
    }
    if access.github_cli {
        exemptions.push(home.join(".config/gh"));
    }
    policy
        .denied_read_paths
        .retain(|path| !exemptions.contains(path));
}

/// 最终策略中的额外写根都必须是已存在目录。Linux bubblewrap 的 bind
/// 源不存在会拒绝启动；其他平台也不应携带无效授权项。
pub(super) fn retain_existing_writable_roots(policy: &mut tiangong_sandbox::SandboxPolicy) {
    policy.extra_writable.retain(|path| {
        if path.is_dir() {
            true
        } else {
            tracing::warn!(
                path = %path.display(),
                "忽略不存在或不是目录的沙箱额外可写根"
            );
            false
        }
    });
}

/// 仅为宿主授权的插件增加用户工具缓存写根。
pub(super) fn apply_user_cache_write(policy: &mut tiangong_sandbox::SandboxPolicy, allowed: bool) {
    if !allowed {
        return;
    }
    if let Some(home) = crate::interpreter_env::user_home_dir() {
        policy.extra_writable.push(home.join(".cache"));
    }
}

#[cfg(test)]
mod sensitive_access_tests {
    use super::*;

    #[test]
    fn mcp_config_write_exemption_scopes_to_mcp_plugin() {
        let storage = std::path::Path::new("/tmp/sensitive-storage");
        let mcp_json = storage.join("mcp.json");

        // mcp 插件（mcp_config 授权）：mcp.json 移出写保护，可写恢复。
        let mut policy = tiangong_sandbox::SandboxPolicy::workspace_write("/tmp/ws");
        policy.protected_paths = tiangong_protected_paths(storage);
        tiangong_sandbox::sandbox::presets::apply_tiangong(&mut policy, storage);
        exempt_mcp_config_write(&mut policy, storage);
        assert!(!policy.protected_paths.contains(&mcp_json));
        // 其他敏感配置的写保护不受影响。
        assert!(policy.protected_paths.contains(&storage.join("keys")));
        assert!(policy.protected_paths.contains(&storage.join("trust.db")));
        assert!(
            policy
                .protected_paths
                .contains(&storage.join("models.json"))
        );
        assert!(policy.protected_paths.contains(&storage.join("app.json")));
        assert!(policy.protected_paths.contains(&storage.join("auth")));
        // 宿主验证记录目录读写双禁：能力快照由宿主维护，插件不得伪造。
        assert!(
            policy
                .protected_paths
                .contains(&storage.join("plugins/.verifications"))
        );

        // 未授权插件：不调用豁免，mcp.json 写保护原样保留。
        let mut strict = tiangong_sandbox::SandboxPolicy::workspace_write("/tmp/ws");
        strict.protected_paths = tiangong_protected_paths(storage);
        tiangong_sandbox::sandbox::presets::apply_tiangong(&mut strict, storage);
        assert!(strict.protected_paths.contains(&mcp_json));
    }
    #[test]
    fn exempt_opens_only_authorized_configs() {
        let storage = std::path::Path::new("/tmp/sensitive-storage");
        let mut policy = tiangong_sandbox::SandboxPolicy::workspace_write("/tmp/ws");
        tiangong_sandbox::sandbox::presets::apply_tiangong(&mut policy, storage);
        let before = policy.denied_read_paths.len();
        assert!(before >= 5, "清单应含配置与信任件（实际 {before}）");

        // 授权模型+MCP：对应配置移出禁读，密钥/信任库/Launcher 保留。
        exempt_authorized_reads(
            &mut policy,
            super::SensitiveStorageAccess {
                model_config: true,
                mcp_config: true,
                ..Default::default()
            },
            storage,
        );
        assert!(
            !policy
                .denied_read_paths
                .contains(&storage.join("models.json"))
        );
        // 模型配置授权同时开放 OAuth 凭据读取，但写保护保持。
        assert!(!policy.denied_read_paths.contains(&storage.join("auth")));
        assert!(!policy.denied_read_paths.contains(&storage.join("mcp.json")));
        assert!(policy.denied_read_paths.contains(&storage.join("keys")));
        assert!(policy.denied_read_paths.contains(&storage.join("trust.db")));
        assert!(policy.denied_read_paths.contains(&storage.join("sandbox")));
        assert!(
            policy
                .denied_read_paths
                .contains(&storage.join("server.json"))
        );

        // 无授权：禁读清单原样保留。
        let mut strict = tiangong_sandbox::SandboxPolicy::workspace_write("/tmp/ws");
        tiangong_sandbox::sandbox::presets::apply_tiangong(&mut strict, storage);
        assert!(strict.denied_read_paths.contains(&storage.join("auth")));
        let strict_before = strict.denied_read_paths.clone();
        exempt_authorized_reads(
            &mut strict,
            super::SensitiveStorageAccess::default(),
            storage,
        );
        assert_eq!(strict.denied_read_paths, strict_before);
    }

    #[test]
    fn git_workflow_credentials_are_readable_but_remain_write_protected() {
        for network in [false, true] {
            for ssh in [false, true] {
                for github_cli in [false, true] {
                    let mut policy = tiangong_sandbox::SandboxPolicy::workspace_write("/tmp/ws");
                    policy.allow_network = network;
                    exempt_authorized_user_credentials(
                        &mut policy,
                        crate::host_policy::UserCredentialReadAccess { ssh, github_cli },
                    );
                    assert_eq!(policy.allow_credential_services, ssh || github_cli);
                    assert_eq!(policy.allow_network, network);
                }
            }
        }

        let Some(home) = crate::interpreter_env::user_home_dir() else {
            return;
        };
        let storage = std::path::Path::new("/tmp/sensitive-storage");
        let ssh = home.join(".ssh");
        let github_cli = home.join(".config/gh");
        let aws = home.join(".aws");
        let mut policy = tiangong_sandbox::SandboxPolicy::workspace_write("/tmp/ws");
        policy.protected_paths = tiangong_protected_paths(storage);
        tiangong_sandbox::sandbox::presets::apply_tiangong(&mut policy, storage);

        exempt_authorized_user_credentials(
            &mut policy,
            crate::host_policy::UserCredentialReadAccess {
                ssh: true,
                github_cli: true,
            },
        );

        assert!(!policy.denied_read_paths.contains(&ssh));
        assert!(!policy.denied_read_paths.contains(&github_cli));
        assert!(policy.denied_read_paths.contains(&aws));
        assert!(policy.protected_paths.contains(&ssh));
        assert!(policy.protected_paths.contains(&github_cli));
        assert!(policy.allow_credential_services);
    }

    #[test]
    fn missing_optional_cache_is_not_added_to_policy() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join(".npm");
        let mut policy = tiangong_sandbox::SandboxPolicy::workspace_write(root.path());

        policy.extra_writable.push(missing.clone());
        retain_existing_writable_roots(&mut policy);
        assert!(!policy.extra_writable.contains(&missing));

        std::fs::create_dir(&missing).unwrap();
        policy.extra_writable.push(missing.clone());
        retain_existing_writable_roots(&mut policy);
        assert!(policy.extra_writable.contains(&missing));
    }

    #[test]
    fn user_cache_write_is_explicitly_opt_in() {
        let Some(home) = crate::interpreter_env::user_home_dir() else {
            return;
        };
        let cache = tiangong_sandbox::sandbox::policy::canonical_or_keep(&home.join(".cache"));
        let mut strict = tiangong_sandbox::SandboxPolicy::workspace_write("/tmp/ws");
        apply_user_cache_write(&mut strict, false);
        assert!(!strict.writable_roots().contains(&cache));

        apply_user_cache_write(&mut strict, true);
        assert!(strict.writable_roots().contains(&cache));
    }
}
