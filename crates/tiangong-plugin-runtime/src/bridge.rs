//! 宿主桥接（Host Bridge）：插件 UI 与宿主之间的统一通信管道。
//!
//! 插件 UI 沙箱内经 `bridge.call(method, payload)` 调用宿主能力、经
//! `bridge.on(channel)` 订阅宿主事件。本模块只做命名空间白名单、权限校验
//! 与负载透传，不解析业务 JSON、不感知具体插件（宿主中性）。
//!
//! M0 范围：`plugin.*` 转发到本插件 WASM 的 `handle-view-message`（等价旧
//! `plugin_call` 通道）；其余命名空间完成白名单与权限定形，宿主服务路由在
//! 对应接缝任务中接入。事件订阅当前只提供登记骨架，事件源在事件接缝任务接入。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};

use crate::manifest::PluginManifest;

/// 一个桥接能力命名空间：方法前缀 → 所需权限。
#[derive(Debug, Clone)]
pub struct BridgeNamespace {
    /// 方法前缀，如 `plugin.`。
    pub prefix: &'static str,
    /// 调用该命名空间所需的权限声明。
    pub permission: &'static str,
    /// 说明（供错误信息与文档）。
    pub description: &'static str,
}

/// 首版桥接命名空间白名单（设计文档 6.3）。
pub const BRIDGE_NAMESPACES: &[BridgeNamespace] = &[
    BridgeNamespace {
        prefix: "plugin.",
        permission: "bridge.call",
        description: "转发到本插件 WASM 逻辑层",
    },
    BridgeNamespace {
        prefix: "session.",
        permission: "session.read",
        description: "读取/操作当前会话",
    },
    BridgeNamespace {
        prefix: "tool.",
        permission: "tool.read",
        description: "主动触发工具、读取工具执行结果",
    },
    BridgeNamespace {
        prefix: "storage.",
        permission: "storage.private",
        description: "读写插件私有数据",
    },
    BridgeNamespace {
        prefix: "terminal.",
        permission: "terminal.use",
        description: "终端原生服务（宿主注入）",
    },
    BridgeNamespace {
        prefix: "browser.",
        permission: "browser.use",
        description: "浏览器原生服务（宿主注入）",
    },
    BridgeNamespace {
        prefix: "webview.",
        permission: "webview.use",
        description: "webview 容器原语",
    },
    BridgeNamespace {
        prefix: "app.",
        permission: "app.use",
        description: "打开/关闭插件 App 实例（声明 extension.tab 贡献的插件）",
    },
    BridgeNamespace {
        prefix: "sidecar.",
        permission: "sidecar.invoke",
        description: "本插件 sidecar 原生逻辑层",
    },
    BridgeNamespace {
        prefix: "plugin-dev.",
        permission: "plugin-dev.use",
        description: "插件开发受限通道（写范围锁定 plugins-dev 开发目录，RFC 0017 D23）",
    },
    BridgeNamespace {
        prefix: "dialog.",
        permission: "dialog.use",
        description: "系统对话框原语（保存文件：原生目录选择 + 宿主落盘）",
    },
];

/// 事件订阅的合法命名空间前缀（设计文档 7.7）。
pub const EVENT_NAMESPACE_PREFIXES: &[&str] = &[
    "session.",
    "tool.",
    "lifecycle.",
    "config.",
    "sidecar.",
    // plugins.*：宿主插件集变更通知（安装/启停/卸载），插件页面据此刷新
    // 自身状态（如插件创作页的项目列表）。
    "plugins.",
    // webview.*：宿主 webview 容器原语的页面事件通道（如浏览器插件的
    // webview.event），插件在 capabilities.events 声明后可订阅。
    "webview.",
];

/// 查找 method 所属的桥接命名空间。
pub fn namespace_of(method: &str) -> Option<&'static BridgeNamespace> {
    BRIDGE_NAMESPACES
        .iter()
        .find(|namespace| method.starts_with(namespace.prefix))
}

fn required_bridge_permission(method: &str, namespace: &BridgeNamespace) -> &'static str {
    if method.starts_with("session.input.") {
        "session.write"
    } else if method == "tool.resolve" {
        "tool.provide"
    } else if method.starts_with("terminal.") {
        "terminal.use"
    } else if method.starts_with("browser.") {
        "browser.use"
    } else if method.starts_with("sidecar.") {
        "sidecar.invoke"
    } else if method.starts_with("webview.") {
        "webview.use"
    } else {
        namespace.permission
    }
}

