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
use crate::webview_host::passkey;
use crate::webview_host::types::{
    BrowserEvent, BrowserEventsEvent, BrowserNavigationStateEvent, BrowserNavigationStateKind,
    BrowserPageLoadedEvent, BrowserPageSnapshot, BrowserResponse, BrowserTab, BrowserTabSource,
    BrowserTabsSnapshot, HistoryEntry, PageStatus, TabHistoryResult, TabListResponse,
};

/// 规范化标识符用于 webview label（避免特殊字符）。
fn sanitize_path_segment(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// webview 标签仅在进程内标识，tab_id 全局唯一（SCRU128）兜底不撞车，
/// 因此用替换式清洗即可；数据目录名（browser_session_directory_name）会
/// 落盘且不同 scope 必须不碰撞，用百分号编码——两处规则刻意不统一，
/// 勿合并。
fn webview_label(session_id: &str, tab_id: &str) -> String {
    format!(
        "browser-webview-{}-{}",
        sanitize_path_segment(session_id),
        sanitize_path_segment(tab_id)
    )
}

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

/// 无头创建矩形：任何代码路径新建 WebView 一律先落在这屏幕外坐标，
/// 展示位置只由前端显式下发（webview.instanceShow / setPosition）。
/// agent 或后台会话拉起的页面因此不会裸浮在窗口上盖住对话区。
/// 1024×720 同时决定无头页面的 viewport（影响媒体查询与布局），
/// 是执行语义的一部分，改动前先确认无头页面的布局假设。
const HEADLESS_RECT: (f64, f64, f64, f64) = (-10000.0, -10000.0, 1024.0, 720.0);

/// 展示矩形是否可直接用于摆放 WebView（初始值 (0,0,0,0) 宽高无效）。
fn rect_is_displayable(rect: (f64, f64, f64, f64)) -> bool {
    rect.2 > 0.0 && rect.3 > 0.0
}

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

fn navigation_signal(url: &str) -> Arc<NavigationSignal> {
    Arc::new(NavigationSignal {
        state: Mutex::new(TabNavigationState {
            navigation_id: 0,
            requested_url: url.to_string(),
            started_url: None,
            document_id: None,
            superseded_document_ids: Vec::new(),
            final_url: Some(url.to_string()),
            history_index: None,
            phase: NavigationPhase::Loaded,
            internal_error_url: None,
        }),
        cvar: Condvar::new(),
    })
}

/// 规范化 URL 用于比较：去除末尾的 /，统一 https://
fn normalize_url_for_compare(url: &str) -> String {
    let s = url.trim_end_matches('/');
    s.to_string()
}

/// 构造中的 file URL 先编码会破坏结构的字符再经 Url 解析规范化编码
/// （空格/非 ASCII 等）；# 与 ? 不预编码会被切成 fragment/query。
fn canonical_file_url(candidate: String) -> String {
    let encoded = candidate
        .replace('%', "%25")
        .replace('#', "%23")
        .replace('?', "%3F");
    match encoded.parse::<Url>() {
        Ok(url) => url.to_string(),
        Err(_) => encoded,
    }
}

/// 裸本地路径转标准 file URL；非本地路径形式原样返回。
/// Windows 盘符路径（`C:\x`、`c:/x`，允许混合斜杠）与 UNC 路径
/// （`\\server\share\x`）按平台无关规则转写，盘符统一大写。
fn local_path_to_file_url(value: &str) -> String {
    // UNC 网络路径：反斜杠写法各平台一致按主机名处理；正斜杠写法
    // （//server/share）在 Windows 生态常见，但 Unix 上前导双斜杠是
    // implementation-defined 的本地路径，因此仅 Windows 按 UNC 归一。
    #[cfg(windows)]
    if let Some(rest) = value.strip_prefix("//") {
        return canonical_file_url(format!("file://{}", rest.replace('\\', "/")));
    }
    if let Some(rest) = value.strip_prefix(r"\\") {
        return canonical_file_url(format!("file://{}", rest.replace('\\', "/")));
    }
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        let drive = bytes[0].to_ascii_uppercase() as char;
        return canonical_file_url(format!("file:///{drive}:{}", value[2..].replace('\\', "/")));
    }
    if bytes.first() == Some(&b'/') {
        return canonical_file_url(format!("file:///{}", value.trim_start_matches('/')));
    }
    value.to_string()
}

/// 导航地址统一归一化：本地路径（Windows 盘符 / UNC / Unix 绝对路径）
/// 与 file: 变体（`file:C:/x`、`file://C:/x`）收敛为标准三斜杠形式并
/// 大写盘符（对齐 Chromium 实际地址，保证导航状态比较一致）；其余
/// 地址原样返回。幂等：已是标准形式的输入输出不变。
pub(crate) fn normalize_navigation_url(raw: &str) -> String {
    let value = raw.trim();
    if value
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
    {
        if let Ok(parsed) = value.parse::<Url>() {
            let path = parsed.path();
            if parsed.host_str().is_none() {
                let bytes = path.as_bytes();
                if bytes.len() >= 3
                    && bytes[0] == b'/'
                    && bytes[1].is_ascii_alphabetic()
                    && bytes[2] == b':'
                {
                    // 盘符统一大写时重拼字符串，query 与 fragment 需一并带回
                    let drive = bytes[1].to_ascii_uppercase() as char;
                    let mut rebuilt = format!("file:///{drive}{}", &path[2..]);
                    if let Some(query) = parsed.query() {
                        rebuilt.push('?');
                        rebuilt.push_str(query);
                    }
                    if let Some(fragment) = parsed.fragment() {
                        rebuilt.push('#');
                        rebuilt.push_str(fragment);
                    }
                    return rebuilt;
                }
            }
            return parsed.to_string();
        }
        // 解析失败的 file: 变体剥掉协议与斜杠后按裸路径重建兜底。
        // 注意与裸路径的语义差异：此处 % 视为已编码序列原样保留，
        // 裸路径则把字面 % 预编码为 %25——两种写法约定不同，勿"统一"。
        return local_path_to_file_url(value[5..].trim_start_matches('/'));
    }
    local_path_to_file_url(value)
}

fn push_recent_unique(values: &mut Vec<String>, value: String) {
    if value.is_empty() || values.iter().any(|item| item == &value) {
        return;
    }
    values.push(value);
    const MAX_RECENT_VALUES: usize = 16;
    if values.len() > MAX_RECENT_VALUES {
        values.drain(0..values.len() - MAX_RECENT_VALUES);
    }
}

fn remember_superseded_navigation(navigation: &mut TabNavigationState) {
    if let Some(document_id) = navigation.document_id.take() {
        push_recent_unique(&mut navigation.superseded_document_ids, document_id);
    }
}

fn parse_web_document_snapshot(result: &str) -> Option<WebDocumentSnapshot> {
    serde_json::from_str(result).ok()
}

/// 图片、PDF、音视频、字体等按扩展名识别的非 HTML 资源地址。此类资源页
/// 没有可注入脚本的 HTML 文档（eval 无响应），不能走 DOM 快照流程。
/// SVG 例外：浏览器中按 XML 文档渲染，有 DOM 且可执行脚本，按正常页面处理。
fn is_non_html_resource_url(url: &str) -> bool {
    let Ok(parsed) = url.parse::<Url>() else {
        return false;
    };
    let Some(extension) = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(|name| name.rsplit_once('.').map(|(_, extension)| extension))
    else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "avif"
            | "ico"
            | "bmp"
            | "pdf"
            | "mp4"
            | "webm"
            | "mov"
            | "mkv"
            | "ogg"
            | "mp3"
            | "wav"
            | "flac"
            | "m4a"
            | "woff"
            | "woff2"
            | "ttf"
            | "otf"
    )
}

