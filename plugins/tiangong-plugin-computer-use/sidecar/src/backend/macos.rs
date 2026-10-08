//! macOS 无障碍后端。
//!
//! 通过 `objc2-app-kit` 的 `NSWorkspace` 列举运行中的应用，作为窗口发现的基础；
//! 通过 ApplicationServices 的 AXUIElement C API 读取控件树、查找控件并执行动作。
//! 辅助功能授权经 `AXIsProcessTrusted` 探测。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use objc2_app_kit::NSWorkspace;

use super::ax::{self, AxElement};
use super::{ActionResult, Backend, FindInfo, SnapshotInfo, StatusInfo, WaitResult};
use tiangong_plugin_computer_use_protocol::ops::DEFAULT_SCREENSHOT_MAX_DIMENSION;
use tiangong_plugin_computer_use_protocol::{
    AccessibilityCapability, ActionKind, Bounds, ControlNode, DesktopError, DesktopResult,
    DesktopSession, ElementRef, Platform, StableIdentifiers, WindowInfo,
};

/// 默认最大遍历深度。
const DEFAULT_MAX_DEPTH: u32 = 8;
/// 默认最大节点数。
const DEFAULT_MAX_NODES: usize = 400;

pub struct MacosBackend {
    snapshot_seq: AtomicU64,
    /// 快照内控件引用缓存：((snapshot, id) -> AxElement)。
    /// 仅保留最近若干个快照，避免内存无限增长。
    elements: RwLock<HashMap<(u64, String), AxElement>>,
    /// 快照节点表缓存：(snapshot -> Vec<ControlNode>)，供 find 在指定快照内筛选。
    snapshot_nodes: RwLock<HashMap<u64, Vec<ControlNode>>>,
    /// 已使用的快照版本，用于清理过期缓存。
    recent_snapshots: Mutex<Vec<u64>>,
}

impl Default for MacosBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MacosBackend {
    pub fn new() -> Self {
        Self {
            snapshot_seq: AtomicU64::new(1),
            elements: RwLock::new(HashMap::new()),
            snapshot_nodes: RwLock::new(HashMap::new()),
            recent_snapshots: Mutex::new(Vec::new()),
        }
    }

    /// 探测辅助功能授权是否已授予。
    fn is_trusted() -> bool {
        ax::is_process_trusted()
    }
    /// 控件矩形中心（AX 全局坐标，主屏左上原点）；零/负尺寸
    /// （bounds 读取失败的默认值）返回 `None`，不显示指针。
    fn bounds_center(bounds: &ax::AxBounds) -> Option<(f64, f64)> {
        if bounds.width <= 0.0 || bounds.height <= 0.0 {
            return None;
        }
        Some((
            bounds.x + bounds.width / 2.0,
            bounds.y + bounds.height / 2.0,
        ))
    }