/// 权限校验。
///
/// - `plugin.*`：只到达本插件自身 WASM 逻辑层，等价旧 `plugin_call` 透传通道。
///   v1 清单早于桥接权限体系（`bridge.call` 为 v2 新增，v1 插件无从声明），
///   对 v1 一律放行，保证现有 WASM 插件零改动；v2 按声明校验。
/// - 其余宿主能力命名空间：仅 v2 声明对应权限后可达；v1 插件先于该体系，
///   一律拒绝，避免未来宿主服务接入时 v1 插件意外获得越权能力。
fn has_bridge_permission(
    manifest: &PluginManifest,
    namespace: &BridgeNamespace,
    permission: &str,
) -> bool {
    if manifest.schema_version == 1 {
        return namespace.prefix == "plugin.";
    }
    manifest.has_permission(permission)
}

/// 判断事件订阅声明（`capabilities.events`）是否覆盖某 channel。
///
/// 支持精确匹配、`<ns>.*` 通配与 `*` 全量。
pub fn event_declaration_allows(declared: &[String], channel: &str) -> bool {
    declared.iter().any(|pattern| {
        pattern == channel
            || pattern == "*"
            || pattern
                .strip_suffix(".*")
                .is_some_and(|prefix| channel.starts_with(&format!("{prefix}.")))
    })
}

// ── bridge.call ──

/// 处理一次 `bridge.call`：鉴权 → 白名单 → 路由 → 返回序列化结果。
///
/// `payload` 为不透明负载，透传不解析。
pub fn bridge_call(plugin_id: &str, method: &str, payload: &str) -> Result<String> {
    bridge_call_with_workspace(plugin_id, method, payload, None)
}