/// 展示用百分号解码：文件名等 UI 文本按 UTF-8 解回原文；
/// 非法编码序列原样保留，不因解码失败丢信息。
fn percent_decode_lossy(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            let hex = |b: u8| (b as char).to_digit(16).unwrap_or(0) as u8;
            out.push(hex(bytes[index + 1]) * 16 + hex(bytes[index + 2]));
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 资源页展示标题：地址最后一段路径解码后的文件名（标签页与工具
/// 结果共用，保证界面与 web_fetch 汇报一致）。
fn resource_page_title(url: &str) -> String {
    let file_name = url
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .split('#')
        .next()
        .unwrap_or_default();
    percent_decode_lossy(file_name)
}

/// 资源页合成快照：Finished 事件本身即代表资源响应完成，不依赖页面脚本。
fn resource_document_snapshot(url: &str, navigation_id: u64) -> WebDocumentSnapshot {
    WebDocumentSnapshot {
        document_id: format!("resource-{navigation_id}"),
        ready_state: "complete".to_string(),
        url: url.to_string(),
        title: resource_page_title(url),
        text: String::new(),
        has_content: false,
        internal_error: false,
    }
}

fn accept_loading_document(
    navigation: &mut TabNavigationState,
    observed_navigation_id: u64,
    snapshot: &WebDocumentSnapshot,
) -> bool {
    if navigation.navigation_id != observed_navigation_id
        || navigation.phase != NavigationPhase::Loading
        || navigation.internal_error_url.as_deref() == Some(snapshot.url.as_str())
        || navigation
            .superseded_document_ids
            .iter()
            .any(|document_id| document_id == &snapshot.document_id)
    {
        return false;
    }

    if let Some(previous_document_id) = navigation.document_id.replace(snapshot.document_id.clone())
    {
        if previous_document_id != snapshot.document_id {
            push_recent_unique(
                &mut navigation.superseded_document_ids,
                previous_document_id,
            );
        }
    }
    navigation.started_url = Some(snapshot.url.clone());
    true
}

fn accepts_completed_document(
    navigation: &TabNavigationState,
    navigation_id: u64,
    expected_document_id: &str,
    snapshot: &WebDocumentSnapshot,
) -> bool {
    snapshot.document_id == expected_document_id
        && (snapshot.ready_state == "complete"
            || (snapshot.ready_state == "interactive"
                && (snapshot.has_content || !snapshot.text.trim().is_empty())))
        && !snapshot.url.is_empty()
        && !snapshot.internal_error
        && navigation.navigation_id == navigation_id
        && navigation.phase == NavigationPhase::Loading
        && navigation.document_id.as_deref() == Some(expected_document_id)
        && navigation.started_url.as_deref().is_some_and(|url| {
            normalize_url_for_compare(url) == normalize_url_for_compare(&snapshot.url)
        })
}

fn agent_domain_for_url(url: &str) -> Result<String, String> {
    let parsed = url
        .parse::<Url>()
        .map_err(|error| format!("URL 解析失败：{error}"))?;
    let Some(host) = parsed.host_str() else {
        return match parsed.scheme() {
            "file" => Ok("file:".to_string()),
            scheme => Err(format!("{scheme} 地址不包含可识别的主机名")),
        };
    };
    let normalized_host = host.trim_end_matches('.').to_ascii_lowercase();
    Ok(psl::domain_str(&normalized_host)
        .unwrap_or(&normalized_host)
        .to_string())
}

fn agent_tab_id_for_domain(state: &BrowserState, agent_domain: &str) -> Option<String> {
    state
        .tabs
        .iter()
        .find(|tab| {
            tab.source == BrowserTabSource::Agent
                && tab.agent_domain.as_deref() == Some(agent_domain)
        })
        .map(|tab| tab.id.clone())
}

/// 浏览器 WebView 的共享状态
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

    fn begin_navigation_for_tab(
        app: &AppHandle<Wry>,
        state: Arc<Mutex<BrowserState>>,
        tab_id: &str,
        url: &str,
        intent: NavigationIntent,
    ) -> Result<u64, String> {
        let (session_id, navigation_id) = {
            let mut state = state.lock().map_err(|e| e.to_string())?;
            if !state.tabs.iter().any(|tab| tab.id == tab_id) {
                return Err(format!("标签 {tab_id} 不存在"));
            }
            let history_index = apply_tab_navigation_intent(&mut state, tab_id, url, intent)?;

            let signal = state
                .navigation_signals
                .entry(tab_id.to_string())
                .or_insert_with(|| navigation_signal(url))
                .clone();
            let navigation_id = {
                let mut navigation = signal.state.lock().map_err(|e| e.to_string())?;
                remember_superseded_navigation(&mut navigation);
                navigation.navigation_id = navigation.navigation_id.wrapping_add(1).max(1);
                navigation.requested_url = url.to_string();
                navigation.started_url = None;
                navigation.document_id = None;
                navigation.final_url = None;
                navigation.history_index = history_index;
                navigation.phase = NavigationPhase::Loading;
                navigation.internal_error_url = None;
                navigation.navigation_id
            };
            signal.cvar.notify_all();

            if let Some(tab) = state.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                tab.url = url.to_string();
                tab.title.clear();
            }
            state.latest_snapshots.insert(
                tab_id.to_string(),
                BrowserPageSnapshot {
                    title: String::new(),
                    url: url.to_string(),
                    text: String::new(),
                    status: PageStatus::Loading,
                    tabs: Vec::new(),
                    active_tab_id: None,
                    events: Vec::new(),
                },
            );
            (state.session_id.clone(), navigation_id)
        };

        let _ = app.emit(
            "browser:navigation_state",
            BrowserNavigationStateEvent {
                session_id: session_id.clone(),
                tab_id: tab_id.to_string(),
                navigation_id,
                state: BrowserNavigationStateKind::Loading,
                url: url.to_string(),
                message: None,
            },
        );
        crate::webview_host::emit_plugin_event(
            &session_id,
            "navigation_started",
            &serde_json::json!({
                "tab_id": tab_id,
                "navigation_id": navigation_id,
                "url": url,
            }),
        );

        let app_for_timeout = app.clone();
        let state_for_timeout = state.clone();
        let tab_id_for_timeout = tab_id.to_string();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(NAVIGATION_TIMEOUT).await;
            Self::fail_navigation_for_tab(
                &app_for_timeout,
                state_for_timeout,
                &tab_id_for_timeout,
                navigation_id,
            );
        });

        Self::start_navigation_completion_probe(app, state, tab_id, navigation_id);

        Ok(navigation_id)
    }

    fn fail_navigation_for_tab(
        app: &AppHandle<Wry>,
        state: Arc<Mutex<BrowserState>>,
        tab_id: &str,
        navigation_id: u64,
    ) {
        let (session_id, requested_url, error_url, webview) = {
            let mut state = match state.lock() {
                Ok(state) => state,
                Err(error) => error.into_inner(),
            };
            let Some(signal) = state.navigation_signals.get(tab_id).cloned() else {
                return;
            };
            let mut navigation = match signal.state.lock() {
                Ok(navigation) => navigation,
                Err(error) => error.into_inner(),
            };
            if navigation.navigation_id != navigation_id
                || navigation.phase != NavigationPhase::Loading
            {
                return;
            }

            let requested_url = navigation.requested_url.clone();
            let error_url = navigation_error_data_url(&requested_url);
            navigation.phase = NavigationPhase::Failed;
            navigation.final_url = None;
            navigation.internal_error_url = Some(error_url.clone());
            signal.cvar.notify_all();
            drop(navigation);

            if let Some(tab) = state.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                tab.url = requested_url.clone();
                tab.title = "页面加载异常".to_string();
            } else {
                return;
            }
            state.latest_snapshots.insert(
                tab_id.to_string(),
                BrowserPageSnapshot {
                    title: "页面加载异常".to_string(),
                    url: requested_url.clone(),
                    text: String::new(),
                    status: PageStatus::Error(PAGE_LOAD_ERROR_MESSAGE.to_string()),
                    tabs: Vec::new(),
                    active_tab_id: None,
                    events: Vec::new(),
                },
            );
            (
                state.session_id.clone(),
                requested_url,
                error_url,
                state.webviews.get(tab_id).cloned(),
            )
        };

        warn!(
            session_id = %session_id,
            tab_id,
            navigation_id,
            url = %requested_url,
            "browser navigation reached the page-load deadline"
        );
        let _ = app.emit(
            "browser:navigation_state",
            BrowserNavigationStateEvent {
                session_id: session_id.clone(),
                tab_id: tab_id.to_string(),
                navigation_id,
                state: BrowserNavigationStateKind::Failed,
                url: requested_url.clone(),
                message: Some(PAGE_LOAD_ERROR_MESSAGE.to_string()),
            },
        );
        let _ = app.emit(
            "browser:tab_updated",
            serde_json::json!({ "session_id": session_id, "tab_id": tab_id }),
        );
        // 阶段 1 事件通道：导航失败（超时）定向投递给插件 UI
        crate::webview_host::emit_plugin_event(
            &session_id,
            "navigation_failed",
            &serde_json::json!({ "tab_id": tab_id, "url": &requested_url }),
        );

        if let Some(webview) = webview {
            let parsed_url = match error_url.parse::<Url>() {
                Ok(url) => url,
                Err(error) => {
                    warn!(%error, "browser error page URL creation failed");
                    return;
                }
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                webview.navigate(parsed_url)
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warn!(%error, "browser error page navigation failed"),
                Err(_) => warn!("browser error page navigation panicked"),
            }
        }
    }

    fn handle_page_load_started(
        app: &AppHandle<Wry>,
        state: Arc<Mutex<BrowserState>>,
        tab_id: &str,
        observed_navigation_id: u64,
        event_url: &str,
        snapshot: WebDocumentSnapshot,
    ) {
        if snapshot.document_id.is_empty() || snapshot.url.is_empty() || snapshot.internal_error {
            return;
        }
        if !event_url.is_empty()
            && normalize_url_for_compare(event_url) != normalize_url_for_compare(&snapshot.url)
        {
            return;
        }

        let begin_intent = {
            let browser_state = match state.lock() {
                Ok(state) => state,
                Err(error) => error.into_inner(),
            };
            let Some(signal) = browser_state.navigation_signals.get(tab_id).cloned() else {
                return;
            };
            let mut navigation = match signal.state.lock() {
                Ok(navigation) => navigation,
                Err(error) => error.into_inner(),
            };
            if navigation.phase == NavigationPhase::Loading {
                accept_loading_document(&mut navigation, observed_navigation_id, &snapshot);
                return;
            }
            if navigation.navigation_id != observed_navigation_id
                || navigation.internal_error_url.as_deref() == Some(snapshot.url.as_str())
                || navigation
                    .superseded_document_ids
                    .iter()
                    .any(|document_id| document_id == &snapshot.document_id)
            {
                return;
            }

            match navigation.phase {
                NavigationPhase::Loading => unreachable!(),
                NavigationPhase::Failed => {
                    Some((NavigationIntent::Retry, navigation.requested_url.clone()))
                }
                NavigationPhase::Loaded => {
                    if navigation.document_id.as_deref() == Some(snapshot.document_id.as_str()) {
                        return;
                    }
                    Some((NavigationIntent::Normal, snapshot.url.clone()))
                }
            }
        };

        let Some((intent, requested_url)) = begin_intent else {
            return;
        };
        let Ok(navigation_id) =
            Self::begin_navigation_for_tab(app, state.clone(), tab_id, &requested_url, intent)
        else {
            return;
        };

        let browser_state = match state.lock() {
            Ok(state) => state,
            Err(error) => error.into_inner(),
        };
        let Some(signal) = browser_state.navigation_signals.get(tab_id) else {
            return;
        };
        let mut navigation = match signal.state.lock() {
            Ok(navigation) => navigation,
            Err(error) => error.into_inner(),
        };
        if navigation.navigation_id != navigation_id
            || navigation.phase != NavigationPhase::Loading
            || navigation
                .superseded_document_ids
                .iter()
                .any(|document_id| document_id == &snapshot.document_id)
        {
            return;
        }
        navigation.started_url = Some(snapshot.url);
        navigation.document_id = Some(snapshot.document_id);
    }

    fn start_navigation_completion_probe(
        app: &AppHandle<Wry>,
        state: Arc<Mutex<BrowserState>>,
        tab_id: &str,
        navigation_id: u64,
    ) {
        let app = app.clone();
        let tab_id_for_thread = tab_id.to_string();
        let state_for_thread = state.clone();
        let result = std::thread::Builder::new()
            .name("browser-load-probe".to_string())
            .spawn(move || {
                let manager = BrowserManager {
                    state: state_for_thread.clone(),
                };
                let mut interactive_ready_polls = 0_u8;
                loop {
                    std::thread::sleep(NAVIGATION_COMPLETION_PROBE_INTERVAL);
                    let still_current = {
                        let browser_state = match state_for_thread.lock() {
                            Ok(state) => state,
                            Err(error) => error.into_inner(),
                        };
                        let Some(signal) = browser_state.navigation_signals.get(&tab_id_for_thread)
                        else {
                            return;
                        };
                        let navigation = match signal.state.lock() {
                            Ok(navigation) => navigation,
                            Err(error) => error.into_inner(),
                        };
                        navigation.navigation_id == navigation_id
                            && navigation.phase == NavigationPhase::Loading
                    };
                    if !still_current {
                        return;
                    }

                    let Some(raw) = manager.eval_tab_with_result_timeout(
                        &tab_id_for_thread,
                        DOCUMENT_STATE_SCRIPT,
                        POLL_EVAL_TIMEOUT,
                    ) else {
                        continue;
                    };
                    let Some(snapshot) = parse_web_document_snapshot(&raw) else {
                        debug!(
                            tab_id = %tab_id_for_thread,
                            navigation_id,
                            "browser load probe parse failed"
                        );
                        continue;
                    };
                    if snapshot.document_id.is_empty()
                        || snapshot.url.is_empty()
                        || snapshot.internal_error
                    {
                        interactive_ready_polls = 0;
                        continue;
                    }

                    let document_ready = if snapshot.ready_state == "complete" {
                        true
                    } else if snapshot.ready_state == "interactive" && snapshot.has_content {
                        interactive_ready_polls = interactive_ready_polls.saturating_add(1);
                        interactive_ready_polls >= 2
                    } else {
                        interactive_ready_polls = 0;
                        false
                    };

                    let expected_document_id = snapshot.document_id.clone();
                    let accepted = {
                        let browser_state = match state_for_thread.lock() {
                            Ok(state) => state,
                            Err(error) => error.into_inner(),
                        };
                        let Some(signal) = browser_state
                            .navigation_signals
                            .get(&tab_id_for_thread)
                            .cloned()
                        else {
                            return;
                        };
                        let mut navigation = match signal.state.lock() {
                            Ok(navigation) => navigation,
                            Err(error) => error.into_inner(),
                        };
                        accept_loading_document(&mut navigation, navigation_id, &snapshot)
                    };
                    if !accepted || !document_ready {
                        continue;
                    }

                    if Self::complete_navigation_for_tab(
                        &app,
                        state_for_thread.clone(),
                        &tab_id_for_thread,
                        navigation_id,
                        &expected_document_id,
                        snapshot,
                    ) {
                        debug!(
                            tab_id = %tab_id_for_thread,
                            navigation_id,
                            "browser load probe accepted completion"
                        );
                        return;
                    }
                }
            });
        if let Err(error) = result {
            warn!(%error, tab_id, navigation_id, "browser load probe spawn failed");
        }
    }

    fn complete_navigation_for_tab(
        app: &AppHandle<Wry>,
        state: Arc<Mutex<BrowserState>>,
        tab_id: &str,
        navigation_id: u64,
        expected_document_id: &str,
        snapshot: WebDocumentSnapshot,
    ) -> bool {
        let final_url = snapshot.url.clone();
        let title = snapshot.title.clone();
        let text = snapshot.text.clone();
        let (session_id, shared, should_persist_history) = {
            let mut state = match state.lock() {
                Ok(state) => state,
                Err(error) => error.into_inner(),
            };
            let Some(signal) = state.navigation_signals.get(tab_id).cloned() else {
                return false;
            };
            let mut navigation = match signal.state.lock() {
                Ok(navigation) => navigation,
                Err(error) => error.into_inner(),
            };
            if !accepts_completed_document(
                &navigation,
                navigation_id,
                expected_document_id,
                &snapshot,
            ) {
                return false;
            }

            navigation.phase = NavigationPhase::Loaded;
            navigation.final_url = Some(final_url.clone());
            navigation.internal_error_url = None;
            let history_index = navigation.history_index;
            signal.cvar.notify_all();
            drop(navigation);

            if let Some(tab) = state.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                tab.url = final_url.clone();
                if !title.is_empty() {
                    tab.title = title.clone();
                }
            } else {
                return false;
            }
            update_tab_navigation_entry(
                &mut state,
                tab_id,
                history_index,
                &final_url,
                Some(&title),
            );
            state.latest_snapshots.insert(
                tab_id.to_string(),
                BrowserPageSnapshot {
                    title: title.clone(),
                    url: final_url.clone(),
                    text: text.clone(),
                    status: PageStatus::Loaded,
                    tabs: Vec::new(),
                    active_tab_id: None,
                    events: Vec::new(),
                },
            );
            let shared = state.shared.clone();
            let should_persist_history = is_recordable_history_url(&final_url);
            if should_persist_history {
                let mut history = shared
                    .global_history
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                upsert_global_history(&mut history, &final_url, &title);
            }
            (state.session_id.clone(), shared, should_persist_history)
        };

        if should_persist_history {
            persist_global_history(&shared);
        }
        let _ = app.emit(
            "browser:navigation_state",
            BrowserNavigationStateEvent {
                session_id: session_id.clone(),
                tab_id: tab_id.to_string(),
                navigation_id,
                state: BrowserNavigationStateKind::Loaded,
                url: final_url.clone(),
                message: None,
            },
        );
        let _ = app.emit(
            "browser:tab_updated",
            serde_json::json!({ "session_id": session_id.clone(), "tab_id": tab_id }),
        );
        // 阶段 1 事件通道：页面加载完成（标题/URL 就绪）定向投递给插件 UI
        crate::webview_host::emit_plugin_event(
            &session_id,
            "page_loaded",
            &serde_json::json!({ "tab_id": tab_id, "title": title, "url": final_url }),
        );
        let summary = crate::webview_host::types::clip_head_tail(
            &text,
            crate::webview_host::types::PAGE_PUSH_MAX_CHARS,
        );
        let _ = app.emit(
            "browser:page_loaded",
            BrowserPageLoadedEvent {
                session_id,
                tab_id: tab_id.to_string(),
                title,
                url: final_url,
                text: summary,
            },
        );
        true
    }

    fn handle_page_load_finished(
        app: &AppHandle<Wry>,
        state: Arc<Mutex<BrowserState>>,
        tab_id: &str,
        observed_navigation_id: u64,
        event_url: &str,
        snapshot: WebDocumentSnapshot,
    ) {
        if snapshot.document_id.is_empty()
            || snapshot.url.is_empty()
            || snapshot.internal_error
            || (!event_url.is_empty()
                && normalize_url_for_compare(event_url) != normalize_url_for_compare(&snapshot.url))
        {
            return;
        }

        let expected_document_id = snapshot.document_id.clone();
        let accepted = {
            let state = match state.lock() {
                Ok(state) => state,
                Err(error) => error.into_inner(),
            };
            let Some(signal) = state.navigation_signals.get(tab_id).cloned() else {
                return;
            };
            let mut navigation = match signal.state.lock() {
                Ok(navigation) => navigation,
                Err(error) => error.into_inner(),
            };
            accept_loading_document(&mut navigation, observed_navigation_id, &snapshot)
        };
        if !accepted {
            return;
        }

        Self::complete_navigation_for_tab(
            app,
            state,
            tab_id,
            observed_navigation_id,
            &expected_document_id,
            snapshot,
        );
    }

    /// 为指定标签创建独立的 WebView 实例
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_webview_for_tab(
        app: &AppHandle<Wry>,
        state: Arc<Mutex<BrowserState>>,
        tab_id: &str,
        url: &str,
        intent: NavigationIntent,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> Result<Webview<Wry>, String> {
        let url = normalize_navigation_url(url);
        let window = app
            .get_window("main")
            .ok_or_else(|| "主窗口未找到".to_string())?;

        // 从目标 session 的 state 读出 session_id（R1 保证可靠）
        let session_id = {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.session_id.clone()
        };
        let navigation_id =
            Self::begin_navigation_for_tab(app, state.clone(), tab_id, &url, intent)?;
        let parsed_url: Url = match url.parse() {
            Ok(url) => url,
            Err(error) => {
                Self::fail_navigation_for_tab(app, state.clone(), tab_id, navigation_id);
                return Err(format!("URL 解析失败：{error}"));
            }
        };
        let data_dir = browser_data_directory(&session_id);
        let data_dir_for_error = data_dir.clone();
        let label = webview_label(&session_id, tab_id);
        let tab_id_for_closure = tab_id.to_string();
        // on_page_load 回调直接写入目标 session 的 state（不再经 app.state().manager() 串台）
        let state_clone_holder = state.clone();
        // 在 state_clone_holder 被 move 进 on_page_load 闭包前读出当前缩放，用于新建 webview 即时应用
        let shared = state_clone_holder
            .lock()
            .map(|s| s.shared.clone())
            .unwrap_or_else(|e| e.into_inner().shared.clone());
        let initial_zoom = *shared.zoom_factor.lock().unwrap_or_else(|e| e.into_inner());
        let app_clone = app.clone();
        // 通行密钥桥接只在签名具备浏览器通行密钥权限时启用（见 passkey 模块）
        let passkey_enabled = passkey::is_available();
        let mut builder = WebviewBuilder::new(&label, WebviewUrl::External(parsed_url))
            .initialization_script(BRIDGE_SCRIPT);
        if passkey_enabled {
            builder = builder.initialization_script(passkey::PASSKEY_SCRIPT);
        }
        let builder = builder
            .data_directory(data_dir)
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/605.1.15")
            .enable_clipboard_access()
            .devtools(true)
            .on_page_load(move |webview, payload| {
                use tauri::webview::PageLoadEvent;
                let event_url = payload.url().to_string();

                if payload.event() == PageLoadEvent::Started {
                    let observed_navigation_id = {
                        let state = match state_clone_holder.lock() {
                            Ok(state) => state,
                            Err(error) => error.into_inner(),
                        };
                        let Some(signal) =
                            state.navigation_signals.get(&tab_id_for_closure)
                        else {
                            return;
                        };
                        let navigation = match signal.state.lock() {
                            Ok(navigation) => navigation,
                            Err(error) => error.into_inner(),
                        };
                        if navigation.internal_error_url.as_deref() == Some(event_url.as_str()) {
                            return;
                        }
                        navigation.navigation_id
                    };

                    let app_for_started = app_clone.clone();
                    let state_for_started = state_clone_holder.clone();
                    let tab_id_for_started = tab_id_for_closure.clone();
                    let event_url_for_started = event_url.clone();
                    if let Err(error) = webview.eval_with_callback(
                        DOCUMENT_STATE_SCRIPT,
                        move |result| {
                            let Some(snapshot) = parse_web_document_snapshot(&result) else {
                                debug!("browser started document state parse failed");
                                return;
                            };
                            Self::handle_page_load_started(
                                &app_for_started,
                                state_for_started.clone(),
                                &tab_id_for_started,
                                observed_navigation_id,
                                &event_url_for_started,
                                snapshot,
                            );
                        },
                    ) {
                        debug!(%error, "browser started document state read failed");
                    }
                    return;
                }

                if payload.event() == PageLoadEvent::Finished {
                    let navigation_id = {
                        let state = match state_clone_holder.lock() {
                            Ok(state) => state,
                            Err(error) => error.into_inner(),
                        };
                        let Some(signal) =
                            state.navigation_signals.get(&tab_id_for_closure)
                        else {
                            return;
                        };
                        let navigation = match signal.state.lock() {
                            Ok(navigation) => navigation,
                            Err(error) => error.into_inner(),
                        };
                        if navigation.internal_error_url.as_deref() == Some(event_url.as_str())
                            || navigation.phase != NavigationPhase::Loading
                        {
                            return;
                        }
                        navigation.navigation_id
                    };

                    // 资源页（图片/PDF/音视频等）没有可注入的 HTML bridge，
                    // eval 读快照永远无响应；Finished 事件即代表资源响应
                    // 完成，用合成快照直接结束导航，避免等到超时。
                    if is_non_html_resource_url(&event_url) {
                        Self::handle_page_load_finished(
                            &app_clone,
                            state_clone_holder.clone(),
                            &tab_id_for_closure,
                            navigation_id,
                            &event_url,
                            resource_document_snapshot(&event_url, navigation_id),
                        );
                        return;
                    }

                    let state_for_finished = state_clone_holder.clone();
                    let tab_id_for_finished = tab_id_for_closure.clone();
                    let app_for_finished = app_clone.clone();
                    let event_url_for_finished = event_url.clone();
                    if let Err(error) = webview.eval_with_callback(
                        PAGE_SNAPSHOT_SCRIPT,
                        move |result| {
                            let Some(snapshot) = parse_web_document_snapshot(&result) else {
                                debug!("browser finished page snapshot parse failed");
                                return;
                            };
                            Self::handle_page_load_finished(
                                &app_for_finished,
                                state_for_finished.clone(),
                                &tab_id_for_finished,
                                navigation_id,
                                &event_url_for_finished,
                                snapshot,
                            );
                        },
                    ) {
                        debug!(%error, "browser finished page snapshot read failed");
                    }
                    let _ = webview.eval("window.__tiangong_bridge.observer.start()");
                }
            });

        let webview =
            match window.add_child(builder, LogicalPosition::new(x, y), LogicalSize::new(w, h)) {
                Ok(webview) => webview,
                Err(error) => {
                    Self::fail_navigation_for_tab(app, state, tab_id, navigation_id);
                    return Err(format!(
                        "创建浏览器 WebView 失败（数据目录 {}）：{error}",
                        data_dir_for_error.display()
                    ));
                }
            };

        if passkey_enabled {
            passkey::attach(&webview);
        }
        // 创建后立即应用当前缩放，避免首屏以 100% 渲染再跳变
        if (initial_zoom - 1.0).abs() > f64::EPSILON {
            if let Err(e) = webview.set_zoom(initial_zoom) {
                warn!(error = %e, "新建 webview 应用初始缩放失败");
            }
        }

        Ok(webview)
    }

    /// 确保指定 URL 的标签与 WebView 存在（webview.create / 首次导航共用）。
    ///
    /// 创建一律"无头"：WebView 落在屏幕外，不改动 `browser_rect`（那是
    /// 最近一次展示矩形，只由 webview.instanceShow / setPosition 维护）。
    /// 是否展示、展示在哪完全由前端下发——面板就绪后经 instanceShow 把
    /// 页面"请"进面板；后台会话无人请显，页面常驻屏幕外照常执行。
    pub fn open(&self, app: &AppHandle<Wry>, url: &str) -> Result<(), String> {
        let url = normalize_navigation_url(url);
        let existing_action = {
            let state = self.state.lock().map_err(|e| e.to_string())?;
            if !state.tabs.is_empty() {
                let target_id = state
                    .active_tab_id
                    .clone()
                    .ok_or_else(|| "当前没有可用标签".to_string())?;
                let current_url = state
                    .tabs
                    .iter()
                    .find(|tab| tab.id == target_id)
                    .map(|tab| tab.url.as_str())
                    .unwrap_or_default();
                let same_url =
                    normalize_url_for_compare(current_url) == normalize_url_for_compare(&url);
                state
                    .visible
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Some((
                    target_id.clone(),
                    !state.webviews.contains_key(&target_id) && url != "about:blank",
                    !same_url && state.webviews.contains_key(&target_id),
                    if same_url {
                        NavigationIntent::Restore
                    } else {
                        NavigationIntent::Normal
                    },
                ))
            } else {
                None
            }
        };

        if let Some((tab_id, should_create, should_navigate, intent)) = existing_action {
            if should_create {
                let webview = Self::create_webview_for_tab(
                    app,
                    self.state.clone(),
                    &tab_id,
                    &url,
                    intent,
                    HEADLESS_RECT.0,
                    HEADLESS_RECT.1,
                    HEADLESS_RECT.2,
                    HEADLESS_RECT.3,
                )?;
                let mut state = self.state.lock().map_err(|e| e.to_string())?;
                state.webviews.insert(tab_id, webview);
                drop(state);
                self.start_url_poll(app, &url);
                self.start_event_poll(app);
            } else if should_navigate {
                self.navigate(app, &url)?;
            }
            return Ok(());
        }

        // 首次创建：创建标签 + WebView（about:blank 跳过 WebView 创建）
        let tab_id = scru128::new().to_string();
        let is_blank = url == "about:blank";

        {
            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            state
                .navigation_signals
                .insert(tab_id.clone(), navigation_signal(&url));
            state.tabs.push(BrowserTab {
                id: tab_id.clone(),
                url: url.clone(),
                title: String::new(),
                source: BrowserTabSource::User,
                agent_domain: None,
            });
            state.active_tab_id = Some(tab_id.clone());
            state
                .visible
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        if !is_blank {
            let webview = Self::create_webview_for_tab(
                app,
                self.state.clone(),
                &tab_id,
                &url,
                NavigationIntent::Normal,
                HEADLESS_RECT.0,
                HEADLESS_RECT.1,
                HEADLESS_RECT.2,
                HEADLESS_RECT.3,
            )?;
            if let Ok(mut state) = self.state.lock() {
                state.webviews.insert(tab_id.clone(), webview);
            }
        }

        self.start_url_poll(app, &url);
        self.start_event_poll(app);

        Ok(())
    }

    pub fn close(&self) -> Result<(), String> {
        if let Ok(mut state) = self.state.lock() {
            reset_runtime_state(&mut state, true);
            state.active_session_id = None;
            state
                .visible
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    /// 启动后台轮询线程，检测 webview URL 变化并发射 browser:page_loaded 事件
    pub(crate) fn start_url_poll(&self, app: &AppHandle<Wry>, initial_url: &str) {
        let state = self.state.clone();
        let app = app.clone();
        let stop = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.poll_stop
                .store(false, std::sync::atomic::Ordering::Relaxed);
            s.last_known_url = initial_url.to_string();
            s.poll_stop.clone()
        };
        let (visible, session_id) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (s.visible.clone(), s.session_id.clone())
        };

        std::thread::Builder::new()
            .name("browser-url-poll".into())
            .spawn(move || {
                let mut tick: u32 = 0;
                let mut no_webview_ticks: u32 = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(URL_POLL_TICK);
                    tick += 1;
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    if !visible.load(std::sync::atomic::Ordering::Relaxed) {
                        continue;
                    }
                    let (active_tab_id, navigation_id, current_url) = {
                        let s = match state.lock() {
                            Ok(s) => s,
                            Err(e) => e.into_inner(),
                        };
                        let Some(active_tab_id) = s.active_tab_id.clone() else {
                            continue;
                        };
                        let Some(navigation_id) = loaded_navigation_id(&s, &active_tab_id) else {
                            continue;
                        };
                        match s.webviews.get(&active_tab_id) {
                            Some(wv) => {
                                no_webview_ticks = 0;
                                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    wv.url()
                                })) {
                                    Ok(Ok(u)) => (active_tab_id, navigation_id, u.to_string()),
                                    _ => continue,
                                }
                            }
                            None => {
                                no_webview_ticks += 1;
                                // 无 WebView 时等待最多 30 秒（30 个 tick × 1000ms），超时退出
                                if no_webview_ticks > 30 {
                                    break;
                                }
                                continue;
                            }
                        }
                    };
                    let changed = {
                        let mut s = match state.lock() {
                            Ok(s) => s,
                            Err(e) => e.into_inner(),
                        };
                        if s.active_tab_id.as_deref() != Some(active_tab_id.as_str())
                            || loaded_navigation_id(&s, &active_tab_id) != Some(navigation_id)
                        {
                            continue;
                        }
                        if current_url != s.last_known_url {
                            s.last_known_url = current_url.clone();
                            true
                        } else {
                            false
                        }
                    };
                    if changed {
                        debug!(url = %current_url, "browser url_poll detected change");
                        // 更新活跃标签 URL（历史记录由 on_page_load 回调负责）
                        {
                            let mut s = match state.lock() {
                                Ok(s) => s,
                                Err(e) => e.into_inner(),
                            };
                            if let Some(tab) =
                                s.tabs.iter_mut().find(|tab| tab.id == active_tab_id)
                            {
                                tab.url = current_url.clone();
                            }
                        }
                        let _ = app.emit(
                            "browser:page_loaded",
                            BrowserPageLoadedEvent {
                                session_id: session_id.clone(),
                                tab_id: active_tab_id.clone(),
                                title: String::new(),
                                url: current_url,
                                text: String::new(),
                            },
                        );
                    }

                    // 每 URL_POLL_CONTENT_TICKS 个 tick 检测页面内容变化
                    if tick.is_multiple_of(URL_POLL_CONTENT_TICKS) {
                        let mgr = BrowserManager {
                            state: state.clone(),
                        };
                        if let Some(sig) = mgr.eval_tab_with_result_timeout(
                            &active_tab_id,
                            "(function(){try{return(document.body.innerText||'').substring(0,500).trim()}catch(e){return''}})()",
                            POLL_EVAL_TIMEOUT,
                        ) {
                            let content_changed = {
                                let mut s = match state.lock() {
                                    Ok(s) => s,
                                    Err(e) => e.into_inner(),
                                };
                                if sig != s.last_known_text_signature && !sig.is_empty() {
                                    s.last_known_text_signature = sig;
                                    true
                                } else {
                                    false
                                }
                            };
                            if content_changed {
                                debug!("browser url_poll detected content change");
                                let mgr2 = BrowserManager {
                                    state: state.clone(),
                                };
                                if let Some(raw) = mgr2.eval_full_text_cached(&active_tab_id, 12000) {
                                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&raw) {
                                        let still_loaded = {
                                            let state = match state.lock() {
                                                Ok(state) => state,
                                                Err(error) => error.into_inner(),
                                            };
                                            state.active_tab_id.as_deref()
                                                == Some(active_tab_id.as_str())
                                                && loaded_navigation_id(&state, &active_tab_id)
                                                    == Some(navigation_id)
                                        };
                                        if !still_loaded {
                                            continue;
                                        }
                                        let title = data["title"].as_str().unwrap_or("").to_string();
                                        let url = data["url"].as_str().unwrap_or("").to_string();
                                        let text = crate::webview_host::types::clip_head_tail(
                                            data["text"].as_str().unwrap_or(""),
                                            crate::webview_host::types::PAGE_PUSH_MAX_CHARS,
                                        );
                                        let _ = app.emit(
                                            "browser:page_loaded",
                                            BrowserPageLoadedEvent {
                                                session_id: session_id.clone(),
                                                tab_id: active_tab_id.clone(),
                                                title,
                                                url,
                                                text,
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                debug!("browser url_poll thread exiting");
            })
            .expect("failed to spawn browser URL poll thread");
    }

    pub fn hide(&self) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        for wv in state.webviews.values() {
            let _ = wv.set_size(LogicalSize::new(0.0, 0.0));
            let _ = wv.set_position(LogicalPosition::new(-10000, -10000));
        }
        // 展示矩形随收起失效：否则后续 tab_switch/关标签切换会按旧矩形把
        // 页面重新摆回窗口，浮层死灰复燃。下次 instanceShow 会带来新矩形。
        state.browser_rect = (0.0, 0.0, 0.0, 0.0);
        state
            .visible
            .store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// 显示 active tab 的 webview（切换 session 回来时，把 webview 重新定位到可见区域）。
    pub fn show_active_webview(
        &self,
        _app: &AppHandle<Wry>,
        rect: &(f64, f64, f64, f64),
    ) -> Result<(), String> {
        let zoom = self.zoom();
        let state = self.state.lock().map_err(|e| e.to_string())?;
        for webview in state.webviews.values() {
            let _ = webview.set_zoom(zoom);
        }
        if let Some(active_id) = state.active_tab_id.as_ref() {
            if let Some(wv) = state.webviews.get(active_id) {
                let _ = wv.set_size(LogicalSize::new(rect.2, rect.3));
                let _ = wv.set_position(LogicalPosition::new(rect.0, rect.1));
            }
        }
        state
            .visible
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    pub fn go_back(&self, app: &AppHandle<Wry>) -> Result<(), String> {
        let (target_index, target_url) = self
            .history_target(-1)
            .ok_or_else(|| "当前标签没有可后退的页面".to_string())?;
        self.navigate_with_intent(app, &target_url, NavigationIntent::History { target_index })
            .map(|_| ())
    }

    pub fn go_forward(&self, app: &AppHandle<Wry>) -> Result<(), String> {
        let (target_index, target_url) = self
            .history_target(1)
            .ok_or_else(|| "当前标签没有可前进的页面".to_string())?;
        self.navigate_with_intent(app, &target_url, NavigationIntent::History { target_index })
            .map(|_| ())
    }

    pub fn reload(&self, app: &AppHandle<Wry>) -> Result<(), String> {
        let (url, intent) = {
            let state = self.state.lock().map_err(|e| e.to_string())?;
            let tab_id = state
                .active_tab_id
                .as_ref()
                .ok_or_else(|| "当前没有可用标签".to_string())?;
            let url = state
                .tab_history_indices
                .get(tab_id)
                .and_then(|index| {
                    state
                        .tab_histories
                        .get(tab_id)
                        .and_then(|entries| entries.get(*index))
                })
                .map(|entry| entry.url.clone())
                .or_else(|| {
                    state
                        .tabs
                        .iter()
                        .find(|tab| &tab.id == tab_id)
                        .map(|tab| tab.url.clone())
                })
                .ok_or_else(|| "当前标签没有可重新加载的地址".to_string())?;
            let intent = if tab_navigation_phase(&state, tab_id) == Some(NavigationPhase::Failed) {
                NavigationIntent::Retry
            } else {
                NavigationIntent::Reload
            };
            (url, intent)
        };
        self.navigate_with_intent(app, &url, intent).map(|_| ())
    }

    fn history_target(&self, offset: isize) -> Option<(usize, String)> {
        let state = self.state.lock().ok()?;
        let tab_id = state.active_tab_id.as_ref()?;
        let entries = state.tab_histories.get(tab_id)?;
        let current_index = *state.tab_history_indices.get(tab_id)? as isize;
        let target_index = current_index.checked_add(offset)?;
        if target_index < 0 {
            return None;
        }
        entries
            .get(target_index as usize)
            .map(|entry| (target_index as usize, entry.url.clone()))
    }

    pub fn set_position(&self, x: f64, y: f64) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        if let Some(wv) = state.active_webview() {
            wv.set_position(LogicalPosition::new(x, y))
                .map_err(|e| format!("设置浏览器位置失败：{e}"))?;
        }
        state.browser_rect.0 = x;
        state.browser_rect.1 = y;
        Ok(())
    }

    /// 启动事件消费线程
    pub(crate) fn start_event_poll(&self, app: &AppHandle<Wry>) {
        let state = self.state.clone();
        let app = app.clone();
        let stop = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.event_poll_stop
                .store(true, std::sync::atomic::Ordering::Relaxed);
            s.event_poll_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            s.event_poll_stop.clone()
        };
        let (visible, session_id) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (s.visible.clone(), s.session_id.clone())
        };

        std::thread::Builder::new()
            .name("browser-event-poll".into())
            .spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    if !visible.load(std::sync::atomic::Ordering::Relaxed) {
                        continue;
                    }
                    let active_loaded = {
                        let state = match state.lock() {
                            Ok(state) => state,
                            Err(error) => error.into_inner(),
                        };
                        state.active_tab_id.as_ref().is_some_and(|tab_id| {
                            tab_navigation_phase(&state, tab_id)
                                == Some(NavigationPhase::Loaded)
                        })
                    };
                    if !active_loaded {
                        continue;
                    }

                    let mgr = BrowserManager {
                        state: state.clone(),
                    };
                    let active_tab_id_for_events = {
                        let state = match state.lock() {
                            Ok(state) => state,
                            Err(error) => error.into_inner(),
                        };
                        match state.active_tab_id.clone() {
                            Some(id) => id,
                            None => continue,
                        }
                    };
                    if let Some(raw) = mgr.eval_tab_with_result_timeout(
                        &active_tab_id_for_events,
                        "(function(){try{return window.__tiangong_bridge.observer.drainAllEvents()}catch(e){return[]}})()",
                        POLL_EVAL_TIMEOUT,
                    ) {
                        if raw == "[]" || raw.is_empty() {
                            continue;
                        }
                        if let Ok(events) =
                            serde_json::from_str::<Vec<crate::webview_host::types::BrowserEvent>>(&raw)
                        {
                            if !events.is_empty() {
                                if let Ok(mut s) = state.lock() {
                                    s.pending_events.extend(events.clone());
                                    if s.pending_events.len() > 200 {
                                        let keep_from = s.pending_events.len() - 100;
                                        s.pending_events.drain(0..keep_from);
                                    }
                                }
                                let _ = app.emit(
                                    "browser:events",
                                    BrowserEventsEvent {
                                        session_id: session_id.clone(),
                                        tab_id: active_tab_id_for_events,
                                        events,
                                    },
                                );
                            }
                        }
                    }
                }
            })
            .expect("failed to spawn browser event poll thread");
    }

    pub fn set_size(&self, w: f64, h: f64) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        if let Some(wv) = state.active_webview() {
            wv.set_size(LogicalSize::new(w, h))
                .map_err(|e| format!("设置浏览器尺寸失败：{e}"))?;
        }
        state.browser_rect.2 = w;
        state.browser_rect.3 = h;
        Ok(())
    }

    pub(crate) fn navigate(
        &self,
        app: &AppHandle<Wry>,
        url: &str,
    ) -> Result<NavigationTicket, String> {
        self.navigate_with_intent(app, url, NavigationIntent::Normal)
    }

    fn navigate_with_intent(
        &self,
        app: &AppHandle<Wry>,
        url: &str,
        intent: NavigationIntent,
    ) -> Result<NavigationTicket, String> {
        let url = normalize_navigation_url(url);
        let (tab_id, webview) = {
            let state = self.state.lock().map_err(|e| e.to_string())?;
            let tab_id = state
                .active_tab_id
                .clone()
                .ok_or_else(|| "当前没有可用标签".to_string())?;
            let webview = state
                .webviews
                .get(&tab_id)
                .cloned()
                .ok_or_else(|| "当前标签尚未创建 WebView".to_string())?;
            (tab_id, webview)
        };
        let navigation_id =
            Self::begin_navigation_for_tab(app, self.state.clone(), &tab_id, &url, intent)?;
        let parsed_url: Url = match url.parse() {
            Ok(url) => url,
            Err(error) => {
                Self::fail_navigation_for_tab(app, self.state.clone(), &tab_id, navigation_id);
                return Err(format!("URL 解析失败：{error}"));
            }
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            webview.navigate(parsed_url)
        }));
        match result {
            Ok(Ok(())) => Ok(NavigationTicket {
                tab_id,
                navigation_id,
            }),
            Ok(Err(error)) => {
                Self::fail_navigation_for_tab(app, self.state.clone(), &tab_id, navigation_id);
                Err(format!("导航失败：{error}"))
            }
            Err(_) => {
                Self::fail_navigation_for_tab(app, self.state.clone(), &tab_id, navigation_id);
                Err("WebView 导航内部错误".to_string())
            }
        }
    }

    /// 用户导航始终作用于当前标签；相同地址不会触发跨标签复用。
    pub(crate) fn navigate_with_app(
        &self,
        app: &AppHandle<Wry>,
        url: &str,
    ) -> Result<NavigationTicket, String> {
        let url = normalize_navigation_url(url);
        self.set_visible(true);
        if !self.is_open() {
            self.open(app, &url)?;
            return self
                .active_navigation_ticket()
                .ok_or_else(|| "浏览器导航状态未初始化".to_string());
        }

        // 一次加锁原子取快照：tab_id 与 needs_create 分离读取会引入竞态
        // 窗口（他人先建好 webview 时此处会重复创建顶掉旧实例）。
        let (tab_id, needs_create) = {
            let state = self.state.lock().map_err(|e| e.to_string())?;
            let tab_id = state
                .active_tab_id
                .clone()
                .ok_or_else(|| "当前没有可用标签".to_string())?;
            let needs_create = !state.webviews.contains_key(&tab_id);
            (tab_id, needs_create)
        };

        if needs_create {
            if url == "about:blank" {
                return Err("空白标签无需创建 WebView".to_string());
            }
            // 无管理界面场景（面板未展开/后台会话）补建 WebView：无头起步，
            // 面板就绪后由前端 instanceShow 请进面板。
            let webview = Self::create_webview_for_tab(
                app,
                self.state.clone(),
                &tab_id,
                &url,
                NavigationIntent::Normal,
                HEADLESS_RECT.0,
                HEADLESS_RECT.1,
                HEADLESS_RECT.2,
                HEADLESS_RECT.3,
            )?;
            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            state.webviews.insert(tab_id.clone(), webview);
            drop(state);
            self.start_url_poll(app, &url);
            self.start_event_poll(app);
            return self
                .navigation_ticket_for_tab(&tab_id)
                .ok_or_else(|| "浏览器导航状态未初始化".to_string());
        }

        self.navigate(app, &url)
    }

    /// Agent 按主域名复用自己的工作标签，不占用用户标签。
    pub(crate) fn navigate_for_agent(
        &self,
        app: &AppHandle<Wry>,
        url: &str,
    ) -> Result<NavigationTicket, String> {
        let url = normalize_navigation_url(url);
        self.set_visible(true);
        let agent_domain = agent_domain_for_url(&url)?;
        let agent_tab_id = {
            let state = self.state.lock().map_err(|e| e.to_string())?;
            agent_tab_id_for_domain(&state, &agent_domain)
        };

        if let Some(tab_id) = agent_tab_id {
            self.tab_switch(&tab_id)?;
            let (needs_create, intent) = {
                let state = self.state.lock().map_err(|e| e.to_string())?;
                let retry = state
                    .navigation_signals
                    .get(&tab_id)
                    .and_then(|signal| signal.state.lock().ok())
                    .is_some_and(|navigation| {
                        navigation.phase == NavigationPhase::Failed
                            && normalize_url_for_compare(&navigation.requested_url)
                                == normalize_url_for_compare(&url)
                    });
                (
                    !state.webviews.contains_key(&tab_id),
                    if retry {
                        NavigationIntent::Retry
                    } else {
                        NavigationIntent::Normal
                    },
                )
            };

            if needs_create {
                let webview = Self::create_webview_for_tab(
                    app,
                    self.state.clone(),
                    &tab_id,
                    &url,
                    intent,
                    HEADLESS_RECT.0,
                    HEADLESS_RECT.1,
                    HEADLESS_RECT.2,
                    HEADLESS_RECT.3,
                )?;
                let mut state = self.state.lock().map_err(|e| e.to_string())?;
                state.webviews.insert(tab_id.clone(), webview);
                drop(state);
                self.start_url_poll(app, &url);
                self.start_event_poll(app);
                return self
                    .navigation_ticket_for_tab(&tab_id)
                    .ok_or_else(|| "浏览器导航状态未初始化".to_string());
            }

            return self.navigate_with_intent(app, &url, intent);
        }

        // Agent 工作标签一律无头创建：是否把页面呈现给用户由前端
        // （browser:open 事件 → 拓展区面板 → instanceShow）决定。
        let tab_id =
            self.tab_new_with_source(app, &url, BrowserTabSource::Agent, Some(agent_domain), None)?;
        self.start_url_poll(app, &url);
        self.start_event_poll(app);
        self.navigation_ticket_for_tab(&tab_id)
            .ok_or_else(|| "浏览器导航状态未初始化".to_string())
    }

    fn navigation_ticket_for_tab(&self, tab_id: &str) -> Option<NavigationTicket> {
        let state = self.state.lock().ok()?;
        let signal = state.navigation_signals.get(tab_id)?;
        let navigation = signal.state.lock().ok()?;
        Some(NavigationTicket {
            tab_id: tab_id.to_string(),
            navigation_id: navigation.navigation_id,
        })
    }

    fn active_navigation_ticket(&self) -> Option<NavigationTicket> {
        let tab_id = self.state.lock().ok()?.active_tab_id.clone()?;
        self.navigation_ticket_for_tab(&tab_id)
    }

    pub fn load_html(&self, html: &str) -> Result<(), String> {
        let state = self.state.lock().map_err(|e| e.to_string())?;
        if let Some(wv) = state.active_webview() {
            let encoded = base64_url::encode(html.as_bytes());
            let data_url = format!("data:text/html;base64,{encoded}");
            let parsed_url: Url = data_url
                .parse()
                .map_err(|e| format!("data URL 构造失败：{e}"))?;
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wv.navigate(parsed_url)));
            result
                .map_err(|_| "WebView 导航内部错误".to_string())?
                .map_err(|e| format!("加载 HTML 失败：{e}"))?;
        }
        Ok(())
    }

    pub fn eval(&self, js: &str) -> Result<(), String> {
        let state = self.state.lock().map_err(|e| e.to_string())?;
        if let Some(wv) = state.active_webview() {
            wv.eval(js).map_err(|e| format!("执行 JS 失败：{e}"))?;
        }
        Ok(())
    }

    /// 执行 JS 并返回结果文本（无活跃页或超时返回 None）；供 webview 原语使用。
    pub fn eval_result_text(&self, js: &str) -> Option<String> {
        self.eval_with_result(js)
    }

    pub(crate) fn eval_with_result(&self, js: &str) -> Option<String> {
        self.eval_with_result_timeout(js, Duration::from_secs(15))
    }

    pub(crate) fn eval_with_result_timeout(&self, js: &str, timeout: Duration) -> Option<String> {
        let tab_id = self.state.lock().ok()?.active_tab_id.clone()?;
        self.eval_tab_with_result_timeout(&tab_id, js, timeout)
    }

    fn eval_tab_with_result_timeout(
        &self,
        tab_id: &str,
        js: &str,
        timeout: Duration,
    ) -> Option<String> {
        let (sender, rx) = std::sync::mpsc::channel();
        let tx = Arc::new(std::sync::Mutex::new(Some(sender)));
        {
            let state = self.state.lock().ok()?;
            let webview = state.webviews.get(tab_id)?;
            webview
                .eval_with_callback(js, move |result| {
                    if let Ok(mut guard) = tx.lock() {
                        if let Some(tx) = guard.take() {
                            let _ = tx.send(result);
                        }
                    }
                })
                .ok()?;
        }
        rx.recv_timeout(timeout).ok()
    }

    /// 执行 `getFullText(max_chars)` 并带 TTL 缓存去重。
    ///
    /// url_poll / ObservePage 等多个轮询线程都会读取页面全文，复杂页面上单次
    /// getFullText 耗时数秒。`FULL_TEXT_CACHE_TTL` 内的重复请求直接返回缓存，
    /// 避免多线程并发遍历 DOM 导致渲染线程饱和。
    pub(crate) fn eval_full_text_cached(&self, tab_id: &str, max_chars: usize) -> Option<String> {
        let now = Instant::now();
        // 先查缓存：TTL 内直接返回，避免并发遍历 DOM
        let cached: Option<(Instant, String)> = {
            let state = self.state.lock().ok()?;
            state
                .full_text_cache
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(|(ts, raw)| (*ts, raw.clone())))
        };
        if let Some((ts, raw)) = cached {
            if now.duration_since(ts) < FULL_TEXT_CACHE_TTL {
                return Some(raw);
            }
        }
        let js = format!(
            "(function(){{try{{var t=window.__tiangong_bridge.getFullText({max_chars});return JSON.stringify(t)}}catch(e){{return '{{}}'}}}})()"
        );
        let raw = self.eval_tab_with_result_timeout(tab_id, &js, POLL_EVAL_TIMEOUT)?;
        if let Ok(state) = self.state.lock() {
            *state
                .full_text_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some((now, raw.clone()));
        }
        Some(raw)
    }

    pub(crate) fn drain_events(&self) -> Vec<BrowserEvent> {
        let mut live_count = 0usize;
        let mut events = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(e) => e.into_inner(),
            };
            std::mem::take(&mut state.pending_events)
        };
        let cached_count = events.len();

        if let Some(raw) = self.eval_with_result_timeout(
            "(function(){try{return window.__tiangong_bridge.observer.drainAllEvents()}catch(e){return[]}})()",
            POLL_EVAL_TIMEOUT,
        ) {
            if raw != "[]" && !raw.is_empty() {
                if let Ok(mut current) = serde_json::from_str::<Vec<BrowserEvent>>(&raw) {
                    live_count = current.len();
                    events.append(&mut current);
                }
            }
        }

        events.sort_by_key(|event| match event {
            BrowserEvent::DialogOpened { timestamp, .. }
            | BrowserEvent::DialogClosed { timestamp }
            | BrowserEvent::ContentChanged { timestamp, .. }
            | BrowserEvent::UserClick { timestamp, .. }
            | BrowserEvent::UserInput { timestamp, .. }
            | BrowserEvent::UserNavigation { timestamp, .. }
            | BrowserEvent::NetworkResponse { timestamp, .. } => *timestamp,
        });
        let network_count = events
            .iter()
            .filter(|event| matches!(event, BrowserEvent::NetworkResponse { .. }))
            .count();
        debug!(
            cached_count,
            live_count,
            total_count = events.len(),
            network_count,
            "browser manager drain_events"
        );
        events
    }

    pub fn ack_events(&self, events: &[BrowserEvent]) -> usize {
        if events.is_empty() {
            return 0;
        }
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(e) => e.into_inner(),
        };
        let before = state.pending_events.len();
        state
            .pending_events
            .retain(|event| !events.iter().any(|acked| acked == event));
        let removed = before.saturating_sub(state.pending_events.len());
        debug!(
            ack_count = events.len(),
            removed,
            pending_len = state.pending_events.len(),
            "browser manager ack_events"
        );
        removed
    }

    fn wait_for_navigation(
        &self,
        ticket: &NavigationTicket,
        timeout: Duration,
    ) -> Option<TabNavigationState> {
        let signal = {
            let state = match self.state.lock() {
                Ok(s) => s,
                Err(_) => return None,
            };
            state.navigation_signals.get(&ticket.tab_id).cloned()
        };
        let signal = signal?;
        let start = std::time::Instant::now();
        let mut navigation = match signal.state.lock() {
            Ok(g) => g,
            Err(_) => return None,
        };
        loop {
            if navigation.navigation_id != ticket.navigation_id
                || navigation.phase != NavigationPhase::Loading
            {
                return Some(navigation.clone());
            }
            let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
                return Some(navigation.clone());
            };
            let result = signal.cvar.wait_timeout(navigation, remaining);
            match result {
                Ok((g, _)) => {
                    navigation = g;
                }
                Err(_) => return None,
            }
        }
    }

    pub(crate) fn fetch_page_content(
        &self,
        url: &str,
        max_chars: usize,
        ticket: &NavigationTicket,
    ) -> BrowserResponse {
        let error_response = |err: String| BrowserResponse {
            ok: false,
            title: String::new(),
            content: String::new(),
            final_url: url.to_string(),
            error: Some(err),
        };

        let t0 = std::time::Instant::now();
        let navigation = self.wait_for_navigation(
            ticket,
            NAVIGATION_TIMEOUT.saturating_add(Duration::from_secs(1)),
        );
        debug!(
            elapsed_ms = t0.elapsed().as_millis() as u64,
            "browser wait_for_navigation"
        );
        let Some(navigation) = navigation else {
            warn!(
                tab_id = %ticket.tab_id,
                url,
                "fetch 等待导航失败：无导航信号"
            );
            return error_response(PAGE_LOAD_ERROR_MESSAGE.to_string());
        };
        if navigation.navigation_id != ticket.navigation_id {
            return error_response("页面加载已被新的导航替代".to_string());
        }
        match navigation.phase {
            NavigationPhase::Failed | NavigationPhase::Loading => {
                warn!(
                    tab_id = %ticket.tab_id,
                    url,
                    phase = ?navigation.phase,
                    started_url = ?navigation.started_url,
                    document_id = ?navigation.document_id,
                    waited_ms = t0.elapsed().as_millis() as u64,
                    "fetch 等待导航未达 Loaded（document_id 为 None 说明页面文档事件从未到达）"
                );
                return error_response(PAGE_LOAD_ERROR_MESSAGE.to_string());
            }
            NavigationPhase::Loaded => {
                // 资源页（图片/PDF/音视频）没有可注入的正文提取脚本，导航
                // 完成即成功：标题取文件名、正文留空，不再等必然超时的 eval。
                if is_non_html_resource_url(&navigation.requested_url) {
                    let final_url = navigation
                        .final_url
                        .clone()
                        .unwrap_or_else(|| url.to_string());
                    let title = resource_page_title(&final_url);
                    return BrowserResponse {
                        ok: true,
                        title,
                        content: String::new(),
                        final_url,
                        error: None,
                    };
                }
            }
        }

        let result = self.eval_tab_with_result_timeout(
            &ticket.tab_id,
            &format!("window.__tiangong_bridge.getFullText({max_chars})"),
            Duration::from_secs(4),
        );
        if result.is_none() {
            warn!(
                tab_id = %ticket.tab_id,
                url,
                "fetch 正文提取脚本未返回（getFullText 注入无响应）"
            );
        }

        let still_current = self
            .navigation_ticket_for_tab(&ticket.tab_id)
            .is_some_and(|current| current.navigation_id == ticket.navigation_id);
        if !still_current {
            return error_response("页面加载已被新的导航替代".to_string());
        }

        match result {
            Some(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(data) => {
                    let title = data["title"].as_str().unwrap_or("").to_string();
                    let content = data["text"].as_str().unwrap_or("").to_string();
                    let final_url = navigation.final_url.unwrap_or_else(|| url.to_string());
                    BrowserResponse {
                        ok: true,
                        title,
                        content,
                        final_url,
                        error: None,
                    }
                }
                Err(e) => {
                    warn!(error = %e, "browser JSON parse error");
                    error_response("解析页面内容失败".to_string())
                }
            },
            None => error_response("获取页面内容超时".to_string()),
        }
    }

    pub fn get_snapshot(&self) -> Option<BrowserPageSnapshot> {
        let state = self.state.lock().ok()?;
        let active_id = state.active_tab_id.as_ref()?;
        state.latest_snapshots.get(active_id).cloned()
    }

    pub fn current_snapshot_without_events(&self, max_chars: usize) -> Option<BrowserPageSnapshot> {
        if !self.is_visible() {
            return None;
        }
        if let Some(mut snapshot) = self.get_snapshot() {
            if matches!(&snapshot.status, PageStatus::Loading | PageStatus::Error(_)) {
                let state = self.state.lock().ok()?;
                snapshot.tabs = state.tabs.clone();
                snapshot.active_tab_id = state.active_tab_id.clone();
                return Some(snapshot);
            }
        }
        let raw = self.eval_with_result(&format!(
            "(function(){{try{{var t=window.__tiangong_bridge.getFullText({max_chars});var a=window.__tiangong_bridge.annotation.getAnnotations();if(a&&a.count>0){{t.text+='\\n\\n[页面批注] '+JSON.stringify(a.annotations);}}return JSON.stringify(t);}}catch(e){{return ''}}}})()"
        ))?;
        let data = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
        let (tabs, active_tab_id) = {
            let state = match self.state.lock() {
                Ok(state) => state,
                Err(e) => e.into_inner(),
            };
            (state.tabs.clone(), state.active_tab_id.clone())
        };
        Some(BrowserPageSnapshot {
            title: data["title"].as_str().unwrap_or("").to_string(),
            url: data["url"].as_str().unwrap_or("").to_string(),
            text: data["text"].as_str().unwrap_or("").to_string(),
            status: PageStatus::Loaded,
            tabs,
            active_tab_id,
            events: Vec::new(),
        })
    }

    pub fn tab_list(&self) -> Vec<BrowserTab> {
        self.state
            .lock()
            .map(|s| s.tabs.clone())
            .unwrap_or_default()
    }

    pub fn tab_list_with_active(&self) -> TabListResponse {
        match self.state.lock() {
            Ok(s) => TabListResponse {
                tabs: s.tabs.clone(),
                active_tab_id: s.active_tab_id.clone(),
            },
            Err(e) => {
                let s = e.into_inner();
                TabListResponse {
                    tabs: s.tabs.clone(),
                    active_tab_id: s.active_tab_id.clone(),
                }
            }
        }
    }

    pub fn snapshot_tabs(&self) -> BrowserTabsSnapshot {
        match self.state.lock() {
            Ok(s) => BrowserTabsSnapshot {
                session_id: s.active_session_id.clone(),
                tabs: s.tabs.clone(),
                active_tab_id: s.active_tab_id.clone(),
            },
            Err(e) => {
                let s = e.into_inner();
                BrowserTabsSnapshot {
                    session_id: s.active_session_id.clone(),
                    tabs: s.tabs.clone(),
                    active_tab_id: s.active_tab_id.clone(),
                }
            }
        }
    }

    pub fn switch_session(
        &self,
        app: &AppHandle<Wry>,
        session_id: &str,
        tabs_to_restore: Vec<BrowserTab>,
        active_tab_id: Option<String>,
    ) -> Result<BrowserTabsSnapshot, String> {
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        if state.active_session_id.as_deref() == Some(session_id)
            && state.tabs == tabs_to_restore
            && state.active_tab_id == active_tab_id
        {
            let active_tab = state
                .active_tab_id
                .as_ref()
                .and_then(|id| state.tabs.iter().find(|tab| tab.id == *id).cloned());
            let needs_webview = active_tab.as_ref().is_some_and(|tab| {
                !tab.url.starts_with("about:") && !state.webviews.contains_key(&tab.id)
            });
            if !needs_webview {
                return Ok(BrowserTabsSnapshot {
                    session_id: state.active_session_id.clone(),
                    tabs: state.tabs.clone(),
                    active_tab_id: state.active_tab_id.clone(),
                });
            }

            if let Some(tab) = active_tab {
                drop(state);
                // 补建一律无头：会话恢复后由前端面板就绪时 instanceShow 请显。
                let webview = Self::create_webview_for_tab(
                    app,
                    self.state.clone(),
                    &tab.id,
                    &tab.url,
                    NavigationIntent::Restore,
                    HEADLESS_RECT.0,
                    HEADLESS_RECT.1,
                    HEADLESS_RECT.2,
                    HEADLESS_RECT.3,
                )?;
                let mut state = self.state.lock().map_err(|e| e.to_string())?;
                state.webviews.insert(tab.id.clone(), webview);
                drop(state);
                self.start_url_poll(app, &tab.url);
                self.start_event_poll(app);
                let state = self.state.lock().map_err(|e| e.to_string())?;
                return Ok(BrowserTabsSnapshot {
                    session_id: state.active_session_id.clone(),
                    tabs: state.tabs.clone(),
                    active_tab_id: state.active_tab_id.clone(),
                });
            }

            return Ok(BrowserTabsSnapshot {
                session_id: state.active_session_id.clone(),
                tabs: state.tabs.clone(),
                active_tab_id: state.active_tab_id.clone(),
            });
        }

        // 同一会话下的重新同步：按 id 增量同步，避免 url/title 元数据差异
        // （后端导航时持续更新 state.tabs）触发全量 reset 而销毁所有 webview。
        // 仅当 session 真正切换时才走完整重建。
        let same_session = state.active_session_id.as_deref() == Some(session_id);
        let next_active_id = resolve_active_browser_tab(&tabs_to_restore, active_tab_id);

        let mut tabs_needing_webview: Vec<BrowserTab> = Vec::new();
        if same_session {
            Self::sync_tabs_by_id(&mut state, &tabs_to_restore);
            state.active_tab_id = next_active_id.clone();
            state
                .visible
                .store(true, std::sync::atomic::Ordering::Relaxed);
        } else {
            reset_runtime_state(&mut state, true);
            state.tabs = tabs_to_restore;
            state.active_tab_id = next_active_id.clone();
            restore_tab_runtime_metadata(&mut state);
            state.active_session_id = Some(session_id.to_string());
            state
                .visible
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        // 仅当 active tab 缺少 webview 且 url 非 about: 时才补建，
        // 其余现有 webview（包括非活跃的）一律保留。
        if let Some(active_id) = state.active_tab_id.as_ref() {
            let active_tab = state.tabs.iter().find(|tab| &tab.id == active_id).cloned();
            if let Some(tab) = active_tab {
                if !tab.url.starts_with("about:") && !state.webviews.contains_key(&tab.id) {
                    tabs_needing_webview.push(tab);
                }
            }
        }
        drop(state);

        for tab in tabs_needing_webview {
            // 补建一律无头：会话恢复后由前端面板就绪时 instanceShow 请显。
            let webview = Self::create_webview_for_tab(
                app,
                self.state.clone(),
                &tab.id,
                &tab.url,
                NavigationIntent::Restore,
                HEADLESS_RECT.0,
                HEADLESS_RECT.1,
                HEADLESS_RECT.2,
                HEADLESS_RECT.3,
            )?;
            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            state.webviews.insert(tab.id.clone(), webview);
            drop(state);
            self.start_url_poll(app, &tab.url);
            self.start_event_poll(app);
        }

        let state = self.state.lock().map_err(|e| e.to_string())?;

        Ok(BrowserTabsSnapshot {
            session_id: state.active_session_id.clone(),
            tabs: state.tabs.clone(),
            active_tab_id: state.active_tab_id.clone(),
        })
    }

    /// 同一会话下按 id 增量同步 tabs。
    ///
    /// - 不在 `next_tabs` 中的 id：关闭其 webview 并移除记录；
    /// - 新增的 id：按元数据补建记录（history/navigation_signals 等），
    ///   但**不**创建 webview（由调用方按需补建）；
    /// - id 仍存在的 tab：保留其现有 webview，仅用传入的 url/title 覆盖元数据，
    ///   避免 url/title 字段差异（后端导航持续更新）触发误销毁。
    fn sync_tabs_by_id(state: &mut BrowserState, next_tabs: &[BrowserTab]) {
        let next_ids: std::collections::HashSet<&str> =
            next_tabs.iter().map(|tab| tab.id.as_str()).collect();

        // 关闭被移除的 tab：先收集 id，再逐个释放锁以关闭 webview。
        let removed_ids: Vec<String> = state
            .tabs
            .iter()
            .filter(|tab| !next_ids.contains(tab.id.as_str()))
            .map(|tab| tab.id.clone())
            .collect();

        // 移除被移除 tab 的记录与 webview（close_webviews 由此处直接处理，
        // 因为这些是显式移除，而非元数据差异）。
        let mut webviews_to_close: Vec<Webview<Wry>> = Vec::new();
        for id in &removed_ids {
            if let Some(wv) = state.webviews.remove(id) {
                webviews_to_close.push(wv);
            }
            state.navigation_signals.remove(id);
            state.latest_snapshots.remove(id);
            state.tab_histories.remove(id);
            state.tab_history_indices.remove(id);
        }
        state.tabs.retain(|tab| next_ids.contains(tab.id.as_str()));
        // 关闭被移除 tab 的 webview（从 map 移除后直接 drop，drop 时关闭）。
        drop(webviews_to_close);

        // 更新仍存在的 tab 元数据（仅 url/title），保留其 webview。
        for next in next_tabs {
            if let Some(existing) = state.tabs.iter_mut().find(|t| t.id == next.id) {
                existing.url = next.url.clone();
                existing.title = next.title.clone();
            }
        }

        // 追加新增 tab 并补建元数据（不含 webview）。
        let existing_ids: std::collections::HashSet<String> =
            state.tabs.iter().map(|tab| tab.id.clone()).collect();
        for tab in next_tabs {
            if existing_ids.contains(&tab.id) {
                continue;
            }
            // 新增 tab：插入记录并补建元数据（不含 webview）。
            state.tabs.push(tab.clone());
            state
                .navigation_signals
                .insert(tab.id.clone(), navigation_signal(&tab.url));
            if !tab.url.starts_with("about:") {
                let title = if tab.title.is_empty() {
                    tab.url.clone()
                } else {
                    tab.title.clone()
                };
                state.tab_histories.insert(
                    tab.id.clone(),
                    vec![HistoryEntry {
                        url: tab.url.clone(),
                        title,
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                    }],
                );
                state.tab_history_indices.insert(tab.id.clone(), 0);
            }
        }
    }

    pub fn tab_new(&self, app: &AppHandle<Wry>, url: &str) -> Result<String, String> {
        self.tab_new_with_source(app, url, BrowserTabSource::User, None, None)
    }

    /// 以插件自带编号新建标签（阶段 3：标签模型上移插件后由插件主导标识）。
    pub fn tab_new_with_id(
        &self,
        app: &AppHandle<Wry>,
        url: &str,
        tab_id: &str,
    ) -> Result<String, String> {
        self.tab_new_with_source(app, url, BrowserTabSource::User, None, Some(tab_id))
    }

    fn tab_new_with_source(
        &self,
        app: &AppHandle<Wry>,
        url: &str,
        source: BrowserTabSource,
        agent_domain: Option<String>,
        external_id: Option<&str>,
    ) -> Result<String, String> {
        // 插件可自带标签编号（阶段 3：标签模型上移插件后由插件主导标识）
        let url = normalize_navigation_url(url);
        let tab_id = external_id
            .map(str::to_string)
            .unwrap_or_else(|| scru128::new().to_string());
        let is_blank = url == "about:blank";

        {
            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            // 隐藏旧活跃 WebView
            if let Some(old_id) = &state.active_tab_id {
                if let Some(old_wv) = state.webviews.get(old_id) {
                    let _ = old_wv.set_position(LogicalPosition::new(-10000, -10000));
                }
            }
            // 新标签一律无头起步（创建位置不可由调用方指定，展示交给前端
            // webview.instanceShow）。
            state
                .navigation_signals
                .insert(tab_id.clone(), navigation_signal(&url));
            state.tabs.push(BrowserTab {
                id: tab_id.clone(),
                url: url.clone(),
                title: String::new(),
                source,
                agent_domain,
            });
            state.active_tab_id = Some(tab_id.clone());
        }

        // about:blank 不创建 WebView（WKWebView 对 about:blank 的 URL() 返回 None，
        // 会导致 Tauri 权限检查内部 panic），延迟到 navigate 时按需创建
        if !is_blank {
            let webview = Self::create_webview_for_tab(
                app,
                self.state.clone(),
                &tab_id,
                &url,
                NavigationIntent::Normal,
                HEADLESS_RECT.0,
                HEADLESS_RECT.1,
                HEADLESS_RECT.2,
                HEADLESS_RECT.3,
            )?;

            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            state.webviews.insert(tab_id.clone(), webview);
        }
        Ok(tab_id)
    }

    /// 实例直达原语：把指定标签的 webview 显示到给定矩形并置为活跃
    /// （阶段 2 插件编排用——显示语义天然互斥，切换时隐藏原活跃实例）。
    /// 标签已有元数据但尚未创建 webview 实例时，此处按需创建。
    pub fn show_tab_at(
        &self,
        app: &AppHandle<Wry>,
        tab_id: &str,
        rect: (f64, f64, f64, f64),
    ) -> Result<(), String> {
        let (tab, has_webview) = {
            let state = self.state.lock().map_err(|e| e.to_string())?;
            let tab = state
                .tabs
                .iter()
                .find(|t| t.id == tab_id)
                .cloned()
                .ok_or_else(|| format!("标签 {tab_id} 不存在"))?;
            (tab, state.webviews.contains_key(tab_id))
        };
        // 标签尚无实例（非空白页）时先创建再显示。
        if !has_webview && !tab.url.starts_with("about:") {
            let webview = Self::create_webview_for_tab(
                app,
                self.state.clone(),
                tab_id,
                &tab.url,
                NavigationIntent::Restore,
                rect.0,
                rect.1,
                rect.2,
                rect.3,
            )?;
            {
                let mut state = self.state.lock().map_err(|e| e.to_string())?;
                state.webviews.insert(tab_id.to_string(), webview);
                state.browser_rect = rect;
                state.active_tab_id = Some(tab_id.to_string());
                state
                    .visible
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            self.start_url_poll(app, &tab.url);
            self.start_event_poll(app);
            return Ok(());
        }
        let is_active = {
            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            state.browser_rect = rect;
            let is_active = state.active_tab_id.as_deref() == Some(tab_id);
            if is_active {
                if let Some(wv) = state.webviews.get(tab_id) {
                    let _ = wv.set_size(LogicalSize::new(rect.2, rect.3));
                    let _ = wv.set_position(LogicalPosition::new(rect.0, rect.1));
                }
                state
                    .visible
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            is_active
        };
        if !is_active {
            // tab_switch 读最新 browser_rect：隐藏原活跃并摆放到目标矩形
            self.tab_switch(tab_id)?;
        }
        Ok(())
    }

    /// 实例直达原语：把指定标签的 webview 挪出可视区（不改变活跃标签）。
    pub fn hide_tab(&self, tab_id: &str) -> Result<(), String> {
        let state = self.state.lock().map_err(|e| e.to_string())?;
        let wv = state
            .webviews
            .get(tab_id)
            .ok_or_else(|| format!("标签 {tab_id} 不存在或尚未加载 webview"))?;
        let _ = wv.set_position(LogicalPosition::new(-10000, -10000));
        Ok(())
    }

    /// 实例直达原语：对指定标签执行脚本并等待结果（与活跃标签 eval 同
    /// 回执机制），阶段 3 协作策略上移插件的基础。
    pub fn eval_tab_result_text(&self, tab_id: &str, js: &str) -> Option<String> {
        self.eval_tab_with_result_timeout(tab_id, js, Duration::from_secs(15))
    }

    pub fn tab_switch(&self, tab_id: &str) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|e| e.to_string())?;

        // 检查标签是否存在
        state
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .ok_or_else(|| format!("标签 {tab_id} 不存在"))?;

        if state.active_tab_id.as_deref() == Some(tab_id) {
            return Ok(());
        }

        // 隐藏旧活跃 WebView
        if let Some(old_id) = &state.active_tab_id {
            if let Some(old_wv) = state.webviews.get(old_id) {
                let _ = old_wv.set_position(LogicalPosition::new(-10000, -10000));
            }
        }

        // 显示目标 WebView（about:blank 标签可能没有 WebView，属于正常情况）。
        // browser_rect 无效（尚无前端展示，全是无头标签）时保持屏幕外。
        if let Some(new_wv) = state.webviews.get(tab_id) {
            if rect_is_displayable(state.browser_rect) {
                let rect = state.browser_rect;
                let _ = new_wv.set_position(LogicalPosition::new(rect.0, rect.1));
                let _ = new_wv.set_size(LogicalSize::new(rect.2, rect.3));
            }
        }

        state.active_tab_id = Some(tab_id.to_string());
        drop(state);
        Ok(())
    }

    pub fn tab_close(&self, tab_id: &str) -> Result<(), String> {
        let (was_active, closed_pos) = {
            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            let pos = state
                .tabs
                .iter()
                .position(|t| t.id == tab_id)
                .ok_or_else(|| format!("标签 {tab_id} 不存在"))?;

            state.tabs.remove(pos);
            // 关闭对应的 WebView
            if let Some(webview) = state.webviews.remove(tab_id) {
                let _ = webview.close();
            }
            state.navigation_signals.remove(tab_id);
            state.latest_snapshots.remove(tab_id);
            // 清除该标签页的历史
            state.tab_histories.remove(tab_id);
            state.tab_history_indices.remove(tab_id);
            let was_active = state.active_tab_id.as_deref() == Some(tab_id);
            (was_active, pos)
        };

        if was_active {
            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            if state.tabs.is_empty() {
                // 最后一个 tab 关闭：隐藏浏览器面板（webview 已在上面 remove+close），
                // 停轮询，清运行时 state，visible=false 让前端感知浏览器已无内容。
                state
                    .poll_stop
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                state
                    .event_poll_stop
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                // 隐藏残留 webview（理论上 tabs 空时 webviews 也空，兜底）
                for wv in state.webviews.values() {
                    let _ = wv.set_size(LogicalSize::new(0.0, 0.0));
                    let _ = wv.set_position(LogicalPosition::new(-10000, -10000));
                }
                state.navigation_signals.clear();
                state.latest_snapshots.clear();
                state.last_known_url.clear();
                state.last_known_text_signature.clear();
                state.pending_events.clear();
                state.active_tab_id = None;
                state.tab_histories.clear();
                state.tab_history_indices.clear();
                state
                    .visible
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            } else {
                // 切换到关闭位置处的相邻标签
                let new_pos = closed_pos.min(state.tabs.len().saturating_sub(1));
                let new_id = state.tabs[new_pos].id.clone();

                // 显示新活跃标签的 WebView（browser_rect 无效时保持屏幕外）
                if let Some(new_wv) = state.webviews.get(&new_id) {
                    if rect_is_displayable(state.browser_rect) {
                        let rect = state.browser_rect;
                        let _ = new_wv.set_position(LogicalPosition::new(rect.0, rect.1));
                        let _ = new_wv.set_size(LogicalSize::new(rect.2, rect.3));
                    }
                }
                state.active_tab_id = Some(new_id);
            }
        }
        Ok(())
    }

    /// 更新活跃标签的 URL 和标题
    pub fn update_active_tab(&self, url: &str, title: &str) {
        if let Ok(mut state) = self.state.lock() {
            let active_id = state.active_tab_id.clone();
            if let Some(active_id) = active_id {
                if let Some(tab) = state.tabs.iter_mut().find(|t| t.id == active_id) {
                    tab.url = url.to_string();
                    if !title.is_empty() {
                        tab.title = title.to_string();
                    }
                }
            }
        }
    }

    /// 获取当前活跃标签的 WebView URL
    pub fn current_url(&self) -> Option<String> {
        let state = self.state.lock().ok()?;
        let wv = state.active_webview()?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wv.url())) {
            Ok(Ok(u)) => Some(u.to_string()),
            _ => None,
        }
    }

    /// 获取标签页浏览历史
    pub fn get_tab_history(&self, tab_id: Option<&str>) -> Option<TabHistoryResult> {
        let state = self.state.lock().ok()?;
        let target_id = tab_id
            .map(|s| s.to_string())
            .or_else(|| state.active_tab_id.clone())?;
        let entries = state.tab_histories.get(&target_id)?.clone();
        let current_index = state
            .tab_history_indices
            .get(&target_id)
            .copied()
            .unwrap_or(0) as i32;
        Some(TabHistoryResult {
            tab_id: target_id,
            entries,
            current_index,
        })
    }

    /// 获取全局浏览历史（分页，最新在前）
    pub fn get_global_history(&self, offset: usize, limit: usize) -> Vec<HistoryEntry> {
        let shared = self.shared_state();
        let history = shared
            .global_history
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let total = history.len();
        if offset >= total {
            return Vec::new();
        }
        // 倒序切片：offset=0 取最后 limit 条
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(limit);
        history[start..end].iter().rev().cloned().collect()
    }

    /// 清空全局浏览历史
    pub fn clear_global_history(&self) {
        let shared = self.shared_state();
        {
            let mut history = shared
                .global_history
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            history.clear();
        }
        persist_global_history(&shared);
    }

    /// 删除全局历史中指定 URL 的条目
    pub fn delete_global_history_entry(&self, url: &str) {
        let shared = self.shared_state();
        let should_persist = {
            let mut history = shared
                .global_history
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let before = history.len();
            history.retain(|entry| entry.url != url);
            before != history.len()
        };
        if should_persist {
            persist_global_history(&shared);
        }
    }

    /// 清除指定标签页的历史
    pub fn clear_tab_history(&self, tab_id: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.tab_histories.remove(tab_id);
            state.tab_history_indices.remove(tab_id);
        }
    }

    /// 初始化标签页历史
    pub fn init_tab_history(&self, tab_id: &str, url: &str, title: &str) {
        if let Ok(mut state) = self.state.lock() {
            let entry = HistoryEntry {
                url: url.to_string(),
                title: if title.is_empty() {
                    url.to_string()
                } else {
                    title.to_string()
                },
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            };
            state.tab_histories.insert(tab_id.to_string(), vec![entry]);
            state.tab_history_indices.insert(tab_id.to_string(), 0);
        }
    }
}

