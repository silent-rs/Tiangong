//! Linux AT-SPI2 无障碍后端。
//!
//! 通过 `atspi` crate（纯 Rust、基于 zbus/D-Bus 的 AT-SPI2 实现）访问桌面会话的
//! accessibility bus，列举应用、读取控件树、查找控件、执行动作并等待状态变化。
//!
//! 适用 GTK、Qt、Electron 等正常暴露 AT-SPI 信息的应用。Wayland 下仍以 AT-SPI
//! 语义动作为主，不依赖 xdotool/wmctrl 等 X11 坐标工具。
//!
//! 无 accessibility bus（纯 SSH/容器/无头环境）或受 Flatpak/Snap 策略限制时，
//! 返回 `desktop_session_unavailable` 及原因，不影响宿主启动。
//! 动作按控件实际暴露的接口（Action/Value/Text/Selection/Component）调用，
//! 动作前重新确认控件身份。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::action::ActionProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::proxy::editable_text::EditableTextProxy;
use atspi::proxy::text::TextProxy;
use atspi::{AccessibilityConnection, Interface, InterfaceSet, ObjectRefOwned, Role, State};

use super::{ActionResult, Backend, FindInfo, SnapshotInfo, StatusInfo, WaitResult};
use tiangong_plugin_computer_use_protocol::ops::{
    ActionRequest, FindConditions, FindRequest, ListWindowsRequest, SnapshotRequest, WaitCondition,
    WaitRequest,
};
use tiangong_plugin_computer_use_protocol::{
    AccessibilityCapability, ActionKind, Bounds, ControlNode, DesktopError, DesktopResult,
    DesktopSession, ElementRef, MatchMode, Platform, StableIdentifiers, WindowInfo,
};

const DEFAULT_MAX_DEPTH: u32 = 8;
const DEFAULT_MAX_NODES: usize = 400;
/// 根桌面对象路径与 AT-SPI 注册表的 well-known 服务名。
const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const ROOT_DEST: &str = "org.a11y.atspi.Registry";

pub struct LinuxBackend {
    snapshot_seq: AtomicU64,
    /// 快照内控件引用缓存：((snapshot, id) -> ObjectRef)。
    /// ObjectRef 含 D-Bus 服务名和路径，轻量可序列化，用于还原真实控件。
    elements: RwLock<HashMap<(u64, String), ObjectRefOwned>>,
    /// 快照节点表缓存：(snapshot -> Vec<ControlNode>)，供 find 筛选与 action 身份校验。
    snapshot_nodes: RwLock<HashMap<u64, Vec<ControlNode>>>,
    /// 已使用的快照版本，用于淘汰旧缓存。
    recent_snapshots: Mutex<Vec<u64>>,
    /// 最近的窗口列表版本；窗口引用与控件快照分别管理。
    recent_window_snapshots: Mutex<Vec<u64>>,
}