/// 带宿主权威会话工作区的桥接调用。
///
/// 工作区是宿主上下文，不属于插件业务负载；仅 sidecar 路由消费它来构造
/// 沙箱可写域，其余桥接能力保持原有行为。
pub fn bridge_call_with_workspace(
    plugin_id: &str,
    method: &str,
    payload: &str,
    workspace: Option<&Path>,
) -> Result<String> {
    let Some(namespace) = namespace_of(method) else {
        tracing::warn!(plugin_id, method, "bridge.call 拒绝未知 method");
        bail!(
            "bridge.call 拒绝未知 method {method}（插件 {plugin_id}），可命名空间：{}",
            BRIDGE_NAMESPACES
                .iter()
                .map(|ns| ns.prefix)
                .collect::<Vec<_>>()
                .join(" ")
        );
    };

    let manifest = crate::registry::plugin_manifest(plugin_id)
        .ok_or_else(|| anyhow::anyhow!("bridge.call 插件 {plugin_id} 未加载"))?;
    let permission = required_bridge_permission(method, namespace);
    if !has_bridge_permission(&manifest, namespace, permission) {
        tracing::warn!(plugin_id, method, permission, "bridge.call 权限不足");
        if manifest.schema_version == 1 {
            bail!(
                "bridge.call 插件 {plugin_id} 为 schema_version 1，宿主能力命名空间 {} 需要升级清单为 schema_version 2 后使用",
                namespace.prefix
            );
        }
        bail!(
            "bridge.call 插件 {plugin_id} 未声明权限 {permission}，无法调用 {}",
            namespace.prefix
        );
    }

    match namespace.prefix {
        // 转发到本插件 WASM 逻辑层：剥掉 `plugin.` 前缀，剩余部分即 WASM 方法名，
        // 负载透传（等价旧 plugin_call 通道）。
        "plugin." => {
            let wasm_method = &method[namespace.prefix.len()..];
            if wasm_method.is_empty() {
                bail!("bridge.call method {method} 缺少插件方法名");
            }
            crate::registry::handle_view_message_result(plugin_id, wasm_method, payload)
        }
        // storage.*：宿主直接路由到插件私有数据（设计 7.8），不经逻辑层
        "storage." => storage_call(plugin_id, method, payload),
        // tool.resolve：Desktop TS 工具插件提交自己声明的工具结果。
        "tool." if method == "tool.resolve" => crate::ts_tools::resolve(plugin_id, payload),
        // session.input.*：由桌面宿主注入输入草稿处理器，运行时仅负责权限与路由。
        "session." if method.starts_with("session.input.") => {
            session_input_call(plugin_id, method, payload)
        }
        // terminal.*：宿主注入的原生服务（PTY 等）
        "terminal." => {
            native_service_call(&TERMINAL_HANDLER, "terminal", plugin_id, method, payload)
        }
        // browser.*：宿主注入的浏览器原生服务
        "browser." => native_service_call(&BROWSER_HANDLER, "browser", plugin_id, method, payload),
        // webview.*：宿主 webview 容器原语（声明式创建/导航/eval），插件
        // 在其上构建浏览器类能力；运行时只做权限与路由，引擎在宿主进程。
        "webview." => native_service_call(&WEBVIEW_HANDLER, "webview", plugin_id, method, payload),
        // app.*：打开插件 App 实例（只能打开调用方自己的贡献）。
        "app." => native_service_call(&APP_HANDLER, "app", plugin_id, method, payload),
        // sidecar.*：TS 插件调用本插件 sidecar（请求-响应；输出流经通知事件）。
        // 仅到达本插件 sidecar，宿主不解析业务负载。
        "sidecar." => {
            let request: serde_json::Value = serde_json::from_str(payload)
                .with_context(|| "sidecar 请求负载必须是 JSON 对象")?;
            let operation = method.strip_prefix("sidecar.").unwrap_or_default();
            if operation.is_empty() {
                bail!("sidecar 方法缺少操作名（如 sidecar.terminalSpawn）");
            }
            let result = invoke_plugin_sidecar(plugin_id, operation, request, workspace)?;
            // 结果观察者（宿主注入的通用机制，不解析业务语义）：宿主可用
            // 于受信产物溯源登记等策略（如创作链构建登记）。
            if let Some(observer) = sidecar_result_observer() {
                let result_text = serde_json::to_string(&result).unwrap_or_default();
                observer(plugin_id, operation, payload, &result_text);
            }
            serde_json::to_string(&result).with_context(|| "序列化 sidecar 结果失败")
        }
        "tool." if method.starts_with("browser.") => {
            native_service_call(&BROWSER_HANDLER, "browser", plugin_id, method, payload)
        }
        // plugin-dev.*：插件开发受限通道（模板 init/构建/安装/校验/日志），
        // 服务实现见 plugin_dev 模块；写范围锁定开发目录（RFC 0017 D23）。
        "plugin-dev." => crate::plugin_dev::call(plugin_id, method, payload),
        // dialog.*：系统对话框原语（保存文件）。宿主 webview 是 WebKit，
        // 无 File System Access API——文件保存必须经宿主原生对话框。
        "dialog." => native_service_call(&DIALOG_HANDLER, "dialog", plugin_id, method, payload),
        // 其余命名空间已定形白名单，宿主服务路由按接缝任务渐进接入。
        _ => {
            tracing::info!(plugin_id, method, "bridge.call 命名空间尚未接入宿主服务");
            bail!(
                "bridge.call 命名空间 {} 已登记但当前版本尚未接入宿主服务",
                namespace.prefix
            )
        }
    }
}

/// 宿主可信调用插件 sidecar（实例生命周期 instanceClosed / listInstances 等
/// 宿主协议操作）：不经 bridge 权限校验，按插件安装目录定位存储根。
pub fn invoke_plugin_sidecar(
    plugin_id: &str,
    operation: &str,
    payload: serde_json::Value,
    workspace: Option<&Path>,
) -> Result<serde_json::Value> {
    let storage_root = crate::registry::plugin_install_directory(plugin_id)
        .and_then(|dir| dir.parent().map(|p| p.to_path_buf()))
        .and_then(|plugins_dir| plugins_dir.parent().map(|p| p.to_path_buf()))
        .ok_or_else(|| anyhow::anyhow!("无法定位插件存储根"))?;
    crate::registry::invoke_sidecar_with_workspace(
        &storage_root,
        plugin_id,
        operation,
        payload,
        workspace,
    )
}

/// 宿主请求打开插件 App 实例（宿主可信调用，路由到注入的 app.open 处理器）。
/// 用于宿主侧为已预留编号的实例资源建立前端标签。
pub fn request_app_open(plugin_id: &str, payload: &str) -> Result<String> {
    open_app_for_plugin(plugin_id, payload)
}

/// 输入草稿宿主处理器：桌面入口注入后，UI 插件可提交经宿主校验的输入附件。
pub type SessionInputHandler = Arc<dyn Fn(&str, &str, &str) -> Result<String> + Send + Sync>;
static SESSION_INPUT_HANDLER: OnceLock<SessionInputHandler> = OnceLock::new();

