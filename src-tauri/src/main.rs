//! 天工统一入口
//!
//! 无参数 → 启动 GUI
//! 其他命令 → 委托给 tiangong_entry（cli/server/mcp/skill）
//!
//! DMG 安装后可通过 symlink 获得 CLI 能力：
//! ln -s /Applications/tiangong.app/Contents/MacOS/tiangong-app /usr/local/bin/tiangong

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use tauri::Emitter;
use tauri::Manager;
use tracing::{debug, info, warn};

const TRAY_SHOW_WINDOW_ID: &str = "show_window";
const TRAY_START_SERVER_ID: &str = "start_server";
const TRAY_STOP_SERVER_ID: &str = "stop_server";
const TRAY_STATUS_ID: &str = "server_status";
const TRAY_QUIT_ID: &str = "quit";

fn browser_events_to_feedback(
    events: Vec<tiangong_app::webview_host::types::BrowserEvent>,
) -> Option<(Vec<tiangong_app::webview_host::types::BrowserEvent>, String)> {
    let network_events: Vec<_> = events
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                tiangong_app::webview_host::types::BrowserEvent::NetworkResponse { .. }
            )
        })
        .collect();
    let feedback = tiangong_app::webview_host::types::format_browser_events(&network_events)?;
    Some((network_events, feedback))
}

