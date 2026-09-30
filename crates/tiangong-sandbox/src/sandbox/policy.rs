//! 沙箱策略：平台无关的中间表示（IR）。
//!
//! command 工具的宿主调用上下文最终归一为 [`SandboxPolicy`]；插件载荷和
//! manifest 不参与安全策略决策。

use std::path::{Path, PathBuf};

/// 沙箱模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// 全盘只读，无任何可写路径。
    ReadOnly,
    /// 读全盘放行、写限白名单（工作区 + 临时目录 + 额外可写）、网络默认禁。
    WorkspaceWrite,
    /// 仅供宿主显式特殊路径表示无沙箱；当前 Launcher 始终拒绝该模式。
    FullAccess,
}

/// 平台无关的沙箱策略（可序列化，经 Launcher 协议传输）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    /// 会话工作区（WorkspaceWrite 模式下的主可写根）。
    pub workspace: PathBuf,
    /// 额外可写根（插件数据目录等）。
    #[serde(default)]
    pub extra_writable: Vec<PathBuf>,
    /// 即使位于可写根内也保持只读的宿主敏感路径。
    #[serde(default)]
    pub protected_paths: Vec<PathBuf>,
    /// 沙箱内不可读取的宿主敏感路径（用户凭据等）。
    #[serde(default)]
    pub denied_read_paths: Vec<PathBuf>,
    /// 是否放行出网（默认 false；放行走宿主代理体系，见 RFC D16）。
    #[serde(default)]
    pub allow_network: bool,
    /// 是否允许访问宿主系统凭据服务（macOS Keychain、OpenDirectory）。
    /// 证书验证由 allow_network 独立授权。Git/SSH/GitHub CLI 依赖凭据服务解析
    /// 用户身份与读取凭据；默认 false，仅宿主显式授权的策略开放。
    #[serde(default)]
    pub allow_credential_services: bool,
    /// 是否允许访问宿主音频输入服务（macOS CoreAudio HAL）。录音类插件需要
    /// 经 coreaudiod 枚举与打开麦克风；默认 false，仅宿主按已验证插件身份授权。
    /// 麦克风隐私授权（TCC）仍由系统按宿主 App 独立把关。
    #[serde(default)]
    pub allow_audio_input: bool,
    /// 单次 command 的资源上限。
    #[serde(default)]
    pub resource_limits: SandboxResourceLimits,
}

/// 单次 command 进程树的资源上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SandboxResourceLimits {
    /// 整次命令可消耗的 CPU 时间上限（秒）。
    pub max_cpu_time_seconds: u64,
    /// Unix 为单进程地址空间上限，Windows 为整个 Job 的提交内存上限。
    pub max_memory_bytes: u64,
    /// Linux 与 Windows 的进程数量上限。
    pub max_processes: u32,
}

impl Default for SandboxResourceLimits {
    fn default() -> Self {
        Self {
            max_cpu_time_seconds: 300,
            max_memory_bytes: 2 * 1024 * 1024 * 1024,
            max_processes: 64,
        }
    }
}

impl SandboxPolicy {
    /// 命令执行位的默认策略：workspace-write、禁网。
    pub fn workspace_write(workspace: impl Into<PathBuf>) -> Self {
        Self {
            mode: SandboxMode::WorkspaceWrite,
            workspace: workspace.into(),
            extra_writable: Vec::new(),
            protected_paths: Vec::new(),
            denied_read_paths: Vec::new(),
            allow_network: false,
            allow_credential_services: false,
            allow_audio_input: false,
            resource_limits: SandboxResourceLimits::default(),
        }
    }

    pub fn full_access() -> Self {
        Self {
            mode: SandboxMode::FullAccess,
            workspace: PathBuf::new(),
            extra_writable: Vec::new(),
            protected_paths: Vec::new(),
            denied_read_paths: Vec::new(),
            allow_network: true,
            allow_credential_services: true,
            allow_audio_input: true,
            resource_limits: SandboxResourceLimits::default(),
        }
    }

    /// 全部可写根（已规范化去重）：工作区、临时目录、额外可写。
    pub fn writable_roots(&self) -> Vec<PathBuf> {
        if self.mode != SandboxMode::WorkspaceWrite {
            return Vec::new();
        }
        // 不自动放行全局系统临时目录（审查修订）：每次执行的专用临时目录
        // 由宿主创建并经 extra_writable 显式传入。
        let mut roots = vec![self.workspace.clone()];
        roots.extend(self.extra_writable.iter().cloned());
        let mut seen: Vec<PathBuf> = Vec::new();
        roots
            .into_iter()
            .filter(|p| p.is_absolute())
            .map(|p| canonical_or_keep(&p))
            .filter(|p| {
                if seen.contains(p) {
                    false
                } else {
                    seen.push(p.clone());
                    true
                }
            })
            .collect()
    }

