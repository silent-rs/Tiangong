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
        // 存储根下旧 mcp.json 只读开放，供 mcp 插件首次启动迁移。
        exemptions.push(storage_root.join("mcp.json"));
        exemptions.push(storage_root.join(MCP_CONFIG_DATA_PATH));
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

/// MCP 配置在 mcp 插件自有数据目录中的位置（相对存储根）。
pub(super) const MCP_CONFIG_DATA_PATH: &str = "data/mcp/mcp.json";

/// MCP 配置的写豁免：`data/mcp/mcp.json` 是 MCP 配置的权威存储，唯一合法
/// 写者是 mcp 插件自身（宿主已验证官方签名身份）。从写保护清单移除后，
/// 写权限随 `data/mcp` 可写恢复；其他插件对它读写双禁不变。存储根下的
/// 旧 `mcp.json` 仍保持写保护。
pub(super) fn exempt_mcp_config_write(
    policy: &mut tiangong_sandbox::SandboxPolicy,
    storage_root: &std::path::Path,
) {
    let target = storage_root.join(MCP_CONFIG_DATA_PATH);
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
        MCP_CONFIG_DATA_PATH,
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

/// 存储根统一写域下的插件自有数据根目录名（`~/.tiangong/data/<插件 id>`）。
pub(crate) const STORAGE_DATA_DIR: &str = "data";
/// 存储根下所有沙箱插件共享可写的顶层目录：`media/` 是跨插件媒体交换区
/// （截图、语音、Bot 附件），`tmp/` 承载 sidecar 临时目录。`logs/` 由宿主
/// 写入，不在此列。
const SHARED_WRITABLE_ENTRIES: [&str; 2] = ["media", "tmp"];

/// 存储根贡献的可写根（按插件 ID 收拢，所有插件同一规则）。
///
/// 只放行 `data/<plugin_id>`（spawn 前由宿主创建）、插件安装目录下的
/// `data/`（`TIANGONG_PLUGIN_DATA_DIR`）、`media/`（跨插件媒体交换）与
/// `tmp/`。存储根本身、`logs/`（宿主写入）、其他插件的数据目录与历史
/// 顶层数据目录一律只读；插件升级时自行把旧数据复制进 `data/<id>`。
/// 条目不跟随符号链接，受保护路径不列入。
pub(super) fn storage_root_writable_roots(
    storage_root: &std::path::Path,
    plugin_id: &str,
    plugin_data_dir: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    let protected = tiangong_protected_paths(storage_root);
    let mut roots = vec![
        storage_root.join(STORAGE_DATA_DIR).join(plugin_id),
        plugin_data_dir.to_path_buf(),
    ];
    roots.extend(
        SHARED_WRITABLE_ENTRIES
            .iter()
            .map(|entry| storage_root.join(entry)),
    );
    roots.retain(|path| {
        !protected.iter().any(|item| path.starts_with(item))
            && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir())
    });
    roots.dedup();
    roots
}

/// 在存储根下创建插件自有数据目录 `data/<plugin_id>`（实体目录；已存在的
/// 符号链接视为篡改并拒绝）。插件 ID 已由清单校验限定为字母数字与 `-_.`，
/// 这里再拒绝 `.`/`..` 等非普通段，防止路径逃逸。
pub(super) fn ensure_storage_data_dir(
    storage_root: &std::path::Path,
    plugin_id: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let mut components = std::path::Path::new(plugin_id).components();
    if !matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) {
        anyhow::bail!("插件 ID 不能作为数据目录名: {plugin_id}");
    }
    let base = storage_root.join(STORAGE_DATA_DIR);
    let dir = base.join(plugin_id);
    // 存储根本身由宿主管理，按需创建；只对其下的 data/ 与 data/<id> 两级
    // 做实体目录校验（存储根是信任锚，不在本函数的篡改防护范围内）。
    std::fs::create_dir_all(storage_root)?;
    for path in [&base, &dir] {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => anyhow::bail!("插件数据目录不是实体目录: {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(path).or_else(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(dir)
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
        let mcp_json = storage.join(MCP_CONFIG_DATA_PATH);
        let legacy_mcp_json = storage.join("mcp.json");

        // mcp 插件（mcp_config 授权）：data/mcp/mcp.json 移出写保护，可写恢复。
        let mut policy = tiangong_sandbox::SandboxPolicy::workspace_write("/tmp/ws");
        policy.protected_paths = tiangong_protected_paths(storage);
        tiangong_sandbox::sandbox::presets::apply_tiangong(&mut policy, storage);
        exempt_mcp_config_write(&mut policy, storage);
        assert!(!policy.protected_paths.contains(&mcp_json));
        // 存储根下旧 mcp.json 仅供迁移读取，写保护保持。
        assert!(policy.protected_paths.contains(&legacy_mcp_json));
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
        assert!(strict.denied_read_paths.contains(&mcp_json));
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
        assert!(
            !policy
                .denied_read_paths
                .contains(&storage.join(MCP_CONFIG_DATA_PATH))
        );
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
    fn storage_root_write_scope_is_per_plugin_id() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path();
        for dir in ["tmp", "media", "logs", "skills", "memory", "keys"] {
            std::fs::create_dir(storage.join(dir)).unwrap();
        }
        std::fs::write(storage.join("app.json"), "{}").unwrap();
        let plugin_data = storage.join("plugins/fs/data");
        std::fs::create_dir_all(&plugin_data).unwrap();
        assert_eq!(
            ensure_storage_data_dir(storage, "fs").unwrap(),
            storage.join("data/fs")
        );
        // 幂等：重复创建不报错。
        ensure_storage_data_dir(storage, "fs").unwrap();
        ensure_storage_data_dir(storage, "memory").unwrap();

        let roots = storage_root_writable_roots(storage, "fs", &plugin_data);
        assert_eq!(
            roots,
            vec![
                storage.join("data/fs"),
                plugin_data.clone(),
                storage.join("media"),
                storage.join("tmp"),
            ]
        );
        assert!(!roots.contains(&storage.to_path_buf()), "根目录本身不可写");
        assert!(!roots.contains(&storage.join("logs")), "logs 只由宿主写入");
        assert!(
            !roots.contains(&storage.join("data/memory")),
            "插件间数据隔离"
        );
        assert!(!roots.contains(&storage.join("memory")), "历史数据目录只读");
        assert!(!roots.contains(&storage.join("skills")));
    }

    #[cfg(unix)]
    #[test]
    fn storage_root_write_scope_never_follows_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), storage.join("media")).unwrap();
        std::fs::create_dir(storage.join("data")).unwrap();
        std::os::unix::fs::symlink(outside.path(), storage.join("data/evil")).unwrap();
        assert!(ensure_storage_data_dir(storage, "evil").is_err());
        assert!(ensure_storage_data_dir(storage, "..").is_err());
        assert!(ensure_storage_data_dir(storage, "a/b").is_err());
        let roots = storage_root_writable_roots(storage, "evil", &storage.join("missing"));
        assert!(
            roots.is_empty(),
            "符号链接与不存在的目录都不进入写域: {roots:?}"
        );
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