async fn observe_browser_snapshot_for_injection(
    app: tauri::AppHandle,
    session_id: String,
) -> Option<tiangong_app::webview_host::types::BrowserPageSnapshot> {
    let manager = {
        let state = app.state::<tiangong_app::webview_host::WebviewHostState>();
        let browser_state = state.registry.existing_session_state(&session_id)?;
        tiangong_app::webview_host::manager::BrowserManager::from_state(browser_state)
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::task::spawn_blocking(move || manager.current_snapshot_without_events(12000)),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .flatten()
}

/// 注入目标对话 id：浏览器事件携带的是作用域标识——插件面板为
/// `webview:<插件>:<会话>`（插件×会话双维度隔离），反解出会话段才能
/// 路由到对话；内置路径原样（对话 id 即作用域）。纯插件级作用域
/// （无会话段，如全局浏览器）不属于任何对话，返回 None 跳过注入。
fn injection_target_session(scope: &str) -> Option<String> {
    if let Some(rest) = scope.strip_prefix("webview:") {
        return rest
            .split_once(':')
            .map(|(_, session)| session.to_string())
            .filter(|session| !session.is_empty());
    }
    Some(scope.to_string())
}

async fn ack_browser_events(
    app: tauri::AppHandle,
    session_id: String,
    events: Vec<tiangong_app::webview_host::types::BrowserEvent>,
) {
    let removed = {
        let state = app.state::<tiangong_app::webview_host::WebviewHostState>();
        state
            .registry
            .existing_session_state(&session_id)
            .map(tiangong_app::webview_host::manager::BrowserManager::from_state)
            .map(|manager| manager.ack_events(&events))
            .unwrap_or(0)
    };
    debug!(
        session_id,
        ack_count = events.len(),
        removed,
        "browser events acknowledged after session injection"
    );
}

/// 初始化日志（所有模式统一）
///
/// - 文件：~/.tiangong/logs/tiangong.log（按天滚动，始终写入）
/// - 终端：CLI 模式静默，其他模式输出到 stderr
fn init_logging(terminal_output: bool) -> anyhow::Result<tiangong_config::logging::WorkerGuard> {
    tiangong_config::logging::init_logging(tiangong_config::logging::LoggingOptions::desktop(
        terminal_output,
    ))
}

fn main() {
    // 解释器环境注入须先于一切后台线程（std::env::set_var 非线程安全）：
    // GUI/受限环境下探测 node/python 并注入 TIANGONG_*_PATH 与 PATH 前置，
    // 供插件 sidecar 与命令通道子进程全树使用。
    tiangong_plugin_runtime::ensure_interpreter_env();

    // 无参数 → GUI
    if std::env::args().len() <= 1 {
        run_gui();
        return;
    }

    // CLI 模式终端静默，其他模式终端输出
    let is_cli = std::env::args().nth(1).as_deref() == Some("cli");
    let _guard = init_logging(!is_cli).expect("failed to initialize logging");

    if let Err(err) = tiangong_entry::run() {
        eprintln!("错误：{err}");
        std::process::exit(1);
    }
}

fn run_gui() {
    let _guard = init_logging(true).expect("failed to initialize logging");

    tauri::Builder::default()
        .manage(tiangong_app::TiangongApp::new())
        .setup(|app| {
            use tauri::Listener;

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Regular);
            setup_tray(app)?;

            // macOS 后台悬停监听：窗口未激活时向前端下发鼠标位置与后台首击
            // 位置，供消息列表右侧导航唤出与点击定位（macOS 不给后台窗口派发
            // hover 事件，系统首击只激活窗口）。
            #[cfg(target_os = "macos")]
            tiangong_app::inactive_hover::spawn(app.handle().clone());

            // 注入 app_handle（builder 链构造时尚无 handle，setup 阶段补入）。
            let state = app.state::<tiangong_app::TiangongApp>();
            state.set_app_handle(app.handle().clone());

            // 启动阶段一次性预加载插件快照。后续状态查询、设置页和 Core 创建
            // 只复用该快照，不隐式扫描、编译或热加载插件。
            state.start_plugin_preload();

            // 注册插件生命周期变化订阅者：插件迁移（安装/升级/启停/卸载/重载）
            // 由 runtime 状态机执行并广播事件，宿主在此响应——主前端刷新 +
            // 会话上下文交接打标（指纹由事件携带）。插件页面的沙箱桥通知由
            // runtime 发布事件时内部下发，宿主不重复扇出。
            {
                let emitter_handle = app.handle().clone();
                let state_handle = app.handle().clone();
                tiangong_plugin_runtime::set_plugins_changed_listener(Some(Arc::new(
                    move |event: &tiangong_plugin_runtime::PluginChangeEvent| {
                        let _ = emitter_handle.emit("plugins_changed", &());
                        let app_state = state_handle.state::<tiangong_app::TiangongApp>();
                        app_state.mark_all_sessions_for_plugin_change(&event.fingerprint);
                        // 自制插件（local 签名）的能力不进 tools 声明：
                        // 清单经注入通道追加到活跃会话的对话历史，避免
                        // 装卸打穿 KV cache 前缀。
                        if tiangong_plugin_runtime::registry::is_local_plugin(&event.plugin_id) {
                            app_state.broadcast_local_plugin_list();
                        }
                    },
                )));
            }

            // Core 插件仍由 ensure_core 现场构造，确保每个 Core 持有独立实例
            //（隔离 per-session 状态如 workspace / recall_attempted / turn_count）。

            // 初始化 webview 引擎宿主（共享状态 + 协作命令通道），供 webview.*
            // 桥接原语驱动；须在任何原语接线之前完成。
            tiangong_app::webview_host::init(app.handle());

            // 媒体生成/转换插件（generate_image / generate_video / text_to_speech /
            // speech_to_text）在 app.rs 的 create_core_if_absent 中统一组装（与其他
            // 进程内插件一致），此处不再单独注册。

            // 注意：GUI 不注册 fetch / command 插件。web_fetch 由 browser 插件提供
            // （内嵌浏览器渲染），run_command / run_shell 由 terminal 插件提供（PTY 执行）。
            // CLI / Server 才注册 fetch / command（基础 reqwest 获取 + 子进程执行）。

            // 启动工具消息注入消费者任务（插件 push → 消费者统一处理）
            let injection_tx = state.tool_injection_tx();
            state.start_tool_injection_consumer(app.handle().clone());

            // 监听浏览器页面加载事件，push 到注入 channel（消费者统一处理）
            let tx1 = injection_tx.clone();
            let inject_handle = app.handle().clone();
            app.listen("browser:page_loaded", move |event| {
                let payload = event.payload().to_string();
                let Ok(data) = serde_json::from_str::<
                    tiangong_app::webview_host::types::BrowserPageLoadedEvent,
                >(&payload) else {
                    warn!("浏览器页面事件 payload 解析失败");
                    return;
                };
                if data.session_id.trim().is_empty() || data.url.is_empty() {
                    return;
                }
                let browser_state =
                    inject_handle.state::<tiangong_app::webview_host::WebviewHostState>();
                let Some(source_state) = browser_state
                    .registry
                    .existing_session_state(&data.session_id)
                else {
                    return;
                };
                let source_manager =
                    tiangong_app::webview_host::manager::BrowserManager::from_state(source_state);
                if !source_manager.is_tab_mounted(&data.tab_id) {
                    return;
                }
                use tiangong_app::webview_host::page_fetcher::BrowserContent;
                let Some(target_session) = injection_target_session(&data.session_id) else {
                    debug!("浏览器页面事件来自非会话作用域，跳过注入");
                    return;
                };
                let _ = tx1.send(tiangong_app::ToolInjection {
                    session_id: Some(target_session),
                    browser_source: Some((data.session_id, data.tab_id)),
                    tool: Box::new(BrowserContent {
                        title: data.title,
                        url: data.url,
                        text: data.text,
                        tabs: vec![],
                        active_tab_id: None,
                        feedback: None,
                    }),
                });
            });

            // 监听浏览器网络响应事件，push 到注入 channel（消费者统一处理）。
            // 这覆盖页面 JS 自行发起 XHR/fetch、且 DOM 没有明显变化的场景。
            let event_inject_handle = app.handle().clone();
            let tx2 = injection_tx.clone();
            app.listen("browser:events", move |event| {
                let payload = event.payload().to_string();
                let Ok(payload) = serde_json::from_str::<
                    tiangong_app::webview_host::types::BrowserEventsEvent,
                >(&payload) else {
                    warn!("浏览器事件 payload 解析失败");
                    return;
                };
                let browser_state =
                    event_inject_handle.state::<tiangong_app::webview_host::WebviewHostState>();
                let Some(source_state) = browser_state
                    .registry
                    .existing_session_state(&payload.session_id)
                else {
                    return;
                };
                let source_manager =
                    tiangong_app::webview_host::manager::BrowserManager::from_state(source_state);
                if !source_manager.is_tab_mounted(&payload.tab_id) {
                    return;
                }
                let session_id = payload.session_id;
                let source_tab_id = payload.tab_id;
                let events = payload.events;
                let total_count = events.len();
                let Some((network_events, feedback)) = browser_events_to_feedback(events) else {
                    debug!(total_count, "浏览器事件无网络响应，跳过主动注入");
                    return;
                };
                let network_count = network_events.len();
                let app_handle = event_inject_handle.clone();
                let injection_tx = tx2.clone();
                tauri::async_runtime::spawn(async move {
                    let snapshot = observe_browser_snapshot_for_injection(
                        app_handle.clone(),
                        session_id.clone(),
                    )
                    .await;
                    let title = snapshot
                        .as_ref()
                        .map(|s| s.title.clone())
                        .unwrap_or_default();
                    // 优先用 snapshot URL，其次用网络事件 URL，最后用 WebView 当前 URL
                    let url = snapshot
                        .as_ref()
                        .map(|s| s.url.clone())
                        .filter(|u| !u.is_empty())
                        .or_else(|| {
                            network_events.iter().find_map(|event| {
                                match event {
                                tiangong_app::webview_host::types::BrowserEvent::NetworkResponse {
                                    url,
                                    ..
                                } => Some(url.clone()),
                                _ => None,
                            }
                            })
                        })
                        .unwrap_or_default();
                    let text = snapshot
                        .as_ref()
                        .map(|s| s.text.clone())
                        .unwrap_or_default();
                    let tabs = snapshot
                        .as_ref()
                        .map(|s| {
                            s.tabs
                                .iter()
                                .map(|tab| (tab.id.clone(), tab.url.clone(), tab.title.clone()))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let active_tab_id = snapshot.as_ref().and_then(|s| s.active_tab_id.clone());
                    let mut page_url = url;
                    if page_url.is_empty() {
                        // 尝试从活跃标签的 WebView 获取当前页面 URL
                        let browser_state =
                            app_handle.state::<tiangong_app::webview_host::WebviewHostState>();
                        page_url = browser_state
                            .registry
                            .existing_session_state(&session_id)
                            .map(tiangong_app::webview_host::manager::BrowserManager::from_state)
                            .and_then(|manager| manager.current_url())
                            .unwrap_or_default();
                    }
                    if page_url.is_empty() {
                        warn!(
                            total_count,
                            network_count, "浏览器网络事件缺少页面 URL，无法注入"
                        );
                        return;
                    }

                    use tiangong_app::webview_host::page_fetcher::BrowserContent;
                    // 插件作用域反解对话 id（见 injection_target_session）
                    let Some(target_session) = injection_target_session(&session_id) else {
                        debug!("浏览器网络事件来自非会话作用域，跳过注入");
                        return;
                    };
                    let browser_state =
                        app_handle.state::<tiangong_app::webview_host::WebviewHostState>();
                    let mounted = browser_state
                        .registry
                        .existing_session_state(&session_id)
                        .map(tiangong_app::webview_host::manager::BrowserManager::from_state)
                        .is_some_and(|manager| manager.is_tab_mounted(&source_tab_id));
                    if !mounted {
                        return;
                    }
                    let queued = injection_tx
                        .send(tiangong_app::ToolInjection {
                            session_id: Some(target_session.clone()),
                            browser_source: Some((session_id.clone(), source_tab_id)),
                            tool: Box::new(BrowserContent {
                                title,
                                url: page_url.clone(),
                                text,
                                tabs,
                                active_tab_id,
                                feedback: Some(feedback),
                            }),
                        })
                        .is_ok();
                    info!(
                        session_id,
                        url = %page_url,
                        total_count, network_count, queued, "浏览器网络事件注入检查完成"
                    );
                    if queued {
                        ack_browser_events(app_handle, session_id, network_events).await;
                    }
                });
            });

            // 定时任务的 cron 调度已下沉到 scheduler sidecar 进程，本进程不再恢复
            // cron job 或启动 silent scheduler 循环。

            // 宿主桥接事件推送：runtime 订阅表命中时经 Tauri event 送达前端。
            {
                let app_handle = app.handle().clone();
                tiangong_plugin_runtime::set_event_emitter(Arc::new(
                    move |plugin_id: &str, channel: &str, payload: &str| {
                        let _ = app_handle.emit(
                            "bridge_event",
                            serde_json::json!({
                                "plugin_id": plugin_id,
                                "channel": channel,
                                "payload": payload,
                            }),
                        );
                    },
                ));
            }

            // app.* 原语：打开/关闭声明 extension.tab 贡献的插件 App（前端
            // open-plugin / close-plugin 通道执行）。工具调用无插件 UI 接应
            // 时宿主内部经同一处理器拉起实例（官方与三方插件一致）；插件
            // 声明 app.use 权限后亦可主动打开/关闭自己的 App。
            {
                let app_handle = app.handle().clone();
                tiangong_plugin_runtime::set_app_handler(Arc::new(
                    move |plugin_id: &str, method: &str, payload: &str| -> anyhow::Result<String> {
                        if method != "app.open" && method != "app.close" {
                            anyhow::bail!("app 原语不支持方法 {method}");
                        }
                        let payload_value =
                            serde_json::from_str::<serde_json::Value>(payload).ok();
                        let session_id = payload_value
                            .as_ref()
                            .and_then(|value| value["session_id"].as_str().map(str::to_string))
                            .unwrap_or_default();
                        // mode=background：带实例编号的为工具静默拉起（建立
                        // 可见标签但不自动展开面板），无编号的为无订阅兜底
                        // （前端挂隐藏执行壳）；缺省 focus：建立标签并弹出
                        // 聚焦面板（用户明确请求展示时）。
                        let background = payload_value
                            .as_ref()
                            .and_then(|value| value["mode"].as_str().map(str::to_string))
                            .is_some_and(|mode| mode == "background");
                        tracing::info!(
                            plugin_id,
                            method,
                            background,
                            session_id,
                            "app 原语调用"
                        );
                        let Some(app_entry) =
                            tiangong_plugin_runtime::registry::list_extension_apps()
                                .into_iter()
                                .find(|app_entry| app_entry.plugin_id == plugin_id)
                        else {
                            anyhow::bail!(
                                "插件 {plugin_id} 没有 extension.tab 贡献，无可操作的 App"
                            );
                        };
                        if method == "app.close" {
                            // 关闭目标必须显式声明：instance_id 精确关一个实例，
                            // all=true 才允许关闭该插件全部实例，避免误全关。
                            let instance_id = payload_value.as_ref().and_then(|value| {
                                value["instance_id"].as_str().map(str::to_string)
                            });
                            let close_all = payload_value
                                .as_ref()
                                .and_then(|value| value["all"].as_bool())
                                .unwrap_or(false);
                            if instance_id.is_none() && !close_all {
                                anyhow::bail!(
                                    "app.close 需要指定 instance_id（精确关闭）或 all=true（全部关闭）"
                                );
                            }
                            let _ = app_handle.emit(
                                "app:close_plugin",
                                serde_json::json!({
                                    "plugin_id": plugin_id,
                                    "session_id": session_id,
                                    "instance_id": instance_id,
                                    "all": close_all,
                                }),
                            );
                            return Ok(r#"{"ok":true}"#.to_string());
                        }
                        // 调用方可指定实例编号（幂等重开/精确关闭的锚点）：
                        // 前端以其创建或聚焦对应 App 实例，缺省由前端生成。
                        let requested_instance_id = payload_value.as_ref().and_then(|value| {
                            value["instance_id"].as_str().map(str::to_string)
                        });
                        let _ = app_handle.emit(
                            "app:open_plugin",
                            serde_json::json!({
                                "plugin_id": app_entry.plugin_id,
                                "contribution_id": app_entry.contribution_id,
                                "title": app_entry.title,
                                "sandbox": app_entry.sandbox,
                                "multi": app_entry.open_mode
                                    == tiangong_plugin_runtime::OpenMode::Multi,
                                "session_id": session_id,
                                "background": background,
                                "instance_id": requested_instance_id,
                            }),
                        );
                        Ok(r#"{"ok":true}"#.to_string())
                    },
                ));
            }

            // 统一签名信任的授权注入（runtime 保持插件中立，策略在宿主层）：
            //
            // 1) 受信安装方 = 官方签名的固定 Plugin Creator。自动签名安装
            //    （用户密钥）只对该插件放行，其他插件（哪怕声明 plugin-dev.use）
            //    不可触发；宿主未注入时 runtime fail-closed。
            // 2) sidecar 结果观察者：官方 Creator 的 devkit.build 在默认开发根
            //    下真实执行成功时登记「受信构建」。install 只接受有登记的
            //    项目——签名授权的是「使用 Creator 开发的产物」，产物必须经
            //    宿主进程内发起的真实构建，堵住前端自报身份冒装任意目录
            //    内容的通道。
            tiangong_plugin_runtime::set_plugin_dev_trusted_installer(Arc::new(
                |identity: &tiangong_plugin_runtime::InstallerIdentity| {
                    identity.plugin_id == "plugin-creator" && identity.official_signed
                },
            ));
            tiangong_plugin_runtime::set_sidecar_result_observer(Arc::new(
                |plugin_id: &str, operation: &str, payload: &str, result: &str| {
                    if plugin_id != "plugin-creator" || operation != "devkit.build" {
                        return;
                    }
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
                        return;
                    };
                    // 非默认根（root 覆盖，测试/CI 用）不产生安装资格。
                    if value
                        .get("root")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|root| !root.trim().is_empty())
                    {
                        return;
                    }
                    let Some(project) = value
                        .get("args")
                        .and_then(serde_json::Value::as_array)
                        .and_then(|args| args.first())
                        .and_then(serde_json::Value::as_str)
                        .filter(|project| !project.trim().is_empty())
                    else {
                        return;
                    };
                    // 仅成功构建登记安装资格；失败构建撤销旧登记（devkit 以
                    // ok:false 报告业务失败——sidecar 通信本身仍算成功）。
                    let build_ok = serde_json::from_str::<serde_json::Value>(result)
                        .ok()
                        .and_then(|value| {
                            value.get("ok").and_then(serde_json::Value::as_bool)
                        })
                        .unwrap_or(false);
                    if !build_ok {
                        tiangong_plugin_runtime::note_trusted_build(plugin_id, project, None);
                        return;
                    }
                    // 指纹锚定构建产物：release 内容清单的整体哈希，install
                    // 时与暂存副本比对（构建后替换产物即失配）。
                    let release_dir = tiangong_config::io::storage_root()
                        .join("plugins-dev")
                        .join(project)
                        .join("release");
                    match tiangong_plugin_runtime::content_manifest_fingerprint(&release_dir) {
                        Ok(fingerprint) => {
                            tiangong_plugin_runtime::note_trusted_build(
                                plugin_id,
                                project,
                                Some(fingerprint),
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "构建成功但产物内容清单读取失败，不登记安装资格"
                            );
                        }
                    }
                },
            ));

            // sidecar 主动通知（如终端 PTY 输出流）统一包装成 sidecar.event，
            // 经订阅表定向转发给已订阅的插件 UI；未注入时通知会被静默丢弃，
            // 终端等流式界面将收不到任何输出。
            tiangong_plugin_runtime::set_sidecar_notification_forwarder(Arc::new(
                |plugin_id: &str, channel: &str, payload: &str| {
                    let wrapped = serde_json::json!({
                        "channel": channel,
                        "payload": payload,
                    })
                    .to_string();
                    tiangong_plugin_runtime::bridge::bridge_emit_to(
                        plugin_id,
                        "sidecar.event",
                        &wrapped,
                    );
                },
            ));


            // 系统对话框原语：插件保存文件（导出结果等）。宿主 webview 是
            // WebKit（macOS），无 File System Access API——保存必须经原生
            // 对话框由宿主落盘；写入位置由用户在对话框中显式选择。
            {
                use tauri_plugin_dialog::DialogExt;
                let app_handle = app.handle().clone();
                tiangong_plugin_runtime::set_dialog_handler(Arc::new(
                    move |plugin_id: &str, method: &str, payload: &str| {
                        if method != "dialog.saveFile" {
                            anyhow::bail!("未知对话框方法 {method}");
                        }
                        #[derive(serde::Deserialize)]
                        struct SaveRequest {
                            suggested_name: String,
                            /// 文件内容（文本原样写入；二进制请先 base64 并带 encoding 字段）。
                            contents: String,
                            #[serde(default)]
                            encoding: Option<String>,
                        }
                        let request: SaveRequest = serde_json::from_str(payload)
                            .map_err(|error| anyhow::anyhow!("保存请求格式无效：{error}"))?;
                        if request.suggested_name.trim().is_empty() {
                            anyhow::bail!("suggested_name 不能为空");
                        }
                        if request.contents.len() > 100 * 1024 * 1024 {
                            anyhow::bail!("内容超过 100MB 上限");
                        }
                        let bytes: Vec<u8> = match request.encoding.as_deref() {
                            Some("base64") => {
                                use base64::Engine;
                                base64::engine::general_purpose::STANDARD
                                    .decode(request.contents.as_bytes())
                                    .map_err(|error| anyhow::anyhow!("base64 内容无效：{error}"))?
                            }
                            _ => request.contents.into_bytes(),
                        };
                        // 原生保存对话框（阻塞等待用户选择，与安装确认同款
                        // 后台线程语义——bridge 层调用方已 spawn_blocking）。
                        let handle = app_handle.clone();
                        let picked = tokio::task::block_in_place(|| {
                            handle
                                .dialog()
                                .file()
                                .set_file_name(&request.suggested_name)
                                .blocking_save_file()
                        });
                        let Some(path) = picked else {
                            return Ok(r#"{"cancelled":true}"#.to_string());
                        };
                        let path = path
                            .into_path()
                            .map_err(|error| anyhow::anyhow!("保存路径无效：{error}"))?;
                        std::fs::write(&path, bytes)
                            .map_err(|error| anyhow::anyhow!("写入文件失败：{}：{error}", path.display()))?;
                        tracing::info!(plugin_id, path = %path.display(), "插件经对话框保存文件");
                        Ok(serde_json::json!({ "cancelled": false, "path": path.display().to_string() })
                            .to_string())
                    },
                ));
            }

            // webview 容器原语（第四种声明式容器）：插件经 bridge webview.*
            // 创建/导航/eval 真实 webview 实例；实例按插件隔离
            // （view_id = webview:<plugin_id>），引擎复用 browser 基础设施。
            {
                use tauri::Manager;
                let app_handle = app.handle().clone();
                tiangong_plugin_runtime::set_webview_handler(Arc::new(
                    move |plugin_id: &str, method: &str, payload: &str| {
                        let state = app_handle
                            .try_state::<tiangong_app::webview_host::WebviewHostState>()
                            .ok_or_else(|| anyhow::anyhow!("webview 引擎未初始化"))?;
                        tiangong_app::webview_host::bridge::handle_webview_primitive(
                            &state,
                            &app_handle,
                            plugin_id,
                            method,
                            payload,
                        )
                    },
                ));
            }

            // 浏览器页面事件定向转发给插件 UI（阶段 1 事件通道）：宿主页面
            // 状态变化（加载完成/失败）经 runtime 订阅表投递给持有对应
            // webview 作用域的插件，插件订阅 webview.event 实时刷新。
            {
                tiangong_app::webview_host::set_plugin_event_forwarder(Arc::new(
                    |plugin_id: &str, channel: &str, payload: &str| {
                        tiangong_plugin_runtime::bridge::bridge_emit_to(
                            plugin_id, channel, payload,
                        );
                    },
                ));
            }

            // 输入草稿桥接：插件提交图片 data URL；宿主验证格式和大小后，
            // 通过定向事件交给当前输入框加入草稿，不自动发送。
            {
                let app_handle = app.handle().clone();
                tiangong_plugin_runtime::set_session_input_handler(Arc::new(
                    move |plugin_id: &str, method: &str, payload: &str| {
                        // 文本发送：插件页面在明确用户手势后把一段指令交给
                        // 当前会话的 Agent 处理（如创作页「开始创建」）。
                        if method == "session.input.sendText" {
                            #[derive(serde::Deserialize)]
                            struct TextInput {
                                text: String,
                            }
                            let input: TextInput = serde_json::from_str(payload)
                                .map_err(|error| anyhow::anyhow!("输入文本格式无效：{error}"))?;
                            if input.text.trim().is_empty() {
                                anyhow::bail!("输入文本不能为空");
                            }
                            if input.text.len() > 10_000 {
                                anyhow::bail!("输入文本超过 10KB 上限");
                            }
                            // 复用截图插件的输入事件通道（同一事件、同一前端
                            // 监听），文本作为 kind="text" 的输入项分流处理。
                            app_handle
                                .emit(
                                    "session_input_attachment",
                                    serde_json::json!({
                                        "plugin_id": plugin_id,
                                        "attachment": {
                                            "kind": "text",
                                            "text": input.text,
                                        },
                                    }),
                                )
                                .map_err(|error| anyhow::anyhow!("推送输入消息失败：{error}"))?;
                            return Ok("true".to_string());
                        }
                        if method != "session.input.addAttachment" {
                            anyhow::bail!("未知输入草稿方法 {method}");
                        }
                        #[derive(serde::Deserialize)]
                        struct InputAttachment {
                            source: String,
                            #[serde(default)]
                            original_name: String,
                            #[serde(default)]
                            mime_type: String,
                        }
                        let attachment: InputAttachment = serde_json::from_str(payload)
                            .map_err(|error| anyhow::anyhow!("输入附件格式无效：{error}"))?;
                        if attachment.mime_type != "image/png"
                            || !attachment.source.starts_with("data:image/png;base64,")
                        {
                            anyhow::bail!("输入草稿附件仅支持 PNG 图片");
                        }
                        let base64 = attachment
                            .source
                            .split_once(',')
                            .map(|(_, value)| value)
                            .unwrap_or_default();
                        if base64.is_empty() || base64.len() as u64 > 50 * 1024 * 1024 {
                            anyhow::bail!("图片内容为空或超过 50MB 限制");
                        }
                        let title = if attachment.original_name.trim().is_empty() {
                            "screenshot.png".to_string()
                        } else {
                            attachment.original_name
                        };
                        app_handle
                            .emit(
                                "session_input_attachment",
                                serde_json::json!({
                                    "plugin_id": plugin_id,
                                    "attachment": {
                                        "kind": "image",
                                        "source": attachment.source,
                                        "original_name": title,
                                        "mime_type": "image/png"
                                    }
                                }),
                            )
                            .map_err(|error| anyhow::anyhow!("推送输入附件失败：{error}"))?;
                        Ok("true".to_string())
                    },
                ));
            }

            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(restore_server_and_bots(app_handle));

            // 插件自动升级：后台按可更新状态升级已启用插件；
            // 离线或失败仅记日志。
            state.start_plugin_auto_updater(app.handle().clone());

            // 沙箱 Launcher 在线更新：后台拉取官方清单，经三重门禁后
            // 激活下载版；内置版保留为回退底线。
            state.start_launcher_updater(app.handle().clone());

            #[cfg(debug_assertions)]
            {
                let app_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    match tokio::signal::ctrl_c().await {
                        Ok(()) => {
                            info!("收到 Ctrl+C，正在正常退出天工");
                            app_handle.exit(0);
                        }
                        Err(error) => {
                            warn!(%error, "监听 Ctrl+C 失败");
                        }
                    }
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                // 隐藏窗口时把可见的 webview 作用域标记为不可见（页面事件
                // 注入据此跳过后台会话），实例本身不销毁。
                let host_state = window.state::<tiangong_app::webview_host::WebviewHostState>();
                if let Some(state) = host_state.registry.active_state() {
                    tiangong_app::webview_host::manager::BrowserManager::from_state(state)
                        .set_visible(false);
                }
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            tiangong_app::commands::get_sessions,
            tiangong_app::commands::get_session_meta,
            tiangong_app::commands::switch_session,
            tiangong_app::commands::load_session,
            tiangong_app::commands::get_session_model,
            tiangong_app::commands::list_session_chat_models,
            tiangong_app::commands::set_session_model,
            tiangong_app::commands::delete_session,
            tiangong_app::commands::delete_sessions_by_cwd,
            tiangong_app::commands::list_trashed_sessions,
            tiangong_app::commands::purge_all_deleted_sessions,
            tiangong_app::commands::restore_deleted_session,
            tiangong_app::commands::update_session_title,
            tiangong_app::commands::request_desktop_notification_permission,
            tiangong_app::commands::send_desktop_notification,
            tiangong_app::commands::send_message,
            tiangong_app::commands::send_message_with_media,
            tiangong_app::commands::read_attachment_as_data_url,
            tiangong_app::commands::cancel_turn,
            tiangong_app::commands::get_input_cache,
            tiangong_app::commands::set_input_cache,
            tiangong_app::commands::new_session_id,
            tiangong_app::commands::remove_input_cache,
            tiangong_app::commands::get_workspace_dir,
            tiangong_app::commands::set_session_cwd,
            tiangong_app::commands::set_workspace_dir,
            tiangong_app::commands::get_mcp_servers,
            tiangong_app::commands::get_mcp_health,
            tiangong_app::commands::probe_mcp_server,
            tiangong_app::commands::register_mcp_server,
            tiangong_app::commands::update_mcp_server,
            tiangong_app::commands::remove_mcp_server,
            tiangong_app::commands::set_mcp_server_enabled,
            tiangong_app::commands::get_server_config,
            tiangong_app::commands::set_server_config,
            tiangong_app::commands::start_server,
            tiangong_app::commands::stop_server,
            tiangong_app::commands::get_models_config,
            tiangong_app::commands::set_models_config,
            tiangong_app::commands::prewarm_workspace_index,
            tiangong_app::commands::get_sandbox_update_state,
            tiangong_app::commands::prepare_startup_resources,
            tiangong_app::commands::get_builtin_env_blocklist,
            tiangong_app::commands::get_model_capabilities,
            tiangong_app::commands::get_model_list,
            tiangong_app::commands::fetch_provider_models,
            tiangong_app::commands::append_message,
            tiangong_app::commands::edit_and_resend,
            tiangong_app::commands::list_plugin_contributions,
            tiangong_app::commands::list_plugins,
            tiangong_app::commands::list_available_plugins,
            tiangong_app::commands::import_local_plugin,
            tiangong_app::commands::install_plugin,
            tiangong_app::commands::upgrade_plugin,
            tiangong_app::commands::set_plugin_enabled,
            tiangong_app::commands::rollback_plugin,
            tiangong_app::commands::uninstall_plugin,
            tiangong_app::commands::reload_plugin,
            tiangong_app::commands::plugin_open_view,
            tiangong_app::commands::plugin_call,
            tiangong_app::commands::list_slot_contributions,
            tiangong_app::commands::set_webview_mounted_tabs,
            tiangong_app::commands::list_extension_apps,
            tiangong_app::commands::plugin_open_entry,
            tiangong_app::commands::plugin_read_entry_resource,
            tiangong_app::commands::plugin_read_icon,
            tiangong_app::commands::plugin_list_trusted_publishers,
            tiangong_app::commands::plugin_import_trusted_publisher,
            tiangong_app::commands::plugin_remove_trusted_publisher,
            tiangong_app::commands::plugin_user_key_fingerprint,
            tiangong_app::commands::plugin_read_public_key_file,
            tiangong_app::commands::bridge_call,
            tiangong_app::commands::bridge_subscribe,
            tiangong_app::commands::bridge_unsubscribe,
            tiangong_app::commands::check_default_plugins,
            tiangong_app::commands::complete_first_launch,
            tiangong_app::commands::get_trust_mode,
            tiangong_app::commands::set_trust_mode,
            tiangong_app::commands::get_default_trust_mode,
            tiangong_app::commands::set_default_trust_mode,
            tiangong_app::commands::get_sandbox_disabled,
            tiangong_app::commands::set_sandbox_disabled,
            tiangong_app::commands::get_sandbox_policy,
            tiangong_app::commands::set_sandbox_policy,
            tiangong_app::commands::get_command_env_blocklist,
            tiangong_app::commands::upgrade_launcher,
            tiangong_app::commands::set_command_env_blocklist,
            tiangong_app::commands::get_reasoning_effort,
            tiangong_app::commands::set_reasoning_effort,
            tiangong_app::commands::get_provider_balance,
            tiangong_app::commands::list_workers,
            tiangong_app::commands::has_model_capability,
            tiangong_app::commands::has_tts_capability,
            tiangong_app::commands::has_stt_capability,
            tiangong_app::commands::get_available_capabilities,
            tiangong_app::commands::get_mention_candidates,
            tiangong_app::commands::get_mention_groups,
            tiangong_app::commands::compress_context,
            tiangong_app::commands::reset_context,
            tiangong_app::commands::webhook_list,
            tiangong_app::commands::webhook_create,
            tiangong_app::commands::webhook_update,
            tiangong_app::commands::webhook_delete,
            tiangong_app::commands::webhook_trigger,
            tiangong_app::commands::webhook_list_runs,
            tiangong_app::commands::bot_list,
            tiangong_app::commands::bot_health,
            tiangong_app::commands::bot_log,
            tiangong_app::commands::bot_config_schema,
            tiangong_app::commands::bot_provision_begin,
            tiangong_app::commands::bot_provision_poll,
            tiangong_app::commands::bot_available,
            tiangong_app::commands::bot_scan_local,
            tiangong_app::commands::bot_push_targets,
            tiangong_app::commands::bot_delete_push_target,
            tiangong_app::commands::bot_register_mcp,
            tiangong_app::commands::bot_register,
            tiangong_app::commands::bot_update,
            tiangong_app::commands::bot_remove,
            tiangong_app::commands::bot_install,
            tiangong_app::commands::bot_start,
            tiangong_app::commands::bot_stop,
            tiangong_app::commands::bot_check_update,
            tiangong_app::commands::bot_upgrade,
            tiangong_app::commands::resolve_model_context_window,
        ])
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .build(generate_tauri_context())
        .expect("error while building tauri application")
        .run(|handle, event| {
            // Desktop 退出时停止自己 supervisor 管理的 bot（仅 entries，不影响 CLI 独立启动的）。
            if let tauri::RunEvent::ExitRequested { api: _exit_api, code: _exit_code, .. } = &event {
                #[cfg(windows)]
                {
                    use std::sync::atomic::{AtomicU8, Ordering};
                    static EXIT_PHASE: AtomicU8 = AtomicU8::new(0);
                    if EXIT_PHASE.load(Ordering::Acquire) != 2 {
                        _exit_api.prevent_exit();
                        if EXIT_PHASE.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                            tiangong_plugin_runtime::registry::begin_sidecar_shutdown();
                            let handle = handle.clone();
                            let exit_code = _exit_code.unwrap_or(0);
                            tauri::async_runtime::spawn(async move {
                                if let Some(state) = handle.try_state::<tiangong_app::TiangongApp>() {
                                    state.bot_runtime.stop_all().await;
                                }
                                if let Err(error) = tauri::async_runtime::spawn_blocking(
                                    tiangong_plugin_runtime::registry::shutdown_all_sidecars
                                ).await {
                                    warn!(%error, "后台插件退出任务失败");
                                }
                                EXIT_PHASE.store(2, Ordering::Release);
                                handle.exit(exit_code);
                            });
                        }
                    }
                }
                #[cfg(not(windows))]
                {
                tiangong_plugin_runtime::registry::begin_sidecar_shutdown();
                if let Some(app_state) = handle.try_state::<tiangong_app::TiangongApp>() {
                    let runtime = app_state.bot_runtime.clone();
                    tauri::async_runtime::block_on(async move {
                        runtime.stop_all().await;
                    });
                }
                // 等待后台准备任务结束，再逐个停止所有 sidecar。
                tiangong_plugin_runtime::registry::shutdown_all_sidecars();
                }
            }
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                show_main_window(handle);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = handle;
        });

    drop(_guard);
}

#[derive(Clone)]
struct TrayServerUi {
    status_item: tauri::menu::MenuItem<tauri::Wry>,
    start_item: tauri::menu::MenuItem<tauri::Wry>,
    stop_item: tauri::menu::MenuItem<tauri::Wry>,
    icon_normal: Option<tauri::image::Image<'static>>,
    icon_running: Option<tauri::image::Image<'static>>,
    icon_error: Option<tauri::image::Image<'static>>,
}

fn setup_tray(app: &mut tauri::App) -> tauri::Result<()> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder};
    use tauri::tray::TrayIconBuilder;

    let app_handle = app.handle().clone();
    let status_item = MenuItemBuilder::with_id(TRAY_STATUS_ID, tray_server_status(&app_handle).0)
        .enabled(false)
        .build(app)?;
    let show_item = MenuItemBuilder::with_id(TRAY_SHOW_WINDOW_ID, "显示天工").build(app)?;
    let start_item = MenuItemBuilder::with_id(TRAY_START_SERVER_ID, "启动 Server").build(app)?;
    let stop_item = MenuItemBuilder::with_id(TRAY_STOP_SERVER_ID, "停止 Server").build(app)?;
    let quit_item = MenuItemBuilder::with_id(TRAY_QUIT_ID, "退出天工").build(app)?;
    let menu = MenuBuilder::new(app)
        .item(&status_item)
        .separator()
        .item(&show_item)
        .item(&start_item)
        .item(&stop_item)
        .separator()
        .item(&quit_item)
        .build()?;

    let ui = TrayServerUi {
        status_item,
        start_item,
        stop_item,
        icon_normal: load_icon_rgba(include_bytes!("../icons/32x32.rgba")),
        icon_running: load_icon_rgba(include_bytes!("../icons/32x32-running.rgba")),
        icon_error: load_icon_rgba(include_bytes!("../icons/32x32-error.rgba")),
    };
    let ui_for_menu = ui.clone();
    let mut tray = TrayIconBuilder::with_id("tiangong")
        .menu(&menu)
        .tooltip("天工")
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| {
            handle_tray_menu_event(app.clone(), event.id().as_ref(), ui_for_menu.clone());
        });
    if let Some(icon) = ui.icon_normal.clone() {
        tray = tray.icon(icon);
    }
    let tray = tray.build(app)?;
    refresh_tray_server_status(&app_handle, &tray, &ui);
    start_tray_status_refresh(app_handle.clone(), tray, ui);

    Ok(())
}

