//! 后台轮询线程：URL / 内容变化检测与页面事件消费。

use super::*;

impl BrowserManager {
    /// 启动后台轮询线程，检测 webview URL 变化并发射 browser:page_loaded 事件
    pub(crate) fn start_url_poll(&self, app: &AppHandle<Wry>, initial_url: &str) {
        let state = self.state.clone();
        let app = app.clone();
        let stop = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // 先通知旧轮询线程退出，再换新停止标志：保证每个作用域同时只有
            // 一个 url_poll 线程（此前复用同一标志并置 false，旧线程永不退出，
            // 每次导航累积一个线程，持续加压主线程）。
            s.poll_stop
                .store(true, std::sync::atomic::Ordering::Relaxed);
            s.poll_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
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
                debug!(%session_id, "browser url_poll thread started");
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
                    // 锁内只克隆句柄，释放锁后再现场读 URL（见 BrowserState 锁约束）。
                    let (active_tab_id, navigation_id, webview) = {
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
                                (active_tab_id, navigation_id, wv.clone())
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
                    let Some(current_url) =
                        read_webview_url(&app, webview, WEBVIEW_URL_READ_TIMEOUT)
                    else {
                        continue;
                    };
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
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
                debug!(%session_id, "browser url_poll thread exiting");
            })
            .expect("failed to spawn browser URL poll thread");
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
}
