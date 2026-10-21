//! 沙箱执行层（RFC 0017，任务边界收敛版）。
//!
//! 职责：命令套壳沙箱——策略编译（macOS Seatbelt / Linux bubblewrap /
//! Windows AppContainer）、命令包装、平台能力探测、fail-closed、
//! 宿主策略表与沙箱程序（bin）。
//!
//! 已拆分到独立分支：
//! - 插件信任模型 → feature/plugin-trust-model
//! - 工作区快照恢复 → feature/workspace-snapshots
//! - 无沙箱审批升级 → feature/sandbox-escalation

pub mod installed;
pub mod launcher_manager;
mod path;
pub mod sandbox;
pub mod update;

/// Windows 上通过进程环境传递一次性 Launcher 请求；Launcher 启动目标前会移除。
/// Unix 使用 fd3，不使用该变量。
pub const POLICY_ENV: &str = "TIANGONG_SANDBOX_REQUEST";
/// Windows 宿主创建的一次性停止事件名称；只供 Launcher 读取并在启动目标前移除。
pub const WINDOWS_STOP_EVENT_ENV: &str = "TIANGONG_SANDBOX_STOP_EVENT";
/// Launcher 用于监视宿主异常退出的进程 ID 环境变量。
pub const HOST_PID_ENV: &str = "TIANGONG_PLUGIN_HOST_PID";
/// Unix fd3 策略帧允许的最大 JSON 长度，防止 Launcher 按不可信长度分配内存。
pub const MAX_POLICY_FRAME_BYTES: usize = 1024 * 1024;

/// 已废弃版本字段的冻结取值（临时兼容层）。
///
/// App ↔ Launcher 不再做协议版本与策略 Schema 版本化：请求新增字段一律
/// 带默认值，旧格式由兼容层转换，特殊能力经自检报告的 `capabilities`
/// 协商，未知能力 fail-closed。这些字段的值冻结不变，新代码不得据此做
/// 兼容性判断，仅为以下旧组件保留：
/// - 已发布 App（≤ v0.17.5）要求 Launcher 自检报告与更新清单的
///   `protocol_version` 恰为 1、`policy_schema`/`policy_schema_max` 恰为 2；
/// - 已发布 Launcher（≤ 0.1.15）要求启动请求携带这两个字段且取值相等。
///
/// 旧组件全部退出支持后连同这些字段一并移除，或决定永久保留。
pub mod legacy {
    /// 冻结的 `protocol_version` 取值。
    pub const PROTOCOL_VERSION: u32 = 1;
    /// 冻结的 `policy_schema` / `policy_schema_max` 取值。
    pub const POLICY_SCHEMA: u32 = 2;
}

pub use path::canonicalize_path;
pub use sandbox::{
    SandboxAvailability, SandboxCapability, SandboxMode, SandboxPolicy, SandboxResourceLimits,
    SandboxedProgram, availability, explain_violation, supported_capabilities, wrap,
};