    /// 下一个快照版本号。
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
            let mut elements = self.elements.write().unwrap();
            elements.retain(|(s, _), _| *s != old);
            self.snapshot_nodes.write().unwrap().remove(&old);
        }
    }

    /// 缓存控件引用，返回分配的 id。
    fn cache_element(&self, snapshot: u64, element: AxElement) -> String {
        let id = format!("ax-{}-{}", snapshot, element.raw() as usize);
        self.elements
            .write()
            .unwrap()
            .insert((snapshot, id.clone()), element);
        id
    }

    /// 列举运行中的应用，构造窗口候选列表。
    fn enumerate_windows(&self) -> Result<Vec<WindowInfo>, DesktopError> {
        let workspace = NSWorkspace::sharedWorkspace();
        let apps = workspace.runningApplications();
        let snapshot = self.next_snapshot();
        let mut windows = Vec::new();
        let mut idx = 0u64;
        for app in apps.iter() {
            let active = app.isActive();
            let name: String = app
                .localizedName()
                .map(|s| s.to_string())
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let pid = app.processIdentifier();
            // element ref 用基于 pid 的确定性 id（格式 macos-app-{pid}-{idx}）。
            // 不在此缓存 AxElement：list_windows 可能在 wait 轮询中高频调用，
            // 缓存会导致原生对象累积。snapshot 时通过 parse_pid_from_id 重新创建应用根。
            let id = format!("macos-app-{pid}-{idx}");
            idx += 1;
            windows.push(WindowInfo {
                app_name: name.clone(),
                pid: pid as u32,
                element: ElementRef { id, snapshot },
                title: name.clone(),
                is_foreground: active,
                bounds: Bounds::default(),
                visible: true,
                enabled: true,
                identifiers: StableIdentifiers {
                    automation_id: None,
                    role: Some("AXApplication".to_string()),
                },
            });
        }
        Ok(windows)
    }

    /// 递归读取控件树，填充节点列表与引用缓存。
    fn build_snapshot(
        &self,
        root: AxElement,
        snapshot: u64,
        max_depth: u32,
        max_nodes: usize,
        include_invisible: bool,
    ) -> (Vec<ControlNode>, bool, Vec<String>) {
        let mut nodes = Vec::new();
        let mut truncated = false;
        let mut warnings = Vec::new();
        self.walk(
            root,
            snapshot,
            0,
            max_depth,
            max_nodes,
            include_invisible,
            None,
            &mut nodes,
            &mut truncated,
            &mut warnings,
        );
        (nodes, truncated, warnings)
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        element: AxElement,
        snapshot: u64,
        depth: u32,
        max_depth: u32,
        max_nodes: usize,
        include_invisible: bool,
        parent_id: Option<String>,
        nodes: &mut Vec<ControlNode>,
        truncated: &mut bool,
        warnings: &mut Vec<String>,
    ) {
        if nodes.len() >= max_nodes {
            *truncated = true;
            return;
        }
        if depth > max_depth {
            *truncated = true;
            return;
        }

        // 先读子控件（返回独立的 owned Vec），再读属性，最后 cache element。
        // children() 返回的 AxElement 与父元素相互独立，不共享所有权。
        // AttributeUnsupported 是正常的（叶子控件无子节点），静默处理；
        // 其余错误（如 CannotComplete）才记入告警，避免整棵树失败。
        let children = match element.children() {
            Ok(cs) => cs,
            Err(crate::backend::ax::AxError::AttributeUnsupported) => Vec::new(),
            Err(_) => {
                warnings.push(format!("读取子控件失败（深度 {depth}）"));
                Vec::new()
            }
        };

        let role = element.string_attribute("AXRole").unwrap_or_default();
        let title = element.string_attribute("AXTitle").unwrap_or_default();
        let identifier = element.string_attribute("AXIdentifier");
        let enabled = element.bool_attribute("AXEnabled").unwrap_or(true);
        let focused = element.bool_attribute("AXFocused").unwrap_or(false);
        let value = element.string_attribute("AXValue");
        // 读取控件边界（AXPosition + AXSize）；失败返回默认零值，不影响整棵树。
        let ax_bounds = element.bounds();
        // 敏感控件（密码框）的值不返回。
        let sensitive = matches!(role.as_str(), "AXSecureTextField");
        let safe_value = if sensitive { None } else { value };
        let visible = true; // 完整可见性需读 AXChildren 可见性，此处放宽。
        if !include_invisible && !visible {
            return;
        }

        // 缓存控件引用（消费 element），递归时按 children 独立处理。
        let id = self.cache_element(snapshot, element);
        let actions = supported_actions_for(&role);
        let bounds = Bounds {
            x: ax_bounds.x,
            y: ax_bounds.y,
            width: ax_bounds.width,
            height: ax_bounds.height,
        };

        // 前序遍历：先 push 父节点（children 暂空），记录索引后递归子节点，
        // 递归结束回填 children，保证输出顺序为“父→子→孙”。
        let parent_index = nodes.len();
        nodes.push(ControlNode {
            element: ElementRef {
                id: id.clone(),
                snapshot,
            },
            role: role.clone(),
            name: title,
            identifiers: StableIdentifiers {
                automation_id: identifier,
                role: Some(role),
            },
            value: safe_value,
            sensitive,
            visible,
            enabled,
            focused,
            bounds,
            actions,
            parent: parent_id.map(|pid| ElementRef { id: pid, snapshot }),
            children: Vec::new(),
        });

        // 递归子节点，收集其 element 引用并回填到父节点 children。
        let mut child_refs = Vec::with_capacity(children.len());
        for child in children {
            let child_index_before = nodes.len();
            self.walk(
                child,
                snapshot,
                depth + 1,
                max_depth,
                max_nodes,
                include_invisible,
                Some(id.clone()),
                nodes,
                truncated,
                warnings,
            );
            if nodes.len() > child_index_before {
                child_refs.push(nodes[child_index_before].element.clone());
            }
        }
        // 回填父节点的 children（父节点一定还在，因为子节点递归只往后追加）。
        if let Some(node) = nodes.get_mut(parent_index) {
            node.children = child_refs;
        }
    }
}

/// 按角色推断控件支持的动作（macOS AX 动作名映射）。
fn supported_actions_for(role: &str) -> Vec<ActionKind> {
    use ActionKind::*;
    match role {
        "AXButton" | "AXPopUpButton" | "AXCheckBox" | "AXMenuButton" => vec![Press],
        "AXTextField" | "AXTextArea" | "AXComboBox" => vec![SetValue, Focus],
        "AXSecureTextField" => vec![],
        "AXSlider" | "AXIncrementor" => vec![SetValue],
        "AXRadioButton" => vec![Select],
        "AXOutline" | "AXRow" | "AXCell" => vec![Select],
        // AXDisclosureTriangle/AXPopOver 的展开折叠在 AX 中无语义动作（AXPress 是
        // 切换，无法保证方向），暂不声明，避免误导。
        _ => vec![],
    }
}

/// AX 动作名常量。
const AX_PRESS: &str = "AXPress";
const AX_SET_VALUE: &str = "AXValue";
const AX_FOCUSED: &str = "AXFocused";

#[async_trait]
impl Backend for MacosBackend {
    fn platform(&self) -> Platform {
        Platform::Macos
    }

    async fn status(&self) -> DesktopResult<StatusInfo> {
        let trusted = Self::is_trusted();
        let (session, accessibility) = if trusted {
            (
                DesktopSession::Available,
                AccessibilityCapability {
                    available: true,
                    reason: None,
                },
            )
        } else {
            (
                DesktopSession::NotReady,
                AccessibilityCapability {
                    available: false,
                    reason: Some("尚未授予辅助功能权限".to_string()),
                },
            )
        };
        DesktopResult::Ok(StatusInfo {
            session,
            accessibility,
            supported_actions: super::macos_supported_actions(),
        })
    }

    async fn list_windows(
        &self,
        req: &tiangong_plugin_computer_use_protocol::ops::ListWindowsRequest,
    ) -> DesktopResult<tiangong_plugin_computer_use_protocol::ListWindowsResponse> {
        // NSWorkspace 列举应用不需要辅助功能授权；只有读取窗口 AX 属性才需要。
        // 因此 list_windows 在未授权时也能返回应用基础信息，便于 Agent 了解当前桌面。
        let mut windows = match self.enumerate_windows() {
            Ok(w) => w,
            Err(e) => return DesktopResult::Err(e),
        };
        if let Some(app_name) = req.app_name.as_deref() {
            let needle = app_name.to_lowercase();
            windows.retain(|w| w.app_name.to_lowercase().contains(&needle));
        }
        if let Some(pid) = req.pid {
            windows.retain(|w| w.pid == pid);
        }
        if req.foreground_only {
            windows.retain(|w| w.is_foreground);
        }
        DesktopResult::Ok(tiangong_plugin_computer_use_protocol::ListWindowsResponse { windows })
    }

