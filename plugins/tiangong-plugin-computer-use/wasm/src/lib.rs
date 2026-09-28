//! Computer Use 插件的 WASM 桥接组件。
//!
//! 本组件只做桥接：声明六个桌面控制工具、解析参数、注入提示词与生命周期入口；
//! 真实无障碍访问经 `sidecar.invoke` 转发到 sidecar 进程（Windows UI Automation、
//! macOS AXUIElement、Linux AT-SPI2 由 sidecar 按平台实现）。wasm 侧不做任何
//! 系统调用。

mod bindings;
mod sidecar_client;

use bindings::exports::tiangong::plugin::plugin::{
    Guest, PluginDescriptor, PluginError, ToolCall, ToolResult, ToolSpec,
};
use bindings::exports::tiangong::plugin::plugin_ui::{
    Contribution, Guest as UiGuest, ResourceResponse, ViewMessageRequest, ViewMessageResponse,
    ViewResponse,
};
use serde::Serialize;
use serde_json::json;
use tiangong_plugin_computer_use_protocol::ops::{
    Action, ActionRequest, ActionRequestKind, DesktopStatus, DesktopStatusRequest, Find,
    FindConditions, FindRequest, Keyboard, KeyboardActionKind, KeyboardRequest, ListWindows,
    ListWindowsRequest, Mouse, MouseGesture, MouseRequest, OpenApp, OpenAppRequest,
    RestoreHostWindow, Screenshot, ScreenshotRequest, SetAccess, SetAccessRequest, Snapshot,
    SnapshotRequest, SplitStateQuery, SplitTurn, SplitTurnRequest, VirtualCursor,
    VirtualCursorRequest, Wait, WaitRequest,
};
use tiangong_plugin_computer_use_protocol::{
    Ack, Bounds, ComputerUseOperation, DesktopResult, ElementRef, MatchMode, TOOL_DESKTOP_APP,
    TOOL_DESKTOP_INPUT, TOOL_DESKTOP_SCREENSHOT, TOOL_DESKTOP_UI, TOOL_DESKTOP_WAIT,
};

mod descriptor {
    pub const ID: &str = tiangong_plugin_computer_use_protocol::PLUGIN_ID;
    pub const NAME: &str = "Computer Use";
    pub const VERSION: &str = tiangong_plugin_computer_use_protocol::PLUGIN_VERSION;
}

/// 全局状态缓存（WASM 单线程，RefCell 安全）。
mod state {
    use std::cell::RefCell;

    struct PluginState {
        full_trust: bool,
        /// 本轮是否执行过鼠标手势（天工指针分身已出现），轮次结束据此收起。
        cursor_summoned: bool,
        /// 本轮是否触发过自动分屏，轮次结束据此显示「恢复窗口」按钮。
        split_used: bool,
    }

    thread_local! {
        static STATE: RefCell<PluginState> = const {
            RefCell::new(PluginState { full_trust: false, cursor_summoned: false, split_used: false })
        };
    }

    pub fn set_full_trust(full_trust: bool) {
        STATE.with(|s| s.borrow_mut().full_trust = full_trust);
    }

    /// 取出并清零本轮分屏标记。
    pub fn take_split_used() -> bool {
        STATE.with(|s| std::mem::take(&mut s.borrow_mut().split_used))
    }

    pub fn mark_split_used() {
        STATE.with(|s| s.borrow_mut().split_used = true);
    }

    pub fn cursor_summoned() -> bool {
        STATE.with(|s| s.borrow().cursor_summoned)
    }

    pub fn set_cursor_summoned(value: bool) {
        STATE.with(|s| s.borrow_mut().cursor_summoned = value);
    }

    pub fn access_context() -> tiangong_plugin_computer_use_protocol::AccessContext {
        STATE.with(|s| tiangong_plugin_computer_use_protocol::AccessContext {
            full_trust: s.borrow().full_trust,
        })
    }
}

fn plugin_err(message: impl Into<String>) -> PluginError {
    PluginError::Message(message.into())
}

struct Component;

impl Guest for Component {
    fn describe() -> Result<PluginDescriptor, PluginError> {
        Ok(PluginDescriptor {
            id: descriptor::ID.to_string(),
            name: descriptor::NAME.to_string(),
            version: descriptor::VERSION.to_string(),
        })
    }

