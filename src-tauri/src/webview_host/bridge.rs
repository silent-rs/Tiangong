pub const BRIDGE_SCRIPT: &str = include_str!("js/bridge.js");
pub const DOCUMENT_STATE_SCRIPT: &str = include_str!("js/document-state.js");
pub const PAGE_SNAPSHOT_SCRIPT: &str = include_str!("js/page-snapshot.js");

// ── webview 容器原语（阶段 3）：宿主中立服务 ──
//
// 方法覆盖实例、标签、导航、缩放、历史与页面批注。直接映射到现有
// 会话/标签管理（中立包装，不含浏览器业务语义——tab 策略与界面逻辑
// 属浏览器插件）。

pub fn handle_webview_primitive(
    state: &crate::webview_host::WebviewHostState,
    app: &tauri::AppHandle,
    plugin_id: &str,
    method: &str,
    payload: &str,
) -> anyhow::Result<String> {
    let request: serde_json::Value = serde_json::from_str(payload)
        .map_err(|error| anyhow::anyhow!("webview 原语负载无效：{error}"))?;
    // 作用域：插件 × 会话 双维度隔离（对齐终端插件）：同一插件在不同对话
    // 各持一套标签与 webview 实例，切换对话互不干扰；负载未带 session_id
    // 时回退插件级共享（与 UI 无会话上下文的调用兼容）。
    let session_scope = request
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|session| !session.is_empty())
        .map(|session| format!("{plugin_id}:{session}"))
        .unwrap_or_else(|| plugin_id.to_string());
    let scope = format!("webview:{session_scope}");
    let manager =
        crate::webview_host::BrowserManager::from_state(state.registry.session_state(&scope));
    let result = match method {
        // 创建 webview 实例：真实创建（open 无头创建，展示由前端
        // instanceShow 下发）→ { view_id, tabs, active_tab_id }
        "webview.create" => {
            let url = request
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("about:blank");
            manager
                .open(app, url)
                .map_err(|error| anyhow::anyhow!("创建 webview 失败：{error}"))?;
            let snapshot = manager.snapshot_tabs();
            serde_json::json!({
                "view_id": scope,
                "tabs": snapshot.tabs,
                "active_tab_id": snapshot.active_tab_id,
            })
        }
        // 导航：真实导航（navigate_with_app 含打开/聚焦/票据）
        "webview.navigate" => {
            let url = request
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("about:blank");
            manager
                .navigate_with_app(app, url)
                .map_err(|error| anyhow::anyhow!("导航失败：{error}"))?;
            let snapshot = manager.snapshot_tabs();
            serde_json::json!({
                "view_id": scope,
                "tabs": snapshot.tabs,
                "active_tab_id": snapshot.active_tab_id,
            })
        }
        // 执行 JS：真实 eval（等待回调结果，15s 超时）
        "webview.eval" => {
            let code = request
                .get("js")
                .or_else(|| request.get("code"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("webview.eval 缺少 js 参数"))?;
            let result = manager.eval_result_text(code);
            serde_json::json!({ "view_id": scope, "result": result })
        }
        "webview.hide" => {
            manager
                .hide()
                .map_err(|error| anyhow::anyhow!("隐藏失败：{error}"))?;
            serde_json::json!({ "view_id": scope })
        }
        // 管理界面位置同步：把 webview 对齐到插件 UI 内容区（窗口逻辑坐标，
        // 与内置浏览器 browserSetPosition 同一通道）
        "webview.setPosition" => {
            let x = request.get("x").and_then(|v| v.as_f64()).unwrap_or(60.0);
            let y = request.get("y").and_then(|v| v.as_f64()).unwrap_or(60.0);
            let width = request
                .get("width")
                .and_then(|v| v.as_f64())
                .unwrap_or(1024.0);
            let height = request
                .get("height")
                .and_then(|v| v.as_f64())
                .unwrap_or(720.0);
            manager
                .set_position(x, y)
                .map_err(|error| anyhow::anyhow!("设置位置失败：{error}"))?;
            manager
                .set_size(width, height)
                .map_err(|error| anyhow::anyhow!("设置尺寸失败：{error}"))?;
            serde_json::json!({ "view_id": scope })
        }
        // 恢复显示（hide 后）：按插件 UI 给定的矩形重新对齐
        "webview.show" => {
            let x = request.get("x").and_then(|v| v.as_f64()).unwrap_or(60.0);
            let y = request.get("y").and_then(|v| v.as_f64()).unwrap_or(60.0);
            let width = request
                .get("width")
                .and_then(|v| v.as_f64())
                .unwrap_or(1024.0);
            let height = request
                .get("height")
                .and_then(|v| v.as_f64())
                .unwrap_or(720.0);
            manager
                .show_active_webview(app, &(x, y, width, height))
                .map_err(|error| anyhow::anyhow!("显示失败：{error}"))?;
            serde_json::json!({ "view_id": scope })
        }
        // 标签快照：插件管理界面渲染 tab 条
        "webview.tabs" => {
            let snapshot = manager.snapshot_tabs();
            serde_json::json!({
                "view_id": scope,
                "tabs": snapshot.tabs,
                "active_tab_id": snapshot.active_tab_id,
            })
        }
        "webview.tabNew" => {
            let url = request
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("about:blank");
            // 插件可自带标签编号（阶段 3 标签模型上移后由插件主导标识）
            let external_id = request.get("tab_id").and_then(|v| v.as_str());
            let tab_id = match external_id {
                Some(id) => manager
                    .tab_new_with_id(app, url, id)
                    .map_err(|error| anyhow::anyhow!("新建标签失败：{error}"))?,
                None => manager
                    .tab_new(app, url)
                    .map_err(|error| anyhow::anyhow!("新建标签失败：{error}"))?,
            };
            let snapshot = manager.snapshot_tabs();
            serde_json::json!({
                "view_id": scope,
                "tab_id": tab_id,
                "tabs": snapshot.tabs,
                "active_tab_id": snapshot.active_tab_id,
            })
        }
        "webview.tabSwitch" => {
            let tab_id = request
                .get("tab_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("webview.tabSwitch 缺少 tab_id 参数"))?;
            manager
                .tab_switch(tab_id)
                .map_err(|error| anyhow::anyhow!("切换标签失败：{error}"))?;
            let snapshot = manager.snapshot_tabs();
            serde_json::json!({
                "view_id": scope,
                "tabs": snapshot.tabs,
                "active_tab_id": snapshot.active_tab_id,
            })
        }
        "webview.tabClose" => {
            let tab_id = request
                .get("tab_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("webview.tabClose 缺少 tab_id 参数"))?;
            manager
                .tab_close(tab_id)
                .map_err(|error| anyhow::anyhow!("关闭标签失败：{error}"))?;
            let snapshot = manager.snapshot_tabs();
            serde_json::json!({
                "view_id": scope,
                "tabs": snapshot.tabs,
                "active_tab_id": snapshot.active_tab_id,
            })
        }
        "webview.back" => {
            manager
                .go_back(app)
                .map_err(|error| anyhow::anyhow!("后退失败：{error}"))?;
            serde_json::json!({ "view_id": scope })
        }
        "webview.forward" => {
            manager
                .go_forward(app)
                .map_err(|error| anyhow::anyhow!("前进失败：{error}"))?;
            serde_json::json!({ "view_id": scope })
        }
        "webview.reload" => {
            manager
                .reload(app)
                .map_err(|error| anyhow::anyhow!("刷新失败：{error}"))?;
            serde_json::json!({ "view_id": scope })
        }
        "webview.getZoom" => serde_json::json!({
            "view_id": scope,
            "scale": manager.zoom(),
        }),
        "webview.setZoom" => {
            let scale = request
                .get("scale")
                .and_then(|value| value.as_f64())
                .ok_or_else(|| anyhow::anyhow!("webview.setZoom 缺少 scale 参数"))?;
            let scale = manager
                .set_zoom(scale)
                .map_err(|error| anyhow::anyhow!("设置缩放失败：{error}"))?;
            serde_json::json!({ "view_id": scope, "scale": scale })
        }
        "webview.resetZoom" => {
            let scale = manager
                .reset_zoom()
                .map_err(|error| anyhow::anyhow!("重置缩放失败：{error}"))?;
            serde_json::json!({ "view_id": scope, "scale": scale })
        }
        "webview.tabHistory" => {
            let tab_id = request.get("tab_id").and_then(|value| value.as_str());
            let history = manager.get_tab_history(tab_id).unwrap_or(
                crate::webview_host::types::TabHistoryResult {
                    tab_id: String::new(),
                    entries: Vec::new(),
                    current_index: -1,
                },
            );
            serde_json::to_value(history)?
        }
        "webview.globalHistory" => {
            let offset = request
                .get("offset")
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as usize;
            let limit = request
                .get("limit")
                .and_then(|value| value.as_u64())
                .unwrap_or(20) as usize;
            serde_json::to_value(manager.get_global_history(offset, limit))?
        }
        "webview.globalHistoryClear" => {
            manager.clear_global_history();
            serde_json::json!({ "view_id": scope })
        }
        "webview.globalHistoryDelete" => {
            let url = request
                .get("url")
                .and_then(|value| value.as_str())
                .ok_or_else(|| anyhow::anyhow!("webview.globalHistoryDelete 缺少 url 参数"))?;
            manager.delete_global_history_entry(url);
            serde_json::json!({ "view_id": scope })
        }
        "webview.annotationExtract" => {
            let result = manager
                .eval_result_text("window.__tiangong_bridge.annotation.extractAnnotatedElements()")
                .and_then(|raw| {
                    serde_json::from_str::<crate::webview_host::types::AnnotationExtractResult>(
                        &raw,
                    )
                    .ok()
                })
                .unwrap_or(crate::webview_host::types::AnnotationExtractResult {
                    elements: Vec::new(),
                    count: 0,
                });
            serde_json::to_value(result)?
        }
        // ── 实例直达原语（阶段 2）：插件按标签编号编排显示与求值 ──
        "webview.instanceShow" => {
            let tab_id = request
                .get("tab_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("webview.instanceShow 缺少 tab_id 参数"))?;
            let rect = (
                request.get("x").and_then(|v| v.as_f64()).unwrap_or(60.0),
                request.get("y").and_then(|v| v.as_f64()).unwrap_or(60.0),
                request
                    .get("width")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1024.0),
                request
                    .get("height")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(720.0),
            );
            manager
                .show_tab_at(app, tab_id, rect)
                .map_err(|error| anyhow::anyhow!("显示实例失败：{error}"))?;
            serde_json::json!({ "view_id": scope, "tab_id": tab_id })
        }
        "webview.instanceHide" => {
            let tab_id = request
                .get("tab_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("webview.instanceHide 缺少 tab_id 参数"))?;
            manager
                .hide_tab(tab_id)
                .map_err(|error| anyhow::anyhow!("隐藏实例失败：{error}"))?;
            serde_json::json!({ "view_id": scope, "tab_id": tab_id })
        }
        "webview.instanceEval" => {
            let tab_id = request
                .get("tab_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("webview.instanceEval 缺少 tab_id 参数"))?;
            let code = request
                .get("js")
                .or_else(|| request.get("code"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("webview.instanceEval 缺少 js 参数"))?;
            let result = manager.eval_tab_result_text(tab_id, code);
            serde_json::json!({ "view_id": scope, "tab_id": tab_id, "result": result })
        }
        "webview.close" => {
            manager
                .close()
                .map_err(|error| anyhow::anyhow!("关闭失败：{error}"))?;
            serde_json::json!({ "view_id": scope })
        }
        // ── 页面协作原语：经命令通道复用 fetcher 实现（策略在插件 TS）──
        // 注入调用方 scope（插件×会话），保证工具抓取/操作的就是该对话
        // 面板正在看的同一实例——此前缺省会落到 webview:default，工具与
        // 面板各看各的页面。
        "webview.fetch"
        | "webview.queryDom"
        | "webview.click"
        | "webview.formFill"
        | "webview.formExtract"
        | "webview.locate" => {
            let mut scoped = request.clone();
            if let Some(map) = scoped.as_object_mut() {
                map.insert("_scope".to_string(), serde_json::json!(session_scope));
            }
            crate::webview_host::bridge::dispatch_collaboration(state, method, &scoped)?
        }
        other => anyhow::bail!("未知 webview 原语方法：{other}"),
    };
    Ok(serde_json::to_string(&result)?)
}

// ── 页面协作原语：命令通道分派（复用 fetcher/handler 全部实现）──
//
// webview.fetch / queryDom / click / formFill / formExtract / locate 经
// BrowserCommand 通道执行——策略（工具 schema、参数映射、结果格式化）在
// browser 的 TS 层，引擎与页面注入在宿主（browser_data 事件流）。

/// 协作原语超时：页面加载与稳定等待受网络影响可能很慢（webview 环境下
/// 页面就绪耗时常见超过 30 秒），给足等待时间；工具级上限
/// （plugin.json timeout_ms）仍兜底。
const COLLABORATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

pub fn dispatch_collaboration(
    state: &crate::webview_host::WebviewHostState,
    method: &str,
    request: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    use crate::webview_host::types::BrowserCommand;
    use tokio::sync::oneshot;

    // 协作命令走调用方插件隔离的作用域（与 webview 原语一致）
    let plugin_scope = request
        .get("_scope")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let session_id = format!("webview:{plugin_scope}");

    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|error| anyhow::anyhow!("协作原语需要在 tokio 上下文执行：{error}"))?;

    let cmd_tx = state.cmd_tx.clone();
    let request = request.clone();
    let method = method.to_string();

    // Handle::block_on 禁止在 runtime 工作线程上调用（插件 UI 经异步命令
    // bridge_call 进入时会 panic "Cannot start a runtime from within a
    // runtime"）。转到独立线程阻塞等待命令响应，不阻塞调用方的 worker。
    let worker = std::thread::spawn(move || {
        runtime.block_on(async move {
            let value = match method.as_str() {
                "webview.fetch" => {
                    let (tx, rx) = oneshot::channel();
                    let _ = cmd_tx
                        .send(BrowserCommand::FetchPage {
                            session_id: session_id.clone(),
                            url: request
                                .get("url")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            max_chars: request
                                .get("max_chars")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(40_000) as usize,
                            show_panel: request
                                .get("open")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                            response_tx: tx,
                        })
                        .await;
                    match tokio::time::timeout(COLLABORATION_TIMEOUT, rx).await {
                        Ok(Ok(response)) => serde_json::json!({
                            "ok": response.ok,
                            "title": response.title,
                            "content": response.content,
                        }),
                        _ => anyhow::bail!("webview.fetch 超时或通道关闭"),
                    }
                }
                "webview.queryDom" => {
                    let (tx, rx) = oneshot::channel();
                    let _ = cmd_tx
                        .send(BrowserCommand::QueryDom {
                            session_id: session_id.clone(),
                            selector: request
                                .get("selector")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            max_results: request
                                .get("max_results")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(20) as usize,
                            response_tx: tx,
                        })
                        .await;
                    match tokio::time::timeout(COLLABORATION_TIMEOUT, rx).await {
                        Ok(Ok(result)) => serde_json::to_value(result)?,
                        _ => anyhow::bail!("webview.queryDom 超时或通道关闭"),
                    }
                }
                "webview.click" => {
                    let (tx, rx) = oneshot::channel();
                    let _ = cmd_tx
                        .send(BrowserCommand::ClickElement {
                            session_id: session_id.clone(),
                            selector: request
                                .get("selector")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            wait_for: None,
                            response_tx: tx,
                        })
                        .await;
                    match tokio::time::timeout(COLLABORATION_TIMEOUT, rx).await {
                        Ok(Ok(result)) => serde_json::to_value(result)?,
                        _ => anyhow::bail!("webview.click 超时或通道关闭"),
                    }
                }
                "webview.formFill" => {
                    let (tx, rx) = oneshot::channel();
                    let _ = cmd_tx
                        .send(BrowserCommand::FormFill {
                            session_id: session_id.clone(),
                            selector: request
                                .get("selector")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            value: request
                                .get("value")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            strategy: request
                                .get("strategy")
                                .and_then(|v| v.as_str())
                                .unwrap_or("auto")
                                .to_string(),
                            wait_for: None,
                            response_tx: tx,
                        })
                        .await;
                    match tokio::time::timeout(COLLABORATION_TIMEOUT, rx).await {
                        Ok(Ok(result)) => serde_json::to_value(result)?,
                        _ => anyhow::bail!("webview.formFill 超时或通道关闭"),
                    }
                }
                "webview.formExtract" => {
                    let (tx, rx) = oneshot::channel();
                    let _ = cmd_tx
                        .send(BrowserCommand::FormExtract {
                            session_id: session_id.clone(),
                            response_tx: tx,
                        })
                        .await;
                    match tokio::time::timeout(COLLABORATION_TIMEOUT, rx).await {
                        Ok(Ok(result)) => serde_json::to_value(result)?,
                        _ => anyhow::bail!("webview.formExtract 超时或通道关闭"),
                    }
                }
                "webview.locate" => {
                    let (tx, rx) = oneshot::channel();
                    let _ = cmd_tx
                        .send(BrowserCommand::LocateElement {
                            session_id: session_id.clone(),
                            query: request
                                .get("query")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            response_tx: tx,
                        })
                        .await;
                    match tokio::time::timeout(COLLABORATION_TIMEOUT, rx).await {
                        Ok(Ok(result)) => serde_json::to_value(result)?,
                        _ => anyhow::bail!("webview.locate 超时或通道关闭"),
                    }
                }
                other => anyhow::bail!("未知协作原语：{other}"),
            };
            Ok::<serde_json::Value, anyhow::Error>(value)
        })
    });
    let result = worker
        .join()
        .map_err(|_| anyhow::anyhow!("协作原语执行线程异常退出"))??;
    Ok(result)
}

#[cfg(test)]
mod webview_primitive_tests {

    #[test]
    fn 未知方法拒绝() {
        let result = handle_webview_primitive_json("{}", "webview.unknown");
        assert!(result.is_err());
    }

    #[test]
    fn eval缺参数拒绝() {
        let result = handle_webview_primitive_json("{}", "webview.eval");
        assert!(result.is_err());
    }

    /// 不依赖 AppHandle 的纯负载校验入口（真实路径需要 Tauri 环境）。
    fn handle_webview_primitive_json(payload: &str, method: &str) -> anyhow::Result<String> {
        // 负载解析与参数校验先行，与真实实现同一前置路径
        let request: serde_json::Value = serde_json::from_str(payload)
            .map_err(|error| anyhow::anyhow!("webview 原语负载无效：{error}"))?;
        match method {
            "webview.eval" => {
                let _ = request
                    .get("js")
                    .or_else(|| request.get("code"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("webview.eval 缺少 js 参数"))?;
                Ok(String::new())
            }
            other
                if !matches!(
                    other,
                    "webview.create" | "webview.navigate" | "webview.hide" | "webview.close"
                ) =>
            {
                anyhow::bail!("未知 webview 原语方法：{other}")
            }
            _ => Ok(String::new()),
        }
    }
}
