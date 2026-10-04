//! 导航状态机：开始/完成/失败判定、加载探测与导航入口。

use super::*;

pub(super) fn navigation_signal(url: &str) -> Arc<NavigationSignal> {
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

pub(super) fn push_recent_unique(values: &mut Vec<String>, value: String) {
    if value.is_empty() || values.iter().any(|item| item == &value) {
        return;
    }
    values.push(value);
    const MAX_RECENT_VALUES: usize = 16;
    if values.len() > MAX_RECENT_VALUES {
        values.drain(0..values.len() - MAX_RECENT_VALUES);
    }
}

pub(super) fn remember_superseded_navigation(navigation: &mut TabNavigationState) {
    if let Some(document_id) = navigation.document_id.take() {
        push_recent_unique(&mut navigation.superseded_document_ids, document_id);
    }
}

pub(super) fn parse_web_document_snapshot(result: &str) -> Option<WebDocumentSnapshot> {
    serde_json::from_str(result).ok()
}

/// 资源页合成快照：Finished 事件本身即代表资源响应完成，不依赖页面脚本。
pub(super) fn resource_document_snapshot(url: &str, navigation_id: u64) -> WebDocumentSnapshot {
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

pub(super) fn accept_loading_document(
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

pub(super) fn accepts_completed_document(
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

pub(super) fn tab_navigation_phase(state: &BrowserState, tab_id: &str) -> Option<NavigationPhase> {
    let signal = state.navigation_signals.get(tab_id)?;
    let navigation = signal.state.lock().ok()?;
    Some(navigation.phase)
}

pub(super) fn loaded_navigation_id(state: &BrowserState, tab_id: &str) -> Option<u64> {
    let signal = state.navigation_signals.get(tab_id)?;
    let navigation = signal.state.lock().ok()?;
    (navigation.phase == NavigationPhase::Loaded).then_some(navigation.navigation_id)
}

impl BrowserManager {
    pub(super) fn begin_navigation_for_tab(
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

    pub(super) fn fail_navigation_for_tab(
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

    pub(super) fn handle_page_load_started(
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

    pub(super) fn start_navigation_completion_probe(
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

    pub(super) fn complete_navigation_for_tab(
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

    pub(super) fn handle_page_load_finished(
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

    pub(super) fn history_target(&self, offset: isize) -> Option<(usize, String)> {
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

    pub(crate) fn navigate(
        &self,
        app: &AppHandle<Wry>,
        url: &str,
    ) -> Result<NavigationTicket, String> {
        self.navigate_with_intent(app, url, NavigationIntent::Normal)
    }

    pub(super) fn navigate_with_intent(
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

    pub(super) fn navigation_ticket_for_tab(&self, tab_id: &str) -> Option<NavigationTicket> {
        let state = self.state.lock().ok()?;
        let signal = state.navigation_signals.get(tab_id)?;
        let navigation = signal.state.lock().ok()?;
        Some(NavigationTicket {
            tab_id: tab_id.to_string(),
            navigation_id: navigation.navigation_id,
        })
    }

    pub(super) fn active_navigation_ticket(&self) -> Option<NavigationTicket> {
        let tab_id = self.state.lock().ok()?.active_tab_id.clone()?;
        self.navigation_ticket_for_tab(&tab_id)
    }

    pub(super) fn wait_for_navigation(
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
}