/// sidecar 调用结果观察者：参数为（插件 ID、操作名、原始请求负载、响应
/// JSON 文本）。仅成功调用触发；观察者异常不影响桥接结果。
pub type SidecarResultObserver = std::sync::Arc<dyn Fn(&str, &str, &str, &str) + Send + Sync>;

static SIDECAR_RESULT_OBSERVER: std::sync::RwLock<Option<SidecarResultObserver>> =
    std::sync::RwLock::new(None);

/// 注入 sidecar 结果观察者（宿主启动时调用，覆盖语义）。
pub fn set_sidecar_result_observer(observer: SidecarResultObserver) {
    if let Ok(mut current) = SIDECAR_RESULT_OBSERVER.write() {
        *current = Some(observer);
    }
}

fn sidecar_result_observer() -> Option<SidecarResultObserver> {
    SIDECAR_RESULT_OBSERVER
        .read()
        .ok()
        .and_then(|current| current.clone())
}

/// 原生能力宿主服务处理器：`(plugin_id, method, payload) -> 结果 JSON`。
pub type NativeServiceHandler = Arc<dyn Fn(&str, &str, &str) -> Result<String> + Send + Sync>;

static TERMINAL_HANDLER: OnceLock<NativeServiceHandler> = OnceLock::new();
static DIALOG_HANDLER: OnceLock<NativeServiceHandler> = OnceLock::new();
static BROWSER_HANDLER: OnceLock<NativeServiceHandler> = OnceLock::new();
static WEBVIEW_HANDLER: OnceLock<NativeServiceHandler> = OnceLock::new();
static APP_HANDLER: OnceLock<NativeServiceHandler> = OnceLock::new();

/// 注入终端原生服务（PTY 会话管理，桌面入口启动时调用）。
pub fn set_terminal_handler(handler: NativeServiceHandler) {
    let _ = TERMINAL_HANDLER.set(handler);
}

/// 注入系统对话框原生服务（保存文件等，桌面入口启动时调用）。
pub fn set_dialog_handler(handler: NativeServiceHandler) {
    let _ = DIALOG_HANDLER.set(handler);
}

/// 注入浏览器原生服务（webview 管理，桌面入口启动时调用）。
pub fn set_browser_handler(handler: NativeServiceHandler) {
    let _ = BROWSER_HANDLER.set(handler);
}

/// 注入 webview 容器原语服务（第四种声明式容器；通用原语，非浏览器业务——
/// 方法如 webview.create/navigate/eval/hide/close，事件经通知通道推送）。
pub fn set_webview_handler(handler: NativeServiceHandler) {
    let _ = WEBVIEW_HANDLER.set(handler);
}

/// 注入 App 实例原语服务（app.open：打开声明 extension.tab 贡献的插件
/// App，前端 open-plugin 通道执行；工具调用无 UI 接应时宿主内部亦经
/// [`open_app_for_plugin`] 使用同一处理器）。
pub fn set_app_handler(handler: NativeServiceHandler) {
    let _ = APP_HANDLER.set(handler);
}

/// 宿主内部请求打开插件 App（工具调用无 UI 接应等场景）。
///
/// 不经 `bridge.call` 权限校验（宿主可信调用），直接路由到注入的
/// `app.open` 原生服务；插件侧主动调用应走 `bridge_call`。
pub(crate) fn open_app_for_plugin(plugin_id: &str, payload: &str) -> Result<String> {
    app_primitive_for_plugin(plugin_id, "app.open", payload)
}

/// 执行插件在工具反馈中请求的 App 原语。只能操作调用插件自己的 App，且
/// 方法限制为打开和关闭；贡献、会话与实例归属继续由宿主处理器校验。
pub(crate) fn app_primitive_for_plugin(
    plugin_id: &str,
    method: &str,
    payload: &str,
) -> Result<String> {
    if !matches!(method, "app.open" | "app.close") {
        bail!("不支持的 App 原语 {method}");
    }
    native_service_call(&APP_HANDLER, "app", plugin_id, method, payload)
}

/// 处理 Handler 经统一进度通道发回的 Runtime 控制反馈。返回 true 表示消息
/// 已被 Runtime 消费；false 表示普通业务进度，应继续交给原有反馈消费者。
pub(crate) fn handle_runtime_feedback(plugin_id: &str, message: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(message) else {
        return false;
    };
    let (action, method) = if let Some(method) = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .filter(|method| matches!(*method, "app.open" | "app.close"))
    {
        (&value, method)
    } else if let Some(action) = value.get("host_action")
        && let Some(method) = action.get("method").and_then(serde_json::Value::as_str)
    {
        (action, method)
    } else {
        return false;
    };
    let payload = action
        .get("payload")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let Ok(payload) = serde_json::to_string(&payload) else {
        return true;
    };
    if let Err(error) = app_primitive_for_plugin(plugin_id, method, &payload) {
        tracing::warn!(plugin_id, method, %error, "插件 App 反馈执行失败");
    }
    true
}