async fn restore_server_and_bots(app: tauri::AppHandle) {
    let state = app.state::<tiangong_app::TiangongApp>();
    // Server 恢复只遵循自身开关，不由 Bot 是否启用决定。恢复失败保留开启
    // 意图，托盘和设置页据此显示红色异常状态。
    match tiangong_app::commands::restore_embedded_server(state.inner()).await {
        Ok(Some(message)) => info!(%message, "嵌入式 Server 已恢复"),
        Ok(None) => {}
        Err(error) => warn!(%error, "嵌入式 Server 自动恢复失败"),
    }

    // Bot 独立恢复，不隐式改变 Server 开关。
    let has_enabled_bot = state.bot_store.list().iter().any(|bot| bot.enabled);
    if !has_enabled_bot {
        return;
    }
    // Server 地址/Token 从配置读取注入（供 bot 回连），但不强制 Server 运行。
    let server_config = state
        .with_state_read(|core_state| Ok(core_state.config.server.clone()))
        .await
        .unwrap_or_default();
    let extra_env = tiangong_app::commands::bot_server_env(&server_config);
    state.bot_runtime.start_enabled(&extra_env).await;
    // MCP 注册需要 Server 运行；Server 未运行时跳过（bot 仍独立运行）。
    let server_ok = tiangong_app::commands::server_health_check(&server_config);
    if !server_ok {
        info!("Server 未运行，已启动的 bot 将独立运行；启动 Server 后可恢复 Agent 调用");
        return;
    }
    for bot in state.bot_store.list().into_iter().filter(|bot| bot.enabled) {
        if !matches!(
            state.bot_runtime.health(&bot.id).await,
            tiangong_bots::BotHealth::Running
        ) {
            continue;
        }
        if let Err(register_error) =
            tiangong_app::commands::ensure_bot_mcp_registered(&bot.id, state.inner()).await
        {
            warn!(
                bot_id = %bot.id,
                error = %register_error,
                "bot 自动启动后注册 MCP 失败"
            );
        }
    }
}

