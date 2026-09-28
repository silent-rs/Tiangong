//! Computer Use 插件私有业务协议。
//!
//! 本 crate 只定义 WASM 与 sidecar 共同使用的操作、请求、响应、平台能力与错误类型，
//! 不包含 IPC、进程或 Wasmtime 依赖，可同时编译为本机与 `wasm32-wasip2`。
//!
//! 将 Windows UI Automation、macOS AXUIElement、Linux AT-SPI2 三套系统无障碍能力
//! 统一为同一组语义结构，对 Agent 暴露一致的请求和返回，不以坐标作为主要定位方式。

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub mod ops;
pub use tiangong_types::InjectedAsset;

// 响应类型重新导出到 crate 根，便于 sidecar backend 以短路径引用。
pub use ops::{
    AccessContext, ActionResponse, DesktopStatusResponse, FindResponse, ListWindowsResponse,
    ScreenshotResponse, SnapshotResponse, WaitResponse,
};

pub const PLUGIN_ID: &str = "computer-use";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const COMPUTER_USE_PROTOCOL_VERSION: u32 = 1;

/// 对 Agent 暴露的工具名（5 个，按职责合并；与 handle_tool 路由对齐）。
///
/// 业务操作（`*_OPERATION`）保持细粒度，wasm 按工具参数分派到对应操作。
/// - `desktop_app`：status / list / open（能力探测、窗口列举、唤起启动）
/// - `desktop_screenshot`：截图（图片注入）
/// - `desktop_input`：鼠标手势 + 键盘输入（可选操作后自动截图）
/// - `desktop_ui`：无障碍控件树读取/查找 + 语义动作
/// - `desktop_wait`：等待窗口/控件状态
pub const TOOL_DESKTOP_APP: &str = "desktop_app";
pub const TOOL_DESKTOP_SCREENSHOT: &str = "desktop_screenshot";
pub const TOOL_DESKTOP_INPUT: &str = "desktop_input";
pub const TOOL_DESKTOP_UI: &str = "desktop_ui";
pub const TOOL_DESKTOP_WAIT: &str = "desktop_wait";

/// 一个类型化 Computer Use 业务操作。
///
/// 每个操作由零字段 marker struct 实现，提供操作名常量与关联的请求/响应类型。
/// WASM 端通过 `sidecar_client::invoke::<O>()` 泛型调用，以 `NAME` 作为 operation、
/// 序列化 `Request`、反序列化 `Response`。
pub trait ComputerUseOperation {
    const NAME: &'static str;
    type Request: Serialize;
    type Response: DeserializeOwned;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ack {}

/// 操作系统平台标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Windows,
    Macos,
    Linux,
}

impl Platform {
    /// 当前编译目标平台。
    ///
    /// 注意：本常量仅供 sidecar 在本机编译时使用；protocol crate 仍需可编译为
    /// `wasm32-wasip2`（给 WASM 组件），因此非三平台 target 下返回占位值而非
    /// `compile_error!`，避免 WASM 侧编译失败。
    #[cfg(target_os = "windows")]
    pub const CURRENT: Self = Self::Windows;
    #[cfg(target_os = "macos")]
    pub const CURRENT: Self = Self::Macos;
    #[cfg(target_os = "linux")]
    pub const CURRENT: Self = Self::Linux;
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    pub const CURRENT: Self = Self::Linux;
}

/// 当前桌面图形会话状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopSession {
    /// 存在图形会话且无障碍后端可用。
    Available,
    /// 当前没有图形会话（纯 SSH、容器、无头环境）。
    Unavailable,
    /// 存在图形会话但无障碍能力未就绪（如未授权）。
    NotReady,
}

/// 平台无障碍能力是否可用及原因。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessibilityCapability {
    /// 无障碍后端是否已就绪可用。
    pub available: bool,
    /// 不可用时的简短原因（available=true 时为空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// 控件可执行的动作类型（对三平台统一）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Focus,
    Press,
    SetValue,
    Toggle,
    Select,
    Expand,
    Collapse,
    ScrollIntoView,
}

/// 屏幕边界（逻辑坐标，单位为平台逻辑像素/点）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
}

