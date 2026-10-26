//! desktop_* 工具直连入口（原 WASM 桥接的参数解析、校验与结果组装迁入）。
//!
//! 五个工具（operation = 工具名，payload = 调用参数对象）在此完成参数
//! 归一化与完整性校验，经内部操作分发执行后组装 ToolOutcome 形状
//! `{ok, summary, stdout, stderr, exit_code}`。行为与原 WASM 桥接逐条
//! 对齐：参数错误返回 ok:false 的工具级结果（而非通道错误），桌面业务
//! 错误用 agent_message 落 stderr。

use anyhow::{Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};
use tiangong_plugin_computer_use_protocol::ops::{
    Action, ActionRequest, ActionRequestKind, DesktopStatus, DesktopStatusRequest, Find,
    FindConditions, FindRequest, Keyboard, KeyboardActionKind, KeyboardRequest, ListWindows,
    ListWindowsRequest, Mouse, MouseGesture, MouseRequest, OpenApp, OpenAppRequest, Screenshot,
    ScreenshotRequest, Snapshot, SnapshotRequest, SnapshotScope, Wait, WaitCondition, WaitRequest,
    WaitTarget,
};
use tiangong_plugin_computer_use_protocol::{
    AccessContext, Bounds, ComputerUseOperation, DesktopResult, ElementRef, MatchMode,
    TOOL_DESKTOP_APP, TOOL_DESKTOP_INPUT, TOOL_DESKTOP_SCREENSHOT, TOOL_DESKTOP_UI,
    TOOL_DESKTOP_WAIT,
};

use crate::service::ComputerUseService;

/// 工具分发：operation = 工具名。未知操作交给调用方报错。
pub async fn dispatch(service: &ComputerUseService, tool: &str, payload: Value) -> Result<Value> {
    match tool {
        TOOL_DESKTOP_APP => Ok(desktop_app(service, payload).await),
        TOOL_DESKTOP_SCREENSHOT => Ok(desktop_screenshot(service, payload).await),
        TOOL_DESKTOP_INPUT => Ok(desktop_input(service, payload).await),
        TOOL_DESKTOP_UI => Ok(desktop_ui(service, payload).await),
        TOOL_DESKTOP_WAIT => Ok(desktop_wait(service, payload).await),
        other => Err(anyhow!("未知工具: {other}")),
    }
}

// ── 内部操作调用（与协议分发共用同一条路径，语义与经宿主转发一致）──

/// 调用内部操作并反序列化响应（桌面操作的 `Response` 即 `DesktopResult<T>`）。
async fn op<O>(service: &ComputerUseService, request: &O::Request) -> Result<O::Response>
where
    O: ComputerUseOperation,
    O::Request: Serialize,
    O::Response: serde::de::DeserializeOwned,
{
    let payload =
        serde_json::to_value(request).map_err(|error| anyhow!("序列化请求失败: {error}"))?;
    let value = service.dispatch_protocol(O::NAME, payload).await?;
    Ok(serde_json::from_value(value)?)
}

/// 统一执行一个桌面操作：调内部操作，把 DesktopResult<T> 转 ToolOutcome。
async fn desktop_op<O, T>(service: &ComputerUseService, request: &O::Request, tool: &str) -> Value
where
    O: ComputerUseOperation<Response = DesktopResult<T>>,
    O::Request: Serialize,
    T: Serialize + serde::de::DeserializeOwned,
{
    match op::<O>(service, request).await {
        Ok(result) => desktop_result_to_outcome(result),
        Err(error) => tool_failure(&format!("{tool} 执行失败: {error:#}"), "sidecar error"),
    }
}

fn access(service: &ComputerUseService) -> AccessContext {
    service.access_context()
}

// ── ToolOutcome 组装 ──

fn tool_failure(summary: &str, stderr: &str) -> Value {
    json!({
        "ok": false,
        "summary": summary,
        "stdout": "",
        "stderr": stderr,
        "exit_code": 1,
    })
}

fn parse_args(tool: &str, payload: Value) -> Result<Value, Value> {
    if payload.is_object() {
        Ok(payload)
    } else {
        Err(tool_failure(
            &format!("{tool} 参数须为 JSON 对象"),
            "invalid json arguments",
        ))
    }
}

