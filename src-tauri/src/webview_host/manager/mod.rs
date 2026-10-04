use tracing::{debug, warn};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Url, Webview, WebviewBuilder,
    WebviewUrl, Wry,
};

use crate::webview_host::bridge::{BRIDGE_SCRIPT, DOCUMENT_STATE_SCRIPT, PAGE_SNAPSHOT_SCRIPT};
use crate::webview_host::types::{
    BrowserEvent, BrowserEventsEvent, BrowserNavigationStateEvent, BrowserNavigationStateKind,
    BrowserPageLoadedEvent, BrowserPageSnapshot, BrowserResponse, BrowserTab, BrowserTabSource,
    BrowserTabsSnapshot, HistoryEntry, PageStatus, TabHistoryResult, TabListResponse,
};

mod history;
mod navigation;
mod page;
mod polling;
mod tabs;
#[cfg(test)]
mod tests;
mod url;
mod webview;

use history::*;
use navigation::*;
use tabs::*;
use url::*;
use webview::*;

/// 缩放下限：避免内容过小不可读
const MIN_ZOOM: f64 = 0.25;
/// 缩放上限：避免 WebKitGTK 高倍率渲染锯齿
const MAX_ZOOM: f64 = 5.0;
/// 天工统一判定页面加载异常的固定截止时间。
const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const PAGE_LOAD_ERROR_MESSAGE: &str = "页面未能在 30 秒内完成加载";
/// 原生页面回调偶尔不会送达；导航开始后轻量复查当前文档状态作为兜底。
const NAVIGATION_COMPLETION_PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// 后台轮询（url_poll / event_poll / ObservePage）执行 eval 的超时上限。
///
/// 复杂页面上单个 eval 可能耗时数秒；此前统一沿用 15 秒，导致多轮询线程
/// 同时挂起多个重量级 eval，webview 渲染线程饱和、桌面端冻结。缩短到 4 秒，
/// 超时后直接跳过本轮（下一轮 tick 会补），从源头避免 eval 堆积。
pub(crate) const POLL_EVAL_TIMEOUT: Duration = Duration::from_secs(4);
/// url_poll 后台线程的 tick 间隔（URL 变化检测延迟）。
const URL_POLL_TICK: Duration = Duration::from_millis(1000);
/// 后台线程现场读取 WebView URL 的等待上限：主线程繁忙时超时跳过本轮，
/// 不无限等待事件循环（见 [`read_webview_url`]）。
pub const WEBVIEW_URL_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// 无头创建矩形：任何代码路径新建 WebView 一律先落在这屏幕外坐标，
/// 展示位置只由前端显式下发（webview.instanceShow / setPosition）。
/// agent 或后台会话拉起的页面因此不会裸浮在窗口上盖住对话区。
/// 1024×720 同时决定无头页面的 viewport（影响媒体查询与布局），
/// 是执行语义的一部分，改动前先确认无头页面的布局假设。
const HEADLESS_RECT: (f64, f64, f64, f64) = (-10000.0, -10000.0, 1024.0, 720.0);