impl Default for LinuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxBackend {
    pub fn new() -> Self {
        Self {
            snapshot_seq: AtomicU64::new(1),
            elements: RwLock::new(HashMap::new()),
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

    /// 保留最近 4 次窗口枚举引用，避免长期运行时缓存无界增长。
    fn remember_window_snapshot(&self, snapshot: u64) {
        let mut recent = self.recent_window_snapshots.lock().unwrap();
        recent.retain(|&s| s != snapshot);
        recent.push(snapshot);
        while recent.len() > 4 {
            let old = recent.remove(0);
            self.elements.write().unwrap().retain(|(s, _), _| *s != old);
        }
    }

    /// 尝试连接 accessibility bus。连接失败说明无图形会话或未启用无障碍。
    /// 加 5 秒超时，防止无 D-Bus 环境下连接调用阻塞。
    async fn connect(&self) -> Result<AccessibilityConnection, DesktopError> {
        let connect = async {
            if let Err(e) = atspi::connection::set_session_accessibility(true).await {
                tracing::debug!(error = %e, "启用会话无障碍失败，继续尝试连接");
            }
            AccessibilityConnection::new().await
        };
        match tokio::time::timeout(Duration::from_secs(5), connect).await {
            Ok(Ok(conn)) => Ok(conn),
            Ok(Err(e)) => Err(DesktopError::DesktopSessionUnavailable {
                reason: format!("无法连接 AT-SPI accessibility bus: {e}"),
            }),
            Err(_) => Err(DesktopError::DesktopSessionUnavailable {
                reason: "连接 AT-SPI accessibility bus 超时（5 秒）".to_string(),
            }),
        }
    }

    /// 构造根桌面对象代理。
    async fn root_proxy(
        conn: &atspi::zbus::Connection,
    ) -> Result<AccessibleProxy<'_>, DesktopError> {
        let dest = atspi::zbus::names::WellKnownName::from_static_str(ROOT_DEST).map_err(|e| {
            DesktopError::BackendUnavailable {
                reason: format!("无效根服务名: {e}"),
            }
        })?;
        let path = atspi::zbus::zvariant::ObjectPath::from_static_str(ROOT_PATH).map_err(|e| {
            DesktopError::BackendUnavailable {
                reason: format!("无效根路径: {e}"),
            }
        })?;
        AccessibleProxy::builder(conn)
            .destination(dest)
            .map_err(|e| DesktopError::BackendUnavailable {
                reason: format!("构造根代理失败: {e}"),
            })?
            .path(path)
            .map_err(|e| DesktopError::BackendUnavailable {
                reason: format!("构造根代理路径失败: {e}"),
            })?
            .build()
            .await
            .map_err(|e| DesktopError::BackendUnavailable {
                reason: format!("连接根桌面对象失败: {e}"),
            })
    }

    /// 探测控件真实支持的统一动作集合（基于已实现接口 + Action 接口动作名）。
    async fn detect_actions(
        element: &AccessibleProxy<'_>,
        interfaces: &InterfaceSet,
    ) -> Vec<ActionKind> {
        use ActionKind::*;
        let mut actions = Vec::new();
        // Component 接口总是支持 grab_focus。
        if interfaces.contains(Interface::Component) {
            actions.push(Focus);
        }
        // Action 接口：按动作名映射统一动作。
        if interfaces.contains(Interface::Action)
            && let Ok(action_proxy) = build_proxy::<ActionProxy<'_>>(element).await
            && let Ok(raw_actions) = action_proxy.get_actions().await
        {
            for atspi::Action { name, .. } in &raw_actions {
                match name.to_lowercase().as_str() {
                    "click" | "press" | "activate" | "jump" => {
                        if !actions.contains(&Press) {
                            actions.push(Press);
                        }
                    }
                    "toggle" if !actions.contains(&Toggle) => actions.push(Toggle),
                    "expand" if !actions.contains(&Expand) => actions.push(Expand),
                    "collapse" if !actions.contains(&Collapse) => actions.push(Collapse),
                    _ => {}
                }
            }
        }
        // EditableText 接口：支持设值。
        if interfaces.contains(Interface::EditableText) {
            actions.push(SetValue);
        }
        // select 尚未实现（AT-SPI Selection 需父选择容器索引），不声明避免假承诺。
        // Component 接口的 scroll_to。
        if interfaces.contains(Interface::Component) {
            actions.push(ScrollIntoView);
        }
        actions
    }

    /// 校验控件身份：role 和 name 是否与快照时一致。
    /// 单独比对 role 不够（同角色控件可能复用同一对象路径），联合 name 提高可靠性。
    async fn verify_identity(element: &AccessibleProxy<'_>, node: &ControlNode) -> bool {
        let expected_role = node.identifiers.role.as_deref().unwrap_or("");
        let Ok(role) = element.get_role().await else {
            return false;
        };
        if !expected_role.is_empty() && expected_role != format!("{role:?}") {
            return false;
        }
        // 名称一致性（空名称控件放宽，避免误判）。
        if !node.name.is_empty() {
            let Ok(current_name) = element.name().await else {
                return false;
            };
            if current_name != node.name {
                return false;
            }
        }
        true
    }
}

#[async_trait]
impl Backend for LinuxBackend {
    fn platform(&self) -> Platform {
        Platform::Linux
    }

