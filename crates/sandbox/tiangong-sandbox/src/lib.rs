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

mod path;
pub mod sandbox;
// 安装、更新与宿主交互约定已拆到轻量的 tiangong-sandbox-update（宿主只依赖
// 它，不编译平台沙箱实现）；这里重导出，Launcher 内部路径保持不变。
pub use path::canonicalize_path;
pub use sandbox::{
    SandboxAvailability, SandboxCapability, SandboxMode, SandboxPolicy, SandboxResourceLimits,
    SandboxedProgram, availability, explain_violation, supported_capabilities, wrap,
};
pub use tiangong_sandbox_update::{
    HOST_PID_ENV, MAX_POLICY_FRAME_BYTES, POLICY_ENV, WINDOWS_STOP_EVENT_ENV, installed,
    launcher_manager, legacy, update,
};
