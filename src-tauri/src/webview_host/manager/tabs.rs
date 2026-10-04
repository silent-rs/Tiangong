//! 标签管理与会话切换。

use super::*;

pub(super) fn agent_tab_id_for_domain(state: &BrowserState, agent_domain: &str) -> Option<String> {
    state
        .tabs
        .iter()
        .find(|tab| {
            tab.source == BrowserTabSource::Agent
                && tab.agent_domain.as_deref() == Some(agent_domain)
        })
        .map(|tab| tab.id.clone())
}

pub(super) fn reset_runtime_state(state: &mut BrowserState, close_webviews: bool) {
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

pub(super) fn resolve_active_browser_tab(
    tabs: &[BrowserTab],
    active_tab_id: Option<String>,
) -> Option<String> {
    active_tab_id
        .filter(|id| tabs.iter().any(|tab| tab.id == *id))
        .or_else(|| tabs.first().map(|tab| tab.id.clone()))
}

pub(super) fn restore_tab_runtime_metadata(state: &mut BrowserState) {
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

impl BrowserManager {
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
    pub(super) fn sync_tabs_by_id(state: &mut BrowserState, next_tabs: &[BrowserTab]) {
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

    pub(super) fn tab_new_with_source(
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
}
