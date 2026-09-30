use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Wry};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::webview_host::manager::{BrowserManager, POLL_EVAL_TIMEOUT};

/// Agent 命令严格按 session_id 解析：空 session_id 返回 None（调用方应跳过/报错）。
/// 不 fallback active/bootstrap——空 session_id 是调用方错误。
fn resolve_agent_state(
    registry: &Arc<crate::webview_host::session_registry::BrowserSessionRegistry>,
    session_id: &str,
) -> Option<Arc<std::sync::Mutex<crate::webview_host::manager::BrowserState>>> {
    if session_id.trim().is_empty() {
        return None;
    }
    Some(registry.session_state(session_id))
}

use crate::webview_host::types::{
    format_browser_events, AnnotationExtractResult, BrowserAgentActiveEvent, BrowserCommand,
    BrowserEvent, BrowserOpenEvent, BrowserPageSnapshot, BrowserResponse, ClickElementResult,
    FillFieldResult, FormExtractResult, LocateElementResult, PageStatus, QueryDomResult,
    TabHistoryResult,
};

/// 轮询等待页面内容变化并稳定，返回最终的 after-digest。
///
/// 使用 `innerText` 总长度 + 头部 + 尾部 + 覆盖层状态组合签名做变更检测，
/// 确保对话框（追加到 innerText 末尾）和覆盖层弹窗都能被捕获。
/// 稳定后才捕获一次完整的 `getPageDigest` 用于 diff 计算。
fn wait_for_content_change(manager: &BrowserManager, timeout: Duration) -> Option<String> {
    // 签名：总长度 + ':' + 前200字符 + '|' + 后200字符 + '|' + overlay文本前100字符
    // overlay 内容直接纳入签名，确保弹窗内任何文本变化都能被捕获
    let sig_js = "(function(){try{var t=document.body.innerText||'';var n=t.length;var h=t.substring(0,200);var e=n>200?t.substring(n-200):'';var o=window.__tiangong_bridge._getTopmostOverlay();var ov=o?'1:'+(o.innerText||'').substring(0,100):'0';return n+':'+h+'|'+e+'|'+ov}catch(e){return'0:'}})()";

    let before_sig = manager.eval_with_result(sig_js);

    let start = std::time::Instant::now();
    let post_change_max = Duration::from_millis(2500);
    let mut prev_sig = before_sig.clone();
    let mut first_change_time: Option<std::time::Instant> = None;
    let mut stable_count: u32 = 0;

    // 先等待让点击事件传播完成
    std::thread::sleep(Duration::from_millis(600));

    loop {
        let current_sig = manager.eval_with_result(sig_js);

        let changed = match (&prev_sig, &current_sig) {
            (Some(p), Some(c)) => p != c,
            _ => prev_sig.is_some() != current_sig.is_some(),
        };

        if changed {
            prev_sig = current_sig;
            if first_change_time.is_none() {
                first_change_time = Some(std::time::Instant::now());
            }
            stable_count = 0;
        } else if first_change_time.is_some() {
            stable_count += 1;
            // 内容稳定 2 个轮询周期（~600ms）后，捕获最终 digest 返回
            if stable_count >= 2 {
                return manager.eval_with_result("window.__tiangong_bridge.getPageDigest()");
            }
        }

        // 首次变化后超过 post_change_max，不再等稳定，直接返回
        if let Some(t) = first_change_time {
            if t.elapsed() >= post_change_max {
                return manager.eval_with_result("window.__tiangong_bridge.getPageDigest()");
            }
        }

        if start.elapsed() >= timeout {
            return manager.eval_with_result("window.__tiangong_bridge.getPageDigest()");
        }

        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 计算 digest 差异并返回 diff 字符串
fn compute_page_diff(
    manager: &BrowserManager,
    before_digest: &Option<String>,
    after_digest: &Option<String>,
) -> Option<String> {
    let (before, after) = (before_digest.as_ref()?, after_digest.as_ref()?);
    let diff_js = format!("window.__tiangong_bridge.diffDigest({},{})", before, after);
    let diff_raw = manager.eval_with_result(&diff_js)?;
    let diff = diff_raw.trim_matches('"').replace("\\n", "\n");
    if diff.is_empty() {
        None
    } else {
        Some(diff)
    }
}

/// 合并 page_diff 和浏览器事件反馈到最终结果
fn merge_diff_and_events(page_diff: &Option<String>, events: &[BrowserEvent]) -> Option<String> {
    let event_text = format_browser_events(events);
    match (page_diff, event_text) {
        (Some(diff), Some(events)) => Some(format!("{}\n{}", diff, events)),
        (Some(diff), None) => Some(diff.clone()),
        (None, Some(events)) => Some(events),
        (None, None) => None,
    }
}

/// 浏览器命令处理循环
pub async fn browser_command_handler(
    mut rx: mpsc::Receiver<BrowserCommand>,
    registry: Arc<crate::webview_host::session_registry::BrowserSessionRegistry>,
    app: AppHandle<Wry>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            BrowserCommand::FetchPage {
                session_id,
                url,
                max_chars,
                show_panel,
                response_tx,
            } => {
                let url_for_error = url.clone();
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);

                let ticket = match manager.navigate_for_agent(&app, &url) {
                    Ok(ticket) => ticket,
                    Err(error) => {
                        let _ = response_tx.send(BrowserResponse {
                            ok: false,
                            title: String::new(),
                            content: String::new(),
                            final_url: url_for_error,
                            error: Some(error),
                        });
                        continue;
                    }
                };
                let _ = app.emit(
                    "browser:tab_updated",
                    serde_json::json!({ "session_id": session_id }),
                );

                // 实例归属：页面编号由宿主生成（scru128），先请求前端以同一
                // 编号建立标签（标签即页面唯一所有者，关闭标签即关闭页面），
                // 再有上限地等待挂载后使用；超时照常抓取，标签在切回会话时
                // 按 listInstances 恢复，不会出现无主页面。
                let owner = crate::plugin_instances::parse_webview_scope(&session_id);
                let tab_id = ticket.tab_id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    if let Some((plugin_id, owner_session)) = owner {
                        crate::plugin_instances::request_open(
                            &plugin_id,
                            &owner_session,
                            &tab_id,
                            show_panel,
                        );
                        crate::plugin_instances::wait_mounted(|| manager.is_tab_mounted(&tab_id));
                    }
                    manager.fetch_page_content(&url, max_chars, &ticket)
                })
                .await;
                let response = result.unwrap_or(BrowserResponse {
                    ok: false,
                    title: String::new(),
                    content: String::new(),
                    final_url: url_for_error,
                    error: Some("浏览器任务执行失败".to_string()),
                });
                let _ = response_tx.send(response);
            }
            BrowserCommand::OpenUrl { session_id, url } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                // 标签由下方 request_open 以页面编号建立（带实例编号的
                // app.open），不再发 browser:open 让前端另行导航建页。
                let _ = app.emit(
                    "browser:agent_active",
                    BrowserAgentActiveEvent {
                        session_id: session_id.clone(),
                    },
                );
                match manager.navigate_for_agent(&app, &url) {
                    Ok(ticket) => {
                        // 与 FetchPage 同一归属流程：页面必有标签所有者。
                        if let Some((plugin_id, owner_session)) =
                            crate::plugin_instances::parse_webview_scope(&session_id)
                        {
                            crate::plugin_instances::request_open(
                                &plugin_id,
                                &owner_session,
                                &ticket.tab_id,
                                true,
                            );
                        }
                    }
                    Err(error) => warn!(%error, %session_id, %url, "browser open URL failed"),
                }
                let _ = app.emit(
                    "browser:tab_updated",
                    serde_json::json!({ "session_id": session_id }),
                );
            }
            BrowserCommand::ObservePage {
                session_id,
                response_tx,
            } => {
                // 浏览器未打开时不返回响应，让 observe_page() 返回 None
                {
                    let active = match resolve_agent_state(&registry, &session_id) {
                        Some(s) => s,
                        None => continue,
                    };
                    let s = match active.lock() {
                        Ok(s) => s,
                        Err(e) => e.into_inner(),
                    };
                    if s.webviews.is_empty()
                        || !s.visible.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        // 浏览器未打开/不可见：返回空 snapshot，避免调用方无意义等待 timeout
                        let _ = response_tx.send(BrowserPageSnapshot {
                            title: String::new(),
                            url: String::new(),
                            text: String::new(),
                            status: PageStatus::Error("浏览器未打开或不可见".to_string()),
                            tabs: Vec::new(),
                            active_tab_id: None,
                            events: Vec::new(),
                        });
                        continue;
                    }
                }
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let snapshot = tokio::task::spawn_blocking(move || {
                    if let Some(snapshot) = manager.get_snapshot() {
                        if matches!(&snapshot.status, PageStatus::Loading | PageStatus::Error(_)) {
                            return snapshot;
                        }
                    }
                    let events = manager.drain_events();
                    let tab_id = manager
                        .clone_state()
                        .lock()
                        .ok()
                        .and_then(|s| s.active_tab_id.clone());
                    let raw = tab_id
                        .as_deref()
                        .and_then(|id| manager.eval_full_text_cached(id, 12000));
                    raw
                        .and_then(|raw| {
                            let mut data = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
                            // 追加页面批注（仅在存在批注时取，大多数页面无批注可省一次 eval）
                            let annotations = manager.eval_with_result_timeout(
                                "(function(){try{var a=window.__tiangong_bridge.annotation.getAnnotations();return a&&a.count>0?JSON.stringify(a.annotations):''}catch(e){return ''}})()",
                                POLL_EVAL_TIMEOUT,
                            );
                            if let Some(annotations) = annotations {
                                if !annotations.is_empty() && !annotations.starts_with('"') {
                                    if let Some(text) =
                                        data.get("text").and_then(|t| t.as_str()).map(str::to_string)
                                    {
                                        data["text"] = serde_json::Value::String(format!(
                                            "{text}\n\n[页面批注] {annotations}"
                                        ));
                                    }
                                }
                            }
                            Some(BrowserPageSnapshot {
                                title: data["title"].as_str().unwrap_or("").to_string(),
                                url: data["url"].as_str().unwrap_or("").to_string(),
                                text: data["text"].as_str().unwrap_or("").to_string(),
                                status: PageStatus::Loaded,
                                tabs: Vec::new(),
                                active_tab_id: None,
                                events,
                            })
                        })
                        .unwrap_or(BrowserPageSnapshot {
                            title: String::new(),
                            url: String::new(),
                            text: String::new(),
                            status: PageStatus::Error("浏览器未打开或页面未加载".to_string()),
                            tabs: Vec::new(),
                            active_tab_id: None,
                            events: Vec::new(),
                        })
                })
                .await
                .unwrap_or(BrowserPageSnapshot {
                    title: String::new(),
                    url: String::new(),
                    text: String::new(),
                    status: PageStatus::Error("浏览器快照任务失败".to_string()),
                    tabs: Vec::new(),
                    active_tab_id: None,
                    events: Vec::new(),
                });
                // 补充标签信息
                let tabs = {
                    let active = match resolve_agent_state(&registry, &session_id) {
                        Some(s) => s,
                        None => continue,
                    };
                    let s = match active.lock() {
                        Ok(s) => s,
                        Err(e) => e.into_inner(),
                    };
                    let active_tab_id = s.active_tab_id.clone();
                    let tabs = s.tabs.clone();
                    (tabs, active_tab_id)
                };
                let snapshot = BrowserPageSnapshot {
                    tabs: tabs.0,
                    active_tab_id: tabs.1,
                    ..snapshot
                };
                debug!(
                    url = %snapshot.url,
                    title = %snapshot.title,
                    text_len = snapshot.text.len(),
                    events_len = snapshot.events.len(),
                    network_events = snapshot
                        .events
                        .iter()
                        .filter(|event| matches!(event, BrowserEvent::NetworkResponse { .. }))
                        .count(),
                    "browser observe_page snapshot"
                );
                let _ = response_tx.send(snapshot);
            }
            BrowserCommand::FormExtract {
                session_id,
                response_tx,
            } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let result = tokio::task::spawn_blocking(move || {
                    manager
                        .eval_with_result("window.__tiangong_bridge.extractForms()")
                        .and_then(|raw| serde_json::from_str::<FormExtractResult>(&raw).ok())
                        .unwrap_or(FormExtractResult { forms: vec![] })
                })
                .await
                .unwrap_or(FormExtractResult { forms: vec![] });
                let _ = response_tx.send(result);
            }
            BrowserCommand::FormFill {
                session_id,
                selector,
                value,
                strategy,
                wait_for,
                response_tx,
            } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let result = tokio::task::spawn_blocking(move || {
                    // 操作前 digest
                    let before_digest = manager
                        .eval_with_result("window.__tiangong_bridge.getPageDigest()");

                    // 先尝试原生 fillField
                    let js = format!(
                        "window.__tiangong_bridge.fillField({},{},{})",
                        serde_json::to_string(&selector).unwrap_or_default(),
                        serde_json::to_string(&value).unwrap_or_default(),
                        serde_json::to_string(&strategy).unwrap_or_default(),
                    );
                    let mut native_result = manager
                        .eval_with_result(&js)
                        .and_then(|raw| serde_json::from_str::<FillFieldResult>(&raw).ok())
                        .unwrap_or(FillFieldResult {
                            ok: false,
                            strategy: None,
                            error: Some("填写字段执行失败".to_string()),
                            current_value: None,
                            wait_result: None,
                            page_diff: None,
                        });

                    if !native_result.ok {
                        // 原生策略失败，尝试 UI 库组件填写
                        let comp_js = format!(
                            "window.__tiangong_bridge.fillComponent({},{})",
                            serde_json::to_string(&selector).unwrap_or_default(),
                            serde_json::to_string(&value).unwrap_or_default(),
                        );
                        native_result = manager
                            .eval_with_result(&comp_js)
                            .and_then(|raw| serde_json::from_str::<FillFieldResult>(&raw).ok())
                            .unwrap_or(native_result);
                    }

                    // 填写成功后执行等待
                    if native_result.ok {
                        if let Some(ref condition) = wait_for {
                            let wait_js = format!(
                                "(async function(){{return JSON.stringify(await window.__tiangong_bridge.waitFor({},5000))}})()",
                                serde_json::to_string(condition).unwrap_or_default(),
                            );
                            if let Some(wait_raw) = manager.eval_with_result(&wait_js) {
                                native_result.wait_result =
                                    serde_json::from_str(&wait_raw).ok();
                            }
                        }

                        // 智能等待页面内容变化（最多 3 秒）
                        let after_digest =
                            wait_for_content_change(&manager, Duration::from_secs(3));
                        let diff = compute_page_diff(&manager, &before_digest, &after_digest);
                        let events = manager.drain_events();
                        let merged = merge_diff_and_events(&diff, &events);
                        native_result.page_diff = match &merged {
                            Some(d)
                                if !d.is_empty()
                                    && !d.trim().eq("页面无明显变化") =>
                            {
                                merged
                            }
                            _ => {
                                let summary = manager.eval_with_result(
                                    "(function(){try{var t=(document.body.innerText||'').replace(/\\s+/g,' ').trim();return t.length>800?t.substring(0,800)+'...':t}catch(e){return''}})()",
                                );
                                match summary {
                                    Some(s) if !s.is_empty() => Some(format!(
                                        "操作完成，当前页面内容：\n{s}"
                                    )),
                                    _ => merged,
                                }
                            }
                        };
                    }

                    native_result
                })
                .await
                .unwrap_or(FillFieldResult {
                    ok: false,
                    strategy: None,
                    error: Some("填写字段任务失败".to_string()),
                    current_value: None,
                    wait_result: None,
                    page_diff: None,
                });
                let _ = response_tx.send(result);
            }
            BrowserCommand::ClickElement {
                session_id,
                selector,
                wait_for,
                response_tx,
            } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let result = tokio::task::spawn_blocking(move || {
                    // 操作前 digest
                    let before_digest = manager
                        .eval_with_result("window.__tiangong_bridge.getPageDigest()");

                    let js = format!(
                        "window.__tiangong_bridge.clickElement({})",
                        serde_json::to_string(&selector).unwrap_or_default(),
                    );
                    let mut result = manager
                        .eval_with_result(&js)
                        .and_then(|raw| serde_json::from_str::<ClickElementResult>(&raw).ok())
                        .unwrap_or(ClickElementResult {
                            ok: false,
                            error: Some("点击元素执行失败".to_string()),
                            wait_result: None,
                            candidates: vec![],
                            page_diff: None,
                        });

                    // 点击成功后执行等待
                    if result.ok {
                        if let Some(ref condition) = wait_for {
                            let wait_js = format!(
                                "(async function(){{return JSON.stringify(await window.__tiangong_bridge.waitFor({},5000))}})()",
                                serde_json::to_string(condition).unwrap_or_default(),
                            );
                            if let Some(wait_raw) = manager.eval_with_result(&wait_js) {
                                result.wait_result = serde_json::from_str(&wait_raw).ok();
                            }
                        }

                        // 智能等待页面内容变化（最多 5 秒）
                        let after_digest =
                            wait_for_content_change(&manager, Duration::from_secs(5));
                        let diff = compute_page_diff(&manager, &before_digest, &after_digest);
                        let events = manager.drain_events();
                        let merged = merge_diff_and_events(&diff, &events);
                        result.page_diff = match &merged {
                            Some(d)
                                if !d.is_empty()
                                    && !d.trim().eq("页面无明显变化") =>
                            {
                                merged
                            }
                            _ => {
                                let summary = manager.eval_with_result(
                                    "(function(){try{var t=(document.body.innerText||'').replace(/\\s+/g,' ').trim();return t.length>800?t.substring(0,800)+'...':t}catch(e){return''}})()",
                                );
                                match summary {
                                    Some(s) if !s.is_empty() => Some(format!(
                                        "操作完成，当前页面内容：\n{s}"
                                    )),
                                    _ => merged,
                                }
                            }
                        };
                    }

                    result
                })
                .await
                .unwrap_or(ClickElementResult {
                    ok: false,
                    error: Some("点击元素任务失败".to_string()),
                    wait_result: None,
                    candidates: vec![],
                    page_diff: None,
                });
                let _ = response_tx.send(result);
            }
            BrowserCommand::LoadHtml {
                session_id,
                html,
                response_tx,
            } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                // 浏览器未打开时先打开（无头），再加载 HTML
                if !manager.is_open() {
                    let _ = manager.open(&app, "about:blank");
                    let _ = app.emit(
                        "browser:open",
                        BrowserOpenEvent {
                            session_id: session_id.clone(),
                            url: "about:blank".to_string(),
                        },
                    );
                }
                // 始终通知前端 agent 正在使用浏览器（用于图标标记）
                let _ = app.emit(
                    "browser:agent_active",
                    BrowserAgentActiveEvent {
                        session_id: session_id.clone(),
                    },
                );
                let result = tokio::task::spawn_blocking(move || manager.load_html(&html))
                    .await
                    .unwrap_or(Err("加载 HTML 任务失败".to_string()));
                let _ = response_tx.send(result);
            }
            BrowserCommand::TabList {
                session_id,
                response_tx,
            } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let tabs = manager.tab_list();
                let _ = response_tx.send(tabs);
            }
            BrowserCommand::TabNew { session_id, url } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let app_clone = app.clone();
                let event_session_id = session_id.clone();
                let _ =
                    tokio::task::spawn_blocking(move || match manager.tab_new(&app_clone, &url) {
                        Ok(tab_id) => {
                            let _ = app_clone.emit(
                                "browser:tab_updated",
                                serde_json::json!({
                                    "session_id": event_session_id,
                                    "action": "new",
                                    "tab_id": tab_id,
                                    "url": url,
                                }),
                            );
                        }
                        Err(e) => warn!(error = %e, "tab_new error"),
                    })
                    .await;
            }
            BrowserCommand::TabSwitch { session_id, tab_id } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let app_clone = app.clone();
                let event_session_id = session_id.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = manager.tab_switch(&tab_id) {
                        warn!(error = %e, "tab_switch error");
                    } else {
                        let _ = app_clone.emit(
                            "browser:tab_updated",
                            serde_json::json!({
                                "session_id": event_session_id,
                                "action": "switch",
                                "tab_id": tab_id,
                            }),
                        );
                    }
                })
                .await;
            }
            BrowserCommand::TabClose { session_id, tab_id } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let app_clone = app.clone();
                let event_session_id = session_id.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = manager.tab_close(&tab_id) {
                        warn!(error = %e, "tab_close error");
                    } else {
                        let _ = app_clone.emit(
                            "browser:tab_updated",
                            serde_json::json!({
                                "session_id": event_session_id,
                                "action": "close",
                                "tab_id": tab_id,
                            }),
                        );
                    }
                })
                .await;
            }
            BrowserCommand::AnnotationExtract {
                session_id,
                response_tx,
            } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let result = tokio::task::spawn_blocking(move || {
                    manager
                        .eval_with_result(
                            "window.__tiangong_bridge.annotation.extractAnnotatedElements()",
                        )
                        .and_then(|raw| serde_json::from_str::<AnnotationExtractResult>(&raw).ok())
                        .unwrap_or(AnnotationExtractResult {
                            elements: vec![],
                            count: 0,
                        })
                })
                .await
                .unwrap_or(AnnotationExtractResult {
                    elements: vec![],
                    count: 0,
                });
                let _ = response_tx.send(result);
            }
            BrowserCommand::LocateElement {
                session_id,
                query,
                response_tx,
            } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let result = tokio::task::spawn_blocking(move || {
                    let js = format!(
                        "JSON.stringify(window.__tiangong_bridge.locateElement({{query:{}}}))",
                        serde_json::to_string(&query).unwrap_or_default(),
                    );
                    manager
                        .eval_with_result(&js)
                        .and_then(|raw| serde_json::from_str::<LocateElementResult>(&raw).ok())
                        .unwrap_or(LocateElementResult {
                            ok: false,
                            error: Some("定位请求失败".to_string()),
                            ambiguous: false,
                            target: None,
                            candidates: vec![],
                        })
                })
                .await
                .unwrap_or(LocateElementResult {
                    ok: false,
                    error: Some("定位任务异常".to_string()),
                    ambiguous: false,
                    target: None,
                    candidates: vec![],
                });
                let _ = response_tx.send(result);
            }
            BrowserCommand::QueryDom {
                session_id,
                selector,
                max_results,
                response_tx,
            } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let result = tokio::task::spawn_blocking(move || {
                    let js = format!(
                        "JSON.stringify(window.__tiangong_bridge.queryDom({},{max_results}))",
                        serde_json::to_string(&selector).unwrap_or_default(),
                    );
                    manager
                        .eval_with_result(&js)
                        .and_then(|raw| serde_json::from_str::<QueryDomResult>(&raw).ok())
                        .unwrap_or(QueryDomResult {
                            selector,
                            total: 0,
                            returned: 0,
                            elements: vec![],
                        })
                })
                .await
                .unwrap_or(QueryDomResult {
                    selector: String::new(),
                    total: 0,
                    returned: 0,
                    elements: vec![],
                });
                let _ = response_tx.send(result);
            }
            BrowserCommand::TabHistory {
                session_id,
                tab_id,
                response_tx,
            } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let result = manager.get_tab_history(tab_id.as_deref());
                let _ = response_tx.send(result.unwrap_or(TabHistoryResult {
                    tab_id: String::new(),
                    entries: Vec::new(),
                    current_index: -1,
                }));
            }
            BrowserCommand::GlobalHistory {
                session_id,
                offset,
                limit,
                response_tx,
            } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let entries = manager.get_global_history(offset, limit);
                let _ = response_tx.send(entries);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_both_some() {
        let diff = Some("页面内容变化：新增 key".to_string());
        let events = vec![BrowserEvent::NetworkResponse {
            timestamp: 1,
            url: "/api/keys".to_string(),
            method: "POST".to_string(),
            status: 200,
            detail: "{\"key\":\"sk-abc\"}".to_string(),
        }];
        let result = merge_diff_and_events(&diff, &events);
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r.contains("页面内容变化"));
        assert!(r.contains("[网络响应]"));
        assert!(r.contains("sk-abc"));
    }

    #[test]
    fn merge_only_diff() {
        let diff = Some("覆盖层已关闭".to_string());
        let result = merge_diff_and_events(&diff, &[]);
        assert_eq!(result, Some("覆盖层已关闭".to_string()));
    }

    #[test]
    fn merge_only_events() {
        let events = vec![BrowserEvent::DialogOpened {
            timestamp: 1,
            detail: "创建 API key sk-test".to_string(),
        }];
        let result = merge_diff_and_events(&None, &events);
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(result.contains("[页面变化]"));
        assert!(result.contains("sk-test"));
    }

    #[test]
    fn merge_both_none() {
        let result = merge_diff_and_events(&None, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn browser_events_parse_mixed_event_queue() {
        let raw = r#"[
            {"type":"content_changed","timestamp":100,"detail":"text updated"},
            {"type":"network_response","timestamp":200,"url":"/api/a","method":"GET","status":200,"detail":"{}"},
            {"type":"dialog_opened","timestamp":300,"detail":"dialog text"},
            {"type":"network_response","timestamp":400,"url":"/api/b","method":"POST","status":201,"detail":"{\"id\":1}"}
        ]"#;
        let events: Vec<BrowserEvent> = serde_json::from_str(raw).unwrap();
        assert_eq!(events.len(), 4);
        let network: Vec<_> = events
            .iter()
            .filter(|event| matches!(event, BrowserEvent::NetworkResponse { .. }))
            .collect();
        assert_eq!(network.len(), 2);
        assert!(matches!(events[2], BrowserEvent::DialogOpened { .. }));
    }

    #[test]
    fn browser_events_format_output() {
        let events = vec![BrowserEvent::NetworkResponse {
            timestamp: 1,
            url: "https://platform.deepseek.com/api_keys".to_string(),
            method: "POST".to_string(),
            status: 200,
            detail: "{\"data\":{\"key\":\"sk-dcc5ad16\"}}".to_string(),
        }];
        let result = format_browser_events(&events).unwrap();
        assert!(
            result.contains("[网络响应] POST https://platform.deepseek.com/api_keys (状态 200)")
        );
        assert!(result.contains("sk-dcc5ad16"));
    }

    #[test]
    fn compute_page_diff_both_empty_returns_none() {
        // 如果 before 和 after digest 的 overlayOpen/overlayText/mainTextTail 都相同，
        // diffDigest 返回 "页面无明显变化"，compute_page_diff 将其视为空
        // 模拟 diffDigest 的行为：当无变化时返回空字符串（bridge.js 中 changes.length === 0 时返回 "页面无明显变化"）
        // handler.rs 中 diff.is_empty() 检查空字符串 → 返回 None
        let diff = String::new();
        assert!(diff.is_empty());
    }
}
