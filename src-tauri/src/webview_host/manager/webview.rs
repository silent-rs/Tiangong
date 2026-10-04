//! WebView 实例的创建、展示与现场读取（主线程交互集中于此）。

use super::*;

/// 规范化标识符用于 webview label（避免特殊字符）。
pub(super) fn sanitize_path_segment(id: &str) -> String {
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
pub(super) fn webview_label(session_id: &str, tab_id: &str) -> String {
    format!(
        "browser-webview-{}-{}",
        sanitize_path_segment(session_id),
        sanitize_path_segment(tab_id)
    )
}

/// 现场读取 WebView 当前 URL：把读取任务投递到主线程执行，调用方最多
/// 等待 `timeout`。
///
/// `Webview::url()` 在非主线程调用时会向事件循环发消息并**无超时**阻塞
/// 等待回复；若调用方此时持有 `BrowserState` 锁，而主线程正在
/// `on_page_load` 回调中等待同一把锁，两者永久互等（app 卡死）。本函数
/// 在主线程内执行 `url()`（tauri 同线程分支直接处理，不再跨线程等待），
/// 后台调用方改为限时等待——主线程繁忙时返回 None，由调用方跳过本轮。
///
/// **调用约束：调用前必须已释放 `BrowserState` 锁。**
pub(crate) fn read_webview_url(
    app: &AppHandle<Wry>,
    webview: Webview<Wry>,
    timeout: Duration,
) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.run_on_main_thread(move || {
        // WKWebView 导航中 URL() 可能返回 nil，wry 内部会 panic；主线程任务
        // 内同样兜住，避免 panic 打断事件循环。
        let url = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| webview.url()))
            .ok()
            .and_then(Result::ok)
            .map(|url| url.to_string());
        let _ = tx.send(url);
    })
    .ok()?;
    rx.recv_timeout(timeout).ok().flatten()
}

/// 展示矩形是否可直接用于摆放 WebView（初始值 (0,0,0,0) 宽高无效）。
pub(super) fn rect_is_displayable(rect: (f64, f64, f64, f64)) -> bool {
    rect.2 > 0.0 && rect.3 > 0.0
}

pub(super) fn browser_data_directory(session_id: &str) -> PathBuf {
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
pub(super) fn browser_session_directory_name(session_id: &str) -> String {
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
pub(super) fn encode_session_directory_name(session_id: &str) -> String {
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

impl BrowserManager {
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

        let builder = WebviewBuilder::new(&label, WebviewUrl::External(parsed_url))
            .initialization_script(BRIDGE_SCRIPT)
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

    /// 获取当前活跃标签的 WebView URL（现场读取，限时等待主线程）。
    ///
    /// 锁内只克隆句柄，释放锁后再读取；主线程繁忙超过
    /// [`WEBVIEW_URL_READ_TIMEOUT`] 时返回 None。会阻塞调用线程，异步上下文
    /// 中请经 `spawn_blocking` 调用。
    pub fn current_url(&self, app: &AppHandle<Wry>) -> Option<String> {
        let webview = {
            let state = self.state.lock().ok()?;
            state.active_webview()?.clone()
        };
        read_webview_url(app, webview, WEBVIEW_URL_READ_TIMEOUT)
    }
}