    async fn status(&self) -> DesktopResult<StatusInfo> {
        match self.connect().await {
            Ok(_conn) => DesktopResult::Ok(StatusInfo {
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
        let conn = match self.connect().await {
            Ok(c) => c,
            Err(e) => return DesktopResult::Err(e),
        };
        let zbus = conn.connection();
        let desktop = match Self::root_proxy(zbus).await {
            Ok(d) => d,
            Err(e) => return DesktopResult::Err(e),
        };
        let children = match desktop.get_children().await {
            Ok(c) => c,
            Err(e) => {
                return DesktopResult::Err(DesktopError::BackendUnavailable {
                    reason: format!("读取桌面子应用失败: {e}"),
                });
            }
        };
        let snapshot = self.next_snapshot();
        let mut windows = Vec::new();
        for (idx, child_ref) in children.into_iter().enumerate() {
            let proxy = match child_ref.as_accessible_proxy(zbus).await {
                Ok(p) => p,
                Err(_) => continue,
            };
            let name = proxy.name().await.unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            // 缓存 ObjectRef，供 snapshot 通过窗口引用还原（而非依赖列表序号）。
            let id = format!("atspi-app-{idx}");
            if let Ok(obj_ref) = ObjectRefOwned::try_from(&proxy) {
                self.elements
                    .write()
                    .unwrap()
                    .insert((snapshot, id.clone()), obj_ref);
            }
            windows.push(WindowInfo {
                app_name: name.clone(),
                pid: 0,
                element: ElementRef { id, snapshot },
                title: name,
                is_foreground: false,
                bounds: Bounds::default(),
                visible: true,
                enabled: true,
                identifiers: StableIdentifiers {
                    automation_id: None,
                    role: Some("AXApplication".to_string()),
                },
            });
        }
        self.remember_window_snapshot(snapshot);
        if let Some(app_name) = req.app_name.as_deref() {
            let needle = app_name.to_lowercase();
            windows.retain(|w| w.app_name.to_lowercase().contains(&needle));
        }
        if req.pid.is_some() {
            return DesktopResult::Err(DesktopError::BackendUnavailable {
                reason: "AT-SPI 后端暂不支持 pid 筛选，请用 app_name".to_string(),
            });
        }
        if req.foreground_only {
            return DesktopResult::Err(DesktopError::BackendUnavailable {
                reason: "AT-SPI 后端暂不支持 foreground_only 筛选".to_string(),
            });
        }
        DesktopResult::Ok(tiangong_plugin_computer_use_protocol::ListWindowsResponse { windows })
    }

    async fn snapshot(&self, req: &SnapshotRequest) -> DesktopResult<SnapshotInfo> {
        let conn = match self.connect().await {
            Ok(c) => c,
            Err(e) => return DesktopResult::Err(e),
        };
        let zbus = conn.connection();
        let desktop = match Self::root_proxy(zbus).await {
            Ok(d) => d,
            Err(e) => return DesktopResult::Err(e),
        };
        let children = match desktop.get_children().await {
            Ok(c) => c,
            Err(e) => {
                return DesktopResult::Err(DesktopError::BackendUnavailable {
                    reason: format!("读取桌面子应用失败: {e}"),
                });
            }
        };
        // 定位根元素：优先 window 引用（从缓存取回 ObjectRef 还原），其次 app_name。
        // ObjectRef 必须在 root 代理整个使用期间存活，因此在分支外持有所有权。
        let cached_window_object = req.scope.window.as_ref().and_then(|window_ref| {
            self.elements
                .read()
                .unwrap()
                .get(&(window_ref.snapshot, window_ref.id.clone()))
                .cloned()
        });
        // 不依赖列表序号定位，避免应用启动/退出导致序号错位。
        let root = if req.scope.pid.is_some() {
            return DesktopResult::Err(DesktopError::BackendUnavailable {
                reason: "AT-SPI 后端暂不支持按 pid 定位，请用 window 或 app_name".to_string(),
            });
        } else if let Some(window_ref) = &req.scope.window {
            match cached_window_object.as_ref() {
                Some(obj_ref) => match obj_ref.as_accessible_proxy(zbus).await {
                    Ok(proxy) => proxy,
                    Err(_) => {
                        return DesktopResult::Err(DesktopError::StaleElement {
                            snapshot: window_ref.snapshot,
                        });
                    }
                },
                None => {
                    return DesktopResult::Err(DesktopError::StaleElement {
                        snapshot: window_ref.snapshot,
                    });
                }
            }
        } else {
            let app_name = match req.scope.app_name.as_deref() {
                Some(n) => n,
                None => {
                    return DesktopResult::Err(DesktopError::ApplicationNotFound {
                        query: "AT-SPI snapshot 需要指定 window 或 app_name".to_string(),
                    });
                }
            };
            let needle = app_name.to_lowercase();
            let mut matches: Vec<String> = Vec::new();
            let mut matched = None;
            for child_ref in &children {
                if let Ok(proxy) = child_ref.as_accessible_proxy(zbus).await {
                    let name = proxy.name().await.unwrap_or_default();
                    if name.to_lowercase().contains(&needle) {
                        matches.push(name);
                        if matched.is_none() {
                            matched = Some(proxy);
                        }
                    }
                }
            }
            if matches.len() > 1 {
                return DesktopResult::Err(DesktopError::AmbiguousMatch {
                    candidates: matches,
                });
            }
            match matched {
                Some(p) => p,
                None => {
                    return DesktopResult::Err(DesktopError::ApplicationNotFound {
                        query: app_name.to_string(),
                    });
                }
            }
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
        let mut nodes = Vec::new();
        let mut truncated = false;
        let warnings = Vec::new();
        self.walk(
            &root,
            zbus,
            snapshot,
            0,
            max_depth,
            max_nodes,
            req.include_invisible,
            None,
            &mut nodes,
            &mut truncated,
        )
        .await;
        // 先构建完成，再登记版本与提交节点表。
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
        let conn = match self.connect().await {
            Ok(c) => c,
            Err(e) => return DesktopResult::Err(e),
        };
        let zbus = conn.connection();
        // 从缓存取回 ObjectRef 并还原 AccessibleProxy。
        let object_ref = {
            let guard = self.elements.read().unwrap();
            guard
                .get(&(req.element.snapshot, req.element.id.clone()))
                .cloned()
        };
        let object_ref = match object_ref {
            Some(r) => r,
            None => {
                return DesktopResult::Err(DesktopError::StaleElement {
                    snapshot: req.element.snapshot,
                });
            }
        };
        let element = match object_ref.as_accessible_proxy(zbus).await {
            Ok(p) => p,
            Err(_) => {
                return DesktopResult::Err(DesktopError::StaleElement {
                    snapshot: req.element.snapshot,
                });
            }
        };
        // 动作前重新确认控件身份（role + name 一致）。
        let identity_node = self
            .snapshot_nodes
            .read()
            .unwrap()
            .get(&req.element.snapshot)
            .and_then(|nodes| {
                nodes
                    .iter()
                    .find(|n| {
                        n.element.id == req.element.id && n.element.snapshot == req.element.snapshot
                    })
                    .cloned()
            });
        let identity_ok = match identity_node {
            Some(node) => Self::verify_identity(&element, &node).await,
            None => false,
        };
        if !identity_ok {
            return DesktopResult::Err(DesktopError::StaleElement {
                snapshot: req.element.snapshot,
            });
        }
        // 按动作类型调用对应接口。
        let action_kind = ActionKind::from(req.action);
        let interfaces = element
            .get_interfaces()
            .await
            .unwrap_or_else(|_| InterfaceSet::empty());
        let result: Result<(), String> = match action_kind {
            ActionKind::Focus => {
                if !interfaces.contains(Interface::Component) {
                    Err("控件不支持 Component 接口（focus）".to_string())
                } else {
                    match build_proxy::<ComponentProxy<'_>>(&element).await {
                        Ok(p) => check_bool(p.grab_focus().await, "grab_focus 被目标拒绝"),
                        Err(e) => Err(e.to_string()),
                    }
                }
            }
            ActionKind::Press | ActionKind::Toggle | ActionKind::Expand | ActionKind::Collapse => {
                if !interfaces.contains(Interface::Action) {
                    Err("控件不支持 Action 接口".to_string())
                } else {
                    match build_proxy::<ActionProxy<'_>>(&element).await {
                        Ok(p) => {
                            // 执行时检查所有探测时映射的动作名（与 detect_actions 一致）。
                            let needles: &[&str] = match action_kind {
                                ActionKind::Press => &["click", "press", "activate", "jump"],
                                ActionKind::Toggle => &["toggle"],
                                ActionKind::Expand => &["expand"],
                                ActionKind::Collapse => &["collapse"],
                                _ => &["click"],
                            };
                            match p.get_actions().await {
                                Ok(acts) => {
                                    let idx = acts.iter().position(|a| {
                                        let lower = a.name.to_lowercase();
                                        needles.iter().any(|n| lower.contains(n))
                                    });
                                    match idx {
                                        Some(i) => check_bool(
                                            p.do_action(i as i32).await,
                                            "do_action 被目标拒绝",
                                        ),
                                        None => Err(format!("控件未暴露 {:?} 对应的动作", needles)),
                                    }
                                }
                                Err(e) => Err(e.to_string()),
                            }
                        }
                        Err(e) => Err(e.to_string()),
                    }
                }
            }
            ActionKind::SetValue => match &req.value {
                Some(v) if interfaces.contains(Interface::EditableText) => {
                    match build_proxy::<EditableTextProxy<'_>>(&element).await {
                        Ok(p) => {
                            check_bool(p.set_text_contents(v).await, "set_text_contents 被目标拒绝")
                        }
                        Err(e) => Err(e.to_string()),
                    }
                }
                Some(_) => Err("控件不支持 EditableText 接口（set_value）".to_string()),
                None => Err("set_value 缺少 value 参数".to_string()),
            },
            ActionKind::Select => Err("__select_unsupported__".to_string()),
            ActionKind::ScrollIntoView => {
                if !interfaces.contains(Interface::Component) {
                    Err("控件不支持 Component 接口（scroll）".to_string())
                } else {
                    match build_proxy::<ComponentProxy<'_>>(&element).await {
                        Ok(p) => check_bool(
                            p.scroll_to(atspi::ScrollType::Anywhere).await,
                            "scroll_to 被目标拒绝",
                        ),
                        Err(e) => Err(e.to_string()),
                    }
                }
            }
        };
        match result {
            Ok(()) => DesktopResult::Ok(ActionResult {
                performed: true,
                summary: format!("已执行 {action_kind:?}"),
                new_window: None,
            }),
            Err(e) if e == "__select_unsupported__" => {
                DesktopResult::Err(DesktopError::ActionNotSupported {
                    action: "select".to_string(),
                    supported: vec![],
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
                    let list_req = ListWindowsRequest::default();
                    let exists = match self.list_windows(&list_req).await {
                        DesktopResult::Ok(r) => r.windows.iter().any(|w| {
                            target.app_name.as_deref().is_some_and(|n| {
                                w.app_name.to_lowercase().contains(&n.to_lowercase())
                            }) || target
                                .title
                                .as_deref()
                                .is_some_and(|t| w.title.to_lowercase().contains(&t.to_lowercase()))
                        }),
                        DesktopResult::Err(e) => return DesktopResult::Err(e),
                    };
                    if looking_appear == exists {
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
            _ => {
                // 控件级等待：开始时从缓存还原 ObjectRef（取不到返回 stale），
                // 循环中属性读取失败视为"条件未满足"继续等待，而非立即 stale——
                // 因为等待场景可能包含控件短暂不可访问后又恢复的情况。
                let element_ref = match &req.condition {
                    WaitCondition::Focus { element }
                    | WaitCondition::Available { element }
                    | WaitCondition::Value { element, .. } => element.clone(),
                    _ => unreachable!(),
                };
                let conn = match self.connect().await {
                    Ok(c) => c,
                    Err(e) => return DesktopResult::Err(e),
                };
                let zbus = conn.connection();
                let object_ref = {
                    let guard = self.elements.read().unwrap();
                    guard
                        .get(&(element_ref.snapshot, element_ref.id.clone()))
                        .cloned()
                };
                let object_ref = match object_ref {
                    Some(r) => r,
                    None => {
                        return DesktopResult::Err(DesktopError::StaleElement {
                            snapshot: element_ref.snapshot,
                        });
                    }
                };
                let element = match object_ref.as_accessible_proxy(zbus).await {
                    Ok(p) => p,
                    Err(_) => {
                        return DesktopResult::Err(DesktopError::StaleElement {
                            snapshot: element_ref.snapshot,
                        });
                    }
                };
                // 等待开始前重新确认控件仍与原快照身份一致。
                let identity_node = self
                    .snapshot_nodes
                    .read()
                    .unwrap()
                    .get(&element_ref.snapshot)
                    .and_then(|nodes| nodes.iter().find(|n| n.element == element_ref).cloned());
                let identity_ok = match identity_node {
                    Some(node) => Self::verify_identity(&element, &node).await,
                    None => false,
                };
                if !identity_ok {
                    return DesktopResult::Err(DesktopError::StaleElement {
                        snapshot: element_ref.snapshot,
                    });
                }
                let expected_value = match &req.condition {
                    WaitCondition::Value { expected, .. } => expected.clone(),
                    _ => None,
                };
                // 无期望值时必须成功读取初始值，否则无法可靠判断变化。
                let initial_value = if expected_value.is_none()
                    && matches!(req.condition, WaitCondition::Value { .. })
                {
                    match read_text_value_result(&element).await {
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
                    let state: Result<bool, String> =
                        match &req.condition {
                            WaitCondition::Focus { .. } => element
                                .get_state()
                                .await
                                .map(|s| s.contains(State::Focused))
                                .map_err(|e| e.to_string()),
                            WaitCondition::Available { .. } => element
                                .get_state()
                                .await
                                .map(|s| s.contains(State::Enabled))
                                .map_err(|e| e.to_string()),
                            WaitCondition::Value { .. } => read_text_value_result(&element)
                                .await
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
                                snapshot: element_ref.snapshot,
                            });
                        }
                    };
                    if satisfied {
                        return DesktopResult::Ok(WaitResult {
                            satisfied: true,
                            waited_ms: start.elapsed().as_millis() as u64,
                            matched_element: Some(element_ref.clone()),
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
}

impl LinuxBackend {
    /// 递归读取控件树（前序遍历），同时缓存 ObjectRef 供 action/wait 还原。
    #[allow(clippy::too_many_arguments)]
    async fn walk(
        &self,
        element: &AccessibleProxy<'_>,
        conn: &atspi::zbus::Connection,
        snapshot: u64,
        depth: u32,
        max_depth: u32,
        max_nodes: usize,
        include_invisible: bool,
        parent_id: Option<String>,
        nodes: &mut Vec<ControlNode>,
        truncated: &mut bool,
    ) {
        if nodes.len() >= max_nodes || depth > max_depth {
            *truncated = true;
            return;
        }
        let role = element.get_role().await.unwrap_or(Role::Unknown);
        let name = element.name().await.unwrap_or_default();
        let state = element.get_state().await.unwrap_or_default();
        let visible = state.contains(State::Visible);
        if !include_invisible && !visible {
            return;
        }
        let enabled = state.contains(State::Enabled);
        let focused = state.contains(State::Focused);
        let interfaces = element
            .get_interfaces()
            .await
            .unwrap_or_else(|_| InterfaceSet::empty());
        // 探测真实支持的动作。
        let actions = Self::detect_actions(element, &interfaces).await;
        // 读 Text 接口的值（敏感控件不返回）。
        let sensitive = matches!(role, Role::PasswordText);
        let value = if sensitive {
            None
        } else {
            read_text_value(element).await
        };
        // 读 Component 接口的边界。
        let bounds = read_bounds(element, &interfaces).await;

        let id = format!("atspi-{snapshot}-{}", nodes.len());
        let parent_index = nodes.len();
        // 缓存 ObjectRef（从 AccessibleProxy 转换）。
        if let Ok(obj_ref) = atspi::ObjectRefOwned::try_from(element) {
            self.elements
                .write()
                .unwrap()
                .insert((snapshot, id.clone()), obj_ref);
        }
        nodes.push(ControlNode {
            element: ElementRef {
                id: id.clone(),
                snapshot,
            },
            role: format!("{role:?}"),
            name,
            identifiers: StableIdentifiers {
                automation_id: None,
                role: Some(format!("{role:?}")),
            },
            value,
            sensitive,
            visible,
            enabled,
            focused,
            bounds,
            actions,
            parent: parent_id.map(|pid| ElementRef { id: pid, snapshot }),
            children: Vec::new(),
        });

        // 递归子节点。
        let children = match element.get_children().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut child_refs = Vec::with_capacity(children.len());
        for child_ref in children {
            let child = match child_ref.as_accessible_proxy(conn).await {
                Ok(p) => p,
                Err(_) => continue,
            };
            let before = nodes.len();
            Box::pin(self.walk(
                &child,
                conn,
                snapshot,
                depth + 1,
                max_depth,
                max_nodes,
                include_invisible,
                Some(id.clone()),
                nodes,
                truncated,
            ))
            .await;
            if nodes.len() > before {
                child_refs.push(nodes[before].element.clone());
            }
        }
        if let Some(node) = nodes.get_mut(parent_index) {
            node.children = child_refs;
        }
    }
}

/// 检查 AT-SPI 布尔返回：Ok(true) 成功，Ok(false) 目标拒绝，Err 转错误字符串。
fn check_bool(r: Result<bool, atspi::zbus::Error>, rejected_msg: &str) -> Result<(), String> {
    match r {
        Ok(true) => Ok(()),
        Ok(false) => Err(rejected_msg.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// 从 AccessibleProxy 构造指定 interface 的 proxy（复用 path/destination）。
/// 所有 proxy 借用 AccessibleProxy 内部的 connection（同生命周期）。
async fn build_proxy<'a, T>(element: &'a AccessibleProxy<'a>) -> Result<T, atspi::zbus::Error>
where
    T: atspi::zbus::proxy::ProxyImpl<'a> + From<atspi::zbus::Proxy<'a>>,
{
    let inner = element.inner();
    let conn = inner.connection();
    let dest = inner.destination().as_str().to_string();
    let path = inner.path().as_str().to_string();
    T::builder(conn)
        .destination(dest)?
        .path(path)?
        .build()
        .await
}

/// 从 Text 接口读取控件文本值（非 Text 控件返回 None）。
async fn read_text_value(element: &AccessibleProxy<'_>) -> Option<String> {
    read_text_value_result(element).await.ok()
}

/// 严格读取 Text 接口，用于等待逻辑区分“未满足”和“对象读取失败”。
async fn read_text_value_result(element: &AccessibleProxy<'_>) -> Result<String, String> {
    let text_proxy = build_proxy::<TextProxy<'_>>(element)
        .await
        .map_err(|e| e.to_string())?;
    let count = text_proxy
        .character_count()
        .await
        .map_err(|e| e.to_string())?;
    if count <= 0 {
        return Ok(String::new());
    }
    text_proxy
        .get_text(0, count)
        .await
        .map_err(|e| e.to_string())
}

/// 从 Component 接口读取控件边界。
async fn read_bounds(element: &AccessibleProxy<'_>, interfaces: &InterfaceSet) -> Bounds {
    if !interfaces.contains(Interface::Component) {
        return Bounds::default();
    }
    let Ok(component) = build_proxy::<ComponentProxy<'_>>(element).await else {
        return Bounds::default();
    };
    let (x, y) = component
        .get_position(atspi::CoordType::Screen)
        .await
        .unwrap_or((0, 0));
    let (width, height) = component.get_size().await.unwrap_or((0, 0));
    Bounds {
        x: x as f64,
        y: y as f64,
        width: width as f64,
        height: height as f64,
    }
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