fn start_tray_status_refresh(app: tauri::AppHandle, tray: tauri::tray::TrayIcon, ui: TrayServerUi) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(3));
        refresh_tray_server_status(&app, &tray, &ui);
    });
}

fn handle_tray_menu_event(app: tauri::AppHandle, menu_id: &str, ui: TrayServerUi) {
    match menu_id {
        TRAY_SHOW_WINDOW_ID => {
            info!("tray: 显示天工 clicked");
            show_main_window(&app);
        }
        TRAY_START_SERVER_ID => {
            let app_clone = app.clone();
            let ui = ui.clone();
            let tray = app.tray_by_id("tiangong");
            tauri::async_runtime::spawn(async move {
                let _ = ui.status_item.set_text("Server 状态：启动中");
                let state = app_clone.state::<tiangong_app::TiangongApp>();
                if let Err(error) =
                    tiangong_app::commands::start_embedded_server_with_intent(state.inner()).await
                {
                    warn!(%error, "菜单栏启动 Server 失败");
                }
                if let Some(tray) = tray.as_ref() {
                    refresh_tray_server_status(&app_clone, tray, &ui);
                }
            });
        }
        TRAY_STOP_SERVER_ID => {
            let app_clone = app.clone();
            let ui = ui.clone();
            let tray = app.tray_by_id("tiangong");
            tauri::async_runtime::spawn(async move {
                let _ = ui.status_item.set_text("Server 状态：停止中");
                let state = app_clone.state::<tiangong_app::TiangongApp>();
                match tiangong_app::commands::stop_server_with_intent(state.inner()).await {
                    // 停止结果经桌面通知反馈，依赖 Server 的插件提示不因
                    // 菜单栏入口而丢失。
                    Ok(message) => {
                        tiangong_app::commands::show_host_notification(
                            &app_clone,
                            "天工 Server",
                            &message,
                        );
                    }
                    Err(error) => warn!(%error, "菜单栏停止 Server 失败"),
                }
                if let Some(tray) = tray.as_ref() {
                    refresh_tray_server_status(&app_clone, tray, &ui);
                }
            });
        }
        TRAY_QUIT_ID => app.exit(0),
        _ => {}
    }
}