/// DesktopResult<T> → ToolOutcome：成功序列化进 stdout，业务错误用
/// agent_message 进 stderr。
fn desktop_result_to_outcome<T: Serialize>(result: DesktopResult<T>) -> Value {
    match result {
        DesktopResult::Ok(value) => {
            let stdout = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string());
            json!({
                "ok": true,
                "summary": "桌面操作完成",
                "stdout": stdout,
                "stderr": "",
                "exit_code": 0,
            })
        }
        DesktopResult::Err(error) => {
            let message = error.agent_message();
            json!({
                "ok": false,
                "summary": message,
                "stdout": "",
                "stderr": message,
                "exit_code": 1,
            })
        }
    }
}

/// 从 ToolOutcome 的 stdout JSON 中取字段。
fn outcome_stdout(outcome: &Value) -> Value {
    outcome
        .get("stdout")
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null)
}

// ── desktop_app：status / list / open 分派 ──

async fn desktop_app(service: &ComputerUseService, payload: Value) -> Value {
    let args = match parse_args("desktop_app", payload) {
        Ok(args) => args,
        Err(failure) => return failure,
    };
    match args.get("action").and_then(Value::as_str) {
        Some("open") => open_app(service, args).await,
        Some("list") => list_windows(service, args).await,
        Some("status") => desktop_status(service).await,
        other => tool_failure(
            &format!(
                "desktop_app 的 action 需为 open/list/status，收到：{}",
                other.unwrap_or("（缺失）")
            ),
            "bad action",
        ),
    }
}

async fn desktop_status(service: &ComputerUseService) -> Value {
    let request = DesktopStatusRequest {
        access: access(service),
    };
    desktop_op::<DesktopStatus, _>(service, &request, "desktop_status").await
}

async fn list_windows(service: &ComputerUseService, args: Value) -> Value {
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
        return tool_failure(
            "desktop_list_windows 的 pid 超出有效范围",
            "pid out of range",
        );
    }
    let request = ListWindowsRequest {
        app_name: args.get("app_name").and_then(as_str_owned),
        pid,
        foreground_only: args
            .get("foreground_only")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        access: access(service),
    };
    desktop_op::<ListWindows, _>(service, &request, "desktop_list_windows").await
}

async fn open_app(service: &ComputerUseService, args: Value) -> Value {
    let request = OpenAppRequest {
        app_name: args.get("app_name").and_then(as_str_owned),
        bundle_id: args.get("bundle_id").and_then(as_str_owned),
        access: access(service),
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
        return tool_failure(
            "desktop_open_app 需要 app_name 或 bundle_id",
            "missing app_name/bundle_id",
        );
    }
    match op::<OpenApp>(service, &request).await {
        Ok(result) => {
            let mut outcome = desktop_result_to_outcome(result.clone());
            if let DesktopResult::Ok(resp) = result {
                outcome["summary"] = json!(resp.summary);
            }
            outcome
        }
        Err(error) => tool_failure(
            &format!("desktop_open_app 执行失败: {error:#}"),
            "sidecar error",
        ),
    }
}

// ── desktop_screenshot：图源工具（RFC 0017）──
//
// 响应 JSON 里的 `injected_assets` 数组是注入声明：core 在工具批次闭合后
// 据此落成宿主注入消息——工具文本只留引用与元数据，媒体由前端展示。

async fn desktop_screenshot(service: &ComputerUseService, payload: Value) -> Value {
    let args = match parse_args("desktop_screenshot", payload) {
        Ok(args) => args,
        Err(failure) => return failure,
    };
    let pid = args.get("pid").and_then(as_u32_bounded).filter(|p| *p > 0);
    if args
        .get("pid")
        .is_some_and(|value| value.as_u64().is_none_or(|p| p > u32::MAX as u64))
    {
        return tool_failure("desktop_screenshot 的 pid 超出有效范围", "pid out of range");
    }
    // region 显式区域（优先于 app 定位）；四个分量必须为有限正数。
    let region = args.get("region").and_then(parse_region);
    if args.get("region").is_some() && region.is_none() {
        return tool_failure(
            "desktop_screenshot 的 region 需要有限的 x/y/width/height（width/height > 0）",
            "bad region",
        );
    }
    let max_dimension = match args.get("max_dimension") {
        Some(v) if !v.is_null() => match as_u32_bounded(v) {
            Some(d) if d > 0 => Some(d),
            _ => {
                return tool_failure(
                    "desktop_screenshot 的 max_dimension 需为正整数",
                    "bad max_dimension",
                );
            }
        },
        _ => None,
    };
    let request = ScreenshotRequest {
        app_name: args.get("app_name").and_then(as_str_owned),
        pid,
        foreground_only: args
            .get("foreground_only")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        region,
        max_dimension,
        delay_ms: None,
        access: access(service),
    };
    match op::<Screenshot>(service, &request).await {
        Ok(DesktopResult::Ok(resp)) => {
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
            json!({
                "ok": true,
                "summary": summary,
                "stdout": stdout,
                "stderr": "",
                "exit_code": 0,
            })
        }
        Ok(DesktopResult::Err(error)) => {
            let message = error.agent_message();
            json!({
                "ok": false,
                "summary": message,
                "stdout": "",
                "stderr": message,
                "exit_code": 1,
            })
        }
        Err(error) => tool_failure(
            &format!("desktop_screenshot 执行失败: {error:#}"),
            "sidecar error",
        ),
    }
}