/// 平台可提供的稳定控件标识集合。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StableIdentifiers {
    /// Windows AutomationId / macOS AXIdentifier / Linux 应用自定义 id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_id: Option<String>,
    /// 平台控件角色（UIA ControlType / AXRole / AT-SPI Role）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// 临时窗口或控件引用，仅在对应快照内短时间有效，不持久化。
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ElementRef {
    /// sidecar 内部分配的引用标识。
    pub id: String,
    /// 引用所属快照版本，用于检测过期。
    pub snapshot: u64,
}

/// 顶层窗口信息。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowInfo {
    /// 应用展示名称。
    pub app_name: String,
    /// 进程编号（仅用于缩小当前范围，不作为跨启动稳定身份）。
    pub pid: u32,
    /// 临时窗口引用。
    pub element: ElementRef,
    /// 窗口标题。
    pub title: String,
    /// 是否前台窗口。
    pub is_foreground: bool,
    /// 窗口边界。
    pub bounds: Bounds,
    /// 是否可见。
    pub visible: bool,
    /// 是否可用（未被禁用）。
    pub enabled: bool,
    /// 平台稳定标识。
    pub identifiers: StableIdentifiers,
}

/// 控件树节点。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControlNode {
    /// 临时控件引用。
    pub element: ElementRef,
    /// 平台控件角色或控件类型描述。
    pub role: String,
    /// 显示名称。
    pub name: String,
    /// 平台稳定标识。
    pub identifiers: StableIdentifiers,
    /// 当前值；密码框等受保护控件为 `sensitive_value_redacted` 标记时不返回真实值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// 是否为受保护控件（密码/令牌等），其值已被隐藏。
    #[serde(default)]
    pub sensitive: bool,
    /// 是否可见。
    pub visible: bool,
    /// 是否可用。
    pub enabled: bool,
    /// 是否拥有焦点。
    pub focused: bool,
    /// 控件边界。
    pub bounds: Bounds,
    /// 支持的动作集合。
    pub actions: Vec<ActionKind>,
    /// 父控件引用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ElementRef>,
    /// 子控件引用列表。
    #[serde(default)]
    pub children: Vec<ElementRef>,
}

/// 匹配模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    #[default]
    Exact,
    Contains,
}

/// Computer Use 业务错误类型，面向 Agent 的简短说明 + 结构化原因。
///
/// 错误返回不包含原始敏感值；密码框等受保护内容统一标记为 `sensitive_value_redacted`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DesktopError {
    /// 当前平台不支持（如 Windows 后端在非 Windows 编译产物上运行）。
    UnsupportedPlatform { platform: String },
    /// 没有图形会话（纯 SSH、容器或无头环境）。
    DesktopSessionUnavailable { reason: String },
    /// 系统拒绝授权或权限等级不匹配。
    PermissionDenied { reason: String },
    /// 目标应用未找到。
    ApplicationNotFound { query: String },
    /// 目标窗口未找到。
    WindowNotFound { query: String },
    /// 目标控件未找到。
    ElementNotFound { query: String },
    /// 同名控件存在多个候选，需用户/Agent 明确指定。
    AmbiguousMatch { candidates: Vec<String> },
    /// 控件引用已过期（界面变化导致），需重新读取快照。
    StaleElement { snapshot: u64 },
    /// 控件不支持请求的动作。
    ActionNotSupported {
        action: String,
        supported: Vec<String>,
    },
    /// 受保护内容已被隐藏。
    SensitiveValueRedacted,
    /// 等待条件在超时内未满足。
    Timeout { waited_ms: u64 },
    /// 无障碍后端不可用。
    BackendUnavailable { reason: String },
}

