//! 宿主侧沙箱启动策略（Launcher JSON 请求约定的宿主表示）。
//!
//! 宿主只负责按已验证的插件身份组装策略，并以 JSON 交给签名的 Launcher；
//! 能力怎样变成系统规则、路径怎样落到平台沙箱由 Launcher 负责。本模块因此
//! 不依赖平台沙箱实现（`tiangong-sandbox`），字段与 Launcher 的
//! `SandboxPolicy` 一一对应，由契约测试（`tests/launch_policy_contract.rs`）
//! 保证 Launcher 能按相同语义解析。
//!
//! 演进规则：新增字段一律带默认值；新增能力名只在 Launcher 自检上报支持后
//! 才会由宿主授予（见 `launcher_update::missing_capabilities`）。
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// 凭据服务（macOS Keychain、OpenDirectory）：Git/SSH/GitHub CLI 解析用户身份与读取凭据。
pub const CAPABILITY_CREDENTIAL_SERVICES: &str = "credential_services";
/// 系统音频输入（CoreAudio HAL 与麦克风数据）。
pub const CAPABILITY_AUDIO_INPUT: &str = "audio_input";
/// 沙箱内运行无头浏览器（Chrome/Chromium）。
pub const CAPABILITY_BROWSER_RUNTIME: &str = "browser_runtime";
/// 进程表查询服务（macOS sysmond）：沙箱内 `pgrep` / `pkill` 可用。
pub const CAPABILITY_PROCESS_INSPECTION: &str = "process_inspection";

/// 沙箱模式（宿主只使用 `workspace_write`，`read_only` 保留与 Launcher 对齐）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    ReadOnly,
    WorkspaceWrite,
}

/// 单次进程树的资源上限（默认值与 Launcher 一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResourceLimits {
    /// 整次命令可消耗的 CPU 时间上限（秒）。
    pub max_cpu_time_seconds: u64,
    /// Unix 为单进程地址空间上限，Windows 为整个 Job 的提交内存上限。
    pub max_memory_bytes: u64,
    /// Linux 与 Windows 的进程数量上限。
    pub max_processes: u32,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_cpu_time_seconds: 300,
            max_memory_bytes: 2 * 1024 * 1024 * 1024,
            max_processes: 64,
        }
    }
}

/// 交给 Launcher 的沙箱策略。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LaunchPolicy {
    pub mode: LaunchMode,
    /// 会话工作区（主可写根）。
    pub workspace: PathBuf,
    /// 额外可写根。
    #[serde(default)]
    pub extra_writable: Vec<PathBuf>,
    /// 即使位于可写根内也保持只读的宿主敏感路径。
    #[serde(default)]
    pub protected_paths: Vec<PathBuf>,
    /// 沙箱内不可读取的宿主敏感路径。
    #[serde(default)]
    pub denied_read_paths: Vec<PathBuf>,
    /// 是否放行出网。
    #[serde(default)]
    pub allow_network: bool,
    /// 宿主授予的特殊能力名（见 `CAPABILITY_*`）。
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
    /// 资源上限。
    #[serde(default)]
    pub resource_limits: ResourceLimits,
}

impl LaunchPolicy {
    /// 默认策略：workspace-write、禁网、无特殊能力。
    pub fn workspace_write(workspace: impl Into<PathBuf>) -> Self {
        Self {
            mode: LaunchMode::WorkspaceWrite,
            workspace: workspace.into(),
            extra_writable: Vec::new(),
            protected_paths: Vec::new(),
            denied_read_paths: Vec::new(),
            allow_network: false,
            capabilities: BTreeSet::new(),
            resource_limits: ResourceLimits::default(),
        }
    }

    /// 全部可写根（已规范化去重），与 Launcher 的计算方式一致。
    pub fn writable_roots(&self) -> Vec<PathBuf> {
        if self.mode != LaunchMode::WorkspaceWrite {
            return Vec::new();
        }
        let mut roots = vec![self.workspace.clone()];
        roots.extend(self.extra_writable.iter().cloned());
        let mut seen: Vec<PathBuf> = Vec::new();
        roots
            .into_iter()
            .filter(|path| path.is_absolute())
            .map(|path| canonical_or_keep(&path))
            .filter(|path| {
                if seen.contains(path) {
                    false
                } else {
                    seen.push(path.clone());
                    true
                }
            })
            .collect()
    }
}

