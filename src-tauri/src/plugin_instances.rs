//! 插件实例生命周期（宿主统一编排）。
//!
//! 原则：拓展区标签是实例资源的唯一所有者。归属分两层——作用域
//! `(plugin_id, session_id)` 与实例编号 `instance_id`（即标签编号）。
//!
//! - 关闭：标签移除时宿主保证向资源方发出一次 `instanceClosed`；
//! - 会话切换：离开的会话隐藏全部 webview 实例；进入的会话按
//!   `listInstances` 以同一编号恢复标签，随后核查并释放无标签的多余资源；
//! - 逻辑删除会话：释放该会话下全部实例资源。
//!
//! 资源方按沙箱区分：webview 沙箱的资源（页面）由宿主 webview 层持有，
//! 其余声明 `instance_resources` 的插件由其 sidecar 持有并实现
//! `instanceClosed` / `listInstances` 协议。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Manager, Wry};
use tiangong_plugin_runtime::protocol::{
    FRONTEND_ATTACH_WAIT_MS, INSTANCE_CLOSED_OPERATION, LIST_INSTANCES_OPERATION,
};
use tiangong_plugin_runtime::registry::ExtensionApp;
use tiangong_plugin_runtime::SandboxKind;
use tracing::warn;

use crate::webview_host::WebviewHostState;

/// 核查宽限期：刚经 app.open 建立（前端可能尚未落地标签）的实例不视为
/// 多余资源。取挂载上限的若干倍，覆盖事件往返与前端渲染延迟。
const RECONCILE_GRACE: Duration = Duration::from_millis(FRONTEND_ATTACH_WAIT_MS * 5);

/// 实例恢复条目：前端据此以同一编号重建标签。
#[derive(Debug, Clone, Serialize)]
pub struct PluginInstanceEntry {
    pub plugin_id: String,
    pub contribution_id: String,
    pub title: String,
    pub sandbox: SandboxKind,
    pub instance_id: String,
    /// webview 实例的页面地址与标题（其他沙箱为空）。
    pub url: String,
    pub page_title: String,
}

/// 前端当前持有的标签（核查用）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LiveInstance {
    pub plugin_id: String,
    pub instance_id: String,
}

/// (plugin_id, session_id, instance_id) → 最近一次 app.open 时刻。
type RecentOpens = Mutex<HashMap<(String, String, String), Instant>>;

fn recent_opens() -> &'static RecentOpens {
    static OPENS: OnceLock<RecentOpens> = OnceLock::new();
    OPENS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 记录一次带实例编号的 app.open（核查宽限期依据）。
pub fn record_open(plugin_id: &str, session_id: &str, instance_id: &str) {
    if session_id.is_empty() || instance_id.is_empty() {
        return;
    }
    let mut opens = recent_opens().lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    opens.retain(|_, at| now.duration_since(*at) < RECONCILE_GRACE);
    opens.insert(
        (
            plugin_id.to_string(),
            session_id.to_string(),
            instance_id.to_string(),
        ),
        now,
    );
}

fn opened_recently(plugin_id: &str, session_id: &str, instance_id: &str) -> bool {
    let opens = recent_opens().lock().unwrap_or_else(|e| e.into_inner());
    opens
        .get(&(
            plugin_id.to_string(),
            session_id.to_string(),
            instance_id.to_string(),
        ))
        .is_some_and(|at| at.elapsed() < RECONCILE_GRACE)
}

/// 宿主生成的实例编号（scru128）。
pub fn new_instance_id() -> String {
    scru128::new().to_string()
}

/// 解析 webview 作用域 `webview:{plugin_id}:{session_id}`。
pub fn parse_webview_scope(scope: &str) -> Option<(String, String)> {
    let (plugin_id, session_id) = scope.strip_prefix("webview:")?.split_once(':')?;
    if plugin_id.is_empty() || session_id.is_empty() {
        return None;
    }
    Some((plugin_id.to_string(), session_id.to_string()))
}

