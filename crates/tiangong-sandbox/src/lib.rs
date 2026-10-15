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

/// App ↔ Launcher 通信协议版本（宿主与 Launcher 双侧引用的唯一定义）。
/// 在线更新清单按此字段判定兼容性：宿主只接受与自身相等的新 Launcher。
pub const LAUNCHER_PROTOCOL_VERSION: u32 = 1;
/// 策略 Schema 版本（安全语义层）。与协议版本分开演进；在线更新同样
/// 只接受相等值，杜绝新旧策略语义错位。
pub const LAUNCHER_POLICY_SCHEMA: u32 = 2;

pub use path::canonicalize_path;
pub use sandbox::{
    SandboxAvailability, SandboxMode, SandboxPolicy, SandboxResourceLimits, SandboxedProgram,
    availability, explain_violation, wrap,
};