    fn tool_specs() -> Result<Vec<ToolSpec>, PluginError> {
        Ok(vec![
            ToolSpec {
                name: TOOL_DESKTOP_APP.to_string(),
                description: "应用与窗口管理。action=open：唤起或启动应用（首选；已运行则取消隐藏/激活/恢复最小化并置前，未运行经系统搜索定位后启动，支持中文本地化名如「微信」），返回前台窗口屏幕坐标 window，可直接作为 desktop_screenshot 的 region；action=list：列出运行中的应用（可按 app_name/pid/foreground_only 筛选）；action=status：查询平台、图形会话与辅助功能授权（仅在其他工具报权限错误时需要）。"
                    .to_string(),
                input_schema: schema_string(json!({
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["open", "list", "status"], "description": "open 唤起/启动；list 列举应用；status 能力探测" },
                        "app_name": { "type": "string", "description": "open：应用名（显示名/本地化名/文件名）；list：按名称筛选" },
                        "bundle_id": { "type": "string", "description": "open：macOS Bundle ID（如 com.tencent.xinWeChat），优先于 app_name；Windows 上视作可执行文件名" },
                        "pid": { "type": "integer", "description": "list：按进程号筛选", "minimum": 0 },
                        "foreground_only": { "type": "boolean", "description": "list：仅前台应用" }
                    },
                    "required": ["action"]
                })),
            },
            ToolSpec {
                name: TOOL_DESKTOP_SCREENSHOT.to_string(),
                description: "截屏并自动注入对话供你直接阅读（无需 OCR）。范围优先级：region 显式区域 > app_name/pid/foreground_only 应用窗口 > 主显示器全屏。产物为 JPEG，按屏幕逻辑尺寸输出（1 图片像素 = 1 point）；逻辑长边超过 max_dimension（默认 1568）时不裁剪，按 1/2、1/4… 整数倍缩小。结果给出 logical_bounds（原图屏幕区域）与 scale：屏幕坐标 = logical_bounds.x/y + 图片坐标 × scale。需要精确点击时对目标区域截小图（scale=1）。macOS 需要屏幕录制授权。"
                    .to_string(),
                input_schema: schema_string(json!({
                    "type": "object",
                    "properties": {
                        "app_name": { "type": "string", "description": "截取该应用最前窗口（包含匹配）" },
                        "pid": { "type": "integer", "description": "截取该进程最前窗口", "minimum": 0 },
                        "foreground_only": { "type": "boolean", "description": "截取前台应用窗口" },
                        "region": region_schema("显式区域（屏幕逻辑坐标 points，优先于 app 定位）"),
                        "max_dimension": { "type": "integer", "description": "产物长边上限（像素，默认 1568）；超过时按 1/2 整数倍缩小，不裁剪", "minimum": 1 }
                    }
                })),
            },
            ToolSpec {
                name: TOOL_DESKTOP_INPUT.to_string(),
                description: "真实输入（系统级键鼠合成：macOS CGEvent / Windows SendInput，行为与用户手动操作一致），点击、输入等交互的首选路径，适用于 Canvas/Qt 自绘等无障碍树外的界面。鼠标：click/double_click/right_click/move/drag/scroll，坐标为屏幕逻辑坐标（由截图 logical_bounds 与 scale 换算）；点击自带平滑移动与到位停顿，无需先 move。首次鼠标操作时天工指针从系统鼠标位置分身出现并演示操作，本轮结束自动收起。键盘：type 输入任意文本（中文不经输入法）、key 按单键、combo 组合键（修饰键在前），先点击建立焦点。screenshot_after=true 时操作完成后自动截图，省去一次单独截图。"
                    .to_string(),
                input_schema: schema_string(json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["click", "double_click", "right_click", "move", "drag", "scroll", "type", "key", "combo"],
                            "description": "鼠标手势或键盘动作"
                        },
                        "x": { "type": "number", "description": "鼠标动作必填：目标点 X（屏幕逻辑坐标）" },
                        "y": { "type": "number", "description": "鼠标动作必填：目标点 Y" },
                        "to_x": { "type": "number", "description": "drag 必填：终点 X" },
                        "to_y": { "type": "number", "description": "drag 必填：终点 Y" },
                        "delta_y": { "type": "number", "description": "scroll：垂直滚动像素，正=向下（与 delta_x 至少一个）" },
                        "delta_x": { "type": "number", "description": "scroll：水平滚动像素，正=向右" },
                        "text": { "type": "string", "description": "type 必填：要输入的文本" },
                        "key": { "type": "string", "description": "key 必填：键名（enter/esc/tab/pageup/方向键/字母数字等，别名不敏感）" },
                        "keys": { "type": "array", "items": { "type": "string" }, "description": "combo 必填：键名数组，修饰键在前，如 [\"cmd\",\"c\"]（Windows 上 cmd 等同 ctrl，徽标键用 win）" },
                        "screenshot_after": { "type": "boolean", "description": "操作后自动截图（默认 false）" },
                        "screenshot_region": region_schema("screenshot_after 的截图区域（缺省按 screenshot_app_name 或全屏）"),
                        "screenshot_app_name": { "type": "string", "description": "screenshot_after 时截取该应用窗口" }
                    },
                    "required": ["action"]
                })),
            },
            ToolSpec {
                name: TOOL_DESKTOP_UI.to_string(),
                description: "无障碍控件树（适合原生控件；微信等自绘界面通常只暴露菜单栏，此时改用截图 + desktop_input）。action=snapshot：读取应用控件树，可带 conditions 只返回匹配控件（传 snapshot 则在已有快照内查找）；action=perform：对快照内控件引用执行语义动作（press/focus/set_value/toggle/select），引用只在所属快照内有效。同名控件多个候选时不得默认操作第一个。"
                    .to_string(),
                input_schema: schema_string(json!({
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["snapshot", "perform"] },
                        "app_name": { "type": "string", "description": "snapshot：按应用名定位（多进程重名时报歧义并列出 pid）" },
                        "pid": { "type": "integer", "description": "snapshot：按进程号定位", "minimum": 0 },
                        "window": element_ref_schema(),
                        "snapshot": { "type": "integer", "description": "snapshot + conditions：在已有快照版本内查找，不重新读取", "minimum": 0 },
                        "conditions": {
                            "type": "object",
                            "description": "snapshot：查找条件（提供时只返回匹配控件）",
                            "properties": {
                                "automation_id": { "type": "string" },
                                "role": { "type": "string" },
                                "name": { "type": "string" },
                                "value": { "type": "string" },
                                "visible": { "type": "boolean" },
                                "enabled": { "type": "boolean" },
                                "focused": { "type": "boolean" },
                                "mode": { "type": "string", "enum": ["exact", "contains"], "default": "exact" }
                            }
                        },
                        "max_depth": { "type": "integer", "description": "snapshot：最大遍历深度，0 使用默认", "minimum": 0 },
                        "max_nodes": { "type": "integer", "description": "snapshot：最大节点数，0 使用默认", "minimum": 0 },
                        "include_invisible": { "type": "boolean", "description": "snapshot：包含不可见控件" },
                        "max_candidates": { "type": "integer", "description": "snapshot + conditions：最大候选数", "minimum": 0 },
                        "element": element_ref_schema(),
                        "operation": {
                            "type": "string",
                            "enum": ["press", "focus", "set_value", "toggle", "select", "expand", "collapse", "scroll_into_view"],
                            "description": "perform 必填：语义动作"
                        },
                        "value": { "type": "string", "description": "perform set_value 的值" },
                        "selection": { "type": "string", "description": "perform select 的选项标识" }
                    },
                    "required": ["action"]
                })),
            },
            ToolSpec {
                name: TOOL_DESKTOP_WAIT.to_string(),
                description: "等待状态变化（有明确超时）：appear/disappear 按 target 应用名判定屏幕上是否有其可见窗口；focus/available/value 按控件引用判定。"
                    .to_string(),
                input_schema: schema_string(json!({
                    "type": "object",
                    "properties": {
                        "condition": {
                            "type": "object",
                            "description": "等待条件。appear/disappear 使用 target；focus/available/value 使用 element；value 可带 expected",
                            "properties": {
                                "kind": { "type": "string", "enum": ["appear", "disappear", "focus", "available", "value"] },
                                "target": {
                                    "type": "object",
                                    "properties": {
                                        "app_name": { "type": "string" },
                                        "title": { "type": "string" }
                                    }
                                },
                                "element": element_ref_schema(),
                                "expected": { "type": "string" }
                            },
                            "required": ["kind"]
                        },
                        "timeout_ms": { "type": "integer", "description": "超时毫秒数，必须大于 0", "minimum": 1 }
                    },
                    "required": ["condition", "timeout_ms"]
                })),
            },
        ])
    }

    fn prompt_sections() -> Result<Vec<String>, PluginError> {
        Ok(vec![
            "桌面应用的交互一律使用 computer-use 插件（5 个工具）：唤起窗口、看界面、点击输入都属本插件职责。终端沙箱无法与图形界面交互（osascript、open -a、screencapture 均不可行），不要在终端里尝试。典型流程：desktop_app(action=open) 把目标应用唤起到前台（未运行会自动启动，返回窗口坐标）→ desktop_screenshot 截取窗口看界面 → desktop_input 点击/输入（可带 screenshot_after 直接看到结果）。原生控件可用 desktop_ui 读取控件树并执行语义动作；需要等界面变化时用 desktop_wait。权限问题会在工具失败时直接报告，无需预先查询状态。网页内容继续优先交给浏览器插件。".to_string(),
            "坐标换算：截图结果给出 logical_bounds（原图对应的屏幕区域；macOS 为 points，Windows 为物理像素，均与鼠标坐标同系）与 scale，屏幕坐标 = logical_bounds.x + 图片x × scale（y 同理）。先截窗口或全屏粗定位，需要精确点击时对目标附近区域截小图（scale=1，图片坐标加区域起点即屏幕坐标）。滚动聊天记录等内容时先确保鼠标位于内容区域内再 scroll，无效时改用 key=pageup/pagedown。".to_string(),
        ])
    }

    fn handle_tool(call: ToolCall) -> Result<ToolResult, PluginError> {
        match call.name.as_str() {
            TOOL_DESKTOP_APP => handle_desktop_app(call.arguments),
            TOOL_DESKTOP_SCREENSHOT => handle_screenshot(call.arguments),
            TOOL_DESKTOP_INPUT => handle_desktop_input(call.arguments),
            TOOL_DESKTOP_UI => handle_desktop_ui(call.arguments),
            TOOL_DESKTOP_WAIT => handle_wait(call.arguments),
            other => Err(plugin_err(format!("未知的 Computer Use 工具: {other}"))),
        }
    }

    fn shutdown() -> Result<(), PluginError> {
        Ok(())
    }

    fn set_workspace(_workspace: Option<String>, full_trust: bool) -> Result<(), PluginError> {
        state::set_full_trust(full_trust);
        let request = SetAccessRequest { full_trust };
        sidecar_client::invoke::<SetAccess>(&request)
            .map_err(|error| plugin_err(format!("set_access 调用 sidecar 失败: {error}")))?;
        Ok(())
    }

    fn on_config_updated(_config_json: String) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_session_ready(_session_json: String) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_turn_started(session_json: String, _turn_start_idx: u32) -> Result<(), PluginError> {
        // 新一轮开始：清掉上轮残留的分屏标记，并隐藏该会话的恢复按钮。
        // 仅在分屏生效时才需要通知 sidecar，但 wasm 侧不掌握该状态；调用
        // 很轻量，失败静默（按钮状态是纯 UI 增益）。
        state::take_split_used();
        if let Some(session_id) = session_id_of(&session_json) {
            let _ = sidecar_client::invoke::<SplitTurn>(&SplitTurnRequest {
                session_id,
                finished: false,
                used_split: false,
            });
        }
        Ok(())
    }

    fn on_turn_finished(session_json: String, _turn_start_idx: u32) -> Result<(), PluginError> {
        // 本轮结束收起天工指针分身（未出现时为无操作）。失败静默：
        // 指针是纯可视化，sidecar 未启动时无需拉起，overlay 也有空闲兜底。
        if state::cursor_summoned() {
            state::set_cursor_summoned(false);
            let _ =
                sidecar_client::invoke::<VirtualCursor>(&VirtualCursorRequest { enabled: false });
        }
        // 对话完成：本会话用过自动分屏时显示「恢复窗口」按钮。
        let used_split = state::take_split_used();
        if let Some(session_id) = session_id_of(&session_json) {
            let _ = sidecar_client::invoke::<SplitTurn>(&SplitTurnRequest {
                session_id,
                finished: true,
                used_split,
            });
        }
        Ok(())
    }

    fn on_session_ended(_session_json: String) -> Result<(), PluginError> {
        Ok(())
    }
}

