//! webview 引擎宿主（原 `tiangong-plugin-browser` crate 的引擎部分）。
//!
//! 终端/浏览器插件化后，浏览器业务语义（工具策略、标签界面）全部上移
//! `plugins/tiangong-plugin-browser`；本模块只保留桌面宿主的中立能力：
//! webview 实例管理、页面事件 watcher、CDP 抓取与协作命令通道，供
//! `webview.*` 桥接原语（插件 UI 与工具壳）驱动。

use std::sync::Arc;

use tauri::{Manager, Wry};
use tokio::sync::mpsc;

use crate::webview_host::handler::browser_command_handler;
use crate::webview_host::manager::BrowserManager;
use crate::webview_host::session_registry::BrowserSessionRegistry;
use crate::webview_host::types::BrowserCommand;

pub mod bridge;
pub mod capability;
pub(crate) mod fetch_scheduler;
pub mod handler;
pub mod manager;
pub mod page_fetcher;
pub mod session_registry;
pub mod session_store;
pub mod types;
pub mod watcher;

// ── 插件事件转发（页面事件 → 插件 UI）──
//
// 宿主把页面状态变化（加载完成/失败、标题与 URL 更新）定向投递给持有
// 对应 webview 作用域的插件 UI，经 runtime 的订阅表（bridge_emit_to）
// 送达；与 sidecar 通知转发器同一模式。插件端订阅 `webview.event`。

/// 插件事件转发器：`(plugin_id, channel, payload_json)`，由桌面入口注入。
pub type PluginEventForwarder = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;
static PLUGIN_EVENT_FORWARDER: std::sync::OnceLock<PluginEventForwarder> =
    std::sync::OnceLock::new();

/// 注入转发器（桌面入口启动时调用一次）。
pub fn set_plugin_event_forwarder(forwarder: PluginEventForwarder) {
    let _ = PLUGIN_EVENT_FORWARDER.set(forwarder);
}

/// 按 webview 作用域（`webview:<插件>[:<会话>]`）把页面事件投给对应插件。
/// 未注入或作用域格式异常时静默跳过。
pub fn emit_plugin_event(scope: &str, event: &str, payload: &serde_json::Value) {
    let Some(forwarder) = PLUGIN_EVENT_FORWARDER.get() else {
        return;
    };
    let Some(plugin_id) = scope
        .strip_prefix("webview:")
        .and_then(|rest| rest.split(':').next())
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let wrapped = serde_json::json!({
        "event": event,
        "scope": scope,
        "payload": payload,
    });
    forwarder(plugin_id, "webview.event", &wrapped.to_string());
}

/// webview 引擎共享状态。
///
/// `registry` 持有所有作用域的 `BrowserState`（按 `webview:<插件>[:<会话>]`
/// 隔离）；`cmd_tx` 是协作命令通道（fetch/queryDom/click 等经 handler 消费）。
pub struct WebviewHostState {
    pub registry: Arc<BrowserSessionRegistry>,
    pub cmd_tx: mpsc::Sender<BrowserCommand>,
}

impl WebviewHostState {
    /// 返回绑定到指定作用域的 manager。
    pub fn manager_for(&self, scope: &str) -> BrowserManager {
        BrowserManager::from_state(self.registry.session_state(scope))
    }
}

/// 初始化 webview 引擎宿主：注册共享状态并启动协作命令消费循环。
///
/// 桌面入口 setup 阶段调用一次；必须在 webview.* 原语接线之前完成。
pub fn init(app: &tauri::AppHandle<Wry>) {
    let (tx, rx) = mpsc::channel::<BrowserCommand>(16);
    let registry = Arc::new(BrowserSessionRegistry::new());
    app.manage(WebviewHostState {
        registry,
        cmd_tx: tx,
    });
    let state = app.state::<WebviewHostState>();
    let browser_registry = state.registry.clone();
    tauri::async_runtime::spawn(browser_command_handler(rx, browser_registry, app.clone()));
}
