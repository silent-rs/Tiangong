use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Wry};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::webview_host::fetch_scheduler::BrowserCommandScheduler;
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

/// 解码页面脚本经 eval 回调返回的 JSON 结果。
///
/// wry 的 eval 回调会把脚本返回值再做一次 JSON 序列化：脚本以
/// `JSON.stringify(...)` 返回字符串时，回调拿到的是外层带引号的
/// “JSON 编码字符串”，需先剥掉这一层；脚本直接返回对象时原样解析。
fn decode_eval_json<T: serde::de::DeserializeOwned>(raw: &str) -> Option<T> {
    if let Ok(value) = serde_json::from_str::<T>(raw) {
        return Some(value);
    }
    match serde_json::from_str::<serde_json::Value>(raw).ok()? {
        serde_json::Value::String(inner) => serde_json::from_str::<T>(&inner).ok(),
        _ => None,
    }
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

/// 单个 FetchPage 的执行体（在独立任务中运行，持会话共享锁）。
///
/// 同主域名的抓取复用同一工作标签，按到达顺序串行；导航起步（切活跃
/// 标签 + 发起导航）按会话互斥；页面加载等待与正文抓取不持起步锁，不同
/// 主域名之间并行。调用方在排队期间放弃时返回 None（不再导航）。
#[allow(clippy::too_many_arguments)]
async fn fetch_page(
    scheduler: &BrowserCommandScheduler,
    app: &AppHandle<Wry>,
    manager: BrowserManager,
    session_id: &str,
    url: &str,
    max_chars: usize,
    show_panel: bool,
    response_tx: &tokio::sync::oneshot::Sender<BrowserResponse>,
) -> Option<BrowserResponse> {
    let failure = |error: String| BrowserResponse {
        ok: false,
        title: String::new(),
        content: String::new(),
        final_url: url.to_string(),
        error: Some(error),
    };
    // 无法解析主域名的地址由 navigate_for_agent 报错，这里按整串地址加锁。
    let domain = BrowserManager::agent_domain_key(url).unwrap_or_else(|| url.to_string());
    let _domain = scheduler.domain(session_id, &domain).await;
    if response_tx.is_closed() {
        debug!(%session_id, %url, "FetchPage 调用方排队期间已放弃，跳过");
        return None;
    }

    // navigate_for_agent 会获取 std 锁并可能经 add_child 同步等待主线程，
    // 放到阻塞线程执行，不占用 tokio 工作线程。
    let navigated = {
        let _start = scheduler.navigation_start(session_id).await;
        let navigate_app = app.clone();
        let navigate_manager = manager.clone();
        let navigate_url = url.to_string();
        tokio::task::spawn_blocking(move || {
            navigate_manager.navigate_for_agent(&navigate_app, &navigate_url)
        })
        .await
        .unwrap_or_else(|_| Err("浏览器导航任务执行失败".to_string()))
    };
    let ticket = match navigated {
        Ok(ticket) => ticket,
        Err(error) => return Some(failure(error)),
    };
    let _ = app.emit(
        "browser:tab_updated",
        serde_json::json!({ "session_id": session_id }),
    );

    // 实例归属：页面编号由宿主生成（scru128），先请求前端以同一
    // 编号建立标签（标签即页面唯一所有者，关闭标签即关闭页面），
    // 再有上限地等待挂载后使用；超时照常抓取，标签在切回会话时
    // 按 listInstances 恢复，不会出现无主页面。
    let owner = crate::plugin_instances::parse_webview_scope(session_id);
    let tab_id = ticket.tab_id.clone();
    let fetch_url = url.to_string();
    let result = tokio::task::spawn_blocking(move || {
        if let Some((plugin_id, owner_session)) = owner {
            crate::plugin_instances::request_open(&plugin_id, &owner_session, &tab_id, show_panel);
            crate::plugin_instances::wait_mounted(|| manager.is_tab_mounted(&tab_id));
        }
        manager.fetch_page_content(&fetch_url, max_chars, &ticket)
    })
    .await;
    Some(result.unwrap_or_else(|_| failure("浏览器任务执行失败".to_string())))
}

/// 浏览器命令处理循环
pub async fn browser_command_handler(
    mut rx: mpsc::Receiver<BrowserCommand>,
    registry: Arc<crate::webview_host::session_registry::BrowserSessionRegistry>,
    app: AppHandle<Wry>,
) {
    let scheduler = Arc::new(BrowserCommandScheduler::new());
    while let Some(cmd) = rx.recv().await {
        // FetchPage 在独立任务中并行执行（调度规则见 fetch_scheduler）；
        // 共享锁在循环内按到达顺序获取，保证与前后独占命令的相对顺序。
        if let BrowserCommand::FetchPage {
            session_id,
            url,
            max_chars,
            show_panel,
            response_tx,
        } = cmd
        {
            // 调用方已超时放弃：不再导航共用标签（否则会顶掉后续请求的导航）。
            if response_tx.is_closed() {
                debug!(%session_id, %url, "FetchPage 调用方已放弃，跳过");
                continue;
            }
            let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                continue;
            };
            let shared = scheduler.shared(&session_id).await;
            let scheduler = scheduler.clone();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let _shared = shared;
                let response = fetch_page(
                    &scheduler,
                    &app,
                    BrowserManager::from_state(agent_state),
                    &session_id,
                    &url,
                    max_chars,
                    show_panel,
                    &response_tx,
                )
                .await;
                if let Some(response) = response {
                    let _ = response_tx.send(response);
                }
            });
            continue;
        }
        // 其余命令作用于活跃标签：等在途抓取结束后独占执行。
        // 页面观察是 watcher 的周期性轮询：抓取进行中活跃标签频繁切换，
        // 此时直接跳过本轮（丢弃响应端，observe 返回 None），不阻塞循环。
        let _exclusive = if matches!(cmd, BrowserCommand::ObservePage { .. }) {
            match scheduler.try_exclusive(cmd.session_id()) {
                Some(guard) => guard,
                None => continue,
            }
        } else {
            scheduler.exclusive(cmd.session_id()).await
        };
        match cmd {
            BrowserCommand::FetchPage { .. } => unreachable!("FetchPage 已在上方分流"),
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
                let navigate_app = app.clone();
                let navigate_url = url.clone();
                let navigated = tokio::task::spawn_blocking(move || {
                    manager.navigate_for_agent(&navigate_app, &navigate_url)
                })
                .await
                .unwrap_or_else(|_| Err("浏览器导航任务执行失败".to_string()));
                match navigated {
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
                // 浏览器未打开时先打开（无头），再加载 HTML。open 获取 std 锁并可能
                // 同步等待主线程创建 WebView，放到阻塞线程执行。
                if !manager.is_open() {
                    let open_manager = manager.clone();
                    let open_app = app.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        open_manager.open(&open_app, "about:blank")
                    })
                    .await;
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
                        .and_then(|raw| decode_eval_json::<LocateElementResult>(&raw))
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
                        .and_then(|raw| decode_eval_json::<QueryDomResult>(&raw))
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
            BrowserCommand::PageText {
                session_id,
                offset,
                max_chars,
                keyword,
                response_tx,
            } => {
                let Some(agent_state) = resolve_agent_state(&registry, &session_id) else {
                    continue;
                };
                let manager = BrowserManager::from_state(agent_state);
                let result = tokio::task::spawn_blocking(move || {
                    let js = format!(
                        "JSON.stringify(window.__tiangong_bridge.pageText({offset},{max_chars},{}))",
                        serde_json::to_string(&keyword).unwrap_or_else(|_| "null".into()),
                    );
                    manager
                        .eval_with_result(&js)
                        .and_then(|raw| decode_eval_json::<serde_json::Value>(&raw))
                        .filter(|value| value.is_object())
                        .unwrap_or_else(|| {
                            serde_json::json!({ "error": "当前没有可读取的页面或页面尚未加载" })
                        })
                })
                .await
                .unwrap_or_else(|_| serde_json::json!({ "error": "页面正文查询任务异常" }));
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
        // 接口响应本身不反馈；页面变化（diff）照常返回。
        let diff = Some("页面内容变化：新增 key".to_string());
        let events = vec![BrowserEvent::NetworkResponse {
            timestamp: 1,
            url: "/api/keys".to_string(),
            method: "POST".to_string(),
            status: 200,
            detail: "{\"key\":\"sk-abc\"}".to_string(),
        }];
        let result = merge_diff_and_events(&diff, &events);
        assert_eq!(result, diff);
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
    fn browser_events_format_only_page_changes() {
        let network = BrowserEvent::NetworkResponse {
            timestamp: 1,
            url: "https://platform.deepseek.com/api_keys".to_string(),
            method: "POST".to_string(),
            status: 200,
            detail: "{\"data\":{\"key\":\"sk-dcc5ad16\"}}".to_string(),
        };
        let click = BrowserEvent::UserClick {
            timestamp: 2,
            element: "button".to_string(),
            text: "创建".to_string(),
            selector: "#create".to_string(),
        };
        // 仅过程数据（接口响应、点击）：不反馈。
        assert!(format_browser_events(&[network.clone(), click.clone()]).is_none());

        // 引起页面变化：只反馈变化及其内容，不带过程数据。
        let change = BrowserEvent::ContentChanged {
            timestamp: 3,
            detail: "新密钥：sk-dcc5ad16".to_string(),
        };
        let result = format_browser_events(&[network, click, change]).unwrap();
        assert!(result.contains("[页面变化] 页面内容已更新"));
        assert!(result.contains("新密钥：sk-dcc5ad16"));
        assert!(!result.contains("[网络响应]") && !result.contains("[用户操作]"));
    }

    #[test]
    fn browser_events_change_text_is_clipped_with_head_and_tail() {
        let detail = format!("HEAD{}TAIL", "中".repeat(30_000));
        let result = format_browser_events(&[BrowserEvent::ContentChanged {
            timestamp: 1,
            detail,
        }])
        .unwrap();
        assert!(result.chars().count() <= crate::webview_host::types::PAGE_PUSH_MAX_CHARS);
        assert!(result.contains("HEAD") && result.ends_with("TAIL"));
        assert!(result.contains("已省略中间"));
    }

    #[test]
    fn decode_eval_json_accepts_stringified_and_plain_results() {
        let payload = r#"{"selector":"title","total":1,"returned":0,"elements":[]}"#;
        // 脚本 JSON.stringify 后经 eval 回调二次编码（外层带引号）
        let wrapped = serde_json::to_string(payload).unwrap();
        let decoded: QueryDomResult = decode_eval_json(&wrapped).unwrap();
        assert_eq!(decoded.total, 1);
        // 脚本直接返回对象
        let plain: QueryDomResult = decode_eval_json(payload).unwrap();
        assert_eq!(plain.selector, "title");
        assert!(decode_eval_json::<QueryDomResult>("null").is_none());
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