// ── 工具处理 ───────────────────────────────────────────────────

/// desktop_app：status / list / open 分派。
fn handle_desktop_app(arguments: String) -> Result<ToolResult, PluginError> {
    let args = match parse_args("desktop_app", &arguments) {
        Ok(v) => v,
        Err(f) => return Ok(f),
    };
    match args.get("action").and_then(serde_json::Value::as_str) {
        Some("open") => handle_open_app(arguments),
        Some("list") => handle_list_windows(arguments),
        Some("status") => handle_desktop_status(arguments),
        other => Ok(tool_failure(
            &format!(
                "desktop_app 的 action 需为 open/list/status，收到：{}",
                other.unwrap_or("（缺失）")
            ),
            "bad action",
        )),
    }
}

/// desktop_ui：snapshot（可带 conditions 查找）/ perform 分派。
fn handle_desktop_ui(arguments: String) -> Result<ToolResult, PluginError> {
    let mut args = match parse_args("desktop_ui", &arguments) {
        Ok(v) => v,
        Err(f) => return Ok(f),
    };
    let action = args
        .get("action")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    match action.as_deref() {
        Some("snapshot") => {
            let has_conditions = args
                .get("conditions")
                .is_some_and(|c| c.as_object().is_some_and(|o| !o.is_empty()));
            if !has_conditions {
                return handle_snapshot(arguments);
            }
            // 带条件查找：未给快照版本/窗口引用时先按 app_name/pid 取一次
            // 快照，再在该快照内查找（底层 find 只接受 snapshot 或 window）。
            let has_scope = args.get("snapshot").is_some_and(|v| !v.is_null())
                || args.get("window").is_some_and(|v| !v.is_null());
            if !has_scope {
                let snapshot_result = handle_snapshot(arguments)?;
                if !snapshot_result.ok {
                    return Ok(snapshot_result);
                }
                let snapshot_id =
                    serde_json::from_str::<serde_json::Value>(&snapshot_result.stdout)
                        .ok()
                        .and_then(|v| v.get("snapshot").and_then(serde_json::Value::as_u64));
                let Some(snapshot_id) = snapshot_id else {
                    return Ok(tool_failure(
                        "desktop_ui 读取快照成功但未返回快照版本，无法查找",
                        "missing snapshot id",
                    ));
                };
                if let Some(object) = args.as_object_mut() {
                    object.insert("snapshot".to_string(), json!(snapshot_id));
                }
                return handle_find(args.to_string());
            }
            handle_find(arguments)
        }
        Some("perform") => {
            // 语义动作字段名 operation → 底层 action。
            let Some(operation) = args.get("operation").cloned() else {
                return Ok(tool_failure(
                    "desktop_ui perform 需要 operation（press/focus/set_value/toggle/select）",
                    "missing operation",
                ));
            };
            if let Some(object) = args.as_object_mut() {
                object.insert("action".to_string(), operation);
            }
            handle_action(args.to_string())
        }
        other => Ok(tool_failure(
            &format!(
                "desktop_ui 的 action 需为 snapshot/perform，收到：{}",
                other.unwrap_or("（缺失）")
            ),
            "bad action",
        )),
    }
}