fn show_main_window(app: &tauri::AppHandle) {
    use tauri::Manager;

    // 恢复窗口时同步 webview 可见标记（实际位置由插件 UI 经原语重新对齐）。
    let host_state = app.state::<tiangong_app::webview_host::WebviewHostState>();
    if let Some(state) = host_state.registry.active_state() {
        tiangong_app::webview_host::manager::BrowserManager::from_state(state).set_visible(true);
    }

    let Some(window) = app.get_webview_window("main") else {
        // 尝试用 get_window 作为后备
        if let Some(win) = app.get_window("main") {
            let _ = win.show();
            let _ = win.set_focus();
        }
        return;
    };
    let _ = window.show();
    let _ = window.set_focus();
    // 延迟通知前端恢复浏览器 WebView 位置（等窗口渲染完成）
    let app_clone = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = app_clone.emit("browser:restore", ());
    });
}

fn refresh_tray_server_status(
    app: &tauri::AppHandle,
    tray: &tauri::tray::TrayIcon,
    ui: &TrayServerUi,
) {
    let (text, status) = tray_server_status(app);
    let _ = ui.status_item.set_text(text);
    let _ = ui
        .start_item
        .set_enabled(status != ServerTrayStatus::Running);
    let _ = ui
        .stop_item
        .set_enabled(status != ServerTrayStatus::Stopped);
    let icon = match status {
        ServerTrayStatus::Running => ui.icon_running.as_ref().or(ui.icon_normal.as_ref()),
        ServerTrayStatus::Error => ui.icon_error.as_ref().or(ui.icon_normal.as_ref()),
        ServerTrayStatus::Stopped => ui.icon_normal.as_ref(),
    };
    if let Some(icon) = icon {
        let _ = tray.set_icon(Some(icon.clone()));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ServerTrayStatus {
    Stopped,
    Running,
    Error,
}

fn tray_server_status(app: &tauri::AppHandle) -> (String, ServerTrayStatus) {
    use tauri::Manager;
    let app_state = app.state::<tiangong_app::TiangongApp>();
    let config = app_state
        .state
        .try_lock()
        .map(|state| state.config.server.clone())
        .unwrap_or_else(|_| tiangong_server::config::load_server_config());
    let running = tiangong_app::commands::server_health_check(&config);
    match tiangong_app::commands::server_status_name(config.enabled, running) {
        "running" => ("Server 状态：运行中".to_string(), ServerTrayStatus::Running),
        "error" => ("Server 状态：异常".to_string(), ServerTrayStatus::Error),
        _ => ("Server 状态：未运行".to_string(), ServerTrayStatus::Stopped),
    }
}

fn generate_tauri_context() -> tauri::Context<tauri::Wry> {
    tauri::generate_context!()
}

fn load_icon_rgba(data: &'static [u8]) -> Option<tauri::image::Image<'static>> {
    if data.len() < 8 {
        return None;
    }
    let width = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let height = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let rgba = data[8..].to_vec();
    Some(tauri::image::Image::new_owned(rgba, width, height))
}
