//! 平台无障碍后端抽象与各平台实现。
//!
//! trait [`Backend`] 定义统一的桌面访问能力，各平台按条件编译提供实现：
//! - macOS：通过 objc2 探测图形会话与辅助功能授权，列举运行中的应用窗口。
//! - Windows：UI Automation 控件树 + SendInput 键鼠 + GDI/WIC 截图 + 指针/HUD。
//! - Linux：AT-SPI2 控件树（键鼠、截图等返回明确的能力不足结果）。

use async_trait::async_trait;

use tiangong_plugin_computer_use_protocol::ops::{
    ActionRequest, FindRequest, KeyboardRequest, ListWindowsRequest, MouseRequest, OpenAppRequest,
    OpenAppResponse, ScreenshotRequest, SnapshotRequest, WaitRequest,
};
use tiangong_plugin_computer_use_protocol::{
    AccessibilityCapability, ActionKind, DesktopError, DesktopResult, DesktopSession,
    ListWindowsResponse, Platform, ScreenshotResponse,
};

/// 平台无障碍后端能力。
///
/// 所有方法返回 `DesktopResult<T>`，业务错误统一由 [`DesktopError`] 表达，
/// 便于上层序列化回 WASM 侧。
#[async_trait]
pub trait Backend: Send + Sync {
    /// 当前编译目标平台。
    fn platform(&self) -> Platform;

    /// 探测图形会话与无障碍能力，返回受支持的动作集合。
    async fn status(&self) -> DesktopResult<StatusInfo>;

    /// 列出当前可访问的应用和顶层窗口。
    async fn list_windows(&self, req: &ListWindowsRequest) -> DesktopResult<ListWindowsResponse>;

    /// 读取控件树快照。
    async fn snapshot(&self, req: &SnapshotRequest) -> DesktopResult<SnapshotInfo>;

    /// 在窗口或快照内查找控件。
    async fn find(&self, req: &FindRequest) -> DesktopResult<FindInfo>;

    /// 对控件执行动作。
    async fn action(&self, req: &ActionRequest) -> DesktopResult<ActionResult>;

    /// 等待条件满足。
    async fn wait(&self, req: &WaitRequest) -> DesktopResult<WaitResult>;

    /// 截取屏幕或指定应用窗口的截图（RFC 0017 图片注入的图源）。
    ///
    /// 默认返回平台不支持：尚未实现原生截图的平台保持能力缺失的
    /// 明确失败，而不是静默空图。
    /// 坐标级鼠标手势（CGEvent 合成，RFC 0018 §2.3）。默认不支持，
    /// macOS 后端实装。
    async fn mouse(&self, _req: &MouseRequest) -> DesktopResult<MouseResult> {
        DesktopResult::Err(DesktopError::ActionNotSupported {
            action: "desktop_mouse".to_string(),
            supported: vec![],
        })
    }
    /// 键盘合成输入（CGEvent，RFC 0018 §2.4）。默认不支持，macOS 实装。
    async fn keyboard(&self, _req: &KeyboardRequest) -> DesktopResult<KeyboardResult> {
        DesktopResult::Err(DesktopError::ActionNotSupported {
            action: "desktop_keyboard".to_string(),
            supported: vec![],
        })
    }
    async fn screenshot(&self, _req: &ScreenshotRequest) -> DesktopResult<ScreenshotResponse> {
        DesktopResult::Err(unsupported_screenshot(self.platform()))
    }
    /// 唤起或启动应用（激活、取消隐藏/最小化、系统搜索定位）。默认不支持，
    /// macOS 实装。
    async fn open_app(&self, _req: &OpenAppRequest) -> DesktopResult<OpenAppResponse> {
        DesktopResult::Err(DesktopError::ActionNotSupported {
            action: "desktop_open_app".to_string(),
            supported: vec![],
        })
    }
    /// 把天工窗口还原到自动分屏前的位置（只动天工窗口）。默认不支持
    /// （Linux 不做自动分屏）。
    async fn restore_host_window(
        &self,
        _saved: crate::split::SavedHostFrame,
    ) -> Result<(), String> {
        Err("当前平台不支持自动分屏".to_string())
    }
}

/// `desktop_status` 返回信息。
#[derive(Debug, Clone)]
pub struct StatusInfo {
    pub session: DesktopSession,
    pub accessibility: AccessibilityCapability,
    pub supported_actions: Vec<ActionKind>,
}

/// `desktop_snapshot` 返回信息（节点平铺）。
#[derive(Debug, Clone, Default)]
pub struct SnapshotInfo {
    pub snapshot: u64,
    pub nodes: Vec<tiangong_plugin_computer_use_protocol::ControlNode>,
    pub truncated: bool,
    pub warnings: Vec<String>,
}

/// `desktop_find` 返回信息。
#[derive(Debug, Clone, Default)]
pub struct FindInfo {
    pub matches: Vec<tiangong_plugin_computer_use_protocol::ControlNode>,
    pub snapshot: u64,
    pub ambiguous: bool,
}