/// desktop_input：鼠标手势 / 键盘动作分派，可选操作后自动截图。
fn handle_desktop_input(arguments: String) -> Result<ToolResult, PluginError> {
    let args = match parse_args("desktop_input", &arguments) {
        Ok(v) => v,
        Err(f) => return Ok(f),
    };
    let action = args
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let mut result = if let Some(gesture) = parse_mouse_gesture(action) {
        let result = run_mouse(&args, gesture)?;
        if result.ok {
            state::set_cursor_summoned(true);
        }
        result
    } else if let Some(kind) = parse_keyboard_action(action) {
        run_keyboard(&args, kind)?
    } else {
        return Ok(tool_failure(
            &format!(
                "desktop_input 的 action 无法识别：{}（鼠标 click/double_click/right_click/move/drag/scroll，键盘 type/key/combo）",
                if action.is_empty() {
                    "（缺失）"
                } else {
                    action
                }
            ),
            "bad action",
        ));
    };
    let wants_shot = args
        .get("screenshot_after")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !result.ok || !wants_shot {
        return Ok(result);
    }
    // 操作后自动截图：留 300ms 给界面刷新；截图注入声明在 stdout JSON 的
    // injected_assets 字段，与操作结果合并为一个 JSON 对象。
    let region = args.get("screenshot_region").and_then(parse_region);
    let shot_request = ScreenshotRequest {
        app_name: args.get("screenshot_app_name").and_then(as_str_owned),
        region,
        delay_ms: Some(300),
        access: state::access_context(),
        ..Default::default()
    };
    let shot = sidecar_client::invoke::<Screenshot>(&shot_request)
        .map_err(|e| plugin_err(format!("desktop_input 截图调用 sidecar 失败: {e}")))?;
    match shot {
        DesktopResult::Ok(resp) => {
            let operation: serde_json::Value =
                serde_json::from_str(&result.stdout).unwrap_or(serde_json::Value::Null);
            let mut merged = serde_json::to_value(&resp).unwrap_or_else(|_| json!({}));
            if let Some(object) = merged.as_object_mut() {
                object.insert("operation".to_string(), operation);
            }
            result.stdout = serde_json::to_string_pretty(&merged).unwrap_or_default();
            result.summary = format!(
                "{}；已自动截图（{}×{}）。{}",
                operation_summary(&result.summary, &merged),
                resp.width,
                resp.height,
                resp.coordinate_hint().unwrap_or_default()
            );
        }
        DesktopResult::Err(error) => {
            result.summary = format!(
                "{}；操作后截图失败：{}",
                result.summary,
                error.agent_message()
            );
        }
    }
    Ok(result)
}

