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

/// Windows 天工通用持久授权根：所有沙箱插件共享、可写的天工目录（媒体交换
/// 区与 tmp）。只取本次策略实际包含的根（不存在或受保护的条目已被剔除）。
/// 这些目录授权一次即持久保存；插件自有数据目录、工作区等差异部分每次
/// 启动临时授予。
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn common_grant_roots(
    storage_root: &std::path::Path,
    writable: &[std::path::PathBuf],
) -> Vec<std::path::PathBuf> {
    SHARED_WRITABLE_ENTRIES
        .iter()
        .map(|entry| storage_root.join(entry))
        .filter(|root| writable.contains(root))
        .collect()
}

/// 插件数据目录隔离：存储根 `data/` 下其他插件的数据目录对当前插件读写双禁。
///
/// 只有 `data/<plugin_id>` 属于当前插件；其余条目（含符号链接与非目录项）
/// 一律禁读。正常情况下它们不在任何可写根内、本就不可写；仅当某个可写根
/// （如工作区设为家目录）覆盖了它们时才追加写保护——Windows 写保护是逐目录
/// 写入的 ACL 拒绝项，对大目录（Memory 数据）每次 spawn 传播开销可观，不应
/// 无谓施加。须在可写根组装完成后调用。按 spawn 时的目录快照计算，之后新
/// 出现的目录在下次 spawn 时纳入。
pub(super) fn isolate_other_plugin_data(
    policy: &mut tiangong_sandbox::SandboxPolicy,
    storage_root: &std::path::Path,
    plugin_id: &str,
) {
    let Ok(entries) = std::fs::read_dir(storage_root.join(STORAGE_DATA_DIR)) else {
        return;
    };
    let writable = policy.writable_roots();
    for entry in entries.flatten() {
        if entry.file_name() == std::ffi::OsStr::new(plugin_id) {
            continue;
        }
        let path = entry.path();
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if writable.iter().any(|root| canonical.starts_with(root)) {
            policy.protected_paths.push(path.clone());
        }
        policy.denied_read_paths.push(path);
    }
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
    fn plugin_data_dirs_are_isolated_from_each_other() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path();
        for id in ["mcp", "memory", "fs"] {
            ensure_storage_data_dir(storage, id).unwrap();
        }
        std::fs::write(storage.join("data/stray.json"), "{}").unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut policy = tiangong_sandbox::SandboxPolicy::workspace_write(workspace.path());
        policy.extra_writable.push(storage.join("data/mcp"));
        isolate_other_plugin_data(&mut policy, storage, "mcp");
        let own = storage.join("data/mcp");
        assert!(!policy.denied_read_paths.contains(&own), "自有数据可读");
        assert!(!policy.protected_paths.contains(&own), "自有数据可写");
        for other in ["data/memory", "data/fs", "data/stray.json"] {
            let path = storage.join(other);
            assert!(policy.denied_read_paths.contains(&path), "{other} 应禁读");
            // 不在任何可写根内：本就不可写，不追加写保护。
            assert!(!policy.protected_paths.contains(&path), "{other}");
        }

        // 可写根覆盖存储根（如工作区设为家目录）：其他插件数据追加写保护。
        let mut broad = tiangong_sandbox::SandboxPolicy::workspace_write(storage);
        isolate_other_plugin_data(&mut broad, storage, "mcp");
        assert!(broad.protected_paths.contains(&storage.join("data/memory")));
        assert!(
            broad
                .read_only_roots()
                .iter()
                .any(|path| path.ends_with("data/memory"))
        );
        assert!(!broad.protected_paths.contains(&own));

        // 没有 data/ 目录时不报错、不改策略。
        let empty = tempfile::tempdir().unwrap();
        let mut untouched = tiangong_sandbox::SandboxPolicy::workspace_write("/tmp/ws");
        isolate_other_plugin_data(&mut untouched, empty.path(), "mcp");
        assert!(untouched.denied_read_paths.is_empty());
        assert!(untouched.protected_paths.is_empty());
    }
    #[test]
    fn exempt_opens_only_authorized_configs() {
        let storage = std::path::Path::new("/tmp/sensitive-storage");
        let mut policy = tiangong_sandbox::SandboxPolicy::workspace_write("/tmp/ws");
        tiangong_sandbox::sandbox::presets::apply_tiangong(&mut policy, storage);
        let before = policy.denied_read_paths.len();
        assert!(before >= 5, "清单应含配置与信任件（实际 {before}）");

        // 授权模型配置：对应配置移出禁读，密钥/信任库/Launcher 保留。
        exempt_authorized_reads(
            &mut policy,
            super::SensitiveStorageAccess {
                model_config: true,
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
        assert!(policy.denied_read_paths.contains(&storage.join("keys")));
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

    #[test]
    fn common_grant_roots_only_cover_shared_storage_entries() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path();
        for dir in ["tmp", "media"] {
            std::fs::create_dir(storage.join(dir)).unwrap();
        }
        let plugin_data = storage.join("plugins/fs/data");
        std::fs::create_dir_all(&plugin_data).unwrap();
        ensure_storage_data_dir(storage, "fs").unwrap();
        let writable = storage_root_writable_roots(storage, "fs", &plugin_data);
        let common = common_grant_roots(storage, &writable);
        assert_eq!(common, vec![storage.join("media"), storage.join("tmp")]);
        // 插件自有数据目录属于针对性授权，不进入通用持久授权。
        assert!(!common.contains(&storage.join("data/fs")));
        assert!(!common.contains(&plugin_data));
        // 策略未包含的共享目录（如不存在）不纳入。
        let partial = vec![storage.join("media")];
        assert_eq!(common_grant_roots(storage, &partial), partial);
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