// ── desktop_input：鼠标手势 / 键盘动作分派，可选操作后自动截图 ──

async fn desktop_input(service: &ComputerUseService, payload: Value) -> Value {
    let args = match parse_args("desktop_input", payload) {
        Ok(args) => args,
        Err(failure) => return failure,
    };
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let mut outcome = if let Some(gesture) = parse_mouse_gesture(action) {
        let outcome = run_mouse(service, &args, gesture).await;
        if outcome.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            // 鼠标手势成功即天工指针分身已出现：轮次结束由 lifecycle 收起。
            service.note_cursor_summoned();
            outcome
        } else {
            outcome
        }
    } else if let Some(kind) = parse_keyboard_action(action) {
        run_keyboard(service, &args, kind).await
    } else {
        return tool_failure(
            &format!(
                "desktop_input 的 action 无法识别：{}（鼠标 click/double_click/right_click/move/drag/scroll，键盘 type/key/combo）",
                if action.is_empty() {
                    "（缺失）"
                } else {
                    action
                }
            ),
            "bad action",
        );
    };
    let wants_shot = args
        .get("screenshot_after")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !outcome["ok"].as_bool().unwrap_or(false) || !wants_shot {
        return outcome;
    }
    // 操作后自动截图：留 300ms 给界面刷新；截图注入声明在 stdout JSON 的
    // injected_assets 字段，与操作结果合并为一个 JSON 对象。
    let region = args.get("screenshot_region").and_then(parse_region);
    let shot_request = ScreenshotRequest {
        app_name: args.get("screenshot_app_name").and_then(as_str_owned),
        region,
        delay_ms: Some(300),
        access: access(service),
        ..Default::default()
    };
    match op::<Screenshot>(service, &shot_request).await {
        Ok(DesktopResult::Ok(resp)) => {
            let operation = outcome_stdout(&outcome);
            let mut merged = serde_json::to_value(&resp).unwrap_or_else(|_| json!({}));
            if let Some(object) = merged.as_object_mut() {
                object.insert("operation".to_string(), operation.clone());
            }
            let summary = merged
                .get("operation")
                .and_then(|op| op.get("summary"))
                .and_then(Value::as_str)
                .unwrap_or_else(|| outcome.get("summary").and_then(Value::as_str).unwrap_or(""))
                .to_string();
            outcome["stdout"] = json!(serde_json::to_string_pretty(&merged).unwrap_or_default());
            outcome["summary"] = json!(format!(
                "{}；已自动截图（{}×{}）。{}",
                summary,
                resp.width,
                resp.height,
                resp.coordinate_hint().unwrap_or_default()
            ));
        }
        Ok(DesktopResult::Err(error)) => {
            let summary = format!(
                "{}；操作后截图失败：{}",
                outcome.get("summary").and_then(Value::as_str).unwrap_or(""),
                error.agent_message()
            );
            outcome["summary"] = json!(summary);
        }
        Err(error) => {
            let summary = format!(
                "{}；操作后截图失败：{error:#}",
                outcome.get("summary").and_then(Value::as_str).unwrap_or("")
            );
            outcome["summary"] = json!(summary);
        }
    }
    outcome
}

