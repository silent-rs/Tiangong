//! sidecar 运行时的宿主权威执行策略。
//!
//! 分层隔离：sidecar 默认经 Launcher 进 OS 沙箱；用户可显式关闭首次实际
//! 使用才启动的按需进程沙箱。
//!
//! # 职责边界
//!
//! 本模块只定义**策略数据结构**与**注入点**：「哪个插件获得哪些特权」是
//! 宿主产品策略，集中在 [`table`]（[`install`] 注入）。未注入时按
//! [`HostExecutionPolicy::sandboxed_default`] 处理——进沙箱、零特权、
//! stdio（fail-closed），保证漏配不会放大权限。
//!
//! 不变式（沙箱不进行 agent 执行过程中升级）：
//! - 策略只在连接构造时求值一次并定死；插件清单与 agent 运行时的任何
//!   声明/请求都不构成提权输入。
//! - 运行中的连接不升级也不降级；策略变更只可能来自用户设置或宿主注入，
//!   且在下一次连接构造时生效。
//! - ephemeral 形态（command）每次请求构造独立快照与独立沙箱实例，
//!   属"每请求隔离"而非升级。
//!
//! 网络维度：沙箱开启即视为执行安全，网络全放行。

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use crate::launch_policy::CAPABILITY_CREDENTIAL_SERVICES;

pub mod table;

pub use table::{execution_policy, install};

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
    /// 是否允许写入插件开发目录（宿主按已验证身份授予创作类插件；
    /// 目录清单由连接构造层经存储布局翻译，传输层不感知来源）。
    pub plugin_dev_write: bool,
    /// 沙箱内授予的特殊能力名（凭据服务、录音、无头浏览器等，见
    /// `launch_policy::CAPABILITY_*`）。整条链路只传这一个集合；能力怎样
    /// 变成系统规则由 Launcher 定义。
    pub capabilities: BTreeSet<String>,
    /// sidecar 采用「每请求一次性进程」形态（命令执行位：每次调用独立
    /// 进程、独立沙箱与工作区）。由宿主策略决定，插件清单不参与。
    pub ephemeral_sidecar: bool,
    /// 未通过官方发布者签名校验即拒绝启动（宿主对特定执行位的硬要求）。
    pub require_official_signature: bool,
}

impl HostExecutionPolicy {
    /// 缺省策略：进沙箱、零特权、stdio。宿主未注入提供者时使用。
    pub fn sandboxed_default() -> Self {
        Self {
            sandbox: true,
            allow_network: true,
            transport: SidecarTransport::Stdio,
            user_credential_reads: UserCredentialReadAccess::default(),
            allow_user_cache_write: false,
            plugin_dev_write: false,
            capabilities: BTreeSet::new(),
            ephemeral_sidecar: false,
            require_official_signature: false,
        }
    }

    /// 是否授予了指定沙箱能力。
    pub fn has(&self, capability: &str) -> bool {
        self.capabilities.contains(capability)
    }

    /// 设置用户凭据目录读取能力。凭据服务能力由 [`resolve`] 统一补齐
    /// （Git/SSH/GitHub CLI 需经系统凭据服务解析用户身份）。
    pub fn with_user_credential_reads(mut self, access: UserCredentialReadAccess) -> Self {
        self.user_credential_reads = access;
        self
    }

    /// 追加一项沙箱特殊能力。
    pub fn with_capability(mut self, capability: &str) -> Self {
        self.capabilities.insert(capability.to_string());
        self
    }
}

/// 宿主权威策略提供者：`(插件 ID, 是否官方签名) → 执行策略`。
///
/// 由宿主启动时注入（覆盖语义）。runtime 不持有任何插件 ID——策略表属于
/// 宿主产品数据；未注入时 [`resolve`] 返回 fail-closed 默认策略。
pub type HostPolicyProvider = Arc<dyn Fn(&str, bool) -> HostExecutionPolicy + Send + Sync>;

static HOST_POLICY_PROVIDER: RwLock<Option<HostPolicyProvider>> = RwLock::new(None);
/// 漏注入告警只发一次：避免每次 resolve 都刷日志。
static PROVIDER_MISSING_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 注入宿主权威策略提供者（覆盖语义；缺失时全沙箱零特权）。
pub fn set_host_policy_provider(provider: HostPolicyProvider) {
    if let Ok(mut current) = HOST_POLICY_PROVIDER.write() {
        *current = Some(provider);
    }
}

