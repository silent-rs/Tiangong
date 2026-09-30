//! sidecar 运行时的宿主权威执行策略。
//!
//! 分层隔离：WASM handler 在宿主进程内由 wasmtime 能力沙箱隔离（进程内
//! 无法再套 OS 沙箱）；sidecar 默认经 Launcher 进 OS 沙箱；用户可显式关闭首次实际使用才启动的按需进程沙箱。
//!
//! 不变式（沙箱不进行 agent 执行过程中升级）：
//! - 策略是纯函数，仅在连接构造时求值一次并定死，不读任何全局可变
//!   状态；插件清单与 agent 运行时的任何声明/请求都不构成提权输入。
//! - 运行中的连接不升级也不降级；策略变更只可能来自用户设置，且在
//!   下一次连接构造时生效。
//! - ephemeral 形态（command）每次请求构造独立快照与独立沙箱实例，
//!   属"每请求隔离"而非升级。
//!
//! 网络维度：沙箱开启即视为执行安全，网络全放行。用户凭据默认禁读；
//! 官方 terminal/command 为保持完整 Git 工作流，固定获得 SSH 与 GitHub
//! CLI 配置目录的只读例外，其他插件不能通过清单或调用载荷申请该能力。

use std::collections::BTreeMap;

/// sidecar 与宿主的通信通道（宿主权威决定，插件不声明——spawn 时由
/// 宿主注入环境变量选择，sidecar 通用库自动适配）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarTransport {
    /// 继承管道：沙箱零监听端口、生命周期与宿主绑定。
    Stdio,
    /// 本地回环 + endpoint 文件（存量默认；流式召回等依赖连接对象）。
    Tcp,
}

/// 用户凭据目录读取能力。能力只由宿主根据已验证的插件身份授予。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserCredentialReadAccess {
    /// `~/.ssh`。
    pub ssh: bool,
    /// `~/.config/gh`。
    pub github_cli: bool,
}

impl UserCredentialReadAccess {
    const GIT_WORKFLOW: Self = Self {
        ssh: true,
        github_cli: true,
    };
}

/// 单个插件的宿主侧执行策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostExecutionPolicy {
    /// sidecar 进程是否进 OS 沙箱（继承式，要求 stdio 传输）。
    pub sandbox: bool,
    /// 沙箱内是否放行网络（沙箱开启时恒为 true，见模块说明）。
    pub allow_network: bool,
    /// 与宿主的通信通道。
    pub transport: SidecarTransport,
    /// 宿主固定授予的用户凭据只读能力。
    pub user_credential_reads: UserCredentialReadAccess,
    /// 是否允许写入当前用户的 `~/.cache`。
    pub allow_user_cache_write: bool,
    /// 沙箱内是否放行系统音频输入服务（麦克风录音）。
    pub allow_audio_input: bool,
}

/// 解析插件的宿主执行策略。
///
/// 本策略只定义宿主的强制沙箱基线，不读取用户开关。是否跟随用户全局
/// 沙箱开关由 registry 按进程生命周期标记，并由 SidecarConfig 在每次 spawn
/// 时组合；预加载常驻服务不接受开关降级。策略本身保持纯函数。
pub fn resolve(plugin_id: &str, official_signed: bool) -> HostExecutionPolicy {
    let git_workflow = official_signed && matches!(plugin_id, "terminal" | "command");
    let user_credential_reads = if git_workflow {
        UserCredentialReadAccess::GIT_WORKFLOW
    } else {
        UserCredentialReadAccess::default()
    };
    // 官方 computer-use 在所有平台都不进 OS 沙箱，必须与用户桌面处于同一
    // 权限级别，否则桌面控制能力整体失效：
    // - macOS：辅助功能授权归属于天工 App，再套 Seatbelt 时 AXIsProcessTrusted
    //   无法继承宿主授权；
    // - Windows：AppContainer 令牌为低完整性级别，SetCursorPos 被拒绝
    //   （0x80070005），跨进程 UI Automation 调用受 UIPI 限制而阻塞，且经它
    //   启动的应用会继承沙箱令牌；
    // - Linux：bubblewrap 隔离会切断 AT-SPI2 会话总线与图形会话访问。
    // 仅对已通过官方发布者签名校验的同名插件保留宿主直启；第三方、自签和
    // 本地插件不得按名称获得例外。通信仍固定使用 stdio，生命周期仍由宿主管理。
    let accessibility_host_child = official_signed && plugin_id == "computer-use";
    // 官方 volcengine 提供按住说话录音，sidecar 需在沙箱内经 CoreAudio 打开
    // 麦克风；仅按已验证官方身份授予，清单声明不构成授权输入。
    let audio_input = official_signed && plugin_id == "volcengine";
    HostExecutionPolicy {
        sandbox: !accessibility_host_child,
        allow_network: true,
        transport: SidecarTransport::Stdio,
        user_credential_reads,
        allow_user_cache_write: git_workflow,
        allow_audio_input: audio_input,
    }
}