/// 操作摘要：优先取操作响应里的 summary 字段（「已在 (x,y) 执行左键点击」）。
fn operation_summary(fallback: &str, merged: &serde_json::Value) -> String {
    merged
        .get("operation")
        .and_then(|op| op.get("summary"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

fn handle_desktop_status(arguments: String) -> Result<ToolResult, PluginError> {
    // desktop_status 无必填参数，但仍校验 JSON 合法性，非法输入返回错误。
    if let Err(f) = parse_args("desktop_status", &arguments) {
        return Ok(f);
    }
    let request = DesktopStatusRequest {
        access: state::access_context(),
    };
    run_desktop_op::<DesktopStatus, _>(&request, "desktop_status")
}

fn handle_list_windows(arguments: String) -> Result<ToolResult, PluginError> {
    let args = match parse_args("desktop_list_windows", &arguments) {
        Ok(v) => v,
        Err(f) => return Ok(f),
    };
    // 工具参数层用 0 表示“不筛选”；协议层则用 None 表示。先归一化，
    // 避免把 pid=0 下传后过滤掉全部窗口。
    let pid = args
        .get("pid")
        .and_then(as_u32_bounded)
        .filter(|pid| *pid > 0);
    // 非零 pid 超范围（超 u32）时返回参数错误，而非截断成另一个有效值。
    if args
        .get("pid")
        .is_some_and(|value| value.as_u64().is_none_or(|pid| pid > u32::MAX as u64))
    {
        return Ok(tool_failure(
            "desktop_list_windows 的 pid 超出有效范围",
            "pid out of range",
        ));
    }
    let request = ListWindowsRequest {
        app_name: args.get("app_name").and_then(as_str_owned),
        pid,
        foreground_only: args
            .get("foreground_only")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        access: state::access_context(),
    };
    run_desktop_op::<ListWindows, _>(&request, "desktop_list_windows")
}

fn handle_snapshot(arguments: String) -> Result<ToolResult, PluginError> {
    let args = match parse_args("desktop_snapshot", &arguments) {
        Ok(v) => v,
        Err(f) => return Ok(f),
    };
    // pid 超范围返回参数错误，而非截断。
    let pid = args.get("pid").and_then(as_u32_bounded);
    if args.get("pid").is_some() && pid.is_none() {
        return Ok(tool_failure(
            "desktop_snapshot 的 pid 超出有效范围",
            "pid out of range",
        ));
    }
    // max_depth/max_nodes 超范围返回参数错误，而非截断。
    let max_depth = match args.get("max_depth") {
        Some(v) if !v.is_null() => match as_u32_bounded(v) {
            Some(d) => d,
            None => {
                return Ok(tool_failure(
                    "desktop_snapshot 的 max_depth 超出有效范围",
                    "max_depth out of range",
                ));
            }
        },
        _ => 0,
    };
    let max_nodes = match args.get("max_nodes") {
        Some(v) if !v.is_null() => match as_u32_bounded(v) {
            Some(n) => n,
            None => {
                return Ok(tool_failure(
                    "desktop_snapshot 的 max_nodes 超出有效范围",
                    "max_nodes out of range",
                ));
            }
        },
        _ => 0,
    };
    let request = SnapshotRequest {
        scope: tiangong_plugin_computer_use_protocol::ops::SnapshotScope {
            window: args.get("window").and_then(parse_element_ref),
            app_name: args.get("app_name").and_then(as_str_owned),
            pid,
        },
        max_depth,
        max_nodes,
        include_invisible: args
            .get("include_invisible")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        access: state::access_context(),
    };
    run_desktop_op::<Snapshot, _>(&request, "desktop_snapshot")
}

fn handle_find(arguments: String) -> Result<ToolResult, PluginError> {
    let args = match parse_args("desktop_find", &arguments) {
        Ok(v) => v,
        Err(f) => return Ok(f),
    };
    // conditions 缺失时返回参数错误（它是必填字段）。
    let conditions_raw = match args.get("conditions") {
        Some(c) if !c.is_null() => c.clone(),
        _ => {
            return Ok(tool_failure(
                "desktop_find 缺少必填的 conditions 参数",
                "missing conditions",
            ));
        }
    };
    let request = FindRequest {
        window: args.get("window").and_then(parse_element_ref),
        snapshot: args.get("snapshot").and_then(serde_json::Value::as_u64),
        conditions: FindConditions {
            // 工具协议会用空字符串表示“未指定”；筛选协议必须归一化为 None，
            // 否则 exact 模式下空 automation_id/value 会排除全部控件。
            automation_id: conditions_raw
                .get("automation_id")
                .and_then(as_non_empty_str_owned),
            role: conditions_raw.get("role").and_then(as_non_empty_str_owned),
            name: conditions_raw.get("name").and_then(as_non_empty_str_owned),
            value: conditions_raw.get("value").and_then(as_non_empty_str_owned),
            visible: conditions_raw
                .get("visible")
                .and_then(serde_json::Value::as_bool),
            enabled: conditions_raw
                .get("enabled")
                .and_then(serde_json::Value::as_bool),
            focused: conditions_raw
                .get("focused")
                .and_then(serde_json::Value::as_bool),
            mode: conditions_raw
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .map(|s| match s {
                    "contains" => MatchMode::Contains,
                    _ => MatchMode::Exact,
                })
                .unwrap_or_default(),
        },
        // max_candidates 超范围返回参数错误，而非截断（与 pid/depth/nodes 一致）。
        max_candidates: match args.get("max_candidates") {
            Some(v) if !v.is_null() => match as_u32_bounded(v) {
                Some(n) => n,
                None => {
                    return Ok(tool_failure(
                        "desktop_find 的 max_candidates 超出有效范围",
                        "max_candidates out of range",
                    ));
                }
            },
            _ => 0,
        },
        access: state::access_context(),
    };
    run_desktop_op::<Find, _>(&request, "desktop_find")
}

fn handle_action(arguments: String) -> Result<ToolResult, PluginError> {
    let args = match parse_args("desktop_action", &arguments) {
        Ok(v) => v,
        Err(f) => return Ok(f),
    };
    let element = match args.get("element").and_then(parse_element_ref) {
        Some(e) => e,
        None => {
            return Ok(tool_failure(
                "desktop_action 缺少 element 参数",
                "missing element",
            ));
        }
    };
    let action_str = args
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let action = match parse_action_kind(action_str) {
        Some(a) => a,
        None => {
            return Ok(tool_failure(
                &format!("desktop_action 不支持的动作: {action_str}"),
                "unsupported action",
            ));
        }
    };
    let request = ActionRequest {
        element,
        action,
        value: args.get("value").and_then(as_str_owned),
        selection: args.get("selection").and_then(as_str_owned),
        access: state::access_context(),
    };
    run_desktop_op::<Action, _>(&request, "desktop_action")
}

fn run_mouse(args: &serde_json::Value, gesture: MouseGesture) -> Result<ToolResult, PluginError> {
    let (Some(x), Some(y)) = (
        args.get("x").and_then(|value| value.as_f64()),
        args.get("y").and_then(|value| value.as_f64()),
    ) else {
        return Ok(tool_failure("鼠标动作缺少 x/y 坐标", "missing x/y"));
    };
    // 手势参数完整性：drag 需终点，scroll 需滚动量。
    let (to_x, to_y) = (
        args.get("to_x").and_then(|value| value.as_f64()),
        args.get("to_y").and_then(|value| value.as_f64()),
    );
    let (delta_y, delta_x) = (
        args.get("delta_y").and_then(|value| value.as_f64()),
        args.get("delta_x").and_then(|value| value.as_f64()),
    );
    if gesture == MouseGesture::Drag && (to_x.is_none() || to_y.is_none()) {
        return Ok(tool_failure(
            "drag 必须同时提供 to_x 与 to_y",
            "missing drag target",
        ));
    }
    if gesture == MouseGesture::Scroll && delta_y.is_none() && delta_x.is_none() {
        return Ok(tool_failure(
            "scroll 必须提供 delta_y 或 delta_x",
            "missing scroll delta",
        ));
    }
    let request = MouseRequest {
        gesture,
        x,
        y,
        to_x,
        to_y,
        delta_y,
        delta_x,
        access: state::access_context(),
    };
    run_desktop_op::<Mouse, _>(&request, "desktop_mouse")
}
fn parse_mouse_gesture(value: &str) -> Option<MouseGesture> {
    match value {
        "move" => Some(MouseGesture::Move),
        "click" => Some(MouseGesture::Click),
        "right_click" => Some(MouseGesture::RightClick),
        "double_click" => Some(MouseGesture::DoubleClick),
        "drag" => Some(MouseGesture::Drag),
        "scroll" => Some(MouseGesture::Scroll),
        _ => None,
    }
}
/// 解析 desktop_screenshot 的 region 参数：四个有限数且 width/height > 0。
fn parse_region(value: &serde_json::Value) -> Option<Bounds> {
    let object = value.as_object()?;
    let number = |key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_f64)
            .filter(|n| n.is_finite())
    };
    let (x, y, width, height) = (
        number("x")?,
        number("y")?,
        number("width")?,
        number("height")?,
    );
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some(Bounds {
        x,
        y,
        width,
        height,
    })
}
fn run_keyboard(
    args: &serde_json::Value,
    action: KeyboardActionKind,
) -> Result<ToolResult, PluginError> {
    let text = args.get("text").and_then(as_str_owned);
    let key = args.get("key").and_then(as_str_owned);
    let keys: Option<Vec<String>> = args.get("keys").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect()
    });
    // 手势参数完整性（键名合法性由 sidecar 单点裁决，此处只查结构）。
    if action == KeyboardActionKind::Type && text.as_deref().map(str::trim).unwrap_or("").is_empty()
    {
        return Ok(tool_failure("type 必须提供非空 text", "missing text"));
    }
    if action == KeyboardActionKind::Key && key.as_deref().map(str::trim).unwrap_or("").is_empty() {
        return Ok(tool_failure("key 手势必须提供非空 key 键名", "missing key"));
    }
    if action == KeyboardActionKind::Combo {
        let valid = keys.as_ref().is_some_and(|k| k.len() >= 2);
        if !valid {
            return Ok(tool_failure(
                "combo 必须提供 keys 数组（至少修饰键 + 普通键两个）",
                "missing keys",
            ));
        }
    }
    let request = KeyboardRequest {
        action,
        text,
        key,
        keys,
        access: state::access_context(),
    };
    run_desktop_op::<Keyboard, _>(&request, "desktop_keyboard")
}
fn parse_keyboard_action(value: &str) -> Option<KeyboardActionKind> {
    match value {
        "type" => Some(KeyboardActionKind::Type),
        "key" => Some(KeyboardActionKind::Key),
        "combo" => Some(KeyboardActionKind::Combo),
        _ => None,
    }
}
fn handle_wait(arguments: String) -> Result<ToolResult, PluginError> {
    let args = match parse_args("desktop_wait", &arguments) {
        Ok(v) => v,
        Err(f) => return Ok(f),
    };
    let condition = match parse_wait_condition(&args) {
        Some(c) => c,
        None => {
            return Ok(tool_failure(
                "desktop_wait 缺少或无法解析 condition",
                "bad condition",
            ));
        }
    };
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if timeout_ms == 0 {
        return Ok(tool_failure(
            "desktop_wait 的 timeout_ms 必须大于 0",
            "bad timeout",
        ));
    }
    // 限制等待不超过 host 请求期限（plugin.json 的 request_timeout_ms=60000）。
    // 超过时截断到 55 秒（留 5 秒余量），避免 host 在 60 秒断开后 sidecar 仍悬挂。
    const MAX_WAIT_MS: u64 = 55_000;
    let timeout_ms = timeout_ms.min(MAX_WAIT_MS);
    let request = WaitRequest {
        condition,
        timeout_ms,
        access: state::access_context(),
    };
    run_desktop_op::<Wait, _>(&request, "desktop_wait")
}