    async fn snapshot(
        &self,
        req: &tiangong_plugin_computer_use_protocol::ops::SnapshotRequest,
    ) -> DesktopResult<SnapshotInfo> {
        if !Self::is_trusted() {
            return DesktopResult::Err(DesktopError::PermissionDenied {
                reason: "尚未授予辅助功能权限".to_string(),
            });
        }
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

        // 定位根元素：优先用窗口引用（从 id 解析 pid），其次按应用名/pid 创建。
        let root = match &req.scope.window {
            Some(window_ref) => match parse_pid_from_id(&window_ref.id) {
                Some(pid) => AxElement::for_application(pid),
                None => {
                    return DesktopResult::Err(DesktopError::WindowNotFound {
                        query: window_ref.id.clone(),
                    });
                }
            },
            None => {
                // 按应用名匹配时收集所有候选，多于一个返回歧义错误，不静默选第一个。
                let pid = match req.scope.pid {
                    Some(pid) => Some(pid),
                    None => match &req.scope.app_name {
                        Some(name) => {
                            let entries = find_app_entries_by_name(name);
                            if entries.len() > 1 {
                                // 逐进程列出候选（含 pid），Agent 可一步改为按 pid 指定。
                                return DesktopResult::Err(DesktopError::AmbiguousMatch {
                                    candidates: entries
                                        .into_iter()
                                        .map(|(pid, app_name)| format!("{app_name}（pid={pid}）"))
                                        .collect(),
                                });
                            }
                            entries.first().map(|(pid, _)| *pid)
                        }
                        None => None,
                    },
                };
                match pid {
                    Some(pid) => AxElement::for_application(pid as i32),
                    None => {
                        let name = req.scope.app_name.as_deref().unwrap_or("未指定目标");
                        return DesktopResult::Err(DesktopError::ApplicationNotFound {
                            query: format!(
                                "{name}（未在运行应用中找到。系统 UI 进程（如 Dock/菜单栏）\
                                 不出现在应用列表：可先用 desktop_list_windows 查看全部窗口进程，\
                                 或用 desktop_screenshot 截屏后按视觉定位）"
                            ),
                        });
                    }
                }
            }
        };

        // 先分配版本号并构建节点，完成后再登记版本与写节点表。
        // 避免先登记导致并发请求在慢快照构建期间将其淘汰，而慢请求随后仍写回残留缓存。
        let snapshot = self.next_snapshot();
        let (nodes, truncated, warnings) =
            self.build_snapshot(root, snapshot, max_depth, max_nodes, req.include_invisible);
        // 构建完成后一次性登记并提交节点表；walk 写入的 elements 也以此版本号为准。
        self.remember_snapshot(snapshot);
        self.snapshot_nodes
            .write()
            .unwrap()
            .insert(snapshot, nodes.clone());
        DesktopResult::Ok(SnapshotInfo {
            snapshot,
            nodes,
            truncated,
            warnings,
        })
    }

