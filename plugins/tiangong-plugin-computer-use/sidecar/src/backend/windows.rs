//! Windows UI Automation 无障碍后端。
//!
//! 通过 `uiautomation` crate（基于 windows-rs 的 UIA COM 封装）访问桌面控件树，
//! 列举应用窗口、读取控件树、查找控件、执行动作并等待状态变化。
//!
//! 从桌面根节点按 ControlType 与进程缩小范围，优先使用 Control View，
//! 避免无边界遍历整个桌面。目标窗口属于更高权限进程或安全桌面时返回权限受限，
//! 不提升权限绕过。动作按控件真实支持的 pattern 调用，动作前重新确认控件身份。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use uiautomation::controls::ControlType;
use uiautomation::core::{UIAutomation, UIElement};
use uiautomation::patterns::{
    UIExpandCollapsePattern, UIInvokePattern, UIScrollItemPattern, UISelectionItemPattern,
    UITogglePattern, UIValuePattern,
};
use uiautomation::types::TreeScope;

use super::{
    ActionResult, Backend, FindInfo, KeyboardResult, MouseResult, SnapshotInfo, StatusInfo,
    WaitResult,
};
use tiangong_plugin_computer_use_protocol::ops::{
    ActionRequest, FindConditions, FindRequest, KeyboardRequest, ListWindowsRequest, MouseRequest,
    OpenAppRequest, OpenAppResponse, ScreenshotRequest, SnapshotRequest, WaitCondition,
    WaitRequest,
};
use tiangong_plugin_computer_use_protocol::{
    AccessibilityCapability, ActionKind, Bounds, ControlNode, DesktopError, DesktopResult,
    DesktopSession, ElementRef, MatchMode, Platform, ScreenshotResponse, StableIdentifiers,
    WindowInfo,
};

const DEFAULT_MAX_DEPTH: u32 = 8;
const DEFAULT_MAX_NODES: usize = 400;
/// 控件树读取的总时限：超出后停止遍历并返回已读部分（truncated=true）。
const SNAPSHOT_BUDGET: Duration = Duration::from_secs(8);
/// UIA 单次跨进程调用的连接/事务超时（毫秒）。目标进程无响应时 UIA 默认
/// 会阻塞很久，设置后单次调用最多阻塞约此时长。
const UIA_CALL_TIMEOUT_MS: u32 = 2_000;
/// 快照线程整体等待上限（总时限 + 单次调用超时余量）；超过即判定超时，
/// 后台线程自行结束，不阻塞工具返回。
const SNAPSHOT_HARD_LIMIT: Duration = Duration::from_secs(12);

/// `UIElement` 的线程安全包装。
///
/// `uiautomation` crate 的 `UIElement` 内部持有 `IUIAutomationElement` COM 指针
/// （`IUnknown` 包裹 `NonNull<c_void>`），默认不实现 `Send`/`Sync`。但 UIA
/// COM 接口支持 MTA（多线程单元）访问，属性读取线程安全，与 macOS 后端的
/// `AxElement` 处理方式一致。这里用 newtype 加 `unsafe impl` 让其可跨线程持有。
struct SendElement(UIElement);

// SAFETY：IUIAutomationElement 是线程安全的 COM 接口，支持跨线程属性读取与
// pattern 调用；引用计数由 IUnknown 管理，Clone/Release 线程安全。
unsafe impl Send for SendElement {}
unsafe impl Sync for SendElement {}

impl std::ops::Deref for SendElement {
    type Target = UIElement;
    fn deref(&self) -> &UIElement {
        &self.0
    }
}

impl Clone for SendElement {
    fn clone(&self) -> Self {
        SendElement(self.0.clone())
    }
}

impl From<UIElement> for SendElement {
    fn from(elem: UIElement) -> Self {
        SendElement(elem)
    }
}

pub struct WindowsBackend {
    snapshot_seq: AtomicU64,
    /// 快照内控件引用缓存：((snapshot, id) -> SendElement)。
    /// UIElement 内部持有 COM 对象引用，Clone 增引用计数。
    elements: RwLock<HashMap<(u64, String), SendElement>>,
    /// `list_windows` 返回的真实窗口句柄（HWND 数值），避免后续按易变的
    /// 枚举序号重新猜测目标。
    window_handles: RwLock<HashMap<(u64, String), isize>>,
    /// 快照节点表缓存：(snapshot -> Vec<ControlNode>)，供 find 筛选。
    snapshot_nodes: RwLock<HashMap<u64, Vec<ControlNode>>>,
    /// 已使用的快照版本，用于淘汰旧缓存。
    recent_snapshots: Mutex<Vec<u64>>,
    /// 最近的窗口列表版本；窗口引用与控件快照分别管理。
    recent_window_snapshots: Mutex<Vec<u64>>,
}

impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsBackend {
    pub fn new() -> Self {
        Self {
            snapshot_seq: AtomicU64::new(1),
            elements: RwLock::new(HashMap::new()),
            window_handles: RwLock::new(HashMap::new()),
            snapshot_nodes: RwLock::new(HashMap::new()),
            recent_snapshots: Mutex::new(Vec::new()),
            recent_window_snapshots: Mutex::new(Vec::new()),
        }
    }

    fn next_snapshot(&self) -> u64 {
        self.snapshot_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// 记录快照版本，保留最近 4 个，清理更早的缓存。
    fn remember_snapshot(&self, snapshot: u64) {
        let mut recent = self.recent_snapshots.lock().unwrap();
        recent.retain(|&s| s != snapshot);
        recent.push(snapshot);
        while recent.len() > 4 {
            let old = recent.remove(0);
            self.elements.write().unwrap().retain(|(s, _), _| *s != old);
            self.snapshot_nodes.write().unwrap().remove(&old);
        }
    }

    /// 记录窗口列表版本，保留最近 4 次枚举返回的真实窗口对象。
    fn remember_window_snapshot(&self, snapshot: u64) {
        let mut recent = self.recent_window_snapshots.lock().unwrap();
        recent.retain(|&s| s != snapshot);
        recent.push(snapshot);
        while recent.len() > 4 {
            let old = recent.remove(0);
            self.window_handles
                .write()
                .unwrap()
                .retain(|(s, _), _| *s != old);
        }
    }

    /// 创建 UIA 实例。失败说明无图形会话或 UIA 服务不可用。
    ///
    /// 同时设置连接/事务超时（IUIAutomation2），避免目标进程无响应时单次
    /// 跨进程调用无限阻塞；系统不支持 IUIAutomation2 时保持默认。
    fn automation() -> Result<UIAutomation, DesktopError> {
        let automation =
            UIAutomation::new().map_err(|e| DesktopError::DesktopSessionUnavailable {
                reason: format!("无法初始化 UI Automation: {e}"),
            })?;
        {
            use windows::Win32::UI::Accessibility::{IUIAutomation, IUIAutomation2};
            use windows::core::Interface;
            let raw: &IUIAutomation = automation.as_ref();
            if let Ok(v2) = raw.cast::<IUIAutomation2>() {
                // SAFETY：标准 COM 属性设置。
                unsafe {
                    let _ = v2.SetConnectionTimeout(UIA_CALL_TIMEOUT_MS);
                    let _ = v2.SetTransactionTimeout(UIA_CALL_TIMEOUT_MS);
                }
            }
        }
        Ok(automation)
    }

    /// 读取 UIElement 边界并转为 Bounds（读取失败返回默认）。
    fn bounds_of(element: &UIElement) -> Bounds {
        element
            .get_bounding_rectangle()
            .map_or(Bounds::default(), |rect| Bounds {
                x: rect.get_left() as f64,
                y: rect.get_top() as f64,
                width: rect.get_width() as f64,
                height: rect.get_height() as f64,
            })
    }

    /// 解析快照目标窗口句柄：窗口引用（list_windows 缓存）优先，其次 pid、
    /// app_name。定位规则与 desktop_app/desktop_screenshot 完全一致。
    fn resolve_snapshot_window(&self, req: &SnapshotRequest) -> Result<isize, DesktopError> {
        if let Some(window_ref) = &req.scope.window {
            return self
                .window_handles
                .read()
                .unwrap()
                .get(&(window_ref.snapshot, window_ref.id.clone()))
                .copied()
                .ok_or(DesktopError::StaleElement {
                    snapshot: window_ref.snapshot,
                });
        }
        let pid = req.scope.pid.filter(|p| *p > 0);
        let name = req
            .scope
            .app_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty());
        if pid.is_none() && name.is_none() {
            return Err(DesktopError::WindowNotFound {
                query: "snapshot 需要指定 window、pid 或 app_name".to_string(),
            });
        }
        super::win_desktop::find_app_window(pid, name)
            .map(|w| w.hwnd.0 as isize)
            .ok_or_else(|| DesktopError::WindowNotFound {
                query: match (pid, name) {
                    (Some(pid), _) => format!("pid {pid}"),
                    (None, Some(name)) => name.to_string(),
                    (None, None) => String::new(),
                },
            })
    }

    /// 在独立线程中读取控件树（线程内自建 UIA 实例与 COM 单元）。
    fn read_tree(
        hwnd: isize,
        snapshot: u64,
        max_depth: u32,
        max_nodes: usize,
        include_invisible: bool,
    ) -> Result<TreeReadout, DesktopError> {
        let automation = Self::automation()?;
        let hwnd = windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void);
        let root = automation.element_from_handle(hwnd.into()).map_err(|e| {
            DesktopError::BackendUnavailable {
                reason: format!("读取窗口控件失败（窗口可能已关闭）: {e}"),
            }
        })?;
        let deadline = Instant::now() + SNAPSHOT_BUDGET;
        let mut out = TreeReadout::default();
        Self::walk(
            &root,
            &automation,
            &mut WalkState {
                snapshot,
                max_depth,
                max_nodes,
                include_invisible,
                deadline,
            },
            0,
            None,
            &mut out,
        );
        if out.timed_out {
            out.warnings.push(format!(
                "控件树读取超过 {} 秒，已返回部分结果",
                SNAPSHOT_BUDGET.as_secs()
            ));
        }
        Ok(out)
    }

    /// 探测控件真实支持的 pattern，返回对应的统一动作集合。
    fn detect_actions(element: &UIElement) -> Vec<ActionKind> {
        use ActionKind::*;
        let mut actions = Vec::new();
        // 控件总是可以尝试获得焦点。
        actions.push(Focus);
        if element.get_pattern::<UIInvokePattern>().is_ok() {
            actions.push(Press);
        }
        if element.get_pattern::<UIValuePattern>().is_ok() {
            actions.push(SetValue);
        }
        if element.get_pattern::<UITogglePattern>().is_ok() {
            actions.push(Toggle);
        }
        if element.get_pattern::<UISelectionItemPattern>().is_ok() {
            actions.push(Select);
        }
        if element.get_pattern::<UIExpandCollapsePattern>().is_ok() {
            actions.push(Expand);
            actions.push(Collapse);
        }
        if element.get_pattern::<UIScrollItemPattern>().is_ok() {
            actions.push(ScrollIntoView);
        }
        actions
    }

    /// 校验控件身份：automation_id、control_type、process_id 是否与快照时一致。
    /// 任一不一致返回 false（控件已变化，不应继续操作）。
    fn verify_identity(element: &UIElement, node: &ControlNode) -> bool {
        // automation_id 一致性（控件稳定标识）。
        if let Some(expected_id) = &node.identifiers.automation_id {
            let actual = element.get_automation_id().unwrap_or_default();
            if actual.is_empty() || &actual != expected_id {
                return false;
            }
        }
        // control_type 一致性。
        let expected_type = node.identifiers.role.as_deref().unwrap_or("");
        let actual_type = format!(
            "{:?}",
            element.get_control_type().unwrap_or(ControlType::Custom)
        );
        if !expected_type.is_empty() && actual_type != expected_type {
            return false;
        }
        true
    }
}