/// 宿主为已预留编号的实例资源建立前端标签（app.open，带 session_id 与
/// instance_id）。`show_panel=false` 时静默建立（不展开面板）。
pub fn request_open(plugin_id: &str, session_id: &str, instance_id: &str, show_panel: bool) {
    record_open(plugin_id, session_id, instance_id);
    let mut payload = serde_json::json!({
        "session_id": session_id,
        "instance_id": instance_id,
    });
    if !show_panel {
        payload["mode"] = serde_json::json!("background");
    }
    if let Err(error) = tiangong_plugin_runtime::request_app_open(plugin_id, &payload.to_string()) {
        warn!(plugin_id, session_id, instance_id, %error, "请求建立实例标签失败");
    }
}

/// 等待前端挂载实例标签（上限 FRONTEND_ATTACH_WAIT_MS），返回是否已挂载。
/// 超时不视为错误：资源照常使用，标签在会话切回时按 listInstances 恢复。
pub fn wait_mounted(is_mounted: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(FRONTEND_ATTACH_WAIT_MS);
    loop {
        if is_mounted() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn webview_scope(plugin_id: &str, session_id: &str) -> String {
    format!("webview:{plugin_id}:{session_id}")
}

fn find_app(plugin_id: &str) -> Option<ExtensionApp> {
    tiangong_plugin_runtime::registry::list_extension_apps()
        .into_iter()
        .find(|app| app.plugin_id == plugin_id)
}

/// 持有实例资源的 App：webview 沙箱（宿主持有页面）或显式声明
/// `instance_resources` 的插件。
fn resource_apps() -> Vec<ExtensionApp> {
    tiangong_plugin_runtime::registry::list_extension_apps()
        .into_iter()
        .filter(|app| app.sandbox == SandboxKind::Webview || app.instance_resources)
        .collect()
}

/// 按会话解析 sidecar 连接使用的权威工作区（与 bridge_call 一致）。
fn session_workspace(app: &AppHandle<Wry>, session_id: &str) -> Option<PathBuf> {
    let state = app.try_state::<crate::app::TiangongApp>()?;
    let session = state.core_manager.load_session(session_id).ok()?;
    let cwd = session.cwd.trim();
    (!cwd.is_empty()).then(|| PathBuf::from(cwd))
}

fn sidecar_list(
    plugin_id: &str,
    session_id: &str,
    workspace: Option<&std::path::Path>,
) -> anyhow::Result<Vec<String>> {
    let result = tiangong_plugin_runtime::invoke_plugin_sidecar(
        plugin_id,
        LIST_INSTANCES_OPERATION,
        serde_json::json!({ "session_id": session_id }),
        workspace,
    )?;
    Ok(result["instances"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["instance_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

fn sidecar_close(
    plugin_id: &str,
    session_id: &str,
    instance_id: &str,
    workspace: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    tiangong_plugin_runtime::invoke_plugin_sidecar(
        plugin_id,
        INSTANCE_CLOSED_OPERATION,
        serde_json::json!({ "session_id": session_id, "instance_id": instance_id }),
        workspace,
    )?;
    Ok(())
}

fn webview_close(app: &AppHandle<Wry>, plugin_id: &str, session_id: &str, instance_id: &str) {
    let Some(state) = app.try_state::<WebviewHostState>() else {
        return;
    };
    let Some(scope_state) = state
        .registry
        .existing_session_state(&webview_scope(plugin_id, session_id))
    else {
        return;
    };
    let manager = crate::webview_host::manager::BrowserManager::from_state(scope_state);
    // 幂等：标签已不存在视为已关闭。
    if manager.tab_close(instance_id).is_err() {
        return;
    }
    notify_browser_tabs_changed(app, session_id, &manager);
}

/// 页面关闭后向会话补投一条最新标签状态（状态快照，顶替会话延迟队列中
/// 已过期的页面快照），避免 Agent 下一轮看到已关闭页面的内容。
fn notify_browser_tabs_changed(
    app: &AppHandle<Wry>,
    session_id: &str,
    manager: &crate::webview_host::manager::BrowserManager,
) {
    let Some(state) = app.try_state::<crate::app::TiangongApp>() else {
        return;
    };
    let tab_list = manager.tab_list_with_active();
    // 最后一个页面关闭后无任何数据：不补投（空快照没有信息量）。
    if tab_list.tabs.is_empty() {
        return;
    }
    let active = tab_list
        .active_tab_id
        .as_ref()
        .and_then(|id| tab_list.tabs.iter().find(|tab| &tab.id == id))
        .cloned();
    let _ = state.tool_injection_tx().send(crate::ToolInjection {
        session_id: Some(session_id.to_string()),
        browser_source: None,
        snapshot: true,
        tool: Box::new(crate::webview_host::page_fetcher::BrowserContent {
            title: active
                .as_ref()
                .map(|tab| tab.title.clone())
                .unwrap_or_default(),
            url: active
                .as_ref()
                .map(|tab| tab.url.clone())
                .unwrap_or_default(),
            text: String::new(),
            tabs: tab_list
                .tabs
                .into_iter()
                .map(|tab| (tab.id, tab.url, tab.title))
                .collect(),
            active_tab_id: tab_list.active_tab_id,
            feedback: None,
        }),
    });
}

/// 标签移除后的唯一释放入口：向资源方发出 instanceClosed（幂等）。
pub fn instance_closed(
    app: &AppHandle<Wry>,
    plugin_id: &str,
    session_id: &str,
    instance_id: &str,
) -> anyhow::Result<()> {
    let Some(entry) = find_app(plugin_id) else {
        return Ok(());
    };
    if entry.sandbox == SandboxKind::Webview {
        webview_close(app, plugin_id, session_id, instance_id);
        return Ok(());
    }
    if !entry.instance_resources {
        return Ok(());
    }
    let workspace = session_workspace(app, session_id);
    sidecar_close(plugin_id, session_id, instance_id, workspace.as_deref())
}

fn list_for_app(
    app: &AppHandle<Wry>,
    entry: &ExtensionApp,
    session_id: &str,
    workspace: Option<&std::path::Path>,
) -> Vec<PluginInstanceEntry> {
    let make = |instance_id: String, url: String, page_title: String| PluginInstanceEntry {
        plugin_id: entry.plugin_id.clone(),
        contribution_id: entry.contribution_id.clone(),
        title: entry.title.clone(),
        sandbox: entry.sandbox,
        instance_id,
        url,
        page_title,
    };
    if entry.sandbox == SandboxKind::Webview {
        let Some(state) = app.try_state::<WebviewHostState>() else {
            return Vec::new();
        };
        let Some(scope_state) = state
            .registry
            .existing_session_state(&webview_scope(&entry.plugin_id, session_id))
        else {
            return Vec::new();
        };
        let snapshot =
            crate::webview_host::manager::BrowserManager::from_state(scope_state).snapshot_tabs();
        return snapshot
            .tabs
            .into_iter()
            .map(|tab| make(tab.id, tab.url, tab.title))
            .collect();
    }
    match sidecar_list(&entry.plugin_id, session_id, workspace) {
        Ok(ids) => ids
            .into_iter()
            .map(|id| make(id, String::new(), String::new()))
            .collect(),
        Err(error) => {
            // 插件未启用、sidecar 未就绪等：无可恢复实例。
            tracing::debug!(plugin_id = %entry.plugin_id, session_id, %error, "listInstances 失败");
            Vec::new()
        }
    }
}

/// 列出会话下全部持有资源的实例（切换会话时恢复标签）。
pub fn list_instances(app: &AppHandle<Wry>, session_id: &str) -> Vec<PluginInstanceEntry> {
    if session_id.trim().is_empty() {
        return Vec::new();
    }
    let workspace = session_workspace(app, session_id);
    resource_apps()
        .iter()
        .flat_map(|entry| list_for_app(app, entry, session_id, workspace.as_deref()))
        .collect()
}

/// 核查：资源方持有、但前端没有对应标签（且不在宽限期内）的实例直接
/// 释放并记 warn。返回被释放的实例编号。
pub fn reconcile(
    app: &AppHandle<Wry>,
    session_id: &str,
    live: &[LiveInstance],
) -> Vec<(String, String)> {
    let mut released = Vec::new();
    if session_id.trim().is_empty() {
        return released;
    }
    let workspace = session_workspace(app, session_id);
    for entry in resource_apps() {
        for instance in list_for_app(app, &entry, session_id, workspace.as_deref()) {
            let owned = live.iter().any(|item| {
                item.plugin_id == entry.plugin_id && item.instance_id == instance.instance_id
            });
            if owned || opened_recently(&entry.plugin_id, session_id, &instance.instance_id) {
                continue;
            }
            warn!(
                plugin_id = %entry.plugin_id,
                session_id,
                instance_id = %instance.instance_id,
                "实例资源无标签归属，直接释放"
            );
            if entry.sandbox == SandboxKind::Webview {
                webview_close(app, &entry.plugin_id, session_id, &instance.instance_id);
            } else if let Err(error) = sidecar_close(
                &entry.plugin_id,
                session_id,
                &instance.instance_id,
                workspace.as_deref(),
            ) {
                warn!(plugin_id = %entry.plugin_id, session_id, instance_id = %instance.instance_id, %error, "释放多余实例资源失败");
                continue;
            }
            released.push((entry.plugin_id.clone(), instance.instance_id));
        }
    }
    released
}

/// 会话离开前台：隐藏该会话全部 webview 实例，并撤销页面挂载登记。
pub fn detach_session(app: &AppHandle<Wry>, session_id: &str) {
    let Some(state) = app.try_state::<WebviewHostState>() else {
        return;
    };
    for entry in resource_apps() {
        if entry.sandbox != SandboxKind::Webview {
            continue;
        }
        let Some(scope_state) = state
            .registry
            .existing_session_state(&webview_scope(&entry.plugin_id, session_id))
        else {
            continue;
        };
        let manager = crate::webview_host::manager::BrowserManager::from_state(scope_state);
        manager.set_mounted_tabs(Vec::new());
        let _ = manager.hide();
    }
}

/// 预先解析会话工作区（逻辑删除前调用：删除后会话不可加载）。
pub fn prepare_release(app: &AppHandle<Wry>, session_id: &str) -> Option<PathBuf> {
    session_workspace(app, session_id)
}

/// 逻辑删除会话：释放该会话下全部实例资源（webview 页面与 sidecar 实例）。
pub fn release_session(app: &AppHandle<Wry>, session_id: &str, workspace: Option<PathBuf>) {
    if session_id.trim().is_empty() {
        return;
    }
    if let Some(state) = app.try_state::<WebviewHostState>() {
        state.registry.destroy_session(session_id);
    }
    for entry in resource_apps() {
        if entry.sandbox == SandboxKind::Webview {
            continue;
        }
        let ids = match sidecar_list(&entry.plugin_id, session_id, workspace.as_deref()) {
            Ok(ids) => ids,
            Err(error) => {
                tracing::debug!(plugin_id = %entry.plugin_id, session_id, %error, "删除会话时 listInstances 失败");
                continue;
            }
        };
        for instance_id in ids {
            if let Err(error) = sidecar_close(
                &entry.plugin_id,
                session_id,
                &instance_id,
                workspace.as_deref(),
            ) {
                warn!(plugin_id = %entry.plugin_id, session_id, instance_id, %error, "删除会话释放实例资源失败");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 宽限期内的实例不视为多余() {
        record_open("p", "s", "i-1");
        assert!(opened_recently("p", "s", "i-1"));
        assert!(!opened_recently("p", "s", "i-2"));
        assert!(!opened_recently("p", "other", "i-1"));
    }

    #[test]
    fn 实例编号为_scru128() {
        let id = new_instance_id();
        assert!(id.parse::<scru128::Id>().is_ok());
    }

    #[test]
    fn 解析_webview_作用域() {
        assert_eq!(
            parse_webview_scope("webview:browser:s-1"),
            Some(("browser".to_string(), "s-1".to_string()))
        );
        assert_eq!(parse_webview_scope("webview:browser"), None);
        assert_eq!(parse_webview_scope("s-1"), None);
        assert_eq!(parse_webview_scope("webview::s"), None);
    }

    #[test]
    fn 挂载等待有上限() {
        let start = Instant::now();
        assert!(!wait_mounted(|| false));
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(FRONTEND_ATTACH_WAIT_MS));
        assert!(waited < Duration::from_millis(FRONTEND_ATTACH_WAIT_MS * 3));
        assert!(wait_mounted(|| true));
    }
}
