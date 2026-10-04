//! 页面脚本执行、正文抓取与快照。

use super::*;

impl BrowserManager {
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

    pub(super) fn eval_tab_with_result_timeout(
        &self,
        tab_id: &str,
        js: &str,
        timeout: Duration,
    ) -> Option<String> {
        let (sender, rx) = std::sync::mpsc::channel();
        let tx = Arc::new(std::sync::Mutex::new(Some(sender)));
        // 锁内只克隆句柄：eval_with_callback 在启用 tauri tracing 特性时会
        // 同步等待主线程，持锁调用存在与 on_page_load 互等的风险。
        let webview = {
            let state = self.state.lock().ok()?;
            state.webviews.get(tab_id)?.clone()
        };
        webview
            .eval_with_callback(js, move |result| {
                if let Ok(mut guard) = tx.lock() {
                    if let Some(tx) = guard.take() {
                        let _ = tx.send(result);
                    }
                }
            })
            .ok()?;
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
}