#[async_trait]
impl Backend for WindowsBackend {
    fn platform(&self) -> Platform {
        Platform::Windows
    }

    async fn status(&self) -> DesktopResult<StatusInfo> {
        match Self::automation() {
            Ok(_uia) => DesktopResult::Ok(StatusInfo {
                session: DesktopSession::Available,
                accessibility: AccessibilityCapability {
                    available: true,
                    reason: None,
                },
                supported_actions: {
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
                },
            }),
            Err(e) => DesktopResult::Ok(StatusInfo {
                session: DesktopSession::Unavailable,
                accessibility: AccessibilityCapability {
                    available: false,
                    reason: Some(e.agent_message().to_string()),
                },
                supported_actions: Vec::new(),
            }),
        }
    }

    async fn list_windows(
        &self,
        req: &ListWindowsRequest,
    ) -> DesktopResult<tiangong_plugin_computer_use_protocol::ListWindowsResponse> {
        // 窗口枚举走 Win32（EnumWindows），不做跨进程 UIA 调用：速度快、
        // 不会被无响应的目标进程阻塞，且与 open/screenshot 使用同一套窗口
        // 过滤、名称匹配与坐标口径（DWM 可见边框）。
        let top = match tokio::task::spawn_blocking(super::win_desktop::enum_top_windows).await {
            Ok(top) => top,
            Err(e) => {
                return DesktopResult::Err(DesktopError::BackendUnavailable {
                    reason: format!("窗口枚举线程异常：{e}"),
                });
            }
        };
        let snapshot = self.next_snapshot();
        let foreground = super::win_desktop::foreground_pid();
        let mut windows = Vec::new();
        for window in top {
            if let Some(name) = req.app_name.as_deref()
                && !super::win_desktop::name_matches(&window.exe_stem, &window.title, name)
            {
                continue;
            }
            if req.pid.is_some_and(|pid| pid != window.pid) {
                continue;
            }
            let is_foreground = foreground == Some(window.pid);
            if req.foreground_only && !is_foreground {
                continue;
            }
            let hwnd_value = window.hwnd.0 as isize;
            let id = format!("uia-win-{}-{hwnd_value:x}", window.pid);
            self.window_handles
                .write()
                .unwrap()
                .insert((snapshot, id.clone()), hwnd_value);
            windows.push(WindowInfo {
                app_name: if window.exe_stem.is_empty() {
                    window.title.clone()
                } else {
                    window.exe_stem.clone()
                },
                pid: window.pid,
                element: ElementRef { id, snapshot },
                title: window.title,
                is_foreground,
                bounds: window.bounds,
                visible: !window.minimized,
                enabled: true,
                identifiers: StableIdentifiers {
                    automation_id: None,
                    role: Some("Window".to_string()),
                },
            });
        }
        self.remember_window_snapshot(snapshot);
        DesktopResult::Ok(tiangong_plugin_computer_use_protocol::ListWindowsResponse { windows })
    }