async fn run_mouse(service: &ComputerUseService, args: &Value, gesture: MouseGesture) -> Value {
    let (Some(x), Some(y)) = (
        args.get("x").and_then(|value| value.as_f64()),
        args.get("y").and_then(|value| value.as_f64()),
    ) else {
        return tool_failure("鼠标动作缺少 x/y 坐标", "missing x/y");
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
        return tool_failure("drag 必须同时提供 to_x 与 to_y", "missing drag target");
    }
    if gesture == MouseGesture::Scroll && delta_y.is_none() && delta_x.is_none() {
        return tool_failure("scroll 必须提供 delta_y 或 delta_x", "missing scroll delta");
    }
    let request = MouseRequest {
        gesture,
        x,
        y,
        to_x,
        to_y,
        delta_y,
        delta_x,
        access: access(service),
    };
    desktop_op::<Mouse, _>(service, &request, "desktop_mouse").await
}

async fn run_keyboard(
    service: &ComputerUseService,
    args: &Value,
    action: KeyboardActionKind,
) -> Value {
    let text = args.get("text").and_then(as_str_owned);
    let key = args.get("key").and_then(as_str_owned);
    let keys: Option<Vec<String>> = args.get("keys").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    });
    // 手势参数完整性（键名合法性由后端单点裁决，此处只查结构）。
    if action == KeyboardActionKind::Type && text.as_deref().map(str::trim).unwrap_or("").is_empty()
    {
        return tool_failure("type 必须提供非空 text", "missing text");
    }
    if action == KeyboardActionKind::Key && key.as_deref().map(str::trim).unwrap_or("").is_empty() {
        return tool_failure("key 手势必须提供非空 key 键名", "missing key");
    }
    if action == KeyboardActionKind::Combo {
        let valid = keys.as_ref().is_some_and(|k| k.len() >= 2);
        if !valid {
            return tool_failure(
                "combo 必须提供 keys 数组（至少修饰键 + 普通键两个）",
                "missing keys",
            );
        }
    }
    let request = KeyboardRequest {
        action,
        text,
        key,
        keys,
        access: access(service),
    };
    desktop_op::<Keyboard, _>(service, &request, "desktop_keyboard").await
}

// ── desktop_ui：snapshot（可带 conditions 查找）/ perform 分派 ──

async fn desktop_ui(service: &ComputerUseService, payload: Value) -> Value {
    let mut args = match parse_args("desktop_ui", payload) {
        Ok(args) => args,
        Err(failure) => return failure,
    };
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .map(str::to_string);
    match action.as_deref() {
        Some("snapshot") => {
            let has_conditions = args
                .get("conditions")
                .is_some_and(|c| c.as_object().is_some_and(|o| !o.is_empty()));
            if !has_conditions {
                return snapshot(service, args).await;
            }
            // 带条件查找：未给快照版本/窗口引用时先按 app_name/pid 取一次
            // 快照，再在该快照内查找（底层 find 只接受 snapshot 或 window）。
            let has_scope = args.get("snapshot").is_some_and(|v| !v.is_null())
                || args.get("window").is_some_and(|v| !v.is_null());
            if !has_scope {
                let snapshot_outcome = snapshot(service, args.clone()).await;
                if !snapshot_outcome["ok"].as_bool().unwrap_or(false) {
                    return snapshot_outcome;
                }
                let snapshot_id = outcome_stdout(&snapshot_outcome)
                    .get("snapshot")
                    .and_then(Value::as_u64);
                let Some(snapshot_id) = snapshot_id else {
                    return tool_failure(
                        "desktop_ui 读取快照成功但未返回快照版本，无法查找",
                        "missing snapshot id",
                    );
                };
                if let Some(object) = args.as_object_mut() {
                    object.insert("snapshot".to_string(), json!(snapshot_id));
                }
                return find(service, args).await;
            }
            find(service, args).await
        }
        Some("perform") => {
            // 语义动作字段名 operation → 底层 action。
            let Some(operation) = args.get("operation").cloned() else {
                return tool_failure(
                    "desktop_ui perform 需要 operation（press/focus/set_value/toggle/select）",
                    "missing operation",
                );
            };
            if let Some(object) = args.as_object_mut() {
                object.insert("action".to_string(), operation);
            }
            action_op(service, args).await
        }
        other => tool_failure(
            &format!(
                "desktop_ui 的 action 需为 snapshot/perform，收到：{}",
                other.unwrap_or("（缺失）")
            ),
            "bad action",
        ),
    }
}