/// desktop_screenshot：图源工具（RFC 0017）。响应 JSON 里的
/// `injected_assets` 数组是注入声明：core 在工具批次闭合后据此落成
/// 宿主注入消息——工具文本只留引用与元数据，媒体由前端 assistant 侧展示。
fn handle_screenshot(arguments: String) -> Result<ToolResult, PluginError> {
    let args = match parse_args("desktop_screenshot", &arguments) {
        Ok(v) => v,
        Err(f) => return Ok(f),
    };
    let pid = args.get("pid").and_then(as_u32_bounded).filter(|p| *p > 0);
    if args
        .get("pid")
        .is_some_and(|value| value.as_u64().is_none_or(|p| p > u32::MAX as u64))
    {
        return Ok(tool_failure(
            "desktop_screenshot 的 pid 超出有效范围",
            "pid out of range",
        ));
    }
    // region 显式区域（优先于 app 定位）；四个分量必须为有限正数。
    let region = args.get("region").and_then(parse_region);
    if args.get("region").is_some() && region.is_none() {
        return Ok(tool_failure(
            "desktop_screenshot 的 region 需要有限的 x/y/width/height（width/height > 0）",
            "bad region",
        ));
    }
    let max_dimension = match args.get("max_dimension") {
        Some(v) if !v.is_null() => match as_u32_bounded(v) {
            Some(d) if d > 0 => Some(d),
            _ => {
                return Ok(tool_failure(
                    "desktop_screenshot 的 max_dimension 需为正整数",
                    "bad max_dimension",
                ));
            }
        },
        _ => None,
    };
    let request = ScreenshotRequest {
        app_name: args.get("app_name").and_then(as_str_owned),
        pid,
        foreground_only: args
            .get("foreground_only")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        region,
        max_dimension,
        delay_ms: None,
        access: state::access_context(),
    };
    let result = sidecar_client::invoke::<Screenshot>(&request)
        .map_err(|e| plugin_err(format!("desktop_screenshot 调用 sidecar 失败: {e}")))?;
    match result {
        DesktopResult::Ok(resp) => {
            let stdout = serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".to_string());
            let app_label = if resp.app_name.is_empty() {
                "屏幕".to_string()
            } else {
                resp.app_name.clone()
            };
            let mut summary = format!(
                "已截取{app_label}（{}×{}，{} 字节）；图片将以原生视觉内容注入对话，直接阅读即可",
                resp.width, resp.height, resp.size_bytes
            );
            if let Some(hint) = resp.coordinate_hint() {
                summary.push('。');
                summary.push_str(&hint);
            }
            Ok(ToolResult {
                ok: true,
                summary,
                stdout,
                stderr: String::new(),
                exit_code: 0,
                execution: None,
            })
        }
        DesktopResult::Err(error) => {
            let message = error.agent_message();
            Ok(ToolResult {
                ok: false,
                summary: message.clone(),
                stdout: String::new(),
                stderr: message,
                exit_code: 1,
                execution: None,
            })
        }
    }
}