    async fn find(
        &self,
        req: &tiangong_plugin_computer_use_protocol::ops::FindRequest,
    ) -> DesktopResult<FindInfo> {
        // find 基于 snapshot：若指定 snapshot 版本，从缓存的节点表筛选；
        // 否则需要 window 引用先取一次快照。
        let (nodes, snapshot) = match req.snapshot {
            Some(s) => {
                // 从缓存的节点表读取；快照已过期则报 stale。
                let cached = self.snapshot_nodes.read().unwrap().get(&s).cloned();
                match cached {
                    Some(n) => (n, s),
                    None => {
                        return DesktopResult::Err(DesktopError::StaleElement { snapshot: s });
                    }
                }
            }
            None => {
                let window = match &req.window {
                    Some(w) => w.clone(),
                    None => {
                        return DesktopResult::Err(DesktopError::ApplicationNotFound {
                            query: "find 需要指定 window 或 snapshot".to_string(),
                        });
                    }
                };
                let snap_req = tiangong_plugin_computer_use_protocol::ops::SnapshotRequest {
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
                let info = match self.snapshot(&snap_req).await {
                    DesktopResult::Ok(i) => i,
                    DesktopResult::Err(e) => return DesktopResult::Err(e),
                };
                (info.nodes, info.snapshot)
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

    async fn action(
        &self,
        req: &tiangong_plugin_computer_use_protocol::ops::ActionRequest,
    ) -> DesktopResult<ActionResult> {
        if !Self::is_trusted() {
            return DesktopResult::Err(DesktopError::PermissionDenied {
                reason: "尚未授予辅助功能权限".to_string(),
            });
        }
        // desktop_list_windows 返回的应用级引用（macos-app-{pid}-{idx}）不入
        // 控件缓存：focus 直接按 pid 唤起应用（激活 + 取消隐藏/最小化），
        // 而不是误报「引用已过期」。
        if let Some(pid) = parse_pid_from_id(&req.element.id) {
            let action_kind = ActionKind::from(req.action);
            if action_kind != ActionKind::Focus {
                return DesktopResult::Err(DesktopError::ActionNotSupported {
                    action: format!("{action_kind:?}（应用级引用）"),
                    supported: vec!["focus".to_string()],
                });
            }
            let steps = super::app_launch::bring_to_front(pid);
            return DesktopResult::Ok(ActionResult {
                performed: !steps.is_empty(),
                summary: if steps.is_empty() {
                    format!("未能唤起应用（pid={pid}），可改用 desktop_open_app")
                } else {
                    format!("已唤起应用（pid={pid}）：{}", steps.join("、"))
                },
                new_window: None,
            });
        }
        // 从缓存取回真实控件。
        let element = {
            let guard = self.elements.read().unwrap();
            guard
                .get(&(req.element.snapshot, req.element.id.clone()))
                .map(|_| req.element.id.clone())
        };
        let element_id = match element {
            Some(id) => id,
            None => {
                return DesktopResult::Err(DesktopError::StaleElement {
                    snapshot: req.element.snapshot,
                });
            }
        };

        // 重新创建控件对象：从缓存取引用。
        let element_ref = {
            let mut guard = self.elements.write().unwrap();
            guard
                .remove(&(req.element.snapshot, element_id.clone()))
                .ok_or(DesktopError::StaleElement {
                    snapshot: req.element.snapshot,
                })
        };
        let element = match element_ref {
            Ok(e) => e,
            Err(e) => return DesktopResult::Err(e),
        };

        let action_kind = ActionKind::from(req.action);
        // 虚拟指针（RFC 0018）：执行动作前读目标控件矩形，成功后把
        // 「天工指针」平滑移动到控件中心——AX 语义动作不移动系统鼠标，
        // 指针是用户感知操作落点的唯一可视化。指针在首次鼠标手势时
        // 自动显示、轮次结束收起；未显示时静默忽略。
        let cursor_center = Self::bounds_center(&element.bounds());
        let result = match action_kind {
            ActionKind::Focus => element.set_bool_attribute(AX_FOCUSED, true),
            ActionKind::Press => element.perform_action(AX_PRESS),
            ActionKind::SetValue => match &req.value {
                Some(v) => element.set_string_attribute(AX_SET_VALUE, v),
                None => Err(ax::AxError::IllegalArgument),
            },
            ActionKind::Toggle => element.perform_action(AX_PRESS),
            ActionKind::Select => element.perform_action(AX_PRESS),
            // Expand/Collapse 在 AX 中无法按语义区分（AXPress 是切换，无法保证
            // 折叠已折叠控件不变成展开）；ScrollIntoView 无对应 AX 动作（AXRaise
            // 是提升窗口而非滚动控件）。无法保证语义时返回不支持，而非错误执行。
            ActionKind::Expand | ActionKind::Collapse | ActionKind::ScrollIntoView => {
                Err(ax::AxError::ActionUnsupported)
            }
        };

        // 重新缓存控件（动作后控件仍可用）。
        self.elements
            .write()
            .unwrap()
            .insert((req.element.snapshot, element_id.clone()), element);

        match result {
            Ok(()) => {
                if let Some((x, y)) = cursor_center {
                    super::overlay::follow_to(x, y);
                }
                DesktopResult::Ok(ActionResult {
                    performed: true,
                    summary: format!("已执行 {:?}", action_kind),
                    new_window: None,
                })
            }
            Err(e) => match e {
                ax::AxError::ActionUnsupported => {
                    DesktopResult::Err(DesktopError::ActionNotSupported {
                        action: format!("{:?}", action_kind),
                        supported: vec![],
                    })
                }
                ax::AxError::InvalidUIElement => DesktopResult::Err(DesktopError::StaleElement {
                    snapshot: req.element.snapshot,
                }),
                ax::AxError::CannotComplete => DesktopResult::Err(DesktopError::PermissionDenied {
                    reason: e.message().to_string(),
                }),
                _ => DesktopResult::Err(DesktopError::BackendUnavailable {
                    reason: e.message().to_string(),
                }),
            },
        }
    }

    async fn wait(
        &self,
        req: &tiangong_plugin_computer_use_protocol::ops::WaitRequest,
    ) -> DesktopResult<WaitResult> {
        let deadline = Instant::now() + Duration::from_millis(req.timeout_ms);
        use tiangong_plugin_computer_use_protocol::ops::WaitCondition;
        match &req.condition {
            WaitCondition::Appear { target } | WaitCondition::Disappear { target } => {
                let looking_appear = matches!(req.condition, WaitCondition::Appear { .. });
                let start = Instant::now();
                loop {
                    let exists = self.target_exists(target);
                    if looking_appear && exists {
                        return DesktopResult::Ok(WaitResult {
                            satisfied: true,
                            waited_ms: start.elapsed().as_millis() as u64,
                            matched_element: None,
                            detail: None,
                        });
                    }
                    if !looking_appear && !exists {
                        return DesktopResult::Ok(WaitResult {
                            satisfied: true,
                            waited_ms: start.elapsed().as_millis() as u64,
                            matched_element: None,
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
            WaitCondition::Focus { element }
            | WaitCondition::Available { element }
            | WaitCondition::Value { element, .. } => {
                // 控件级等待：从缓存取回控件引用，轮询对应属性直到满足或超时。
                let expected_value = match &req.condition {
                    WaitCondition::Value { expected, .. } => expected.clone(),
                    _ => None,
                };
                let start = Instant::now();
                let cached = self
                    .elements
                    .read()
                    .unwrap()
                    .get(&(element.snapshot, element.id.clone()))
                    .cloned();
                let ax_element = match cached {
                    Some(e) => e,
                    None => {
                        return DesktopResult::Err(DesktopError::StaleElement {
                            snapshot: element.snapshot,
                        });
                    }
                };
                // 记录初始值：value 等待无期望值时，需等到值真正发生变化才算成功。
                let initial_value = if expected_value.is_none()
                    && matches!(&req.condition, WaitCondition::Value { .. })
                {
                    ax_element.string_attribute("AXValue")
                } else {
                    None
                };
                loop {
                    let satisfied = match &req.condition {
                        WaitCondition::Focus { .. } => {
                            ax_element.bool_attribute("AXFocused").unwrap_or(false)
                        }
                        WaitCondition::Available { .. } => {
                            ax_element.bool_attribute("AXEnabled").unwrap_or(false)
                        }
                        WaitCondition::Value { .. } => match &expected_value {
                            Some(want) => ax_element
                                .string_attribute("AXValue")
                                .is_some_and(|v| v == *want),
                            None => {
                                // 无期望值：比较当前值与初始值，不同才算变化。
                                let current = ax_element.string_attribute("AXValue");
                                current != initial_value
                            }
                        },
                        // Appear/Disappear 已在外层处理，这里不会到达。
                        _ => false,
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

    async fn mouse(
        &self,
        req: &tiangong_plugin_computer_use_protocol::ops::MouseRequest,
    ) -> DesktopResult<crate::backend::MouseResult> {
        if !Self::is_trusted() {
            return DesktopResult::Err(DesktopError::PermissionDenied {
                reason: "尚未授予辅助功能权限".to_string(),
            });
        }
        // 参数完整性在协议层校验后进入；此处直接执行手势。
        let to = match (req.to_x, req.to_y) {
            (Some(tx), Some(ty)) => Some((tx, ty)),
            (None, None) => None,
            _ => {
                return DesktopResult::Err(DesktopError::BackendUnavailable {
                    reason: "drag 的 to_x/to_y 必须同时提供".to_string(),
                });
            }
        };
        let (gesture, x, y) = (req.gesture, req.x, req.y);
        let scroll = (req.delta_y.unwrap_or(0.0), req.delta_x.unwrap_or(0.0));
        let cancel = super::input_cancel::current();
        let outcome = tokio::task::spawn_blocking(move || {
            super::mouse::perform(gesture, x, y, to, scroll, &cancel)
        })
        .await
        .unwrap_or_else(|e| Err(format!("鼠标手势执行线程异常：{e}")));
        match outcome {
            Ok(summary) => DesktopResult::Ok(crate::backend::MouseResult {
                performed: true,
                summary,
            }),
            Err(reason) => DesktopResult::Err(DesktopError::BackendUnavailable { reason }),
        }
    }
    async fn keyboard(
        &self,
        req: &tiangong_plugin_computer_use_protocol::ops::KeyboardRequest,
    ) -> DesktopResult<crate::backend::KeyboardResult> {
        if !Self::is_trusted() {
            return DesktopResult::Err(DesktopError::PermissionDenied {
                reason: "尚未授予辅助功能权限".to_string(),
            });
        }
        let (action, text, key, keys) = (
            req.action,
            req.text.clone(),
            req.key.clone(),
            req.keys.clone(),
        );
        // 手势内含阻塞等待与进程级互斥锁，放到阻塞线程池执行；
        // 取消令牌须在进入阻塞线程前取出（阻塞线程不继承任务上下文）。
        let cancel = super::input_cancel::current();
        let outcome = tokio::task::spawn_blocking(move || {
            super::keyboard::perform(action, text, key, keys, &cancel)
        })
        .await
        .unwrap_or_else(|e| Err(format!("键盘输入执行线程异常：{e}")));
        match outcome {
            Ok(summary) => DesktopResult::Ok(crate::backend::KeyboardResult {
                performed: true,
                summary,
            }),
            Err(reason) => DesktopResult::Err(DesktopError::BackendUnavailable { reason }),
        }
    }
    async fn screenshot(
        &self,
        req: &tiangong_plugin_computer_use_protocol::ops::ScreenshotRequest,
    ) -> DesktopResult<tiangong_plugin_computer_use_protocol::ScreenshotResponse> {
        if let Some(delay) = req.delay_ms.filter(|d| *d > 0) {
            tokio::time::sleep(Duration::from_millis(u64::from(delay.min(2000)))).await;
        }
        capture_screenshot(req)
    }
    async fn open_app(
        &self,
        req: &tiangong_plugin_computer_use_protocol::ops::OpenAppRequest,
    ) -> DesktopResult<tiangong_plugin_computer_use_protocol::ops::OpenAppResponse> {
        let mut result = super::app_launch::open_app(req).await;
        // 应用窗口出现后自动分屏：天工靠左、目标占右侧。
        if let DesktopResult::Ok(resp) = &mut result
            && resp.window.is_some()
            && let Ok(pid) = i32::try_from(resp.pid)
        {
            let outcome = super::mac_split::split_with_host(pid).await;
            if let Some(target) = outcome.target {
                resp.window = Some(target);
            }
            resp.summary = format!("{}。{}", resp.summary, outcome.detail);
            resp.split = Some(outcome);
        }
        result
    }
    async fn restore_host_window(&self, saved: crate::split::SavedHostFrame) -> Result<(), String> {
        super::mac_split::restore_host(saved).await
    }
}

impl MacosBackend {
    /// appear/disappear 判定：目标应用须有**屏幕上可见**的普通层窗口
    /// （CGWindowList OnScreenOnly），而不是进程在运行即算出现——后者
    /// 会让最小化/其他空间/已关闭主窗口的应用立刻误报 satisfied。
    ///
    /// NSWorkspace 列举的「窗口」标题即应用名，title 条件同样按窗口
    /// 所属应用名匹配。
    fn target_exists(
        &self,
        target: &tiangong_plugin_computer_use_protocol::ops::WaitTarget,
    ) -> bool {
        [target.app_name.as_deref(), target.title.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .any(|name| window_frame_for_app(None, Some(name)).is_some())
    }
}

/// 从 element id（格式 macos-app-{pid}-{idx}）解析 pid。
fn parse_pid_from_id(id: &str) -> Option<i32> {
    let parts: Vec<&str> = id.split('-').collect();
    if parts.len() >= 3 && parts[0] == "macos" && parts[1] == "app" {
        parts[2].parse().ok()
    } else {
        None
    }
}

/// 按应用名查找所有匹配的（pid, 应用名）对（包含匹配，大小写不敏感）。
///
/// 返回应用名供歧义错误逐进程列出候选（多进程应用如浏览器/IM 常见），
/// Agent 可一步改为按 pid 指定，不必逐个试探。
fn find_app_entries_by_name(name: &str) -> Vec<(u32, String)> {
    let workspace = NSWorkspace::sharedWorkspace();
    let apps = workspace.runningApplications();
    let needle = name.to_lowercase();
    apps.iter()
        .filter_map(|app| {
            let app_name: String = app
                .localizedName()
                .map(|s| s.to_string())
                .unwrap_or_default();
            if app_name.to_lowercase().contains(&needle) {
                Some((app.processIdentifier() as u32, app_name))
            } else {
                None
            }
        })
        .collect()
}

/// 按条件筛选节点。
fn filter_nodes(
    nodes: &[ControlNode],
    conditions: &tiangong_plugin_computer_use_protocol::ops::FindConditions,
) -> Vec<ControlNode> {
    use tiangong_plugin_computer_use_protocol::MatchMode;
    nodes
        .iter()
        .filter(|n| {
            // 稳定标识 + 类型 优先，名称补充。
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
            let name_match = conditions.name.as_ref().map(|want| {
                let have = n.name.as_str();
                match conditions.mode {
                    MatchMode::Exact => have == want,
                    MatchMode::Contains => have.to_lowercase().contains(&want.to_lowercase()),
                }
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
            id_match
                && role_match
                && name_match.unwrap_or(true)
                && value_match.unwrap_or(true)
                && visible_match
                && enabled_match
                && focused_match
        })
        .cloned()
        .collect()
}

// ── desktop_screenshot（RFC 0017 图片注入的图源，Phase 1：主显示器全屏）──

// 屏幕录制 TCC（macOS 10.15+）。macOS 没有屏幕录制的 Info.plist 用途
// 声明键（Apple 文档不存在 NSScreenCaptureUsageDescription）：授权对话
// 框只能由应用进程实际发起捕获请求触发。sidecar 是天工 App 的子进程，
// 责任进程归属宿主，在此调用等价于以天工身份申请。
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// 确保屏幕录制授权可用：已授权直接通过；未决时触发系统授权弹窗；
/// 被拒绝后返回带设置引导的错误。缺省这步时，未授权状态下
/// `screencapture` 只会产出壁纸级空图或失败，且系统设置的屏幕录制
/// 列表里根本不会出现天工的开关项。
fn ensure_screen_capture_access() -> DesktopResult<()> {
    // SAFETY: 纯查询/请求函数，无跨调用可变状态。
    if unsafe { CGPreflightScreenCaptureAccess() } {
        return DesktopResult::Ok(());
    }
    if unsafe { CGRequestScreenCaptureAccess() } {
        return DesktopResult::Ok(());
    }
    DesktopResult::Err(DesktopError::PermissionDenied {
        reason: "天工未获得屏幕录制授权。请打开 系统设置 → 隐私与安全性 → 屏幕录制，勾选天工并重启应用后重试".to_string(),
    })
}

/// 执行截图：`screencapture` 由 sidecar（宿主直启、无 Seatbelt）调用，
/// 屏幕录制 TCC 的责任进程归属天工 App。
///
/// 截取范围按优先级：显式 `region` > 应用窗口（CGWindowList 按 pid 定位
/// 最前普通层窗口，bounds 即屏幕坐标 points）> 主显示器全屏。区域用
/// `-R x,y,w,h`（所见即所得：被遮挡部分包含遮挡物，与真人看到的屏幕
/// 一致）；`max_dimension` 超限时用 `sips -Z` 原地等比缩小。
fn capture_screenshot(
    req: &tiangong_plugin_computer_use_protocol::ops::ScreenshotRequest,
) -> DesktopResult<tiangong_plugin_computer_use_protocol::ScreenshotResponse> {
    use tiangong_plugin_computer_use_protocol::ScreenshotResponse;

    if let DesktopResult::Err(error) = ensure_screen_capture_access() {
        return DesktopResult::Err(error);
    }
    let dir = screenshot_output_dir();
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return DesktopResult::Err(DesktopError::BackendUnavailable {
            reason: format!("创建截图目录失败 {}：{error}", dir.display()),
        });
    }
    // screencapture 先落原始 PNG（Retina 为物理像素），归一缩放并转 JPEG
    // 后删除中间产物。
    let id = scru128::new();
    let raw_path = dir.join(format!("desktop-{id}.raw.png"));
    let path = dir.join(format!("desktop-{id}.jpg"));

    // 决定截取矩形（None = 全屏）。
    let rect: Option<(f64, f64, f64, f64)> = if let Some(region) = req
        .region
        .filter(|r| r.width.is_finite() && r.height.is_finite() && r.width > 0.0 && r.height > 0.0)
    {
        Some((region.x, region.y, region.width, region.height))
    } else {
        // 应用定位：显式 pid / app_name（CGWindowList owner 名主路径，见
        // window_frame_for_app）/ 前台应用。
        let (pid_opt, name_opt): (Option<u32>, Option<&str>) =
            if let Some(pid) = req.pid.filter(|p| *p > 0) {
                (Some(pid), None)
            } else if let Some(name) = req
                .app_name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
            {
                (None, Some(name))
            } else if req.foreground_only {
                let workspace = NSWorkspace::sharedWorkspace();
                (
                    workspace
                        .frontmostApplication()
                        .map(|app| app.processIdentifier() as u32),
                    None,
                )
            } else {
                (None, None)
            };
        let located = window_frame_for_app(pid_opt, name_opt);
        if located.is_none() && (pid_opt.is_some() || name_opt.is_some()) {
            return DesktopResult::Err(DesktopError::BackendUnavailable {
                reason: "未找到目标应用可截取的屏幕窗口（窗口可能已最小化或不在屏幕上）"
                    .to_string(),
            });
        }
        located
    };

    // -x 静音；-o 去除阴影边饰；-R 指定区域（points）。
    let mut command = std::process::Command::new("/usr/sbin/screencapture");
    command.arg("-x").arg("-o");
    if let Some((x, y, w, h)) = rect {
        command.arg(format!("-R{x:.0},{y:.0},{w:.0},{h:.0}"));
    }
    let output = match command.arg(&raw_path).output() {
        Ok(output) => output,
        Err(error) => {
            return DesktopResult::Err(DesktopError::BackendUnavailable {
                reason: format!("启动 screencapture 失败：{error}"),
            });
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return DesktopResult::Err(DesktopError::PermissionDenied {
            reason: format!(
                "screencapture 退出码 {} {stderr}；请确认天工已获得屏幕录制授权（系统设置 → 隐私与安全性 → 屏幕录制）",
                output.status.code().unwrap_or(-1),
            ),
        });
    }

    // 原始产物尺寸不可读说明截图无效（授权被撤销后 screencapture 可能
    // 产出空文件），明确失败而不是返回 (0,0) 的失真元数据。
    let Some((raw_width, raw_height)) = png_dimensions(&raw_path) else {
        let _ = std::fs::remove_file(&raw_path);
        return DesktopResult::Err(DesktopError::BackendUnavailable {
            reason: format!("截图产物不是有效 PNG：{}", raw_path.display()),
        });
    };
    // 截取范围的逻辑坐标（points）：区域/窗口即 rect（与 -R 参数同样取整），
    // 全屏为主显示器 bounds（screencapture 单文件只截主屏）。
    let logical = match rect {
        Some((x, y, w, h)) => Bounds {
            x: x.round(),
            y: y.round(),
            width: w.round(),
            height: h.round(),
        },
        None => {
            let b = core_graphics::display::CGDisplay::main().bounds();
            Bounds {
                x: b.origin.x,
                y: b.origin.y,
                width: b.size.width,
                height: b.size.height,
            }
        }
    };
    let max_dimension = req
        .max_dimension
        .filter(|m| *m > 0)
        .unwrap_or(DEFAULT_SCREENSHOT_MAX_DIMENSION);
    let plan = plan_screenshot_output(
        (raw_width, raw_height),
        (logical.width, logical.height),
        max_dimension,
    );
    // 统一经 sips 重采样到目标尺寸并转 JPEG：Retina 物理像素归一为逻辑
    // 尺寸（1 像素 = 1 point），超上限时再按 1/2、1/4… 整数倍缩小，
    // 不裁剪画面。
    let converted = std::process::Command::new("/usr/bin/sips")
        .arg("-s")
        .arg("format")
        .arg("jpeg")
        .arg("-s")
        .arg("formatOptions")
        .arg(SCREENSHOT_JPEG_QUALITY.to_string())
        .arg("-z")
        .arg(plan.height.to_string())
        .arg(plan.width.to_string())
        .arg(&raw_path)
        .arg("--out")
        .arg(&path)
        .output();
    let _ = std::fs::remove_file(&raw_path);
    match converted {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            return DesktopResult::Err(DesktopError::BackendUnavailable {
                reason: format!(
                    "sips 转换失败（退出码 {}）：{}",
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stderr).trim(),
                ),
            });
        }
        Err(error) => {
            return DesktopResult::Err(DesktopError::BackendUnavailable {
                reason: format!("启动 sips 失败：{error}"),
            });
        }
    }

    let size_bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let Some((width, height)) = jpeg_dimensions(&path) else {
        return DesktopResult::Err(DesktopError::BackendUnavailable {
            reason: format!("截图转换产物不是有效 JPEG：{}", path.display()),
        });
    };
    let app_name = screenshot_target_app_name(req);
    let original_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("screenshot.jpg")
        .to_string();
    DesktopResult::Ok(ScreenshotResponse {
        path: path.display().to_string(),
        width,
        height,
        app_name,
        size_bytes,
        logical_bounds: Some(logical),
        scale: f64::from(plan.factor),
        // 注入声明（RFC 0017）：core 读到此数组后在工具批次闭合处
        // 落成宿主注入的图片消息。
        injected_assets: vec![tiangong_plugin_computer_use_protocol::InjectedAsset {
            local_path: path.display().to_string(),
            mime_type: "image/jpeg".to_string(),
            original_name: Some(original_name.clone()),
            size_bytes,
            kind: tiangong_types::MediaKind::Image,
            source: Some("desktop_screenshot".to_string()),
        }],
    })
}

use super::screenshot_plan::{
    SCREENSHOT_JPEG_QUALITY, plan_screenshot_output, screenshot_output_dir,
};

/// 读取 JPEG 像素尺寸：扫描到 SOF0..SOF15（排除 DHT/JPG/DAC）段取宽高。
fn jpeg_dimensions(path: &std::path::Path) -> Option<(u32, u32)> {
    let bytes = std::fs::read(path).ok()?;
    parse_jpeg_dimensions(&bytes)
}

fn parse_jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        let marker = bytes[i + 1];
        // 填充字节 / 无长度标记。
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
        let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            if i + 9 > bytes.len() {
                return None;
            }
            let height = u32::from(u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]));
            let width = u32::from(u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]));
            return (width > 0 && height > 0).then_some((width, height));
        }
        if len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

/// CGWindowList 窗口定位：按 pid（精确）或 owner 名（包含匹配，大小写
/// 不敏感）取最前的普通层（layer 0）屏幕窗口 frame（points，主屏左上
/// 原点全局坐标，与 AX bounds 同系）。按 z 序取第一个；最小化/离屏窗口
/// 不在 OnScreenOnly 列表内。
///
/// owner 名匹配是主路径：多进程应用（微信 4.x 等）的主窗口可能挂在
/// NSWorkspace 应用名匹配不到的 helper 进程上，pid 交集会漏；CGWindowList
/// 的 OwnerName 与窗口同源，不存在集合错位。
pub(super) fn window_frame_for_app(
    pid: Option<u32>,
    app_name: Option<&str>,
) -> Option<(f64, f64, f64, f64)> {
    use core_foundation::base::{CFType, FromVoid, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::window::{
        copy_window_info, kCGNullWindowID, kCGWindowBounds, kCGWindowLayer,
        kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly, kCGWindowOwnerName,
        kCGWindowOwnerPID,
    };
    if pid.is_none() && app_name.is_none() {
        return None;
    }

    let array = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    )?;
    // SAFETY: 静态 CFStringRef 由 CoreGraphics 框架常驻提供。
    let key_pid: CFString = unsafe { CFString::wrap_under_get_rule(kCGWindowOwnerPID) };
    let key_owner: CFString = unsafe { CFString::wrap_under_get_rule(kCGWindowOwnerName) };
    let key_layer: CFString = unsafe { CFString::wrap_under_get_rule(kCGWindowLayer) };
    let key_bounds: CFString = unsafe { CFString::wrap_under_get_rule(kCGWindowBounds) };
    let needle = app_name.map(str::to_lowercase);
    let dict_number = |dict: &CFDictionary<CFString, CFType>, key: &CFString| -> Option<i64> {
        let value = dict.find(key)?;
        let number: CFNumber = value.downcast()?;
        number.to_i64()
    };
    for ptr in array.get_all_values() {
        // SAFETY: 数组存活期间元素指针有效；ItemRef 只借用不接管所有权。
        let item: core_foundation::base::ItemRef<'_, CFDictionary<CFString, CFType>> =
            unsafe { <CFDictionary<CFString, CFType>>::from_void(ptr) };
        let owner_pid = dict_number(&item, &key_pid);
        let pid_match = pid.is_some_and(|p| owner_pid == Some(p as i64));
        let name_match = needle.as_deref().is_some_and(|n| {
            item.find(&key_owner)
                .and_then(|v| {
                    // CFType 无 Display：先 downcast 到 CFString 再取文本。
                    v.downcast::<CFString>()
                })
                .is_some_and(|owner| owner.to_string().to_lowercase().contains(n))
        });
        if !pid_match && !name_match {
            continue;
        }
        if dict_number(&item, &key_layer) != Some(0) {
            continue;
        }
        let Some(bounds_ref) = item.find(&key_bounds) else {
            continue;
        };
        // SAFETY: CGWindowList 契约保证该值是 CFDictionary；get-rule 包装
        // 只借用指针（数组存活期间有效），不接管所有权。
        let bounds: CFDictionary<CFString, CFType> = unsafe {
            <CFDictionary<CFString, CFType>>::wrap_under_get_rule(
                bounds_ref.as_CFTypeRef() as core_foundation::dictionary::CFDictionaryRef
            )
        };
        let bounds_number = |name: &str| -> Option<f64> {
            let key = CFString::new(name);
            let value = bounds.find(&key)?;
            let number: CFNumber = value.downcast()?;
            number.to_f64()
        };
        let (x, y) = (bounds_number("X")?, bounds_number("Y")?);
        let (w, h) = (bounds_number("Width")?, bounds_number("Height")?);
        if w > 0.0 && h > 0.0 {
            return Some((x, y, w, h));
        }
    }
    None
}

/// 解析截图目标应用名：显式 app_name 优先，pid 次之（反查应用名），
/// foreground_only 取前台应用；均未指定时为空（全屏截图）。
fn screenshot_target_app_name(
    req: &tiangong_plugin_computer_use_protocol::ops::ScreenshotRequest,
) -> String {
    if let Some(name) = req.app_name.as_deref().filter(|n| !n.trim().is_empty()) {
        return name.trim().to_string();
    }
    let workspace = NSWorkspace::sharedWorkspace();
    if let Some(pid) = req.pid.filter(|pid| *pid > 0) {
        let apps = workspace.runningApplications();
        if let Some(app) = apps
            .iter()
            .find(|app| app.processIdentifier() as u32 == pid)
        {
            return app
                .localizedName()
                .map(|s| s.to_string())
                .unwrap_or_default();
        }
    }
    if req.foreground_only
        && let Some(app) = workspace.frontmostApplication()
    {
        return app
            .localizedName()
            .map(|s| s.to_string())
            .unwrap_or_default();
    }
    String::new()
}

/// 直接读取 PNG IHDR 获得像素尺寸（签名 8 字节 + 块头 8 字节后即
/// 宽高各 4 字节大端），不再为每次截图额外 fork 一个 `sips` 进程。
fn png_dimensions(path: &std::path::Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut header = [0_u8; 24];
    std::fs::File::open(path)
        .ok()?
        .read_exact(&mut header)
        .ok()?;
    const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if header[..8] != PNG_SIGNATURE || &header[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(header[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(header[20..24].try_into().ok()?);
    Some((width, height))
}

#[cfg(test)]
mod screenshot_tests {
    use super::parse_jpeg_dimensions;

    #[test]
    fn jpeg_dimensions_reads_sof_after_app_segments() {
        // SOI + APP0(长度 16) + DQT 占位(长度 4) + SOF0(高 720 宽 1280)。
        let mut bytes = vec![0xFF, 0xD8];
        bytes.extend([0xFF, 0xE0, 0x00, 0x10]);
        bytes.extend([0u8; 14]);
        bytes.extend([0xFF, 0xDB, 0x00, 0x04, 0x00, 0x00]);
        // DHT 不是 SOF，需跳过。
        bytes.extend([0xFF, 0xC4, 0x00, 0x03, 0x00]);
        bytes.extend([0xFF, 0xC0, 0x00, 0x11, 0x08, 0x02, 0xD0, 0x05, 0x00]);
        bytes.extend([0u8; 12]);
        assert_eq!(parse_jpeg_dimensions(&bytes), Some((1280, 720)));
        assert_eq!(parse_jpeg_dimensions(b"\x89PNG"), None);
        assert_eq!(parse_jpeg_dimensions(&[0xFF, 0xD8, 0xFF]), None);
    }
}