fn tab_navigation_phase(state: &BrowserState, tab_id: &str) -> Option<NavigationPhase> {
    let signal = state.navigation_signals.get(tab_id)?;
    let navigation = signal.state.lock().ok()?;
    Some(navigation.phase)
}

fn loaded_navigation_id(state: &BrowserState, tab_id: &str) -> Option<u64> {
    let signal = state.navigation_signals.get(tab_id)?;
    let navigation = signal.state.lock().ok()?;
    (navigation.phase == NavigationPhase::Loaded).then_some(navigation.navigation_id)
}

fn is_recordable_history_url(url: &str) -> bool {
    !url.is_empty() && !url.starts_with("about:") && !url.starts_with("data:")
}

fn history_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn append_tab_navigation(
    state: &mut BrowserState,
    tab_id: &str,
    url: &str,
    title: Option<&str>,
) -> Option<usize> {
    if !is_recordable_history_url(url) {
        return None;
    }
    let title = title.filter(|title| !title.is_empty()).unwrap_or(url);
    let current_index = state.tab_history_indices.get(tab_id).copied();
    let entries = state.tab_histories.entry(tab_id.to_string()).or_default();
    if let Some(index) = current_index.filter(|index| *index < entries.len()) {
        entries.truncate(index.saturating_add(1));
    }
    entries.push(HistoryEntry {
        url: url.to_string(),
        title: title.to_string(),
        timestamp: history_timestamp(),
    });
    if entries.len() > 200 {
        let remove_count = entries.len().saturating_sub(160);
        entries.drain(0..remove_count);
    }
    let index = entries.len().saturating_sub(1);
    state.tab_history_indices.insert(tab_id.to_string(), index);
    Some(index)
}