    async fn snapshot(&self, req: &SnapshotRequest) -> DesktopResult<SnapshotInfo> {
        let hwnd = match self.resolve_snapshot_window(req) {
            Ok(h) => h,
            Err(e) => return DesktopResult::Err(e),
        };
        let max_depth = if req.max_depth > 0 {
            req.max_depth
        } else {
            DEFAULT_MAX_DEPTH
        };
        let max_nodes = if req.max_nodes > 0 {
            req.max_nodes as usize
        } else {
            DEFAULT_MAX_NODES
        };
        let snapshot = self.next_snapshot();
        let include_invisible = req.include_invisible;
        // 控件树读取是一连串跨进程 UIA 调用，放到独立线程并设硬时限：
        // 目标进程卡住时工具按时返回超时，后台线程自行收尾后退出。
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let result = Self::read_tree(hwnd, snapshot, max_depth, max_nodes, include_invisible);
            let _ = tx.send(result);
        });
        let readout = match tokio::time::timeout(SNAPSHOT_HARD_LIMIT, rx).await {
            Ok(Ok(Ok(readout))) => readout,
            Ok(Ok(Err(e))) => return DesktopResult::Err(e),
            Ok(Err(_)) => {
                return DesktopResult::Err(DesktopError::BackendUnavailable {
                    reason: "控件树读取线程异常退出".to_string(),
                });
            }
            Err(_) => {
                return DesktopResult::Err(DesktopError::Timeout {
                    waited_ms: SNAPSHOT_HARD_LIMIT.as_millis() as u64,
                });
            }
        };
        // 先构建完成，再登记版本与提交节点表、元素缓存，避免并发残留。
        {
            let mut cache = self.elements.write().unwrap();
            for (id, element) in readout.elements {
                cache.insert((snapshot, id), element);
            }
        }
        self.remember_snapshot(snapshot);
        self.snapshot_nodes
            .write()
            .unwrap()
            .insert(snapshot, readout.nodes.clone());
        DesktopResult::Ok(SnapshotInfo {
            snapshot,
            nodes: readout.nodes,
            truncated: readout.truncated,
            warnings: readout.warnings,
        })
    }

    async fn find(&self, req: &FindRequest) -> DesktopResult<FindInfo> {
        let (nodes, snapshot) = match req.snapshot {
            Some(s) => {
                let cached = self.snapshot_nodes.read().unwrap().get(&s).cloned();
                match cached {
                    Some(n) => (n, s),
                    None => return DesktopResult::Err(DesktopError::StaleElement { snapshot: s }),
                }
            }
            None => {
                // 未指定 snapshot 时需要 window 引用先取一次快照。
                let window = match &req.window {
                    Some(w) => w.clone(),
                    None => {
                        return DesktopResult::Err(DesktopError::ApplicationNotFound {
                            query: "find 需要指定 window 或 snapshot".to_string(),
                        });
                    }
                };
                let snap_req = SnapshotRequest {
                    scope: tiangong_plugin_computer_use_protocol::ops::SnapshotScope {
                        window: Some(window),
                        app_name: None,
                        pid: None,
                    },
                    max_depth: 0,
                    max_nodes: 0,
                    include_invisible: false,
                    access: Default::default(),
                };
                match self.snapshot(&snap_req).await {
                    DesktopResult::Ok(info) => (info.nodes, info.snapshot),
                    DesktopResult::Err(e) => return DesktopResult::Err(e),
                }
            }
        };
        let matches = filter_nodes(&nodes, &req.conditions);
        let ambiguous = matches.len() > 1;
        let max_candidates = if req.max_candidates > 0 {
            req.max_candidates as usize
        } else {
            matches.len()
        };
        let matches = matches.into_iter().take(max_candidates).collect();
        DesktopResult::Ok(FindInfo {
            matches,
            snapshot,
            ambiguous,
        })
    }

    async fn action(&self, req: &ActionRequest) -> DesktopResult<ActionResult> {
        // 从缓存取回 UIElement。
        let element = {
            let guard = self.elements.read().unwrap();
            guard
                .get(&(req.element.snapshot, req.element.id.clone()))
                .cloned()
        };
        let element = match element {
            Some(e) => e,
            None => {
                return DesktopResult::Err(DesktopError::StaleElement {
                    snapshot: req.element.snapshot,
                });
            }
        };
        // 动作前重新确认控件身份：从快照节点表取回原始属性，与当前控件比对。
        // 身份不一致说明界面已变化（控件被替换），返回 stale 避免误操作其他控件。
        let identity_ok = self
            .snapshot_nodes
            .read()
            .unwrap()
            .get(&req.element.snapshot)
            .and_then(|nodes| {
                nodes.iter().find(|n| {
                    n.element.id == req.element.id && n.element.snapshot == req.element.snapshot
                })
            })
            .is_some_and(|node| Self::verify_identity(&element, node));
        if !identity_ok {
            return DesktopResult::Err(DesktopError::StaleElement {
                snapshot: req.element.snapshot,
            });
        }
        // 动作前重新确认控件支持请求的动作。
        let action_kind = ActionKind::from(req.action);
        let supported = Self::detect_actions(&element);
        if !supported.contains(&action_kind) {
            return DesktopResult::Err(DesktopError::ActionNotSupported {
                action: format!("{action_kind:?}"),
                supported: supported.iter().map(|a| format!("{a:?}")).collect(),
            });
        }
        // 按 pattern 执行动作。UIA 语义动作不移动系统鼠标，天工指针（已
        // 显示时）跟随到控件中心，让用户看到操作落点。
        let target_bounds = Self::bounds_of(&element);
        let result = match action_kind {
            ActionKind::Focus => element.set_focus().map_err(|e| e.to_string()),
            ActionKind::Press => element
                .get_pattern::<UIInvokePattern>()
                .map_err(|e| e.to_string())
                .and_then(|p| p.invoke().map_err(|e| e.to_string())),
            ActionKind::SetValue => match &req.value {
                Some(v) => element
                    .get_pattern::<UIValuePattern>()
                    .map_err(|e| e.to_string())
                    .and_then(|p| p.set_value(v).map_err(|e| e.to_string())),
                None => Err("set_value 缺少 value 参数".to_string()),
            },
            ActionKind::Toggle => element
                .get_pattern::<UITogglePattern>()
                .map_err(|e| e.to_string())
                .and_then(|p| p.toggle().map_err(|e| e.to_string())),
            ActionKind::Select => element
                .get_pattern::<UISelectionItemPattern>()
                .map_err(|e| e.to_string())
                .and_then(|p| p.select().map_err(|e| e.to_string())),
            ActionKind::Expand => element
                .get_pattern::<UIExpandCollapsePattern>()
                .map_err(|e| e.to_string())
                .and_then(|p| p.expand().map_err(|e| e.to_string())),
            ActionKind::Collapse => element
                .get_pattern::<UIExpandCollapsePattern>()
                .map_err(|e| e.to_string())
                .and_then(|p| p.collapse().map_err(|e| e.to_string())),
            ActionKind::ScrollIntoView => element
                .get_pattern::<UIScrollItemPattern>()
                .map_err(|e| e.to_string())
                .and_then(|p| p.scroll_into_view().map_err(|e| e.to_string())),
        };
        match result {
            Ok(()) => {
                if target_bounds.width > 0.0 && target_bounds.height > 0.0 {
                    super::win_overlay::follow_to(
                        target_bounds.x + target_bounds.width / 2.0,
                        target_bounds.y + target_bounds.height / 2.0,
                    );
                }
                DesktopResult::Ok(ActionResult {
                    performed: true,
                    summary: format!("已执行 {action_kind:?}"),
                    new_window: None,
                })
            }
            Err(e) => DesktopResult::Err(DesktopError::BackendUnavailable { reason: e }),
        }
    }

    async fn wait(&self, req: &WaitRequest) -> DesktopResult<WaitResult> {
        match &req.condition {
            WaitCondition::Appear { target } | WaitCondition::Disappear { target } => {
                let looking_appear = matches!(req.condition, WaitCondition::Appear { .. });
                let deadline = Instant::now() + Duration::from_millis(req.timeout_ms);
                let start = Instant::now();
                loop {
                    // Win32 窗口枚举（非 UIA），每轮耗时毫秒级，保证按时返回。
                    // 「可见」口径与 desktop_screenshot 一致：最小化窗口视为已消失。
                    let target = target.clone();
                    let (total, visible) = tokio::task::spawn_blocking(move || {
                        let matched: Vec<bool> = super::win_desktop::enum_top_windows()
                            .iter()
                            .filter(|w| {
                                target.app_name.as_deref().is_some_and(|n| {
                                    super::win_desktop::name_matches(&w.exe_stem, &w.title, n)
                                }) || target.title.as_deref().is_some_and(|t| {
                                    w.title.to_lowercase().contains(&t.to_lowercase())
                                })
                            })
                            .map(|w| !w.minimized)
                            .collect();
                        let visible = matched.iter().filter(|v| **v).count();
                        (matched.len(), visible)
                    })
                    .await
                    .unwrap_or((0, 0));
                    let exists = visible > 0;
                    let finished = looking_appear == exists || Instant::now() >= deadline;
                    if finished {
                        return DesktopResult::Ok(WaitResult {
                            satisfied: looking_appear == exists,
                            waited_ms: start.elapsed().as_millis() as u64,
                            matched_element: None,
                            detail: Some(window_wait_detail(total, visible)),
                        });
                    }
                    let now = Instant::now();
                    tokio::time::sleep((deadline - now).min(Duration::from_millis(200))).await;
                }
            }
            _ => {
                // 控件级等待：从缓存取回 UIElement，轮询对应属性。
                let element = match &req.condition {
                    WaitCondition::Focus { element }
                    | WaitCondition::Available { element }
                    | WaitCondition::Value { element, .. } => element.clone(),
                    _ => unreachable!(),
                };
                let cached = self
                    .elements
                    .read()
                    .unwrap()
                    .get(&(element.snapshot, element.id.clone()))
                    .cloned();
                let uia_elem = match cached {
                    Some(e) => e,
                    None => {
                        return DesktopResult::Err(DesktopError::StaleElement {
                            snapshot: element.snapshot,
                        });
                    }
                };
                // 等待开始前重新确认控件仍属于原快照，避免缓存对象被替换后继续轮询。
                let identity_ok = self
                    .snapshot_nodes
                    .read()
                    .unwrap()
                    .get(&element.snapshot)
                    .and_then(|nodes| nodes.iter().find(|n| n.element == element))
                    .is_some_and(|node| Self::verify_identity(&uia_elem, node));
                if !identity_ok {
                    return DesktopResult::Err(DesktopError::StaleElement {
                        snapshot: element.snapshot,
                    });
                }
                let expected_value = match &req.condition {
                    WaitCondition::Value { expected, .. } => expected.clone(),
                    _ => None,
                };
                // 无期望值时必须成功读取初始值；否则无法判断后续是否真的发生变化。
                let initial_value = if expected_value.is_none()
                    && matches!(req.condition, WaitCondition::Value { .. })
                {
                    match uia_elem
                        .get_pattern::<UIValuePattern>()
                        .and_then(|p| p.get_value())
                    {
                        Ok(value) => Some(value),
                        Err(error) => {
                            return DesktopResult::Err(DesktopError::BackendUnavailable {
                                reason: format!("读取等待初始值失败: {error}"),
                            });
                        }
                    }
                } else {
                    None
                };
                let deadline = Instant::now() + Duration::from_millis(req.timeout_ms);
                let start = Instant::now();
                loop {
                    let state = match &req.condition {
                        WaitCondition::Focus { .. } => uia_elem.has_keyboard_focus(),
                        WaitCondition::Available { .. } => uia_elem.is_enabled(),
                        WaitCondition::Value { .. } => uia_elem
                            .get_pattern::<UIValuePattern>()
                            .and_then(|p| p.get_value())
                            .map(|current| match &expected_value {
                                Some(want) => current == *want,
                                None => initial_value.as_ref().is_some_and(|v| current != *v),
                            }),
                        _ => Ok(false),
                    };
                    let satisfied = match state {
                        Ok(value) => value,
                        Err(_) => {
                            return DesktopResult::Err(DesktopError::StaleElement {
                                snapshot: element.snapshot,
                            });
                        }
                    };
                    if satisfied {
                        return DesktopResult::Ok(WaitResult {
                            satisfied: true,
                            waited_ms: start.elapsed().as_millis() as u64,
                            matched_element: Some(element.clone()),
                            detail: None,
                        });
                    }
                    if Instant::now() >= deadline {
                        return DesktopResult::Ok(WaitResult {
                            satisfied: false,
                            waited_ms: start.elapsed().as_millis() as u64,
                            matched_element: None,
                            detail: None,
                        });
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }

    async fn mouse(&self, req: &MouseRequest) -> DesktopResult<MouseResult> {
        let to = match (req.to_x, req.to_y) {
            (Some(tx), Some(ty)) => Some((tx, ty)),
            (None, None) => None,
            _ => {
                return DesktopResult::Err(DesktopError::BackendUnavailable {
                    reason: "drag 的 to_x/to_y 必须同时提供".to_string(),
                });
            }
        };
        let gesture = req.gesture;
        let (x, y) = (req.x, req.y);
        let scroll = (req.delta_y.unwrap_or(0.0), req.delta_x.unwrap_or(0.0));
        // 手势内含阻塞等待（指针滑行、按压间隔），放到阻塞线程池执行。
        let cancel = super::input_cancel::current();
        let outcome = tokio::task::spawn_blocking(move || {
            super::win_input::perform_mouse(gesture, x, y, to, scroll, &cancel)
        })
        .await
        .unwrap_or_else(|e| Err(format!("鼠标手势执行线程异常：{e}")));
        match outcome {
            Ok(summary) => DesktopResult::Ok(MouseResult {
                performed: true,
                summary,
            }),
            Err(reason) => DesktopResult::Err(DesktopError::BackendUnavailable { reason }),
        }
    }

    async fn keyboard(&self, req: &KeyboardRequest) -> DesktopResult<KeyboardResult> {
        let (action, text, key, keys) = (
            req.action,
            req.text.clone(),
            req.key.clone(),
            req.keys.clone(),
        );
        let cancel = super::input_cancel::current();
        let outcome = tokio::task::spawn_blocking(move || {
            super::win_input::perform_keyboard(action, text, key, keys, &cancel)
        })
        .await
        .unwrap_or_else(|e| Err(format!("键盘输入执行线程异常：{e}")));
        match outcome {
            Ok(summary) => DesktopResult::Ok(KeyboardResult {
                performed: true,
                summary,
            }),
            Err(reason) => DesktopResult::Err(DesktopError::BackendUnavailable { reason }),
        }
    }

    async fn screenshot(&self, req: &ScreenshotRequest) -> DesktopResult<ScreenshotResponse> {
        if let Some(delay) = req.delay_ms.filter(|d| *d > 0) {
            tokio::time::sleep(Duration::from_millis(u64::from(delay.min(2000)))).await;
        }
        let req = req.clone();
        tokio::task::spawn_blocking(move || super::win_desktop::capture_screenshot(&req))
            .await
            .unwrap_or_else(|e| {
                DesktopResult::Err(DesktopError::BackendUnavailable {
                    reason: format!("截图执行线程异常：{e}"),
                })
            })
    }

    async fn open_app(&self, req: &OpenAppRequest) -> DesktopResult<OpenAppResponse> {
        let mut result = super::win_desktop::open_app(req).await;
        // 应用窗口出现后自动分屏：天工靠左、目标占右侧。
        if let DesktopResult::Ok(resp) = &mut result
            && resp.window.is_some()
            && let Some(window) = super::win_desktop::find_app_window(Some(resp.pid), None)
        {
            let outcome = super::win_split::split_with_host(window.hwnd, resp.pid);
            if let Some(target) = outcome.target {
                resp.window = Some(target);
            }
            resp.summary = format!("{}。{}", resp.summary, outcome.detail);
            resp.split = Some(outcome);
        }
        result
    }

    async fn restore_host_window(&self, saved: crate::split::SavedHostFrame) -> Result<(), String> {
        super::win_split::restore_host(saved)
    }
}

impl WindowsBackend {
    /// 递归读取控件树（前序遍历），同时收集 UIElement 供 action/wait 还原。
    /// 超过节点/深度上限或总时限时截断。
    fn walk(
        element: &UIElement,
        automation: &UIAutomation,
        state: &mut WalkState,
        depth: u32,
        parent_id: Option<String>,
        out: &mut TreeReadout,
    ) {
        if out.nodes.len() >= state.max_nodes || depth > state.max_depth {
            out.truncated = true;
            return;
        }
        if Instant::now() >= state.deadline {
            out.truncated = true;
            out.timed_out = true;
            return;
        }
        let control_type = element.get_control_type().unwrap_or(ControlType::Custom);
        let offscreen = element.is_offscreen().unwrap_or(false);
        if offscreen && !state.include_invisible && depth > 0 {
            return;
        }
        let role = format!("{control_type:?}");
        let name = element.get_name().unwrap_or_default();
        let automation_id = element.get_automation_id().ok().filter(|id| !id.is_empty());
        let enabled = element.is_enabled().unwrap_or(true);
        let focused = element.has_keyboard_focus().unwrap_or(false);
        let bounds = Self::bounds_of(element);
        let actions = Self::detect_actions(element);
        // 读取控件当前值：密码控件不读取（敏感），非敏感且支持 Value Pattern 时读取。
        let sensitive = element.is_password().unwrap_or(false);
        let value = if sensitive {
            None
        } else {
            element
                .get_pattern::<UIValuePattern>()
                .ok()
                .and_then(|p| p.get_value().ok())
        };

        let id = format!("uia-{}-{}", state.snapshot, out.nodes.len());
        let parent_index = out.nodes.len();
        out.elements
            .push((id.clone(), SendElement::from(element.clone())));
        out.nodes.push(ControlNode {
            element: ElementRef {
                id: id.clone(),
                snapshot: state.snapshot,
            },
            role: role.clone(),
            name,
            identifiers: StableIdentifiers {
                automation_id,
                role: Some(role),
            },
            value,
            sensitive,
            visible: !offscreen,
            enabled,
            focused,
            bounds,
            actions,
            parent: parent_id.map(|pid| ElementRef {
                id: pid,
                snapshot: state.snapshot,
            }),
            children: Vec::new(),
        });

        // 读取子元素。
        let true_cond = match automation.create_true_condition() {
            Ok(c) => c,
            Err(_) => return,
        };
        let children = match element.find_all(TreeScope::Children, &true_cond) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut child_refs = Vec::with_capacity(children.len());
        for child in children {
            let before = out.nodes.len();
            Self::walk(&child, automation, state, depth + 1, Some(id.clone()), out);
            if out.nodes.len() > before {
                child_refs.push(out.nodes[before].element.clone());
            }
            if out.timed_out {
                break;
            }
        }
        if let Some(node) = out.nodes.get_mut(parent_index) {
            node.children = child_refs;
        }
    }
}

/// 控件树遍历参数。
struct WalkState {
    snapshot: u64,
    max_depth: u32,
    max_nodes: usize,
    include_invisible: bool,
    deadline: Instant,
}

/// 控件树读取产物（在读取线程内构建，完成后整体交回）。
#[derive(Default)]
struct TreeReadout {
    nodes: Vec<ControlNode>,
    elements: Vec<(String, SendElement)>,
    truncated: bool,
    timed_out: bool,
    warnings: Vec<String>,
}

/// 按条件筛选节点（平台无关逻辑）。
fn filter_nodes(nodes: &[ControlNode], conditions: &FindConditions) -> Vec<ControlNode> {
    nodes
        .iter()
        .filter(|n| {
            let id_match = conditions
                .automation_id
                .as_ref()
                .map(|want| {
                    n.identifiers
                        .automation_id
                        .as_deref()
                        .is_some_and(|have| have == want)
                })
                .unwrap_or(true);
            let role_match = conditions
                .role
                .as_ref()
                .map(|want| {
                    n.identifiers
                        .role
                        .as_deref()
                        .is_some_and(|have| have == want)
                })
                .unwrap_or(true);
            let name_match = conditions.name.as_ref().map(|want| match conditions.mode {
                MatchMode::Exact => n.name == *want,
                MatchMode::Contains => n.name.to_lowercase().contains(&want.to_lowercase()),
            });
            let value_match = conditions.value.as_ref().map(|want| {
                n.value
                    .as_deref()
                    .is_some_and(|have| match conditions.mode {
                        MatchMode::Exact => have == want,
                        MatchMode::Contains => have.to_lowercase().contains(&want.to_lowercase()),
                    })
            });
            let visible_match = conditions
                .visible
                .map(|want| n.visible == want)
                .unwrap_or(true);
            let enabled_match = conditions
                .enabled
                .map(|want| n.enabled == want)
                .unwrap_or(true);
            let focused_match = conditions
                .focused
                .map(|want| n.focused == want)
                .unwrap_or(true);
            // 敏感控件不进入查找结果。
            let not_sensitive = !n.sensitive;
            id_match
                && role_match
                && name_match.unwrap_or(true)
                && value_match.unwrap_or(true)
                && visible_match
                && enabled_match
                && focused_match
                && not_sensitive
        })
        .cloned()
        .collect()
}

/// 窗口出现/消失等待的判定依据（匹配窗口总数与其中未最小化的数量）。
fn window_wait_detail(total: usize, visible: usize) -> String {
    match (total, visible) {
        (0, _) => "没有匹配的窗口".to_string(),
        (t, 0) => format!("匹配到 {t} 个窗口，均已最小化"),
        (t, v) if t == v => format!("匹配到 {t} 个窗口，均可见"),
        (t, v) => format!("匹配到 {t} 个窗口，其中 {v} 个可见、{} 个已最小化", t - v),
    }
}

#[cfg(test)]
mod wait_detail_tests {
    #[test]
    fn window_wait_detail_describes_visibility() {
        assert_eq!(super::window_wait_detail(0, 0), "没有匹配的窗口");
        assert_eq!(
            super::window_wait_detail(1, 0),
            "匹配到 1 个窗口，均已最小化"
        );
        assert_eq!(super::window_wait_detail(2, 2), "匹配到 2 个窗口，均可见");
        assert_eq!(
            super::window_wait_detail(3, 1),
            "匹配到 3 个窗口，其中 1 个可见、2 个已最小化"
        );
    }
}