async fn snapshot(service: &ComputerUseService, args: Value) -> Value {
    // pid 超范围返回参数错误，而非截断。
    let pid = args.get("pid").and_then(as_u32_bounded);
    if args.get("pid").is_some() && pid.is_none() {
        return tool_failure("desktop_snapshot 的 pid 超出有效范围", "pid out of range");
    }
    // max_depth/max_nodes 超范围返回参数错误，而非截断。
    let max_depth = match args.get("max_depth") {
        Some(v) if !v.is_null() => match as_u32_bounded(v) {
            Some(d) => d,
            None => {
                return tool_failure(
                    "desktop_snapshot 的 max_depth 超出有效范围",
                    "max_depth out of range",
                );
            }
        },
        _ => 0,
    };
    let max_nodes = match args.get("max_nodes") {
        Some(v) if !v.is_null() => match as_u32_bounded(v) {
            Some(n) => n,
            None => {
                return tool_failure(
                    "desktop_snapshot 的 max_nodes 超出有效范围",
                    "max_nodes out of range",
                );
            }
        },
        _ => 0,
    };
    let request = SnapshotRequest {
        scope: SnapshotScope {
            window: args.get("window").and_then(parse_element_ref),
            app_name: args.get("app_name").and_then(as_str_owned),
            pid,
        },
        max_depth,
        max_nodes,
        include_invisible: args
            .get("include_invisible")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        access: access(service),
    };
    desktop_op::<Snapshot, _>(service, &request, "desktop_snapshot").await
}

async fn find(service: &ComputerUseService, args: Value) -> Value {
    // conditions 缺失时返回参数错误（它是必填字段）。
    let conditions_raw = match args.get("conditions") {
        Some(c) if !c.is_null() => c.clone(),
        _ => {
            return tool_failure(
                "desktop_find 缺少必填的 conditions 参数",
                "missing conditions",
            );
        }
    };
    let request = FindRequest {
        window: args.get("window").and_then(parse_element_ref),
        snapshot: args.get("snapshot").and_then(Value::as_u64),
        conditions: FindConditions {
            // 工具协议会用空字符串表示“未指定”；筛选协议必须归一化为 None，
            // 否则 exact 模式下空 automation_id/value 会排除全部控件。
            automation_id: conditions_raw
                .get("automation_id")
                .and_then(as_non_empty_str_owned),
            role: conditions_raw.get("role").and_then(as_non_empty_str_owned),
            name: conditions_raw.get("name").and_then(as_non_empty_str_owned),
            value: conditions_raw.get("value").and_then(as_non_empty_str_owned),
            visible: conditions_raw.get("visible").and_then(Value::as_bool),
            enabled: conditions_raw.get("enabled").and_then(Value::as_bool),
            focused: conditions_raw.get("focused").and_then(Value::as_bool),
            mode: conditions_raw
                .get("mode")
                .and_then(Value::as_str)
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
                    return tool_failure(
                        "desktop_find 的 max_candidates 超出有效范围",
                        "max_candidates out of range",
                    );
                }
            },
            _ => 0,
        },
        access: access(service),
    };
    desktop_op::<Find, _>(service, &request, "desktop_find").await
}

async fn action_op(service: &ComputerUseService, args: Value) -> Value {
    let element = match args.get("element").and_then(parse_element_ref) {
        Some(e) => e,
        None => return tool_failure("desktop_action 缺少 element 参数", "missing element"),
    };
    let action_str = args.get("action").and_then(Value::as_str).unwrap_or("");
    let action = match parse_action_kind(action_str) {
        Some(a) => a,
        None => {
            return tool_failure(
                &format!("desktop_action 不支持的动作: {action_str}"),
                "unsupported action",
            );
        }
    };
    let request = ActionRequest {
        element,
        action,
        value: args.get("value").and_then(as_str_owned),
        selection: args.get("selection").and_then(as_str_owned),
        access: access(service),
    };
    desktop_op::<Action, _>(service, &request, "desktop_action").await
}

// ── desktop_wait ──