fn apply_tab_navigation_intent(
    state: &mut BrowserState,
    tab_id: &str,
    url: &str,
    intent: NavigationIntent,
) -> Result<Option<usize>, String> {
    match intent {
        NavigationIntent::Normal => Ok(append_tab_navigation(state, tab_id, url, None)),
        NavigationIntent::History { target_index } => {
            let entries = state
                .tab_histories
                .get(tab_id)
                .ok_or_else(|| "当前标签没有导航记录".to_string())?;
            if entries.get(target_index).is_none() {
                return Err("目标导航记录不存在".to_string());
            }
            state
                .tab_history_indices
                .insert(tab_id.to_string(), target_index);
            Ok(Some(target_index))
        }
        NavigationIntent::Reload | NavigationIntent::Retry | NavigationIntent::Restore => {
            let current_index = state
                .tab_history_indices
                .get(tab_id)
                .copied()
                .filter(|index| {
                    state
                        .tab_histories
                        .get(tab_id)
                        .is_some_and(|entries| *index < entries.len())
                });
            Ok(current_index.or_else(|| append_tab_navigation(state, tab_id, url, None)))
        }
    }
}

fn update_tab_navigation_entry(
    state: &mut BrowserState,
    tab_id: &str,
    history_index: Option<usize>,
    url: &str,
    title: Option<&str>,
) {
    if !is_recordable_history_url(url) {
        return;
    }
    let index = history_index.or_else(|| append_tab_navigation(state, tab_id, url, title));
    let Some(index) = index else {
        return;
    };
    let Some(entry) = state
        .tab_histories
        .get_mut(tab_id)
        .and_then(|entries| entries.get_mut(index))
    else {
        return;
    };
    entry.url = url.to_string();
    if let Some(title) = title.filter(|title| !title.is_empty()) {
        entry.title = title.to_string();
    }
    entry.timestamp = history_timestamp();
    state.tab_history_indices.insert(tab_id.to_string(), index);
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn navigation_error_data_url(requested_url: &str) -> String {
    let escaped_url = escape_html(requested_url);
    let html = format!(
        r#"<!doctype html>
<html lang="zh-CN" data-tiangong-navigation-error="true">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>页面加载异常</title>
  <style>
    :root {{ color-scheme: light dark; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
    * {{ box-sizing: border-box; }}
    body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: Canvas; color: CanvasText; }}
    main {{ width: min(560px, calc(100% - 40px)); }}
    h1 {{ margin: 0 0 12px; font-size: 24px; letter-spacing: 0; }}
    p {{ margin: 0 0 20px; line-height: 1.6; color: GrayText; }}
    code {{ display: block; margin-bottom: 24px; padding: 12px; overflow-wrap: anywhere; border: 1px solid color-mix(in srgb, CanvasText 18%, transparent); border-radius: 6px; font-family: ui-monospace, monospace; font-size: 13px; }}
    a {{ display: inline-block; padding: 9px 14px; border-radius: 6px; background: CanvasText; color: Canvas; text-decoration: none; font-weight: 600; }}
  </style>
</head>
<body>
  <main>
    <h1>页面加载异常</h1>
    <p>{PAGE_LOAD_ERROR_MESSAGE}</p>
    <code>{escaped_url}</code>
    <a href="{escaped_url}">重新加载</a>
  </main>
</body>
</html>"#
    );
    format!(
        "data:text/html;base64,{}",
        base64_url::encode(html.as_bytes())
    )
}

fn reset_runtime_state(state: &mut BrowserState, close_webviews: bool) {
    state
        .poll_stop
        .store(true, std::sync::atomic::Ordering::Relaxed);
    state
        .event_poll_stop
        .store(true, std::sync::atomic::Ordering::Relaxed);
    state.poll_stop = Arc::new(std::sync::atomic::AtomicBool::new(true));
    state.event_poll_stop = Arc::new(std::sync::atomic::AtomicBool::new(true));

    if close_webviews {
        for (_, webview) in state.webviews.drain() {
            let _ = webview.close();
        }
    } else {
        state.webviews.clear();
    }

    for signal in state.navigation_signals.values() {
        if let Ok(mut navigation) = signal.state.lock() {
            navigation.phase = NavigationPhase::Failed;
        }
        signal.cvar.notify_all();
    }

    state.navigation_signals.clear();
    state.latest_snapshots.clear();
    state.last_known_url.clear();
    state.last_known_text_signature.clear();
    state.pending_events.clear();
    state.tabs.clear();
    state.active_tab_id = None;
    state.tab_histories.clear();
    state.tab_history_indices.clear();
}

fn resolve_active_browser_tab(
    tabs: &[BrowserTab],
    active_tab_id: Option<String>,
) -> Option<String> {
    active_tab_id
        .filter(|id| tabs.iter().any(|tab| tab.id == *id))
        .or_else(|| tabs.first().map(|tab| tab.id.clone()))
}

fn restore_tab_runtime_metadata(state: &mut BrowserState) {
    for tab in &state.tabs {
        state
            .navigation_signals
            .insert(tab.id.clone(), navigation_signal(&tab.url));
        if !tab.url.starts_with("about:") {
            let title = if tab.title.is_empty() {
                tab.url.clone()
            } else {
                tab.title.clone()
            };
            state.tab_histories.insert(
                tab.id.clone(),
                vec![HistoryEntry {
                    url: tab.url.clone(),
                    title,
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                }],
            );
            state.tab_history_indices.insert(tab.id.clone(), 0);
        }
    }
}

fn global_history_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".tiangong")
        .join("browser-history.json")
}