    /// 防篡改段：宿主敏感路径保持只读。
    ///
    /// 当敏感根是工作区的祖先时不能整体锁死，否则默认的
    /// `<storage>/workspaces/<session>` 会失去写权限。白名单外本就默认禁写，
    /// 因此此时跳过祖先根仍会保护其它会话和宿主文件。
    /// 工作区内 `.git` 不在此列（用户裁定 2026-08-30）：agent 需要完整
    /// git 工作流，`.git` 与源码同等对待——可写。
    pub fn read_only_roots(&self) -> Vec<PathBuf> {
        let writable = self.writable_roots();
        let mut paths = self
            .protected_paths
            .iter()
            .map(|path| canonical_or_keep(path))
            .filter(|protected| {
                !writable
                    .iter()
                    .any(|root| root != protected && root.starts_with(protected))
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        paths
    }

    /// 不可读取路径（规范化、去重，只保留绝对路径）。
    pub fn denied_read_roots(&self) -> Vec<PathBuf> {
        let mut paths = self
            .denied_read_paths
            .iter()
            .filter(|path| path.is_absolute())
            .map(|path| canonical_or_keep(path))
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        paths
    }
}

/// 规范化路径：能解析则用真实路径（macOS 上 `/var` → `/private/var`），
/// 解析失败（路径不存在）保留原样。
pub fn canonical_or_keep(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path))
}

/// 无需文件系统访问的路径规范化：拆段、处理 `.` 与 `..`、去尾部 `/`。
fn normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in path.components() {
        use std::path::Component;
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    let mut out = PathBuf::new();
    for part in parts {
        out.push(part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_roots_are_canonical_and_deduped() {
        let root = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::workspace_write(root.path().join("workspace"));
        let roots = policy.writable_roots();
        // 不自动包含全局系统临时目录：可写根 = 工作区 + 显式 extra。
        assert_eq!(roots.len(), 1);
        let mut policy = policy;
        policy.extra_writable = vec![root.path().join("exec-tmp")];
        assert_eq!(policy.writable_roots().len(), 2);
    }

    #[test]
    fn full_access_has_no_writable_roots() {
        assert!(SandboxPolicy::full_access().writable_roots().is_empty());
    }

    #[test]
    fn protected_storage_root_does_not_shadow_nested_workspace() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspaces/session");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut policy = SandboxPolicy::workspace_write(&workspace);
        policy.protected_paths = vec![root.path().to_path_buf()];

        assert!(
            !policy
                .read_only_roots()
                .contains(&canonical_or_keep(root.path()))
        );
        assert_eq!(policy.writable_roots(), vec![canonical_or_keep(&workspace)]);
    }

    #[test]
    fn protected_storage_root_remains_locked_for_broad_workspace() {
        let root = tempfile::tempdir().unwrap();
        let mut policy = SandboxPolicy::workspace_write(root.path().parent().unwrap());
        policy.protected_paths = vec![root.path().to_path_buf()];

        assert!(
            policy
                .read_only_roots()
                .contains(&canonical_or_keep(root.path()))
        );
    }

    #[test]
    fn policy_serialization_is_platform_stable() {
        let mut policy = SandboxPolicy::workspace_write("/workspace");
        policy.extra_writable = vec!["/execution/tmp".into()];
        policy.protected_paths = vec!["/workspace/.git".into()];
        policy.denied_read_paths = vec!["/home/user/.ssh".into()];

        let value = serde_json::to_value(&policy).unwrap();
        assert_eq!(value["mode"], "workspace_write");
        assert_eq!(value["workspace"], "/workspace");
        assert_eq!(value["allow_network"], false);
        assert_eq!(value["allow_credential_services"], false);
        assert_eq!(value["allow_audio_input"], false);
        assert_eq!(value["resource_limits"]["max_cpu_time_seconds"], 300);
        assert_eq!(value["resource_limits"]["max_processes"], 64);

        let decoded: SandboxPolicy = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn git_metadata_is_writable_in_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let git_file = workspace.path().join(".git");
        std::fs::write(&git_file, "gitdir: ../metadata").unwrap();
        let policy = SandboxPolicy::workspace_write(workspace.path());

        // .git 与源码同等对待（用户裁定 2026-08-30）：agent 需要完整 git
        // 工作流，不得出现在只读根中。
        assert!(
            !policy
                .read_only_roots()
                .contains(&canonical_or_keep(&git_file))
        );
    }
}