fn native_service_call(
    handler: &OnceLock<NativeServiceHandler>,
    service: &str,
    plugin_id: &str,
    method: &str,
    payload: &str,
) -> Result<String> {
    let handler = handler
        .get()
        .ok_or_else(|| anyhow::anyhow!("宿主尚未接入 {service} 原生服务"))?;
    handler(plugin_id, method, payload)
}

pub fn set_session_input_handler(handler: SessionInputHandler) {
    let _ = SESSION_INPUT_HANDLER.set(handler);
}

fn session_input_call(plugin_id: &str, method: &str, payload: &str) -> Result<String> {
    // 方法白名单：新增输入方法须在此放行（宿主 handler 负责各自校验）。
    // - addAttachment：向当前输入草稿加入附件（PNG 图片 / 媒体目录内文件）；
    // - sendText：把一段文本（可带媒体目录内文件附件）作为用户消息发送；
    // - insertText：把文本写入输入草稿，不发送；
    // - showOverlay / hideOverlay：`session.input-overlay` 贡献申请接管 / 归还输入区。
    if !matches!(
        method,
        "session.input.addAttachment"
            | "session.input.sendText"
            | "session.input.insertText"
            | "session.input.showOverlay"
            | "session.input.hideOverlay"
    ) {
        bail!("未知输入草稿桥接方法 {method}");
    }
    let handler = SESSION_INPUT_HANDLER
        .get()
        .ok_or_else(|| anyhow::anyhow!("宿主尚未接入输入草稿服务"))?;
    handler(plugin_id, method, payload)
}

// ── bridge.on / bridge.off ──

/// 事件推送回调：`(plugin_id, channel, payload)`。
pub type BridgeEventEmitter = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

static EVENT_EMITTER: OnceLock<BridgeEventEmitter> = OnceLock::new();
static EVENT_SUBSCRIPTIONS: OnceLock<Mutex<BTreeMap<String, BTreeMap<String, usize>>>> =
    OnceLock::new();

fn event_subscriptions() -> &'static Mutex<BTreeMap<String, BTreeMap<String, usize>>> {
    EVENT_SUBSCRIPTIONS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// 注入事件推送回调（宿主入口启动时调用，把订阅事件送达前端/插件 UI）。
pub fn set_event_emitter(emitter: BridgeEventEmitter) {
    let _ = EVENT_EMITTER.set(emitter);
}

/// 把插件集变更通知（安装/启停/卸载）下发给订阅了 `plugins.changed` 的
/// 插件页面——主前端的 Tauri 事件到达不了插件沙箱，插件页面经桥接订阅获知。
pub fn emit_plugins_changed() {
    let Some(emitter) = EVENT_EMITTER.get() else {
        return;
    };
    let Ok(subscriptions) = event_subscriptions().lock() else {
        return;
    };
    for (plugin_id, channels) in subscriptions.iter() {
        if channels.contains_key("plugins.changed") {
            emitter(plugin_id, "plugins.changed", "{}");
        }
    }
}