fn upsert_global_history(history: &mut Vec<HistoryEntry>, url: &str, title: &str) {
    let entry = HistoryEntry {
        url: url.to_string(),
        title: if title.is_empty() {
            url.to_string()
        } else {
            title.to_string()
        },
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    };
    if let Some(index) = history.iter().position(|item| item.url == url) {
        history.remove(index);
    }
    history.push(entry);
    if history.len() > 1000 {
        let keep_from = history.len() - 800;
        history.drain(0..keep_from);
    }
}

pub(crate) fn load_global_history() -> Vec<HistoryEntry> {
    let path = global_history_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn persist_global_history(shared: &Arc<BrowserSharedState>) {
    let entries = shared
        .global_history
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let path = global_history_path();
    if let Ok(content) = serde_json::to_string(&*entries) {
        let _ = std::fs::write(path, content);
    }
}

fn zoom_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".tiangong")
        .join("browser-zoom.json")
}

pub(crate) fn load_zoom() -> f64 {
    let path = zoom_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str::<f64>(&content).unwrap_or(1.0),
        Err(_) => 1.0,
    }
}

fn persist_zoom(shared: &Arc<BrowserSharedState>) {
    let zoom = shared.zoom_factor.lock().unwrap_or_else(|e| e.into_inner());
    let path = zoom_path();
    if let Ok(content) = serde_json::to_string(&*zoom) {
        let _ = std::fs::write(path, content);
    }
}

