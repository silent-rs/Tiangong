//! 宿主权威执行策略表：插件身份 → 沙箱与特权。
//!
//! 这是**产品策略**而非运行时机制：runtime 只保留策略数据结构与注入点
//! （`tiangong_plugin_runtime::host_policy`），「哪个插件 ID 获得哪些特权」
//! 集中在这里，由各宿主（桌面、CLI、Server、e2e）在启动时经 [`install`]
//! 注入。未注入时 runtime 按全沙箱零特权处理（fail-closed）。
//!
//! 授权原则：特权只按**已验证的官方发布者签名 + 精确插件 ID** 授予；插件
//! 清单的自声明与调用载荷都不构成提权输入。

use std::sync::Arc;

use tiangong_plugin_runtime::host_policy::{
    HostExecutionPolicy, UserCredentialReadAccess, set_host_policy_provider,
};
use tiangong_plugin_runtime::launch_policy::{
    CAPABILITY_AUDIO_INPUT, CAPABILITY_BROWSER_RUNTIME, CAPABILITY_PROCESS_INSPECTION,
};

/// 安装本策略表为当前进程的宿主权威策略。
pub fn install() {
    set_host_policy_provider(Arc::new(execution_policy));
}

/// 解析插件在宿主侧的强制执行策略。
pub fn execution_policy(plugin_id: &str, official_signed: bool) -> HostExecutionPolicy {
    // Git/SSH/GitHub CLI 完整工作流：凭据目录只读 + 凭据服务 + 包缓存写。
    let git_workflow = official_signed && matches!(plugin_id, "terminal" | "command");
    // 命令执行位（浏览器运行时、进程表查询）：排查后台进程、跑无头浏览器
    // 属于命令执行的日常操作，与 Git 工作流同源，保持独立能力名以便分开
    // 调整。command 处于淘汰序列，不新增授权。
    let command_slot = official_signed && matches!(plugin_id, "terminal" | "command");
    // 插件开发目录写域：devkit 工具链在 terminal（命令通道手动构建/调试）
    // 与 plugin-creator（sidecar 内真实执行 init/build/add）两个执行位落地。
    let plugin_dev_write = official_signed && matches!(plugin_id, "terminal" | "plugin-creator");
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
    // 麦克风；仅按已验证官方身份授予。
    let audio_input = official_signed && plugin_id == "volcengine";
    // 命令执行位：每请求一次性进程（独立工作区与沙箱），且必须官方签名——
    // 未签名的同名插件在连接构造时即拒绝启动。
    let command_execution_slot = plugin_id == "command";

    let mut policy =
        HostExecutionPolicy::sandboxed_default().with_user_credential_reads(if git_workflow {
            UserCredentialReadAccess {
                ssh: true,
                github_cli: true,
            }
        } else {
            UserCredentialReadAccess::default()
        });
    policy.allow_user_cache_write = git_workflow;
    policy.plugin_dev_write = plugin_dev_write;
    policy.sandbox = !accessibility_host_child;
    policy.ephemeral_sidecar = command_execution_slot;
    policy.require_official_signature = command_execution_slot;
    if command_slot {
        policy = policy
            .with_capability(CAPABILITY_BROWSER_RUNTIME)
            .with_capability(CAPABILITY_PROCESS_INSPECTION);
    }
    if audio_input {
        policy = policy.with_capability(CAPABILITY_AUDIO_INPUT);
    }
    policy
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_plugin_runtime::host_policy::{SidecarTransport, clear_host_policy_provider};

    const OFFICIAL_IDS: &[&str] = &[
        "terminal",
        "command",
        "computer-use",
        "volcengine",
        "plugin-creator",
        "fetch",
        "fs",
        "mcp",
        "memory",
        "scheduler",
        "index",
    ];

    #[test]
    fn 默认全沙箱走_stdio() {
        for id in OFFICIAL_IDS.iter().chain(["unknown-third-party"].iter()) {
            let policy = execution_policy(id, true);
            assert_eq!(
                policy.transport,
                SidecarTransport::Stdio,
                "{id} 沙箱形态走 stdio"
            );
            assert!(policy.allow_network, "{id} 沙箱开启即网络全放行");
        }
        for id in ["fetch", "fs", "mcp", "unknown-third-party"] {
            assert!(execution_policy(id, true).sandbox, "{id} 默认必须进沙箱");
        }
    }

    #[test]
    fn git_工作流只授予官方_terminal_与_command() {
        for id in ["terminal", "command"] {
            let policy = execution_policy(id, true);
            assert!(policy.user_credential_reads.ssh, "{id} 官方需读 SSH");
            assert!(policy.user_credential_reads.github_cli);
            assert!(policy.allow_user_cache_write);
        }
        for id in ["terminal", "command"] {
            let third_party = execution_policy(id, false);
            assert!(
                !third_party.user_credential_reads.ssh,
                "{id} 第三方不得读凭据"
            );
            assert!(!third_party.allow_user_cache_write);
        }
        for id in ["coding", "fetch", "mcp", "unknown-third-party"] {
            let policy = execution_policy(id, true);
            assert!(!policy.user_credential_reads.ssh, "{id} 官方也不得读凭据");
            assert!(!policy.allow_user_cache_write);
        }
    }

    #[test]
    fn computer_use_免沙箱例外只认官方签名() {
        assert!(!execution_policy("computer-use", true).sandbox);
        assert!(execution_policy("computer-use", false).sandbox);
        for id in ["terminal", "command", "mcp", "unknown-third-party"] {
            assert!(execution_policy(id, true).sandbox, "官方 {id} 不应获得例外");
        }
    }

    #[test]
    fn 录音能力只授予官方_volcengine() {
        assert!(
            execution_policy("volcengine", true).has(CAPABILITY_AUDIO_INPUT),
            "官方 volcengine 需要麦克风"
        );
        assert!(!execution_policy("volcengine", false).has(CAPABILITY_AUDIO_INPUT));
        for id in ["terminal", "command", "computer-use", "unknown-third-party"] {
            assert!(!execution_policy(id, true).has(CAPABILITY_AUDIO_INPUT));
        }
    }

    #[test]
    fn 命令执行位能力只授予官方_terminal_与_command() {
        for id in ["terminal", "command"] {
            let policy = execution_policy(id, true);
            assert!(
                policy.has(CAPABILITY_BROWSER_RUNTIME),
                "{id} 需跑无头浏览器"
            );
            assert!(policy.has(CAPABILITY_PROCESS_INSPECTION), "{id} 需查进程表");
        }
        for id in ["terminal", "command"] {
            let third_party = execution_policy(id, false);
            assert!(!third_party.has(CAPABILITY_BROWSER_RUNTIME));
            assert!(!third_party.has(CAPABILITY_PROCESS_INSPECTION));
        }
        for id in ["volcengine", "computer-use", "fetch", "unknown-third-party"] {
            assert!(!execution_policy(id, true).has(CAPABILITY_BROWSER_RUNTIME));
            assert!(!execution_policy(id, true).has(CAPABILITY_PROCESS_INSPECTION));
        }
    }

    #[test]
    fn 插件开发写域只授予官方_terminal_与_plugin_creator() {
        for id in ["terminal", "plugin-creator"] {
            assert!(
                execution_policy(id, true).plugin_dev_write,
                "{id} 需要开发写域"
            );
        }
        for (id, official) in [
            ("terminal", false),
            ("plugin-creator", false),
            ("command", true),
            ("fetch", true),
        ] {
            assert!(
                !execution_policy(id, official).plugin_dev_write,
                "{id}（official={official}）不应获得开发写域"
            );
        }
    }

    #[test]
    fn 命令执行位采用一次性进程且要求官方签名() {
        let command = execution_policy("command", true);
        assert!(command.ephemeral_sidecar, "command 需每请求一次性进程");
        assert!(command.require_official_signature, "command 必须官方签名");
        for id in ["terminal", "fetch", "fs", "unknown-third-party"] {
            let policy = execution_policy(id, true);
            assert!(!policy.ephemeral_sidecar, "{id} 不应采用一次性进程");
            assert!(!policy.require_official_signature, "{id} 不应强制官方签名");
        }
    }

    #[test]
    fn 同一身份解析稳定() {
        for id in OFFICIAL_IDS {
            assert_eq!(execution_policy(id, true), execution_policy(id, true));
            assert_eq!(execution_policy(id, false), execution_policy(id, false));
        }
    }

    #[test]
    #[serial_test::serial]
    fn install_后运行时按本表解析() {
        install();
        let policy = tiangong_plugin_runtime::host_policy::resolve("terminal", true);
        assert!(policy.plugin_dev_write);
        assert!(policy.user_credential_reads.ssh);
        clear_host_policy_provider();
        let default = tiangong_plugin_runtime::host_policy::resolve("terminal", true);
        assert!(!default.plugin_dev_write, "未注入时必须 fail-closed");
    }
}