/// 策略表快照（审计与测试用）；沙箱开关默认开启。
pub fn catalog_snapshot() -> BTreeMap<String, HostExecutionPolicy> {
    [
        "command",
        "fetch",
        "mcp",
        "scheduler",
        "memory",
        "fs",
        "terminal",
        "index",
    ]
    .iter()
    .map(|id| ((*id).to_string(), resolve(id, true)))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_plugins_are_sandboxed_with_stdio_by_default() {
        for id in [
            "command",
            "fetch",
            "mcp",
            "scheduler",
            "memory",
            "fs",
            "terminal",
            "index",
            "unknown-third-party",
        ] {
            let policy = resolve(id, true);
            assert!(policy.sandbox, "{id} 默认必须进沙箱");
            assert_eq!(
                policy.transport,
                SidecarTransport::Stdio,
                "{id} 沙箱形态走 stdio"
            );
            assert!(policy.allow_network, "{id} 沙箱开启即网络全放行");
        }
    }

    #[test]
    fn resolve_defines_forced_sandbox_baseline() {
        // 权威策略表只表达强制沙箱基线；用户开关在 SidecarConfig spawn 层组合。
        for id in ["command", "fetch", "fs"] {
            let policy = resolve(id, true);
            assert!(policy.sandbox, "{id} 恒沙箱");
            assert_eq!(policy.transport, SidecarTransport::Stdio);
        }
    }

    #[test]
    fn computer_use_accessibility_exception_requires_official_signature() {
        let official = resolve("computer-use", true);
        let third_party = resolve("computer-use", false);

        assert!(
            !official.sandbox,
            "官方 computer-use 在所有平台都应与用户桌面同权限运行"
        );
        assert_eq!(official.transport, SidecarTransport::Stdio);

        assert!(third_party.sandbox, "非官方同名插件必须保持 OS 沙箱");
        assert_eq!(third_party.transport, SidecarTransport::Stdio);
    }

    #[test]
    fn official_signature_does_not_unsandbox_other_plugins() {
        for id in ["terminal", "command", "mcp", "unknown-third-party"] {
            assert!(resolve(id, true).sandbox, "官方 {id} 不应获得辅助功能例外");
        }
    }

    #[test]
    fn git_credentials_are_readable_only_to_official_terminal_and_command() {
        for id in ["terminal", "command"] {
            assert_eq!(
                resolve(id, true).user_credential_reads,
                UserCredentialReadAccess::GIT_WORKFLOW,
                "官方 {id} 应获得 Git 凭据只读能力"
            );
            assert!(resolve(id, true).allow_user_cache_write);
            assert_eq!(
                resolve(id, false).user_credential_reads,
                UserCredentialReadAccess::default(),
                "非官方 {id} 不得按名称获得凭据能力"
            );
            assert!(!resolve(id, false).allow_user_cache_write);
        }
        for id in ["coding", "fetch", "mcp", "unknown-third-party"] {
            assert_eq!(
                resolve(id, true).user_credential_reads,
                UserCredentialReadAccess::default(),
                "{id} 不需要 Git 凭据读取能力"
            );
            assert!(!resolve(id, true).allow_user_cache_write);
        }
    }

    #[test]
    fn audio_input_is_granted_only_to_official_volcengine() {
        let official = resolve("volcengine", true);
        assert!(official.allow_audio_input, "官方 volcengine 需要录音能力");
        assert!(official.sandbox, "录音授权不改变沙箱基线");
        assert!(
            !resolve("volcengine", false).allow_audio_input,
            "非官方同名插件不得按名称获得录音能力"
        );
        for id in ["terminal", "command", "computer-use", "unknown-third-party"] {
            assert!(!resolve(id, true).allow_audio_input, "{id} 不需要录音能力");
        }
    }

    #[test]
    fn policy_is_pure_and_stable() {
        // 不变式：同一输入恒返回同一策略（无隐藏全局状态），策略仅由
        // 插件 id 与已验证发布者身份决定——agent 执行过程中不存在升级输入。
        for id in ["command", "fetch", "mcp"] {
            assert_eq!(resolve(id, true), resolve(id, true));
            assert_eq!(resolve(id, false), resolve(id, false));
        }
    }
}