/// 订阅宿主事件通道。
///
/// channel 必须属于 [`EVENT_NAMESPACE_PREFIXES`] 登记的命名空间，且插件在
/// `capabilities.events` 中声明过对应命名空间（最小授权）。
pub fn bridge_subscribe(plugin_id: &str, channel: &str) -> Result<()> {
    if !EVENT_NAMESPACE_PREFIXES
        .iter()
        .any(|prefix| channel.starts_with(prefix))
    {
        tracing::warn!(plugin_id, channel, "bridge.on 拒绝未知 channel");
        bail!(
            "bridge.on 拒绝未知 channel {channel}（插件 {plugin_id}），可命名空间：{}",
            EVENT_NAMESPACE_PREFIXES.join(" ")
        );
    }
    let manifest = crate::registry::plugin_manifest(plugin_id)
        .ok_or_else(|| anyhow::anyhow!("bridge.on 插件 {plugin_id} 未加载"))?;
    let declared = manifest
        .capabilities
        .as_ref()
        .map(|capabilities| capabilities.events.as_slice())
        .unwrap_or_default();
    if !event_declaration_allows(declared, channel) {
        tracing::warn!(
            plugin_id,
            channel,
            "bridge.on 超出 capabilities.events 授权"
        );
        bail!("bridge.on 插件 {plugin_id} 未在 capabilities.events 声明覆盖 {channel} 的命名空间");
    }

    let mut subscriptions = event_subscriptions()
        .lock()
        .map_err(|_| anyhow::anyhow!("事件订阅表已损坏"))?;
    let channels = subscriptions.entry(plugin_id.to_string()).or_default();
    *channels.entry(channel.to_string()).or_default() += 1;
    drop(subscriptions);
    tracing::info!(plugin_id, channel, "插件订阅建立（将重放等待中的调用）");

    // 工具调用可能早于插件页面完成订阅。订阅生效后重放仍在等待的调用；
    // invocation_id 保持不变，插件可据此幂等处理并忽略重复投递。
    crate::ts_tools::replay_pending(plugin_id, channel);
    Ok(())
}

/// 取消订阅（插件 UI 卸载时调用）。
pub fn bridge_unsubscribe(plugin_id: &str, channel: &str) -> Result<()> {
    let mut subscriptions = event_subscriptions()
        .lock()
        .map_err(|_| anyhow::anyhow!("事件订阅表已损坏"))?;
    let mut channel_removed = false;
    if let Some(channels) = subscriptions.get_mut(plugin_id) {
        if let Some(count) = channels.get_mut(channel) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                channels.remove(channel);
                channel_removed = true;
            }
        }
        if channels.is_empty() {
            subscriptions.remove(plugin_id);
        }
    }
    drop(subscriptions);

    // 最后一个工具提供页面退出后，已无人能够处理挂起调用，立即取消而不是
    // 一直占用 Agent 工具任务到宿主超时。
    if channel_removed && channel == "tool.requested" {
        crate::ts_tools::cancel_plugin_calls(plugin_id);
    }
    Ok(())
}

/// 卸载插件时清空其全部订阅（贡献可逆原则）。
pub fn clear_plugin_subscriptions(plugin_id: &str) {
    if let Ok(mut subscriptions) = event_subscriptions().lock() {
        subscriptions.remove(plugin_id);
    }
}