/// `desktop_action` 返回信息。
#[derive(Debug, Clone, Default)]
pub struct ActionResult {
    pub performed: bool,
    pub summary: String,
    pub new_window: Option<tiangong_plugin_computer_use_protocol::ElementRef>,
}

/// `desktop_mouse` 返回信息。
#[derive(Debug, Clone, Default)]
pub struct MouseResult {
    pub performed: bool,
    pub summary: String,
}
/// `desktop_keyboard` 返回信息。
#[derive(Debug, Clone, Default)]
pub struct KeyboardResult {
    pub performed: bool,
    pub summary: String,
}
/// `desktop_wait` 返回信息。
#[derive(Debug, Clone, Default)]
pub struct WaitResult {
    pub satisfied: bool,
    pub waited_ms: u64,
    pub matched_element: Option<tiangong_plugin_computer_use_protocol::ElementRef>,
    /// 判定依据（人读），无则省略。
    pub detail: Option<String>,
}

/// 三平台均受支持的动作集合（统一暴露给 Agent）。
pub fn all_supported_actions() -> Vec<ActionKind> {
    use ActionKind::*;
    vec![
        Focus,
        Press,
        SetValue,
        Toggle,
        Select,
        Expand,
        Collapse,
        ScrollIntoView,
    ]
}

/// 未实现原生截图平台的统一错误。
pub fn unsupported_screenshot(platform: Platform) -> DesktopError {
    DesktopError::UnsupportedPlatform {
        platform: format!("{platform:?}"),
    }
}

/// macOS 实际能执行的动作子集。
/// 排除 Expand/Collapse（AX 中 AXPress 是切换，无法保证方向）和
/// ScrollIntoView（AX 无对应语义动作），避免 status 暗示支持但 action 失败。
pub fn macos_supported_actions() -> Vec<ActionKind> {
    use ActionKind::*;
    vec![Focus, Press, SetValue, Toggle, Select]
}

/// 在运行时构造当前平台的后端实例。
pub fn current_backend() -> Box<dyn Backend> {
    cfg_if_current_backend()
}

// ── 平台分发 ───────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn cfg_if_current_backend() -> Box<dyn Backend> {
    Box::new(crate::backend::macos::MacosBackend::new())
}

#[cfg(target_os = "windows")]
fn cfg_if_current_backend() -> Box<dyn Backend> {
    Box::new(crate::backend::windows::WindowsBackend::new())
}

#[cfg(target_os = "linux")]
fn cfg_if_current_backend() -> Box<dyn Backend> {
    Box::new(crate::backend::linux::LinuxBackend::new())
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn cfg_if_current_backend() -> Box<dyn Backend> {
    Box::new(crate::backend::stub::StubBackend::unsupported(
        Platform::Windows,
    ))
}

// ── 各平台后端 ─────────────────────────────────────────────────

#[cfg(target_os = "macos")]
pub mod app_launch;
#[cfg(target_os = "macos")]
pub mod ax;
/// 键盘合成输入（CGEvent，RFC 0018 §2.4，仅 macOS）。
#[cfg(target_os = "macos")]
pub mod keyboard;
#[cfg(target_os = "macos")]
pub mod keycast;
/// 按键 HUD 卡片栈（平台无关逻辑）。
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod keycast_stack;
/// 键名归一化与 HUD 显示符号（平台无关）。
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod keys;
#[cfg(target_os = "linux")]
pub mod linux;
/// 自动分屏（AX 摆放窗口，仅 macOS）。公开仅供 `examples/split_demo` 真机验证。
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub mod mac_split;
#[cfg(target_os = "macos")]
pub mod macos;
/// 坐标级鼠标手势（CGEvent 合成，RFC 0018 §2.3，仅 macOS）。
#[cfg(target_os = "macos")]
pub mod mouse;
/// 天工虚拟指针 overlay（RFC 0018，仅 macOS）。
#[cfg(target_os = "macos")]
pub mod overlay;
/// 截图产物规划（平台无关）。
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod screenshot_plan;
/// Windows 截图、窗口枚举与应用唤起。
#[cfg(target_os = "windows")]
pub mod win_desktop;
/// Windows SendInput 键鼠合成。
#[cfg(target_os = "windows")]
pub mod win_input;
/// Windows 按键 HUD。
#[cfg(target_os = "windows")]
pub mod win_keycast;
/// Windows 天工虚拟指针 overlay。
#[cfg(target_os = "windows")]
pub mod win_overlay;
/// Windows 自动分屏（SetWindowPos）。
#[cfg(target_os = "windows")]
pub mod win_split;
#[cfg(target_os = "windows")]
pub mod windows;

/// 通用存根后端：用于尚未实现原生能力的平台，返回明确的能力不足结果。
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub mod stub;
