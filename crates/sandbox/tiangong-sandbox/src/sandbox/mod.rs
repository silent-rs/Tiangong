//! 沙箱执行层：策略编译、命令包装与违规归因。
//!
//! 平台选型：macOS Seatbelt / Linux bubblewrap / Windows AppContainer。
//! 平台能力缺失时始终拒绝执行。

pub mod bwrap;
pub mod policy;
pub mod presets;
pub mod seatbelt;
#[cfg(windows)]
pub mod windows;

pub use policy::{
    SandboxCapability, SandboxMode, SandboxPolicy, SandboxResourceLimits, supported_capabilities,
};

/// 平台沙箱可用性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxAvailability {
    /// 平台工具就绪，可执行包装。
    Available,
    /// 平台实现或必需程序缺失（Windows、无 bwrap 的 Linux 等）。
    Unsupported(String),
    /// 当前宿主已经处于限制环境，无法再次嵌套应用系统沙箱。
    EnvironmentRestricted(String),
}

pub fn availability() -> SandboxAvailability {
    #[cfg(target_os = "macos")]
    {
        match seatbelt::seatbelt_probe() {
            Ok(()) => SandboxAvailability::Available,
            Err(detail) => SandboxAvailability::EnvironmentRestricted(format!(
                "当前宿主环境无法嵌套应用 macOS Seatbelt（{detail}）"
            )),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match bwrap::bwrap_available() {
            Some(_) => SandboxAvailability::Available,
            None => SandboxAvailability::Unsupported(
                "未找到 bwrap（需要 bubblewrap 且系统允许非特权 user namespace）".into(),
            ),
        }
    }
    #[cfg(windows)]
    {
        windows::availability()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        SandboxAvailability::Unsupported("当前平台没有可用的原生沙箱".into())
    }
}

/// 包装后的命令形态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxedProgram {
    /// 不包装（仅宿主显式特殊无沙箱路径；Launcher 不接受）。
    Direct,
    /// 平台沙箱不可用（缺 bwrap / 嵌套 Seatbelt / Windows 未实现）。
    /// 命令默认路径必须拒绝执行，防止"用户以为在沙箱内实则裸奔"；
    /// 调用方必须报告能力缺失，不得退回裸进程。
    Unavailable(String),
    /// 实际 program 与其 argv 前缀（前缀 + 原命令 + 原参数 = 完整命令行）。
    Wrapped {
        program: String,
        prefix: Vec<String>,
    },
}

/// 把命令包装进沙箱。平台不可用时返回明确错误，不会降级直跑。
pub fn wrap(policy: &SandboxPolicy) -> SandboxedProgram {
    if policy.mode == SandboxMode::FullAccess {
        return SandboxedProgram::Direct;
    }
    match availability() {
        SandboxAvailability::Available => {
            #[cfg(target_os = "macos")]
            {
                SandboxedProgram::Wrapped {
                    program: "/usr/bin/sandbox-exec".into(),
                    prefix: seatbelt::wrap_argv(policy),
                }
            }
            #[cfg(target_os = "linux")]
            {
                let bin = bwrap::bwrap_available().expect("availability 已确认");
                SandboxedProgram::Wrapped {
                    program: bin,
                    prefix: bwrap::wrap_argv(policy),
                }
            }
            #[cfg(windows)]
            {
                SandboxedProgram::Unavailable(
                    "Windows 策略必须由 Launcher 的 AppContainer 启动路径执行".to_string(),
                )
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
            {
                SandboxedProgram::Unavailable("当前平台没有可用的原生沙箱".to_string())
            }
        }
        SandboxAvailability::Unsupported(reason)
        | SandboxAvailability::EnvironmentRestricted(reason) => {
            // 沙箱不可用时拒绝执行，不静默降级裸奔。
            SandboxedProgram::Unavailable(reason)
        }
    }
}

/// 沙箱违规归因：识别失败输出中的沙箱拒绝特征，返回给 Agent 的行动提示。
///
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

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// 真实沙箱执行的前置探测：宿主环境若已在 Seatbelt 沙箱内
    /// （如受限 CI 外壳、嵌套终端），sandbox-exec 无法再次应用沙箱，
    /// 此时跳过真实拦截测试——逻辑由 profile 快照测试覆盖。
    fn can_apply_seatbelt() -> bool {
        let result = seatbelt::seatbelt_probe();
        let ok = result.is_ok();
        if !ok {
            eprintln!(
                "跳过：当前环境已在沙箱内，无法嵌套应用 Seatbelt：{}",
                result.unwrap_err()
            );
        }
        ok
    }

    fn run_in_sandbox(policy: &SandboxPolicy, script: &str) -> (i32, String) {
        let wrapped = wrap(policy);
        let SandboxedProgram::Wrapped { program, prefix } = wrapped else {
            panic!("沙箱应可用并完成包装");
        };
        let mut cmd = std::process::Command::new(program);
        cmd.args(&prefix);
        cmd.arg("/bin/bash").arg("-c").arg(script);
        let output = cmd.output().expect("执行沙箱命令失败");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        (output.status.code().unwrap_or(-1), stderr)
    }

    #[test]
    fn seatbelt_allows_workspace_write_and_blocks_outside() {
        if !seatbelt::seatbelt_available() || !can_apply_seatbelt() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::workspace_write(workspace.path());

        // 工作区内写：允许。
        let inside = workspace.path().join("ok.txt");
        let (code, _) = run_in_sandbox(&policy, &format!("echo ok > {}", inside.display()));
        assert_eq!(code, 0, "工作区内写入应成功");
        assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "ok");

        // 工作区外写：拦截（家目录不在白名单）。
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("blocked.txt");
        let script = format!("echo x > {}", outside.display());
        let (code, stderr) = run_in_sandbox(&policy, &script);
        assert_ne!(code, 0, "工作区外写入应被沙箱拦截");
        assert!(!outside.exists(), "被拦截时目标文件不应被创建");
        assert!(
            explain_violation(&stderr).is_some(),
            "应能从失败输出归因到沙箱约束: {stderr}"
        );
    }

    #[test]
    fn seatbelt_allows_git_history_operations() {
        if !seatbelt::seatbelt_available() || !can_apply_seatbelt() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        // 建一个真实 .git 目录验证完整 git 工作流可用（用户裁定
        // 2026-08-30：.git 与源码同等对待，不再防篡改）。
        let git_dir = workspace.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let policy = SandboxPolicy::workspace_write(workspace.path());

        let target = git_dir.join("config");
        let (code, _) = run_in_sandbox(&policy, &format!("echo x > {}", target.display()));
        assert_eq!(code, 0, "工作区内 .git 写入应放行");
        assert_eq!(std::fs::read_to_string(&target).unwrap().trim(), "x");
    }

    #[test]
    fn full_access_runs_direct() {
        let policy = SandboxPolicy::full_access();
        assert_eq!(wrap(&policy), SandboxedProgram::Direct);
    }

    #[test]
    fn sandboxed_policy_never_degrades_silently() {
        // 任何沙箱模式（非 FullAccess）在不可用环境必须显式失败，
        // 不允许静默降级为直跑。
        let policy = SandboxPolicy::workspace_write("/tmp/ws");
        match wrap(&policy) {
            SandboxedProgram::Direct => panic!("沙箱模式不允许静默直跑"),
            SandboxedProgram::Wrapped { .. } => {}
            SandboxedProgram::Unavailable(reason) => {
                assert!(!reason.is_empty());
            }
        }
    }
}