async fn desktop_wait(service: &ComputerUseService, payload: Value) -> Value {
    let args = match parse_args("desktop_wait", payload) {
        Ok(args) => args,
        Err(failure) => return failure,
    };
    let condition = match parse_wait_condition(&args) {
        Some(c) => c,
        None => return tool_failure("desktop_wait 缺少或无法解析 condition", "bad condition"),
    };
    let timeout_ms = args.get("timeout_ms").and_then(Value::as_u64).unwrap_or(0);
    if timeout_ms == 0 {
        return tool_failure("desktop_wait 的 timeout_ms 必须大于 0", "bad timeout");
    }
    // 限制等待不超过 host 请求期限（plugin.json 的 request_timeout_ms=60000）。
    // 超过时截断到 55 秒（留 5 秒余量），避免 host 在 60 秒断开后 sidecar 仍悬挂。
    const MAX_WAIT_MS: u64 = 55_000;
    let timeout_ms = timeout_ms.min(MAX_WAIT_MS);
    let request = WaitRequest {
        condition,
        timeout_ms,
        access: access(service),
    };
    desktop_op::<Wait, _>(service, &request, "desktop_wait").await
}

// ── 参数解析辅助（与原 WASM 桥接逐条对齐）──

/// 把 JSON 数值校验后转为 u32，超范围返回 None（调用方决定如何处理）。
fn as_u32_bounded(v: &Value) -> Option<u32> {
    let n = v.as_u64()?;
    if n <= u32::MAX as u64 {
        Some(n as u32)
    } else {
        None
    }
}

/// 把 JSON 值的字符串视图转为 owned String。
fn as_str_owned(v: &Value) -> Option<String> {
    v.as_str().map(String::from)
}

/// 筛选条件中的空字符串表示“未指定”，不能作为 exact 条件下传。
fn as_non_empty_str_owned(v: &Value) -> Option<String> {
    v.as_str()
        .filter(|value| !value.is_empty())
        .map(String::from)
}

fn parse_element_ref(v: &Value) -> Option<ElementRef> {
    let obj = v.as_object()?;
    let id = obj.get("id")?.as_str()?.to_string();
    let snapshot = obj.get("snapshot")?.as_u64()?;
    Some(ElementRef { id, snapshot })
}

/// 解析 desktop_screenshot 的 region 参数：四个有限数且 width/height > 0。
fn parse_region(value: &Value) -> Option<Bounds> {
    let object = value.as_object()?;
    let number = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_f64)
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

fn parse_keyboard_action(value: &str) -> Option<KeyboardActionKind> {
    match value {
        "type" => Some(KeyboardActionKind::Type),
        "key" => Some(KeyboardActionKind::Key),
        "combo" => Some(KeyboardActionKind::Combo),
        _ => None,
    }
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

fn parse_wait_target(v: &Value) -> Option<WaitTarget> {
    Some(WaitTarget {
        app_name: v.get("app_name").and_then(as_str_owned),
        title: v.get("title").and_then(as_str_owned),
    })
}

fn parse_wait_condition(args: &Value) -> Option<WaitCondition> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 手势与键名解析() {
        assert_eq!(
            parse_mouse_gesture("double_click"),
            Some(MouseGesture::DoubleClick)
        );
        assert_eq!(parse_mouse_gesture("tap"), None);
        assert_eq!(
            parse_keyboard_action("combo"),
            Some(KeyboardActionKind::Combo)
        );
        assert_eq!(parse_keyboard_action("paste"), None);
    }

    #[test]
    fn region_须为有限正数() {
        let good = json!({"x": 0, "y": 10, "width": 100, "height": 50});
        assert!(parse_region(&good).is_some());
        let zero = json!({"x": 0, "y": 0, "width": 0, "height": 50});
        assert!(parse_region(&zero).is_none());
        let missing = json!({"x": 0, "y": 0, "width": 10});
        assert!(parse_region(&missing).is_none());
    }

    #[test]
    fn wait_条件目标不能全空() {
        let bad = json!({"condition": {"kind": "appear", "target": {}}});
        assert!(parse_wait_condition(&bad).is_none());
        let good = json!({"condition": {"kind": "appear", "target": {"app_name": "微信"}}});
        assert!(parse_wait_condition(&good).is_some());
    }

    #[test]
    fn 数值边界校验() {
        assert_eq!(as_u32_bounded(&json!(5)), Some(5));
        assert_eq!(as_u32_bounded(&json!(u32::MAX as u64 + 1)), None);
        assert_eq!(as_u32_bounded(&json!(-1)), None);
    }
}