/// 统一执行一个桌面操作：调 sidecar，把 DesktopResult<T> 转 ToolResult。
/// desktop_open_app：唤起/启动应用。summary 直接给出窗口坐标，便于
/// Agent 接着按区域截图。
fn handle_open_app(arguments: String) -> Result<ToolResult, PluginError> {
    let args = match parse_args("desktop_open_app", &arguments) {
        Ok(v) => v,
        Err(f) => return Ok(f),
    };
    let request = OpenAppRequest {
        app_name: args.get("app_name").and_then(as_str_owned),
        bundle_id: args.get("bundle_id").and_then(as_str_owned),
        access: state::access_context(),
    };
    if request
        .app_name
        .as_deref()
        .is_none_or(|s| s.trim().is_empty())
        && request
            .bundle_id
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
    {
        return Ok(tool_failure(
            "desktop_open_app 需要 app_name 或 bundle_id",
            "missing app_name/bundle_id",
        ));
    }
    let result = sidecar_client::invoke::<OpenApp>(&request)
        .map_err(|e| plugin_err(format!("desktop_open_app 调用 sidecar 失败: {e}")))?;
    let mut tool_result = desktop_result_to_tool_result(result.clone());
    if let DesktopResult::Ok(resp) = result {
        tool_result.summary = resp.summary;
        if resp.split.as_ref().is_some_and(|split| split.applied) {
            state::mark_split_used();
        }
    }
    Ok(tool_result)
}

/// 从生命周期钩子的 session 快照中取会话 ID。
fn session_id_of(session_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(session_json)
        .ok()?
        .get("id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn run_desktop_op<O, T>(request: &O::Request, tool: &str) -> Result<ToolResult, PluginError>
where
    O: ComputerUseOperation<Response = DesktopResult<T>>,
    O::Request: Serialize,
    T: Serialize,
{
    let result = sidecar_client::invoke::<O>(request)
        .map_err(|e| plugin_err(format!("{tool} 调用 sidecar 失败: {e}")))?;
    Ok(desktop_result_to_tool_result(result))
}

/// DesktopResult<T> → ToolResult：成功序列化进 stdout，业务错误用 agent_message 进 stderr。
fn desktop_result_to_tool_result<T: Serialize>(result: DesktopResult<T>) -> ToolResult {
    match result {
        DesktopResult::Ok(value) => {
            let stdout = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string());
            ToolResult {
                ok: true,
                summary: "桌面操作完成".to_string(),
                stdout,
                stderr: String::new(),
                exit_code: 0,
                execution: None,
            }
        }
        DesktopResult::Err(error) => {
            let message = error.agent_message();
            ToolResult {
                ok: false,
                summary: message.clone(),
                stdout: String::new(),
                stderr: message,
                exit_code: 1,
                execution: None,
            }
        }
    }
}

// ── 参数解析辅助 ───────────────────────────────────────────────

/// 解析工具参数 JSON。非法 JSON 返回参数错误 ToolResult，而非当作空对象。
#[allow(clippy::result_large_err)]
fn parse_args(tool: &str, arguments: &str) -> Result<serde_json::Value, ToolResult> {
    serde_json::from_str(arguments).map_err(|e| {
        tool_failure(
            &format!("{tool} 参数不是合法 JSON: {e}"),
            "invalid json arguments",
        )
    })
}

/// 把 JSON 数值校验后转为 u32，超范围返回 None（调用方决定如何处理）。
fn as_u32_bounded(v: &serde_json::Value) -> Option<u32> {
    let n = v.as_u64()?;
    if n <= u32::MAX as u64 {
        Some(n as u32)
    } else {
        None
    }
}

/// 把 JSON 值的字符串视图转为 owned String。
fn as_str_owned(v: &serde_json::Value) -> Option<String> {
    v.as_str().map(String::from)
}

/// 筛选条件中的空字符串表示“未指定”，不能作为 exact 条件下传。
fn as_non_empty_str_owned(v: &serde_json::Value) -> Option<String> {
    v.as_str()
        .filter(|value| !value.is_empty())
        .map(String::from)
}

fn parse_element_ref(v: &serde_json::Value) -> Option<ElementRef> {
    let obj = v.as_object()?;
    let id = obj.get("id")?.as_str()?.to_string();
    let snapshot = obj.get("snapshot")?.as_u64()?;
    Some(ElementRef { id, snapshot })
}

/// 屏幕区域参数 schema（逻辑坐标 points）。
fn region_schema(description: &str) -> serde_json::Value {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "x": { "type": "number" },
            "y": { "type": "number" },
            "width": { "type": "number", "minimum": 0 },
            "height": { "type": "number", "minimum": 0 }
        },
        "required": ["x", "y", "width", "height"]
    })
}