impl DesktopError {
    /// 面向 Agent 的简短说明。
    pub fn agent_message(&self) -> String {
        match self {
            Self::UnsupportedPlatform { platform } => {
                format!("当前平台 {platform} 暂不支持桌面控制能力")
            }
            Self::DesktopSessionUnavailable { reason } => {
                format!("当前没有可用的图形桌面会话：{reason}")
            }
            Self::PermissionDenied { reason } => {
                format!("系统未授权桌面控制：{reason}")
            }
            Self::ApplicationNotFound { query } => format!("未找到目标应用：{query}"),
            Self::WindowNotFound { query } => format!("未找到目标窗口：{query}"),
            Self::ElementNotFound { query } => format!("未找到目标控件：{query}"),
            Self::AmbiguousMatch { candidates } => {
                format!("匹配到多个控件，请明确指定：{}", candidates.join("、"))
            }
            Self::StaleElement { snapshot } => {
                format!("控件引用已过期（快照 {snapshot}），请重新读取界面")
            }
            Self::ActionNotSupported { action, supported } => {
                if supported.is_empty() {
                    format!("控件不支持动作 {action}")
                } else {
                    format!(
                        "控件不支持动作 {action}，支持的动作：{}",
                        supported.join("、")
                    )
                }
            }
            Self::SensitiveValueRedacted => "受保护内容已隐藏".to_string(),
            Self::Timeout { waited_ms } => format!("等待条件在 {waited_ms} 毫秒内未满足"),
            Self::BackendUnavailable { reason } => format!("无障碍后端不可用：{reason}"),
        }
    }
}

/// 工具执行结果：成功携带数据，失败携带业务错误。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DesktopResult<T> {
    Ok(T),
    Err(DesktopError),
}