/// 常见用户凭据位置（默认禁读，按宿主验证的插件身份再豁免）。
pub fn common_credential_paths() -> Vec<PathBuf> {
    let Some(home) = credential_home_dir() else {
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

/// 凭据预设使用的家目录：Windows 只认 `USERPROFILE`（或
/// `HOMEDRIVE`+`HOMEPATH`），不受 Git Bash 等注入的 `HOME` 影响。
fn credential_home_dir() -> Option<PathBuf> {
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

/// 天工宿主的禁读预设：常见用户凭据 + App 层配置与信任件（签名密钥、
/// 账号凭据、Launcher 目录）。插件数据由宿主按插件数据目录隔离，不在此列。
pub fn apply_tiangong_denied_reads(policy: &mut LaunchPolicy, storage_root: &Path) {
    policy.denied_read_paths.extend(common_credential_paths());
    policy.denied_read_paths.extend(
        [
            "keys",
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

/// 规范化路径：能解析则用真实路径（macOS 上 `/var` → `/private/var`），
/// 解析失败（路径不存在）则做纯词法规范化。
pub fn canonical_or_keep(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path))
}

/// 无需文件系统访问的路径规范化：拆段、处理 `.` 与 `..`、去尾部 `/`。
fn normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        use std::path::Component;
        match component {
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

/// 解析真实路径。Windows AppContainer 无法查询标准规范化名称，因此仅在
/// 已确认处于 AppContainer 时使用绝对路径展开；普通进程完成真实路径解析。
pub fn canonicalize_path(path: &Path) -> std::io::Result<PathBuf> {
    #[cfg(windows)]
    {
        let resolved = if windows_token::current_process_is_app_container().unwrap_or(false) {
            std::path::absolute(path)?
        } else {
            std::fs::canonicalize(path)?
        };
        Ok(dunce::simplified(&resolved).to_path_buf())
    }
    #[cfg(not(windows))]
    {
        std::fs::canonicalize(path)
    }
}

#[cfg(windows)]
mod windows_token {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenIsAppContainer};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub(super) fn current_process_is_app_container() -> std::io::Result<bool> {
        let mut token = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut is_app_container = 0u32;
        let mut returned = 0u32;
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenIsAppContainer,
                (&mut is_app_container as *mut u32).cast(),
                std::mem::size_of_val(&is_app_container) as u32,
                &mut returned,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(is_app_container != 0)
    }
}

/// 沙箱违规归因：识别失败输出中的沙箱拒绝特征，返回给 Agent 的行动提示。
/// 无法确认是沙箱拦截时返回 None（可能是普通权限问题）。
pub fn explain_violation(stderr_text: &str) -> Option<&'static str> {
    if stderr_text.contains("Operation not permitted")
        || stderr_text.contains("Read-only file system")
        || stderr_text.contains("Sandbox denial")
    {
        Some(
            "该操作被沙箱约束拒绝（写入范围或网络未在工作区白名单内）。\
请改为只写当前工作区或本次专用临时目录，并避免访问被禁止的网络。",
        )
    } else {
        None
    }
}

/// 测试辅助：经与真实链路相同的 JSON 往返，把宿主策略交给 Launcher 的
/// 策略类型解析（仅测试编译，依赖 dev-dependency 的 `tiangong-sandbox`）。
#[cfg(test)]
pub(crate) mod tests_support {
    pub(crate) fn to_launcher_policy(
        policy: &super::LaunchPolicy,
    ) -> tiangong_sandbox::SandboxPolicy {
        serde_json::from_value(serde_json::to_value(policy).unwrap())
            .expect("Launcher 必须能解析宿主策略")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialization_matches_launcher_contract() {
        let mut policy = LaunchPolicy::workspace_write("/workspace");
        policy
            .capabilities
            .insert(CAPABILITY_BROWSER_RUNTIME.into());
        policy
            .capabilities
            .insert(CAPABILITY_CREDENTIAL_SERVICES.into());
        let value = serde_json::to_value(&policy).unwrap();
        assert_eq!(value["mode"], "workspace_write");
        assert_eq!(value["allow_network"], false);
        // 有序集合：序列化顺序稳定。
        assert_eq!(
            value["capabilities"],
            serde_json::json!(["browser_runtime", "credential_services"])
        );
        assert_eq!(value["resource_limits"]["max_processes"], 64);
        let decoded: LaunchPolicy = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn tiangong_denied_reads_cover_config_and_trust_anchors() {
        let root = Path::new("/storage");
        let mut policy = LaunchPolicy::workspace_write("/workspace");
        apply_tiangong_denied_reads(&mut policy, root);
        for name in [
            "keys",
            "models.json",
            "auth",
            "server.json",
            "app.json",
            "sandbox",
        ] {
            assert!(
                policy.denied_read_paths.contains(&root.join(name)),
                "{name}"
            );
        }
    }

    #[test]
    fn writable_roots_are_canonical_and_deduped() {
        let root = tempfile::tempdir().unwrap();
        let mut policy = LaunchPolicy::workspace_write(root.path());
        policy.extra_writable = vec![root.path().to_path_buf(), "relative".into()];
        assert_eq!(
            policy.writable_roots(),
            vec![canonical_or_keep(root.path())]
        );
        policy.mode = LaunchMode::ReadOnly;
        assert!(policy.writable_roots().is_empty());
    }

    #[test]
    fn violation_hint_only_for_sandbox_denials() {
        assert!(explain_violation("touch: x: Operation not permitted").is_some());
        assert!(explain_violation("file not found").is_none());
    }
}
