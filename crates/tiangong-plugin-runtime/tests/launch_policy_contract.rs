//! 宿主策略 ↔ Launcher 解析契约。
//!
//! 宿主不链接平台沙箱实现，只按 JSON 约定构造请求；本测试以 dev-dependency
//! 引入 Launcher 的策略类型，保证宿主写出的每个字段都被 Launcher 按相同
//! 语义解析，防止两侧格式漂移。
use std::collections::BTreeSet;
use std::path::PathBuf;

use tiangong_plugin_runtime::launch_policy::{
    CAPABILITY_AUDIO_INPUT, CAPABILITY_BROWSER_RUNTIME, CAPABILITY_CREDENTIAL_SERVICES, LaunchMode,
    LaunchPolicy, ResourceLimits,
};
use tiangong_sandbox::{SandboxCapability, SandboxMode, SandboxPolicy, SandboxResourceLimits};

fn to_launcher(policy: &LaunchPolicy) -> SandboxPolicy {
    serde_json::from_value(serde_json::to_value(policy).unwrap())
        .expect("Launcher 必须能解析宿主策略")
}

fn full_host_policy() -> LaunchPolicy {
    let mut policy = LaunchPolicy::workspace_write("/workspace");
    policy.extra_writable = vec!["/execution/tmp".into(), "/storage/data/fs".into()];
    policy.protected_paths = vec!["/storage/keys".into()];
    policy.denied_read_paths = vec!["/home/user/.ssh".into(), "/storage/sandbox".into()];
    policy.allow_network = true;
    policy.resource_limits = ResourceLimits {
        max_cpu_time_seconds: 7,
        max_memory_bytes: 123_456_789,
        max_processes: 9,
    };
    policy
}

#[test]
fn every_field_round_trips_with_same_meaning() {
    let host = full_host_policy();
    let launcher = to_launcher(&host);
    assert_eq!(launcher.mode, SandboxMode::WorkspaceWrite);
    assert_eq!(launcher.workspace, host.workspace);
    assert_eq!(launcher.extra_writable, host.extra_writable);
    assert_eq!(launcher.protected_paths, host.protected_paths);
    assert_eq!(launcher.denied_read_paths, host.denied_read_paths);
    assert_eq!(launcher.allow_network, host.allow_network);
    assert!(launcher.capabilities.is_empty());
    assert_eq!(
        launcher.resource_limits,
        SandboxResourceLimits {
            max_cpu_time_seconds: 7,
            max_memory_bytes: 123_456_789,
            max_processes: 9,
        }
    );
    // 反向：Launcher 的序列化也能被宿主读回，字段集合一致。
    let back: LaunchPolicy = serde_json::from_value(serde_json::to_value(&launcher).unwrap())
        .expect("宿主必须能读回 Launcher 策略");
    assert_eq!(back, host);
    let read_only: LaunchPolicy = serde_json::from_value(
        serde_json::to_value(SandboxPolicy {
            mode: SandboxMode::ReadOnly,
            ..launcher
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(read_only.mode, LaunchMode::ReadOnly);
}

#[test]
fn defaults_match_launcher_defaults() {
    assert_eq!(
        to_launcher(&LaunchPolicy::workspace_write("/workspace")),
        SandboxPolicy::workspace_write("/workspace")
    );
    let host = ResourceLimits::default();
    let launcher = SandboxResourceLimits::default();
    assert_eq!(host.max_cpu_time_seconds, launcher.max_cpu_time_seconds);
    assert_eq!(host.max_memory_bytes, launcher.max_memory_bytes);
    assert_eq!(host.max_processes, launcher.max_processes);
}

#[test]
fn capability_names_are_known_to_launcher() {
    // 宿主可授予的每个能力名都必须是 Launcher 的已知能力（否则 Launcher
    // 按 fail-closed 拒绝启动），且映射到同一能力。
    let pairs = [
        (
            CAPABILITY_CREDENTIAL_SERVICES,
            SandboxCapability::CredentialServices,
        ),
        (CAPABILITY_AUDIO_INPUT, SandboxCapability::AudioInput),
        (
            CAPABILITY_BROWSER_RUNTIME,
            SandboxCapability::BrowserRuntime,
        ),
    ];
    for (name, capability) in pairs {
        assert_eq!(name, capability.as_str());
        let mut host = LaunchPolicy::workspace_write("/workspace");
        host.capabilities.insert(name.to_string());
        assert!(to_launcher(&host).has(capability), "{name}");
    }
    // 宿主侧的能力常量覆盖 Launcher 定义的全部能力。
    let host_names: BTreeSet<&str> = pairs.iter().map(|(name, _)| *name).collect();
    let launcher_names: BTreeSet<&str> = SandboxCapability::ALL
        .iter()
        .map(|capability| capability.as_str())
        .collect();
    assert_eq!(host_names, launcher_names);
}

#[test]
fn unknown_capability_is_rejected_by_launcher() {
    let mut host = LaunchPolicy::workspace_write("/workspace");
    host.capabilities.insert("future_capability".into());
    assert!(serde_json::from_value::<SandboxPolicy>(serde_json::to_value(&host).unwrap()).is_err());
}

#[test]
fn host_and_launcher_compute_same_writable_roots() {
    let root = tempfile::tempdir().unwrap();
    let mut host = LaunchPolicy::workspace_write(root.path());
    host.extra_writable = vec![
        root.path().to_path_buf(),
        PathBuf::from("relative"),
        root.path().join("missing/../other"),
    ];
    assert_eq!(host.writable_roots(), to_launcher(&host).writable_roots());
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_profile_is_identical_for_every_capability_combination() {
    use tiangong_sandbox::sandbox::seatbelt::compile_profile;
    let names = [
        CAPABILITY_CREDENTIAL_SERVICES,
        CAPABILITY_AUDIO_INPUT,
        CAPABILITY_BROWSER_RUNTIME,
    ];
    for mask in 0u8..8 {
        let mut host = full_host_policy();
        host.allow_network = mask & 1 == 0;
        let mut direct = to_launcher(&full_host_policy());
        direct.allow_network = host.allow_network;
        for (bit, name) in names.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                host.capabilities.insert(name.to_string());
                direct.capabilities.insert(
                    serde_json::from_value(serde_json::Value::String(name.to_string())).unwrap(),
                );
            }
        }
        assert_eq!(
            compile_profile(&to_launcher(&host)),
            compile_profile(&direct),
            "mask={mask}"
        );
    }
}