/// 清除宿主策略提供者（恢复 fail-closed 默认）。测试隔离用。
pub fn clear_host_policy_provider() {
    if let Ok(mut current) = HOST_POLICY_PROVIDER.write() {
        *current = None;
    }
    // 同步复位漏注入告警标志：下个未注入场景仍应可见（测试隔离亦然）。
    PROVIDER_MISSING_WARNED.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// 解析插件的宿主执行策略：有注入则用宿主表，否则 fail-closed 默认。
///
/// 本策略只定义宿主的强制沙箱基线，不读取用户开关。是否跟随用户全局
/// 沙箱开关由 registry 按进程生命周期标记，并由 SidecarConfig 在每次 spawn
/// 时组合；预加载常驻服务不接受开关降级。
pub fn resolve(plugin_id: &str, official_signed: bool) -> HostExecutionPolicy {
    let policy = HOST_POLICY_PROVIDER
        .read()
        .ok()
        .and_then(|current| current.clone());
    let policy = match policy {
        Some(provider) => provider(plugin_id, official_signed),
        None => {
            if !PROVIDER_MISSING_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    plugin_id,
                    "宿主策略提供者未注入：按 fail-closed 全沙箱零特权处理，受限能力（桌面控制等）将不可用；生产入口应在启动时调用 host_policy::install()"
                );
            }
            HostExecutionPolicy::sandboxed_default()
        }
    };
    normalize(policy)
}

/// 补齐策略派生关系：凭据目录读取必然需要凭据服务能力。放在 runtime 而
/// 不是宿主表里，保证宿主只声明"能读哪些凭据"也不会漏掉服务授权。
fn normalize(mut policy: HostExecutionPolicy) -> HostExecutionPolicy {
    if policy.user_credential_reads.ssh || policy.user_credential_reads.github_cli {
        policy
            .capabilities
            .insert(CAPABILITY_CREDENTIAL_SERVICES.to_string());
    }
    policy
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch_policy::{
        CAPABILITY_AUDIO_INPUT, CAPABILITY_BROWSER_RUNTIME, CAPABILITY_PROCESS_INSPECTION,
    };

    fn provider(policy: HostExecutionPolicy) -> HostPolicyProvider {
        Arc::new(move |_, _| policy.clone())
    }

    #[test]
    #[serial_test::serial]
    fn 未注入提供者时按沙箱零特权处理() {
        clear_host_policy_provider();
        let policy = resolve("terminal", true);
        assert!(policy.sandbox);
        assert_eq!(policy.transport, SidecarTransport::Stdio);
        assert!(policy.allow_network);
        assert!(policy.capabilities.is_empty());
        assert!(!policy.user_credential_reads.ssh);
        assert!(!policy.allow_user_cache_write);
        assert!(!policy.plugin_dev_write);
    }

    #[test]
    #[serial_test::serial]
    fn 注入提供者后按宿主表解析且与插件身份无关() {
        let mut granted = HostExecutionPolicy::sandboxed_default();
        granted = granted.with_capability(CAPABILITY_AUDIO_INPUT);
        set_host_policy_provider(provider(granted));
        // runtime 不区分插件 ID：同一份策略对任意身份生效。
        assert!(resolve("任意插件", true).has(CAPABILITY_AUDIO_INPUT));
        assert!(resolve("另一个插件", false).has(CAPABILITY_AUDIO_INPUT));
        clear_host_policy_provider();
        assert!(!resolve("任意插件", true).has(CAPABILITY_AUDIO_INPUT));
    }

    #[test]
    #[serial_test::serial]
    fn 凭据读取连带授予凭据服务能力() {
        let with_credentials = HostExecutionPolicy::sandboxed_default().with_user_credential_reads(
            UserCredentialReadAccess {
                ssh: true,
                github_cli: false,
            },
        );
        set_host_policy_provider(provider(with_credentials));
        assert!(resolve("任意插件", true).has(CAPABILITY_CREDENTIAL_SERVICES));

        let without = HostExecutionPolicy::sandboxed_default();
        set_host_policy_provider(provider(without));
        assert!(!resolve("任意插件", true).has(CAPABILITY_CREDENTIAL_SERVICES));
        clear_host_policy_provider();
    }

    #[test]
    fn 默认策略常量覆盖沙箱与能力() {
        // 能力常量与策略字段一一对应，避免宿主表引用了 runtime 不认识的名称。
        let policy = HostExecutionPolicy::sandboxed_default()
            .with_capability(CAPABILITY_BROWSER_RUNTIME)
            .with_capability(CAPABILITY_PROCESS_INSPECTION);
        assert!(policy.has(CAPABILITY_BROWSER_RUNTIME));
        assert!(policy.has(CAPABILITY_PROCESS_INSPECTION));
        assert!(!policy.has(CAPABILITY_CREDENTIAL_SERVICES));
        assert!(!policy.has(CAPABILITY_AUDIO_INPUT));
    }
}