fn element_ref_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "snapshot": { "type": "integer", "minimum": 0 }
        },
        "required": ["id", "snapshot"]
    })
}

fn parse_action_kind(s: &str) -> Option<ActionRequestKind> {
    Some(match s {
        "focus" => ActionRequestKind::Focus,
        "press" => ActionRequestKind::Press,
        "set_value" => ActionRequestKind::SetValue,
        "toggle" => ActionRequestKind::Toggle,
        "select" => ActionRequestKind::Select,
        "expand" => ActionRequestKind::Expand,
        "collapse" => ActionRequestKind::Collapse,
        "scroll_into_view" => ActionRequestKind::ScrollIntoView,
        _ => return None,
    })
}

fn parse_wait_target(
    v: &serde_json::Value,
) -> Option<tiangong_plugin_computer_use_protocol::ops::WaitTarget> {
    Some(tiangong_plugin_computer_use_protocol::ops::WaitTarget {
        app_name: v.get("app_name").and_then(as_str_owned),
        title: v.get("title").and_then(as_str_owned),
    })
}

fn parse_wait_condition(
    args: &serde_json::Value,
) -> Option<tiangong_plugin_computer_use_protocol::ops::WaitCondition> {
    use tiangong_plugin_computer_use_protocol::ops::WaitCondition;
    let cond = args.get("condition")?;
    let kind = cond.get("kind")?.as_str()?;
    Some(match kind {
        "appear" | "disappear" => {
            let target = parse_wait_target(cond.get("target")?)?;
            // appear/disappear 的目标必须至少提供 app_name 或 title，
            // 否则 appear 会一直等到超时、disappear 会立即误判成功。
            if target.app_name.is_none() && target.title.is_none() {
                return None;
            }
            if kind == "appear" {
                WaitCondition::Appear { target }
            } else {
                WaitCondition::Disappear { target }
            }
        }
        "focus" => WaitCondition::Focus {
            element: parse_element_ref(cond.get("element")?)?,
        },
        "available" => WaitCondition::Available {
            element: parse_element_ref(cond.get("element")?)?,
        },
        "value" => WaitCondition::Value {
            element: parse_element_ref(cond.get("element")?)?,
            expected: cond.get("expected").and_then(as_str_owned),
        },
        _ => return None,
    })
}

fn schema_string(value: serde_json::Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

/// 构造简单失败 ToolResult。
fn tool_failure(summary: &str, stderr: &str) -> ToolResult {
    ToolResult {
        ok: false,
        summary: summary.to_string(),
        stdout: String::new(),
        stderr: stderr.to_string(),
        exit_code: 1,
        execution: None,
    }
}

/// Computer Use 插件无设置页；输入区「恢复窗口」按钮由清单 ui.contributions
/// 声明（静态入口 app/restore-window.html），经 handle_view_message 取数。
impl UiGuest for Component {
    fn contributions() -> Result<Vec<Contribution>, PluginError> {
        Ok(Vec::new())
    }

    fn open_view(_id: String) -> Result<ViewResponse, PluginError> {
        Err(plugin_err("Computer Use 插件暂无设置页面"))
    }

    fn get_view_resource(_path: String) -> Result<ResourceResponse, PluginError> {
        Err(plugin_err("Computer Use 插件暂无页面资源"))
    }

    fn handle_view_message(
        request: ViewMessageRequest,
    ) -> Result<ViewMessageResponse, PluginError> {
        // 「恢复窗口」按钮（session.input-action）的数据通道。
        let payload = match request.method.as_str() {
            "splitState" => serde_json::to_string(
                &sidecar_client::invoke::<SplitStateQuery>(&Ack {})
                    .map_err(|e| plugin_err(format!("读取分屏状态失败: {e}")))?,
            ),
            "restoreHostWindow" => serde_json::to_string(
                &sidecar_client::invoke::<RestoreHostWindow>(&Ack {})
                    .map_err(|e| plugin_err(format!("恢复天工窗口失败: {e}")))?,
            ),
            other => return Err(plugin_err(format!("Computer Use 未知页面消息: {other}"))),
        }
        .map_err(|e| plugin_err(format!("序列化页面响应失败: {e}")))?;
        Ok(ViewMessageResponse { payload })
    }
}

bindings::export!(Component with_types_in bindings);