impl<T> DesktopResult<T> {
    pub fn ok(value: T) -> Self {
        Self::Ok(value)
    }
    pub fn err(error: DesktopError) -> Self {
        Self::Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::*;

    #[test]
    fn split_layout_places_host_left_and_target_right() {
        let work = Bounds {
            x: 0.0,
            y: 25.0,
            width: 1512.0,
            height: 920.0,
        };
        let (host, target) = split_layout(work, SPLIT_HOST_WIDTH).unwrap();
        assert_eq!(
            host,
            Bounds {
                x: 0.0,
                y: 25.0,
                width: 400.0,
                height: 920.0
            }
        );
        assert_eq!(
            target,
            Bounds {
                x: 400.0,
                y: 25.0,
                width: 1112.0,
                height: 920.0
            }
        );
    }

    #[test]
    fn split_layout_respects_secondary_screen_origin() {
        let work = Bounds {
            x: -1920.0,
            y: 0.0,
            width: 1920.0,
            height: 1040.0,
        };
        let (host, target) = split_layout(work, 400.0).unwrap();
        assert_eq!(host.x, -1920.0);
        assert_eq!(target.x, -1520.0);
        assert_eq!(target.width, 1520.0);
    }

    #[test]
    fn split_layout_skips_narrow_or_invalid_work_area() {
        let narrow = Bounds {
            x: 0.0,
            y: 0.0,
            width: 799.0,
            height: 600.0,
        };
        assert!(split_layout(narrow, 400.0).is_none());
        let empty = Bounds::default();
        assert!(split_layout(empty, 400.0).is_none());
        assert!(split_layout(narrow, 0.0).is_none());
    }

    #[test]
    fn open_app_response_without_split_stays_compatible() {
        let json = r#"{"app_name":"微信","pid":1,"launched":false,"summary":"ok"}"#;
        let resp: OpenAppResponse = serde_json::from_str(json).unwrap();
        assert!(resp.split.is_none());
        let back = serde_json::to_value(&resp).unwrap();
        assert!(back.get("split").is_none());
    }

    #[test]
    fn platform_current_matches_target() {
        #[cfg(target_os = "macos")]
        assert_eq!(Platform::CURRENT, Platform::Macos);
        #[cfg(target_os = "windows")]
        assert_eq!(Platform::CURRENT, Platform::Windows);
        #[cfg(target_os = "linux")]
        assert_eq!(Platform::CURRENT, Platform::Linux);
    }

    #[test]
    fn desktop_status_roundtrip() {
        let resp = DesktopStatusResponse {
            platform: Platform::Macos,
            session: DesktopSession::Available,
            accessibility: AccessibilityCapability {
                available: true,
                reason: None,
            },
            supported_actions: vec![ActionKind::Press, ActionKind::SetValue],
        };
        let result = DesktopResult::ok(resp);
        let json = serde_json::to_string(&result).unwrap();
        let back: DesktopResult<DesktopStatusResponse> = serde_json::from_str(&json).unwrap();
        match back {
            DesktopResult::Ok(r) => {
                assert_eq!(r.platform, Platform::Macos);
                assert_eq!(r.session, DesktopSession::Available);
                assert_eq!(r.supported_actions.len(), 2);
            }
            DesktopResult::Err(_) => panic!("应为成功"),
        }
    }

    #[test]
    fn list_windows_roundtrip() {
        let resp = ListWindowsResponse {
            windows: vec![WindowInfo {
                app_name: "访达".to_string(),
                pid: 501,
                element: ElementRef {
                    id: "macos-app-501-1".to_string(),
                    snapshot: 7,
                },
                title: "访达".to_string(),
                is_foreground: true,
                bounds: Bounds::default(),
                visible: true,
                enabled: true,
                identifiers: StableIdentifiers {
                    automation_id: None,
                    role: Some("AXApplication".to_string()),
                },
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: ListWindowsResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.windows.len(), 1);
        assert_eq!(back.windows[0].app_name, "访达");
        assert_eq!(back.windows[0].element.snapshot, 7);
    }

    #[test]
    fn desktop_error_roundtrip_and_message() {
        let err = DesktopError::PermissionDenied {
            reason: "未授权".to_string(),
        };
        let json = serde_json::to_string(&err).unwrap();
        let back: DesktopError = serde_json::from_str(&json).unwrap();
        assert_eq!(err, back);
        assert!(err.agent_message().contains("未授权"));
    }

    #[test]
    fn desktop_result_err_roundtrip() {
        let result: DesktopResult<ListWindowsResponse> =
            DesktopResult::Err(DesktopError::StaleElement { snapshot: 3 });
        let json = serde_json::to_string(&result).unwrap();
        // DesktopResult<ListWindowsResponse> 的 unagged 序列化：成功项含 windows，
        // 失败项含 kind 字段。验证可正确还原为 Err。
        let back: DesktopResult<ListWindowsResponse> = serde_json::from_str(&json).unwrap();
        match back {
            DesktopResult::Err(DesktopError::StaleElement { snapshot }) => {
                assert_eq!(snapshot, 3);
            }
            other => panic!("期望 StaleElement，得到 {other:?}"),
        }
    }

    #[test]
    fn action_request_kind_default_and_from() {
        assert_eq!(ActionRequestKind::default(), ActionRequestKind::Focus);
        let kind: ActionKind = ActionRequestKind::Press.into();
        assert_eq!(kind, ActionKind::Press);
    }

    #[test]
    fn wait_condition_roundtrip() {
        let cond = WaitCondition::Appear {
            target: WaitTarget {
                app_name: Some("终端".to_string()),
                title: None,
            },
        };
        let json = serde_json::to_string(&cond).unwrap();
        let back: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(cond, back);
    }

    #[test]
    fn match_mode_default_is_exact() {
        assert_eq!(MatchMode::default(), MatchMode::Exact);
    }

    #[test]
    fn screenshot_response_roundtrips_injected_asset_protocol() {
        let response = ScreenshotResponse {
            path: "/tmp/desktop-1.png".to_string(),
            width: 1280,
            height: 800,
            app_name: "微信".to_string(),
            size_bytes: 4096,
            logical_bounds: Some(Bounds {
                x: 100.0,
                y: 50.0,
                width: 2560.0,
                height: 1600.0,
            }),
            scale: 2.0,
            injected_assets: vec![InjectedAsset {
                local_path: "/tmp/desktop-1.png".to_string(),
                mime_type: "image/png".to_string(),
                original_name: Some("desktop-1.png".to_string()),
                size_bytes: 4096,
                kind: tiangong_types::MediaKind::Image,
                source: Some("desktop_screenshot".to_string()),
            }],
        };
        let json = serde_json::to_string(&response).unwrap();
        let decoded: ScreenshotResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.path, response.path);
        assert_eq!(decoded.width, 1280);
        assert_eq!(decoded.height, 800);
        assert_eq!(decoded.injected_assets.len(), 1);
        assert_eq!(
            decoded.injected_assets[0].source.as_deref(),
            Some("desktop_screenshot")
        );
        assert_eq!(
            decoded.injected_assets[0].kind,
            tiangong_types::MediaKind::Image
        );
        assert_eq!(decoded.scale, 2.0);
        assert_eq!(decoded.logical_bounds.map(|b| b.width), Some(2560.0));
        let hint = decoded
            .coordinate_hint()
            .expect("有 logical_bounds 时应给出换算说明");
        assert!(hint.contains("屏幕x = 100 + 图片x × 2"), "{hint}");
    }

    #[test]
    fn screenshot_response_legacy_json_defaults_scale_to_one() {
        let legacy = r#"{"path":"/tmp/a.png","width":10,"height":10,"size_bytes":1}"#;
        let decoded: ScreenshotResponse = serde_json::from_str(legacy).unwrap();
        assert_eq!(decoded.scale, 1.0);
        assert!(decoded.logical_bounds.is_none());
        assert!(decoded.coordinate_hint().is_none());
    }

    #[test]
    fn screenshot_downscale_factor_uses_power_of_two_halving() {
        use crate::ops::screenshot_downscale_factor as f;
        // 逻辑长边不超过上限：保持 1:1，不缩放。
        assert_eq!(f(1400.0, 900.0, 1568), 1);
        assert_eq!(f(1568.0, 800.0, 1568), 1);
        // 2560 宽主屏 → 1/2。
        assert_eq!(f(2560.0, 1440.0, 1568), 2);
        // 双屏拼接 5120 宽 → 1/4。
        assert_eq!(f(5120.0, 1440.0, 1568), 4);
        // 纵向长图同样按长边计算。
        assert_eq!(f(300.0, 3200.0, 1568), 4);
        // 退化输入不死循环。
        assert_eq!(f(0.0, 0.0, 0), 1);
    }

    #[test]
    fn open_app_request_accepts_either_identifier() {
        let by_name: crate::ops::OpenAppRequest =
            serde_json::from_value(serde_json::json!({ "app_name": "微信" })).unwrap();
        assert_eq!(by_name.app_name.as_deref(), Some("微信"));
        assert!(by_name.bundle_id.is_none());
        let by_id: crate::ops::OpenAppRequest = serde_json::from_value(
            serde_json::json!({ "bundle_id": "com.tencent.xinWeChat", "full_trust": true }),
        )
        .unwrap();
        assert_eq!(by_id.bundle_id.as_deref(), Some("com.tencent.xinWeChat"));
        assert!(by_id.access.full_trust);
    }

    #[test]
    fn operation_names_have_prefix() {
        // 所有私有操作名都应以插件前缀开头，避免多插件冲突。
        assert!(DESKTOP_STATUS_OPERATION.starts_with("computer_use."));
        assert!(DESKTOP_LIST_WINDOWS_OPERATION.starts_with("computer_use."));
        assert!(DESKTOP_SNAPSHOT_OPERATION.starts_with("computer_use."));
        assert!(DESKTOP_FIND_OPERATION.starts_with("computer_use."));
        assert!(DESKTOP_ACTION_OPERATION.starts_with("computer_use."));
        assert!(DESKTOP_WAIT_OPERATION.starts_with("computer_use."));
        assert!(DESKTOP_SCREENSHOT_OPERATION.starts_with("computer_use."));
        assert!(DESKTOP_OPEN_APP_OPERATION.starts_with("computer_use."));
    }

    #[test]
    fn all_tool_constants_match_protocol() {
        let tools = [
            TOOL_DESKTOP_APP,
            TOOL_DESKTOP_SCREENSHOT,
            TOOL_DESKTOP_INPUT,
            TOOL_DESKTOP_UI,
            TOOL_DESKTOP_WAIT,
        ];
        assert_eq!(
            tools,
            [
                "desktop_app",
                "desktop_screenshot",
                "desktop_input",
                "desktop_ui",
                "desktop_wait"
            ]
        );
    }
}
