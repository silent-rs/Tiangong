//! Sandbox Launcher 的安装、更新与宿主交互约定（轻量，不含平台沙箱实现）。
//!
//! 宿主（App）与 Launcher 共同依赖本 crate：
//! - [`update`]：官方清单拉取、下载、验签、候选自检与原子替换；
//! - [`installed`]：已安装 Launcher 的官方签名与自检验证；
//! - [`launcher_manager`]：安装目录内的程序名与伴生签名名约定；
//! - 宿主 ↔ Launcher 的传输约定常量（环境变量名、帧长度上限、冻结兼容值）。
//!
//! 平台沙箱实现（Seatbelt / bubblewrap / AppContainer）只在
//! `tiangong-sandbox` 中，宿主不依赖它：宿主按 JSON 约定构造启动请求，
//! 由 Launcher 负责解析与执行。
pub mod installed;
pub mod launcher_manager;
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