/// 宿主事件源向已订阅插件推送事件。
///
/// `channel` 为具体事件名（如 `session.updated`），`payload` 为不透明负载。
/// 事件源在事件接缝任务接入，本接口先行定形。
pub fn bridge_emit(channel: &str, payload: &str) {
    let Some(emitter) = EVENT_EMITTER.get() else {
        return;
    };
    let targets = event_subscriptions()
        .lock()
        .map(|subscriptions| {
            subscriptions
                .iter()
                .filter(|(_, channels)| channels.contains_key(channel))
                .map(|(plugin_id, _)| plugin_id.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for plugin_id in targets {
        emitter(&plugin_id, channel, payload);
    }
}

/// 向一个已订阅的插件定向发送事件。
///
/// TS 工具调用属于声明它的插件，不能广播给其他同样订阅 `tool.*` 的插件。
pub fn bridge_emit_to(plugin_id: &str, channel: &str, payload: &str) {
    let Some(emitter) = EVENT_EMITTER.get() else {
        return;
    };
    let subscribed = event_subscriptions()
        .lock()
        .map(|subscriptions| {
            subscriptions
                .get(plugin_id)
                .is_some_and(|channels| channels.contains_key(channel))
        })
        .unwrap_or(false);
    if subscribed {
        emitter(plugin_id, channel, payload);
    } else if channel == "tool.requested" {
        tracing::info!(
            plugin_id,
            channel,
            "定向事件无订阅者，暂不投递（等待后台挂载后重放）"
        );
    }
}

/// 查询插件当前是否有存活订阅者（如判断 `tool.requested` 是否有人接应）。
pub fn plugin_has_subscriber(plugin_id: &str, channel: &str) -> bool {
    event_subscriptions()
        .lock()
        .map(|subscriptions| {
            subscriptions
                .get(plugin_id)
                .is_some_and(|channels| channels.contains_key(channel))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 命名空间识别正确() {
        assert_eq!(namespace_of("plugin.getConfig").unwrap().prefix, "plugin.");
        assert_eq!(
            namespace_of("session.getMessages").unwrap().prefix,
            "session."
        );
        assert_eq!(namespace_of("storage.read").unwrap().prefix, "storage.");
        assert!(namespace_of("rag.query").is_none());
        assert!(namespace_of("unknown").is_none());
    }

    #[test]
    fn 未知_method_错误信息可读() {
        let error = bridge_call("missing", "rag.query", "{}").unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("拒绝未知 method"));
        assert!(message.contains("rag.query"));
        assert!(message.contains("plugin."));
    }

    #[test]
    fn 事件声明匹配规则() {
        let declared = vec![
            "session.*".to_string(),
            "tool.executed".to_string(),
            "*".to_string(),
        ];
        // 上面同时声明了通配与精确，覆盖判断应任一命中
        assert!(event_declaration_allows(&declared, "session.updated"));
        assert!(event_declaration_allows(&declared, "session.a.b"));
        assert!(event_declaration_allows(&declared, "tool.executed"));
        assert!(event_declaration_allows(&declared, "anything.else"));

        let declared = vec!["session.*".to_string()];
        assert!(event_declaration_allows(&declared, "session.updated"));
        assert!(!event_declaration_allows(&declared, "tool.started"));
        assert!(!event_declaration_allows(&declared, "sessions.updated"));

        let declared = vec!["tool.executed".to_string()];
        assert!(event_declaration_allows(&declared, "tool.executed"));
        assert!(!event_declaration_allows(&declared, "tool.started"));

        assert!(!event_declaration_allows(&[], "session.updated"));
    }

    #[test]
    fn 未知_channel_拒绝() {
        let error = bridge_subscribe("missing", "rag.updated").unwrap_err();
        assert!(format!("{error:#}").contains("拒绝未知 channel"));
        assert!(format!("{error:#}").contains("session."));
    }

    #[test]
    fn 多实例退订不会移除其他实例的订阅() {
        const PLUGIN_ID: &str = "bridge-ref-count-test";
        const CHANNEL: &str = "session.ref-count-test";

        clear_plugin_subscriptions(PLUGIN_ID);
        event_subscriptions()
            .lock()
            .unwrap()
            .entry(PLUGIN_ID.to_string())
            .or_default()
            .insert(CHANNEL.to_string(), 2);

        bridge_unsubscribe(PLUGIN_ID, CHANNEL).unwrap();
        assert_eq!(
            event_subscriptions()
                .lock()
                .unwrap()
                .get(PLUGIN_ID)
                .and_then(|channels| channels.get(CHANNEL))
                .copied(),
            Some(1)
        );

        bridge_unsubscribe(PLUGIN_ID, CHANNEL).unwrap();
        assert!(
            !event_subscriptions()
                .lock()
                .unwrap()
                .contains_key(PLUGIN_ID)
        );
    }

    #[test]
    fn 权限校验规则覆盖_v1_v2() {
        use crate::manifest::PluginManifest;

        let plugin_ns = namespace_of("plugin.x").unwrap();
        let session_ns = namespace_of("session.x").unwrap();

        // v1 + 空 permissions：plugin.* 放行（等价旧 plugin_call 通道）
        let v1_empty: PluginManifest = serde_json::from_str(
            r#"{"schema_version":1,"id":"a","version":"1.0.0","wasm":{"binary":"p.wasm"},"permissions":[]}"#,
        )
        .unwrap();
        assert!(has_bridge_permission(&v1_empty, plugin_ns, "bridge.call"));
        assert!(!has_bridge_permission(
            &v1_empty,
            session_ns,
            "session.read"
        ));

        // v1 + 声明了其他权限（如 model-config.read）：plugin.* 同样放行。
        // v1 清单早于 bridge 权限体系，不能因声明过其他权限就要求 bridge.call，
        // 否则 generate-image-openai 等现有插件的设置页会被误拒。
        let v1_other_permissions: PluginManifest = serde_json::from_str(
            r#"{"schema_version":1,"id":"a","version":"1.0.0","wasm":{"binary":"p.wasm"},"permissions":["model-config.read","sidecar.invoke"]}"#,
        )
        .unwrap();
        assert!(has_bridge_permission(
            &v1_other_permissions,
            plugin_ns,
            "bridge.call"
        ));
        assert!(!has_bridge_permission(
            &v1_other_permissions,
            session_ns,
            "session.read"
        ));

        // v2：一律按声明校验
        let v2: PluginManifest = serde_json::from_str(
            r#"{"schema_version":2,"id":"a","version":"1.0.0","wasm":{"binary":"p.wasm"},"permissions":[]}"#,
        )
        .unwrap();
        assert!(!has_bridge_permission(&v2, plugin_ns, "bridge.call"));
        assert!(!has_bridge_permission(&v2, session_ns, "session.read"));

        let v2_declared: PluginManifest = serde_json::from_str(
            r#"{"schema_version":2,"id":"a","version":"1.0.0","wasm":{"binary":"p.wasm"},"permissions":["bridge.call"]}"#,
        )
        .unwrap();
        assert!(has_bridge_permission(
            &v2_declared,
            plugin_ns,
            "bridge.call"
        ));
        assert!(!has_bridge_permission(
            &v2_declared,
            session_ns,
            "session.read"
        ));
    }
}

// ── storage.* 宿主路由 ──

/// 插件私有桥接存储文件（插件 data 目录下）。
fn bridge_storage_path(directory: &std::path::Path) -> std::path::PathBuf {
    directory.join("data").join("bridge-storage.json")
}

/// 读取插件桥接存储（无文件视为空对象）。
fn load_bridge_storage(
    directory: &std::path::Path,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let path = bridge_storage_path(directory);
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("读取插件存储失败: {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("解析插件存储失败: {}", path.display()))?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        _ => Ok(serde_json::Map::new()),
    }
}

/// 写回插件桥接存储。
fn save_bridge_storage(
    directory: &std::path::Path,
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let path = bridge_storage_path(directory);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建插件存储目录失败: {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(&serde_json::Value::Object(map.clone()))?;
    std::fs::write(&path, content).with_context(|| format!("写入插件存储失败: {}", path.display()))
}

/// `storage.*` 宿主路由（设计 7.8）：插件 UI 沙箱经桥接读写私有数据，
/// 落盘在插件 data 目录（不经逻辑层，纯 UI 插件也可用）。
/// 方法：`storage.get(key)` / `storage.set(key,value)` / `storage.delete(key)` /
/// `storage.list()`；key/value 均为字符串，value 缺省按 JSON null 存取。
fn storage_call(plugin_id: &str, method: &str, payload: &str) -> Result<String> {
    let directory = crate::registry::plugin_install_directory(plugin_id)
        .ok_or_else(|| anyhow::anyhow!("bridge.call 插件 {plugin_id} 未加载"))?;
    let request: serde_json::Value =
        serde_json::from_str(payload).with_context(|| "storage 请求负载必须是 JSON 对象")?;

    let result = match method {
        "storage.get" => {
            let key = request_key(&request)?;
            load_bridge_storage(&directory)?
                .get(&key)
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        }
        "storage.set" => {
            let key = request_key(&request)?;
            let value = request
                .get("value")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let mut map = load_bridge_storage(&directory)?;
            map.insert(key, value);
            save_bridge_storage(&directory, &map)?;
            serde_json::Value::Bool(true)
        }
        "storage.delete" => {
            let key = request_key(&request)?;
            let mut map = load_bridge_storage(&directory)?;
            map.remove(&key);
            save_bridge_storage(&directory, &map)?;
            serde_json::Value::Bool(true)
        }
        "storage.list" => {
            let map = load_bridge_storage(&directory)?;
            serde_json::Value::Array(map.keys().cloned().map(serde_json::Value::String).collect())
        }
        other => bail!("未知 storage 方法 {other}（可用：get/set/delete/list）"),
    };
    serde_json::to_string(&result).with_context(|| "序列化 storage 结果失败")
}

fn request_key(request: &serde_json::Value) -> Result<String> {
    request
        .get("key")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .filter(|key| !key.trim().is_empty() && key.len() <= 256)
        .ok_or_else(|| anyhow::anyhow!("storage 请求缺少合法 key（非空、≤256 字符）"))
}

#[cfg(test)]
mod storage_tests {
    use super::*;

    #[test]
    fn 未知_storage_方法拒绝() {
        let error = storage_call("missing", "storage.clear", "{}").unwrap_err();
        // 未加载插件先失败
        assert!(format!("{error:#}").contains("未加载"));
    }

    #[test]
    fn 请求_key_校验() {
        for payload in [r#"{}"#, r#"{"key":""}"#, r#"{"key":"  "}"#] {
            let request: serde_json::Value = serde_json::from_str(payload).unwrap();
            assert!(request_key(&request).is_err());
        }
        let request: serde_json::Value = serde_json::from_str(r#"{"key":"theme"}"#).unwrap();
        assert_eq!(request_key(&request).unwrap(), "theme");
    }
}