fn browser_data_directory(session_id: &str) -> PathBuf {
    // per-session data 目录隔离 cookie/storage；空 session_id 回退全局目录（兼容）
    let base = tiangong_config::io::storage_root().join("browser-data");
    let dir = if session_id.is_empty() {
        base
    } else {
        base.join(browser_session_directory_name(session_id))
    };
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 会话 scope 转合法的数据目录名。scope 形如 `webview:browser:<id>`，
/// Windows 文件名不允许冒号，WebView2 拿到含非法字符的 userDataFolder
/// 会报 os error 123（文件名、目录名或卷标语法不正确）导致创建失败；
/// 非 Windows 平台冒号合法，保持原样以沿用既有目录。
fn browser_session_directory_name(session_id: &str) -> String {
    #[cfg(windows)]
    {
        encode_session_directory_name(session_id)
    }
    #[cfg(not(windows))]
    {
        session_id.to_string()
    }
}

/// 只保留 ASCII 字母数字与短横线/下划线，其余 UTF-8 字节做稳定百分号
/// 编码，保证不同 scope 不碰撞、同一 scope 每次得到相同目录。
/// 非 Windows 构建下仅测试引用（平台分支不调用），豁免 dead_code。
#[cfg_attr(not(windows), allow(dead_code))]
fn encode_session_directory_name(session_id: &str) -> String {
    let mut encoded = String::from("session-");
    for byte in session_id.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            encoded.push(*byte as char);
        } else {
            encoded.push('%');
            encoded.push(
                char::from_digit((byte >> 4) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
            encoded.push(
                char::from_digit((byte & 0x0f) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_is_displayable_rejects_invalid_rects() {
        // 初始/收起后的失效矩形不可用于摆位（无头页面保持屏幕外）
        assert!(!rect_is_displayable((0.0, 0.0, 0.0, 0.0)));
        assert!(!rect_is_displayable((60.0, 60.0, 0.0, 720.0)));
        assert!(!rect_is_displayable((60.0, 60.0, 1024.0, 0.0)));
        assert!(!rect_is_displayable((60.0, 60.0, -1024.0, 720.0)));
        // 前端 instanceShow 下发的展示矩形宽高有效
        assert!(rect_is_displayable((60.0, 60.0, 1024.0, 720.0)));
        assert!(rect_is_displayable((0.0, 0.0, 1.0, 1.0)));
    }

    #[test]
    fn session_directory_name_encoding_is_windows_legal() {
        // scope 含冒号（webview:browser:<id>），Windows 不允许冒号出现在
        // 文件名中；编码后只含字母数字与 %，且同 scope 编码稳定。
        let scope = "webview:browser:03guj8e2h1rko94862xx59zf2";
        let encoded = encode_session_directory_name(scope);
        assert_eq!(
            encoded,
            "session-webview%3Abrowser%3A03guj8e2h1rko94862xx59zf2"
        );
        assert_eq!(encoded, encode_session_directory_name(scope));
        assert!(!encoded.contains(':'));
        assert_ne!(
            encode_session_directory_name("webview:browser:a"),
            encode_session_directory_name("webview:browser:b")
        );
        // 非 ASCII（如中文会话名）按 UTF-8 字节稳定编码
        assert_eq!(
            encode_session_directory_name("会话"),
            "session-%E4%BC%9A%E8%AF%9D"
        );
    }

    #[test]
    fn non_html_resource_url_detection_by_extension() {
        assert!(is_non_html_resource_url("file:///C:/Users/EDY/pic.PNG"));
        assert!(is_non_html_resource_url("file:///C:/Users/EDY/report.pdf"));
        assert!(is_non_html_resource_url("https://cdn.example.com/a.mp4"));
        // 查询串不影响扩展名识别
        assert!(is_non_html_resource_url(
            "https://cdn.example.com/a.png?v=2&size=large"
        ));
        // SVG 有 DOM 可脚本，按正常页面处理，不算资源页
        assert!(!is_non_html_resource_url("file:///C:/Users/EDY/icon.svg"));
        // HTML、无扩展名、非法 URL 不是资源页
        assert!(!is_non_html_resource_url("file:///C:/Users/EDY/page.html"));
        assert!(!is_non_html_resource_url("file:///C:/Users/EDY/noext"));
        assert!(!is_non_html_resource_url("not a url"));
    }

    #[test]
    fn fetch_page_content_returns_success_for_resource_pages() {
        // 资源页导航完成即成功：不再等待正文脚本（对图片/PDF 必然超时），
        // 标题取文件名、正文留空
        let manager = BrowserManager::new();
        let url = "file:///C:/docs/pic.png";
        {
            let mut state = manager.state.lock().unwrap();
            state
                .navigation_signals
                .insert("tab-1".to_string(), navigation_signal(url));
        }
        let response = manager.fetch_page_content(
            url,
            1000,
            &NavigationTicket {
                tab_id: "tab-1".to_string(),
                navigation_id: 0,
            },
        );
        assert!(response.ok);
        assert_eq!(response.error, None);
        assert_eq!(response.title, "pic.png");
        assert_eq!(response.final_url, url);
        assert!(response.content.is_empty());
        // 编码文件名解码为原文展示（%E6%8A%A5%E8%A1%A8 → 报表）
        {
            let mut state = manager.state.lock().unwrap();
            state.navigation_signals.insert(
                "tab-2".to_string(),
                navigation_signal("file:///C:/docs/%E6%8A%A5%E8%A1%A8.pdf"),
            );
        }
        let response = manager.fetch_page_content(
            "file:///C:/docs/%E6%8A%A5%E8%A1%A8.pdf",
            1000,
            &NavigationTicket {
                tab_id: "tab-2".to_string(),
                navigation_id: 0,
            },
        );
        assert_eq!(response.title, "报表.pdf");
    }

    #[test]
    fn resource_document_snapshot_is_complete_and_matching() {
        let snapshot = resource_document_snapshot("file:///C:/a.png", 7);
        assert_eq!(snapshot.document_id, "resource-7");
        assert_eq!(snapshot.ready_state, "complete");
        assert_eq!(snapshot.url, "file:///C:/a.png");
        assert_eq!(snapshot.title, "a.png");
        assert!(!snapshot.internal_error);
        assert!(snapshot.text.is_empty());
    }

    #[test]
    fn normalize_navigation_url_converts_windows_drive_paths() {
        assert_eq!(
            normalize_navigation_url(r"C:\Users\foo\bar.png"),
            "file:///C:/Users/foo/bar.png"
        );
        assert_eq!(
            normalize_navigation_url("c:/users/foo/bar.png"),
            "file:///C:/users/foo/bar.png"
        );
        assert_eq!(
            normalize_navigation_url(r"d:\docs\mixed/slashes\x.pdf"),
            "file:///D:/docs/mixed/slashes/x.pdf"
        );
    }

    #[test]
    fn normalize_navigation_url_encodes_local_path_specials() {
        assert_eq!(
            normalize_navigation_url(r"C:\docs\报表 图.pdf"),
            "file:///C:/docs/%E6%8A%A5%E8%A1%A8%20%E5%9B%BE.pdf"
        );
        assert_eq!(
            normalize_navigation_url(r"C:\tmp\a#b.png"),
            "file:///C:/tmp/a%23b.png"
        );
        assert_eq!(
            normalize_navigation_url(r"C:\tmp\a?b.png"),
            "file:///C:/tmp/a%3Fb.png"
        );
    }

    #[test]
    fn normalize_navigation_url_converts_unix_and_unc_paths() {
        assert_eq!(
            normalize_navigation_url("/Users/foo/bar.png"),
            "file:///Users/foo/bar.png"
        );
        assert_eq!(
            normalize_navigation_url(r"\\server\share\doc.pdf"),
            "file://server/share/doc.pdf"
        );
        // 正斜杠 UNC 仅 Windows 按网络路径归一；Unix 视为本地路径剥多余斜杠
        #[cfg(windows)]
        assert_eq!(
            normalize_navigation_url("//server/share/doc.pdf"),
            "file://server/share/doc.pdf"
        );
        #[cfg(not(windows))]
        assert_eq!(
            normalize_navigation_url("//server/share/doc.pdf"),
            "file:///server/share/doc.pdf"
        );
    }

    #[test]
    fn normalize_navigation_url_keeps_query_and_fragment_for_drive_urls() {
        // 盘符大写重拼时 query 与 fragment 必须一并带回（Unix 与网络路径
        // 本就走 Url::to_string 保留，仅盘符分支是手工重拼）
        assert_eq!(
            normalize_navigation_url("file:///c:/x/index.html?name=1#sec2"),
            "file:///C:/x/index.html?name=1#sec2"
        );
        assert_eq!(
            normalize_navigation_url("file:///C:/x/index.html#sec2"),
            "file:///C:/x/index.html#sec2"
        );
        assert_eq!(
            normalize_navigation_url("file:///c:/x/index.html?name=1"),
            "file:///C:/x/index.html?name=1"
        );
        // 参照：Unix 与网络路径形式原样保留
        assert_eq!(
            normalize_navigation_url("file:///Users/x/index.html?q=1#s2"),
            "file:///Users/x/index.html?q=1#s2"
        );
    }

    #[test]
    fn normalize_navigation_url_percent_encodes_literal_percent() {
        // 字面 % 预编码为 %25，避免被按百分号解码误读（a%20b.png 是字面
        // 文件名而非已编码空格）
        assert_eq!(
            normalize_navigation_url(r"C:\tmp\a%20b.png"),
            "file:///C:/tmp/a%2520b.png"
        );
    }

    #[test]
    fn normalize_navigation_url_normalizes_file_scheme_variants() {
        assert_eq!(
            normalize_navigation_url("file:C:/Users/x/a.png"),
            "file:///C:/Users/x/a.png"
        );
        assert_eq!(
            normalize_navigation_url("file://C:/Users/x/a.png"),
            "file:///C:/Users/x/a.png"
        );
        assert_eq!(
            normalize_navigation_url("file:///c:/Users/x/a.png"),
            "file:///C:/Users/x/a.png"
        );
        assert_eq!(
            normalize_navigation_url("file:/c:/Users/x/a.png"),
            "file:///C:/Users/x/a.png"
        );
        // UNC 形式（带主机名）保持结构
        assert_eq!(
            normalize_navigation_url("file://server/share/a.png"),
            "file://server/share/a.png"
        );
    }

    #[test]
    fn normalize_navigation_url_is_idempotent_and_keeps_remote_urls() {
        assert_eq!(
            normalize_navigation_url("file:///C:/Users/x/a.png"),
            "file:///C:/Users/x/a.png"
        );
        assert_eq!(
            normalize_navigation_url("file:///Users/x/a.png"),
            "file:///Users/x/a.png"
        );
        assert_eq!(
            normalize_navigation_url("https://example.com/a"),
            "https://example.com/a"
        );
        assert_eq!(normalize_navigation_url("about:blank"), "about:blank");
        assert_eq!(normalize_navigation_url("example.com"), "example.com");
        assert_eq!(
            normalize_navigation_url("  C:\\a.png  "),
            "file:///C:/a.png"
        );
    }

    #[test]
    fn ack_events_removes_only_injected_events() {
        let manager = BrowserManager::new();
        let first = BrowserEvent::NetworkResponse {
            timestamp: 1,
            url: "/api/a".to_string(),
            method: "POST".to_string(),
            status: 200,
            detail: "{}".to_string(),
        };
        let second = BrowserEvent::NetworkResponse {
            timestamp: 2,
            url: "/api/b".to_string(),
            method: "POST".to_string(),
            status: 200,
            detail: "{}".to_string(),
        };
        {
            let mut state = manager.state.lock().unwrap();
            state.pending_events.push(first.clone());
            state.pending_events.push(second.clone());
        }

        let removed = manager.ack_events(std::slice::from_ref(&first));

        assert_eq!(removed, 1);
        let state = manager.state.lock().unwrap();
        assert_eq!(state.pending_events, vec![second]);
    }

    fn tab(id: &str, url: &str, title: &str) -> BrowserTab {
        BrowserTab {
            id: id.to_string(),
            url: url.to_string(),
            title: title.to_string(),
            source: BrowserTabSource::User,
            agent_domain: None,
        }
    }

    #[test]
    fn sync_tabs_by_id_preserves_records_when_only_metadata_differs() {
        // 模拟后端导航后 url/title 已更新，而前端传入的是旧元数据：
        // 按 id 同步时不应因 url/title 差异移除 tab 记录。
        let manager = BrowserManager::new();
        let mut state = manager.state.lock().unwrap();
        state.tabs = vec![tab("t1", "https://real.example.com", "真实标题")];

        // 前端传入同 id 但 url/title 过时（仍是 about:blank）。
        BrowserManager::sync_tabs_by_id(&mut state, &[tab("t1", "about:blank", "")]);

        // tab 记录仍在，元数据被前端传入值覆盖。
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].id, "t1");
        assert_eq!(state.tabs[0].url, "about:blank");
    }

    #[test]
    fn sync_tabs_by_id_closes_records_for_removed_ids() {
        let manager = BrowserManager::new();
        let mut state = manager.state.lock().unwrap();
        state.tabs = vec![
            tab("t1", "https://a.example.com", "A"),
            tab("t2", "https://b.example.com", "B"),
        ];

        // 前端仅保留 t1（t2 被显式关闭）。
        BrowserManager::sync_tabs_by_id(&mut state, &[tab("t1", "https://a.example.com", "A")]);

        let ids: Vec<&str> = state.tabs.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["t1"]);
    }

    #[test]
    fn sync_tabs_by_id_adds_new_tab_records() {
        let manager = BrowserManager::new();
        let mut state = manager.state.lock().unwrap();
        state.tabs = vec![tab("t1", "https://a.example.com", "A")];

        // 前端新增 t2（非 about: 链接，应补建 history）。
        BrowserManager::sync_tabs_by_id(
            &mut state,
            &[
                tab("t1", "https://a.example.com", "A"),
                tab("t2", "https://b.example.com", "B"),
            ],
        );

        assert_eq!(state.tabs.len(), 2);
        assert!(state.tab_histories.contains_key("t2"));
        assert!(state.navigation_signals.contains_key("t2"));
    }

    #[test]
    fn sync_tabs_by_id_clears_metadata_for_removed_ids() {
        let manager = BrowserManager::new();
        let mut state = manager.state.lock().unwrap();
        state.tabs = vec![tab("t1", "https://a.example.com", "A")];
        state.tab_histories.insert("t1".to_string(), Vec::new());
        state
            .navigation_signals
            .insert("t1".to_string(), navigation_signal("https://a.example.com"));

        // t1 被移除后其元数据应一并清理。
        BrowserManager::sync_tabs_by_id(&mut state, &[]);

        assert!(state.tabs.is_empty());
        assert!(!state.tab_histories.contains_key("t1"));
        assert!(!state.navigation_signals.contains_key("t1"));
    }

    #[test]
    fn tab_history_keeps_repeated_visits_and_truncates_forward_branch() {
        let manager = BrowserManager::new();
        let mut state = manager.state.lock().unwrap();
        state.tabs = vec![tab("t1", "about:blank", "")];
        state.active_tab_id = Some("t1".to_string());

        apply_tab_navigation_intent(
            &mut state,
            "t1",
            "https://example.com/a",
            NavigationIntent::Normal,
        )
        .unwrap();
        apply_tab_navigation_intent(
            &mut state,
            "t1",
            "https://example.com/b",
            NavigationIntent::Normal,
        )
        .unwrap();
        apply_tab_navigation_intent(
            &mut state,
            "t1",
            "https://example.com/a",
            NavigationIntent::Normal,
        )
        .unwrap();

        let urls: Vec<&str> = state.tab_histories["t1"]
            .iter()
            .map(|entry| entry.url.as_str())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://example.com/a",
                "https://example.com/b",
                "https://example.com/a"
            ]
        );
        assert_eq!(state.tab_history_indices["t1"], 2);

        apply_tab_navigation_intent(
            &mut state,
            "t1",
            "https://example.com/b",
            NavigationIntent::History { target_index: 1 },
        )
        .unwrap();
        apply_tab_navigation_intent(
            &mut state,
            "t1",
            "https://example.com/d",
            NavigationIntent::Normal,
        )
        .unwrap();

        let urls: Vec<&str> = state.tab_histories["t1"]
            .iter()
            .map(|entry| entry.url.as_str())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://example.com/a",
                "https://example.com/b",
                "https://example.com/d"
            ]
        );
        assert_eq!(state.tab_history_indices["t1"], 2);
    }

    #[test]
    fn reload_retry_and_redirect_update_current_history_entry_only() {
        let manager = BrowserManager::new();
        let mut state = manager.state.lock().unwrap();
        state.tabs = vec![tab("t1", "about:blank", "")];
        state.active_tab_id = Some("t1".to_string());
        let index = apply_tab_navigation_intent(
            &mut state,
            "t1",
            "https://example.com/start",
            NavigationIntent::Normal,
        )
        .unwrap();

        assert_eq!(
            apply_tab_navigation_intent(
                &mut state,
                "t1",
                "https://example.com/start",
                NavigationIntent::Reload,
            )
            .unwrap(),
            index
        );
        assert_eq!(
            apply_tab_navigation_intent(
                &mut state,
                "t1",
                "https://example.com/start",
                NavigationIntent::Retry,
            )
            .unwrap(),
            index
        );
        update_tab_navigation_entry(
            &mut state,
            "t1",
            index,
            "https://example.com/final",
            Some("最终页面"),
        );

        assert_eq!(state.tab_histories["t1"].len(), 1);
        assert_eq!(state.tab_history_indices["t1"], 0);
        assert_eq!(
            state.tab_histories["t1"][0].url,
            "https://example.com/final"
        );
        assert_eq!(state.tab_histories["t1"][0].title, "最终页面");
    }

    #[test]
    fn agent_tabs_are_grouped_by_registrable_domain_and_never_match_user_tabs() {
        assert_eq!(
            agent_domain_for_url("https://docs.example.co.uk/a").unwrap(),
            "example.co.uk"
        );
        assert_eq!(
            agent_domain_for_url("https://api.example.co.uk/b").unwrap(),
            "example.co.uk"
        );
        assert_eq!(
            agent_domain_for_url("http://localhost:8080/a").unwrap(),
            "localhost"
        );

        let manager = BrowserManager::new();
        let mut state = manager.state.lock().unwrap();
        let mut user_tab = tab("user", "https://docs.example.com", "用户标签");
        user_tab.agent_domain = Some("example.com".to_string());
        let mut agent_tab = tab("agent", "https://api.example.com", "Agent 标签");
        agent_tab.source = BrowserTabSource::Agent;
        agent_tab.agent_domain = Some("example.com".to_string());
        state.tabs = vec![user_tab, agent_tab];

        assert_eq!(
            agent_tab_id_for_domain(&state, "example.com").as_deref(),
            Some("agent")
        );
        assert_eq!(agent_tab_id_for_domain(&state, "github.com"), None);
    }

    fn loading_navigation(requested_url: &str, navigation_id: u64) -> TabNavigationState {
        TabNavigationState {
            navigation_id,
            requested_url: requested_url.to_string(),
            started_url: None,
            document_id: None,
            superseded_document_ids: Vec::new(),
            final_url: None,
            history_index: Some(0),
            phase: NavigationPhase::Loading,
            internal_error_url: None,
        }
    }

    fn document_snapshot(document_id: &str, ready_state: &str, url: &str) -> WebDocumentSnapshot {
        WebDocumentSnapshot {
            document_id: document_id.to_string(),
            ready_state: ready_state.to_string(),
            url: url.to_string(),
            title: String::new(),
            text: String::new(),
            has_content: false,
            internal_error: false,
        }
    }

    #[test]
    fn loading_navigation_rejects_superseded_document_and_accepts_redirect_to_same_url() {
        let mut navigation = loading_navigation("https://example.com/b", 2);
        navigation
            .superseded_document_ids
            .push("old-document-a".to_string());

        let stale = document_snapshot("old-document-a", "complete", "https://example.com/a");
        assert!(!accept_loading_document(&mut navigation, 2, &stale));
        assert!(navigation.document_id.is_none());

        let requested = document_snapshot("document-b", "loading", "https://example.com/b");
        assert!(accept_loading_document(&mut navigation, 2, &requested));

        let redirect = document_snapshot("new-document-a", "loading", "https://example.com/a");
        assert!(accept_loading_document(&mut navigation, 2, &redirect));
        assert_eq!(navigation.navigation_id, 2);
        assert_eq!(navigation.document_id.as_deref(), Some("new-document-a"));
        assert_eq!(
            navigation.started_url.as_deref(),
            Some("https://example.com/a")
        );
        assert!(navigation
            .superseded_document_ids
            .iter()
            .any(|document_id| document_id == "document-b"));

        let completed = document_snapshot("new-document-a", "complete", "https://example.com/a");
        assert!(accepts_completed_document(
            &navigation,
            2,
            "new-document-a",
            &completed,
        ));
        assert!(!accepts_completed_document(
            &navigation,
            2,
            "old-document-a",
            &completed,
        ));
    }

    #[test]
    fn loading_navigation_does_not_supersede_repeated_current_document() {
        let mut navigation = loading_navigation("https://example.com/b", 2);
        let current = document_snapshot("document-b", "loading", "https://example.com/b");

        assert!(accept_loading_document(&mut navigation, 2, &current));
        assert!(accept_loading_document(&mut navigation, 2, &current));
        assert_eq!(navigation.document_id.as_deref(), Some("document-b"));
        assert!(!navigation
            .superseded_document_ids
            .iter()
            .any(|document_id| document_id == "document-b"));
    }

    #[test]
    fn completion_requires_current_readable_document() {
        let mut navigation = loading_navigation("https://example.com/b", 2);
        navigation.started_url = Some("https://example.com/final".to_string());
        navigation.document_id = Some("document-b".to_string());

        let loading = document_snapshot("document-b", "interactive", "https://example.com/final");
        assert!(!accepts_completed_document(
            &navigation,
            2,
            "document-b",
            &loading,
        ));

        let mut interactive =
            document_snapshot("document-b", "interactive", "https://example.com/final");
        interactive.has_content = true;
        assert!(accepts_completed_document(
            &navigation,
            2,
            "document-b",
            &interactive,
        ));

        let complete = document_snapshot("document-b", "complete", "https://example.com/final");
        assert!(accepts_completed_document(
            &navigation,
            2,
            "document-b",
            &complete,
        ));
        assert!(!accepts_completed_document(
            &navigation,
            1,
            "document-b",
            &complete,
        ));
        assert!(!accepts_completed_document(
            &navigation,
            2,
            "document-a",
            &complete,
        ));
    }
}