/// url_poll 内容变化检测的 tick 周期（URL_POLL_TICK 的倍数）。
const URL_POLL_CONTENT_TICKS: u32 = 8;
/// full_text 缓存的有效期：TTL 内多个轮询线程的 getFullText 请求复用同一结果，
/// 避免并发遍历 DOM。
const FULL_TEXT_CACHE_TTL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavigationPhase {
    Loading,
    Loaded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NavigationIntent {
    Normal,
    History { target_index: usize },
    Reload,
    Retry,
    Restore,
}

#[derive(Debug, Clone)]
struct TabNavigationState {
    navigation_id: u64,
    requested_url: String,
    started_url: Option<String>,
    document_id: Option<String>,
    superseded_document_ids: Vec<String>,
    final_url: Option<String>,
    history_index: Option<usize>,
    phase: NavigationPhase,
    internal_error_url: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct WebDocumentSnapshot {
    #[serde(default)]
    document_id: String,
    #[serde(default)]
    ready_state: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    has_content: bool,
    #[serde(default)]
    internal_error: bool,
}

struct NavigationSignal {
    state: Mutex<TabNavigationState>,
    cvar: Condvar,
}

#[derive(Debug, Clone)]
pub(crate) struct NavigationTicket {
    pub tab_id: String,
    pub navigation_id: u64,
}

/// 浏览器 WebView 的共享状态
///
/// # 锁约束
///
/// `on_page_load` 等回调在主线程上获取本锁。因此**持有本锁期间禁止调用任何
/// 会同步等待主线程的接口**（`Webview::url()` / `bounds()` / `position()`、
/// `Window::add_child`、菜单项 getter/setter 等），否则后台线程持锁等主线程、
/// 主线程等锁，形成死锁。需要这些信息时先在锁内克隆 `Webview` 句柄，释放
/// 锁后再调用（URL 用 [`read_webview_url`] 限时读取）。
pub struct BrowserState {
    /// 每个标签页对应的独立 WebView 实例
    pub webviews: HashMap<String, Webview<Wry>>,
    /// 每个标签页当前导航的编号、状态和等待信号。
    navigation_signals: HashMap<String, Arc<NavigationSignal>>,
    /// 每个标签页的最近一次页面快照
    pub latest_snapshots: HashMap<String, BrowserPageSnapshot>,
    /// 轮询检测的最后一次已知 URL
    pub last_known_url: String,
    /// 轮询检测的最后一次内容签名（前 500 字符）
    pub last_known_text_signature: String,
    /// 轮询线程停止信号
    pub poll_stop: Arc<std::sync::atomic::AtomicBool>,
    /// 事件消费线程停止信号
    pub event_poll_stop: Arc<std::sync::atomic::AtomicBool>,
    /// 浏览器"活跃"开关（并非窗口可见性）：控制 url/event 后台轮询是否
    /// 产出页面数据与事件。页面在窗口中的实际呈现只由展示矩形决定，
    /// 与本标记无关——无头页面（后台会话/面板收起）同样可为 true。
    pub visible: Arc<std::sync::atomic::AtomicBool>,
    /// 已由后台事件线程读取、等待 Agent 消费的浏览器事件
    pub pending_events: Vec<BrowserEvent>,
    /// 标签列表
    pub tabs: Vec<BrowserTab>,
    /// 前端拓展区中实际存在的浏览器标签。
    pub mounted_tab_ids: HashSet<String>,
    /// 活跃标签 ID
    pub active_tab_id: Option<String>,
    /// 当前可见区域 (x, y, w, h)，用于标签切换时定位新 WebView
    pub browser_rect: (f64, f64, f64, f64),
    /// 进程级共享的浏览历史和缩放设置。
    pub(crate) shared: Arc<BrowserSharedState>,
    /// 每个标签页的浏览历史栈
    pub tab_histories: HashMap<String, Vec<HistoryEntry>>,
    /// 每个标签页当前在历史栈中的位置
    pub tab_history_indices: HashMap<String, usize>,
    /// 该 state 所属的 session id（registry 创建时注入，不可变，作为可靠标识）。
    /// 用于 create_webview 的 data_dir、webview label 和 global_history 路由。
    pub session_id: String,
    /// 当前浏览器运行时绑定的对话会话 ID（兼容旧字段，T5 后由 registry.active_session_id 取代）
    pub active_session_id: Option<String>,
    /// 最近一次 `getFullText` 的结果与时间戳，用于跨线程去重。
    ///
    /// url_poll、event_poll、watcher(ObservePage) 都可能调用 getFullText，
    /// 复杂页面上单次执行耗时数秒。TTL 内的重复请求直接返回缓存，
    /// 避免多线程并发遍历 DOM 导致渲染线程饱和。
    pub(crate) full_text_cache: Mutex<Option<(Instant, String)>>,
}

/// 浏览器进程级共享状态。所有 session 通过同一个实例读写，避免磁盘共享但内存分叉。
pub(crate) struct BrowserSharedState {
    global_history: Mutex<Vec<HistoryEntry>>,
    zoom_factor: Mutex<f64>,
}

impl BrowserSharedState {
    pub(crate) fn load() -> Self {
        Self {
            global_history: Mutex::new(load_global_history()),
            zoom_factor: Mutex::new(load_zoom()),
        }
    }
}

impl BrowserState {
    /// 构造一个空的 per-session 状态（不含 webview/tab/历史）。
    ///
    /// `shared` 由 [`BrowserSessionRegistry`](crate::webview_host::session_registry::BrowserSessionRegistry)
    /// 创建并注入，所有 session 共用同一份全局历史和缩放设置。
    pub(crate) fn new_empty(session_id: String, shared: Arc<BrowserSharedState>) -> Self {
        Self {
            webviews: HashMap::new(),
            navigation_signals: HashMap::new(),
            latest_snapshots: HashMap::new(),
            last_known_url: String::new(),
            last_known_text_signature: String::new(),
            poll_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            event_poll_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            visible: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            pending_events: Vec::new(),
            tabs: Vec::new(),
            mounted_tab_ids: HashSet::new(),
            active_tab_id: None,
            browser_rect: (0.0, 0.0, 0.0, 0.0),
            shared,
            tab_histories: HashMap::new(),
            tab_history_indices: HashMap::new(),
            session_id,
            active_session_id: None,
            full_text_cache: Mutex::new(None),
        }
    }

    fn active_webview(&self) -> Option<&Webview<Wry>> {
        let active_id = self.active_tab_id.as_ref()?;
        self.webviews.get(active_id)
    }
}

#[derive(Clone)]
pub struct BrowserManager {
    pub(crate) state: Arc<Mutex<BrowserState>>,
}

impl Default for BrowserManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserManager {
    pub fn new() -> Self {
        let shared = Arc::new(BrowserSharedState::load());
        Self {
            state: Arc::new(Mutex::new(BrowserState {
                webviews: HashMap::new(),
                navigation_signals: HashMap::new(),
                latest_snapshots: HashMap::new(),
                last_known_url: String::new(),
                last_known_text_signature: String::new(),
                poll_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                event_poll_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                visible: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                pending_events: Vec::new(),
                tabs: Vec::new(),
                mounted_tab_ids: HashSet::new(),
                active_tab_id: None,
                browser_rect: (0.0, 0.0, 0.0, 0.0),
                shared,
                tab_histories: HashMap::new(),
                tab_history_indices: HashMap::new(),
                session_id: String::new(),
                active_session_id: None,
                full_text_cache: Mutex::new(None),
            })),
        }
    }

    pub fn clone_state(&self) -> Arc<Mutex<BrowserState>> {
        self.state.clone()
    }

    fn shared_state(&self) -> Arc<BrowserSharedState> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shared
            .clone()
    }

    /// 绑定到指定 session state 构造 manager（per-session 路由用）。
    ///
    /// manager 的全部方法操作该 state；多 manager 可并存，各绑各的 session。
    pub fn from_state(state: Arc<Mutex<BrowserState>>) -> Self {
        Self { state }
    }

    /// 浏览器是否已初始化（有标签即为已打开，包括 about:blank 延迟创建 WebView 的情况）
    pub fn is_open(&self) -> bool {
        self.state
            .lock()
            .map(|s| !s.tabs.is_empty())
            .unwrap_or(false)
    }

    pub fn is_visible(&self) -> bool {
        self.state
            .lock()
            .map(|s| s.visible.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
    }

    pub fn set_visible(&self, visible: bool) {
        if let Ok(s) = self.state.lock() {
            s.visible
                .store(visible, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn set_mounted_tabs(&self, tab_ids: Vec<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.mounted_tab_ids = tab_ids.into_iter().collect();
        }
    }

    pub fn is_tab_mounted(&self, tab_id: &str) -> bool {
        self.state
            .lock()
            .map(|state| state.mounted_tab_ids.contains(tab_id))
            .unwrap_or(false)
    }

    /// 当前页面缩放比例（来自持久化状态）
    pub fn zoom(&self) -> f64 {
        let shared = self.shared_state();
        let zoom = shared.zoom_factor.lock().unwrap_or_else(|e| e.into_inner());
        *zoom
    }

    /// 设置缩放：clamp 到 [MIN_ZOOM, MAX_ZOOM]，同步到所有 webview 并持久化，返回生效值
    pub fn set_zoom(&self, scale: f64) -> Result<f64, String> {
        let clamped = scale.clamp(MIN_ZOOM, MAX_ZOOM);
        let shared = self.shared_state();
        {
            let mut zoom = shared
                .zoom_factor
                .lock()
                .map_err(|e| format!("锁浏览器缩放设置失败：{e}"))?;
            if (*zoom - clamped).abs() < f64::EPSILON {
                return Ok(clamped);
            }
            *zoom = clamped;
        }
        {
            let s = self
                .state
                .lock()
                .map_err(|e| format!("锁 BrowserState 失败：{e}"))?;
            for webview in s.webviews.values() {
                if let Err(e) = webview.set_zoom(clamped) {
                    warn!(error = %e, "webview set_zoom 失败");
                }
            }
        }
        persist_zoom(&shared);
        Ok(clamped)
    }

    /// 重置缩放到 1.0
    pub fn reset_zoom(&self) -> Result<f64, String> {
        self.set_zoom(1.0)
    }
}
