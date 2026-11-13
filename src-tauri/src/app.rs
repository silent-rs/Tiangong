use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use tauri::Manager;

use tiangong_core::agent_input::AgentInputKind;
use tiangong_core::config::core::CoreConfigProvider;
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

type RemoteTurnResult = Result<tiangong_types::OutgoingMessage, String>;
type RemoteTurnWaiter = tokio::sync::oneshot::Sender<RemoteTurnResult>;

/// 天工应用状态
///
/// state: 应用数据 — Arc<tokio Mutex> 以支持嵌入式 server 共享
/// core_manager: 会话资源与 Core 操作入口
/// config: 共享配置提供者
/// embedded_server: 嵌入式 Server 句柄（Desktop 模式下 Server 运行在 app 进程内）
pub struct TiangongApp {
    plugin_preload: tokio::sync::watch::Sender<
        Option<Result<tiangong_plugin_runtime::registry::StartupPluginReadiness, String>>,
    >,
    pub state: std::sync::Arc<AsyncMutex<tiangong_app_state::app_state::TiangongState>>,
    /// 覆盖单个会话从附件准备到 Core 持久化确认的完整串行区间。
    /// 不同会话使用不同锁，可以并行发送；同一会话不会并发创建 Core 或抢占输入。
    session_send_locks: Mutex<HashMap<String, std::sync::Arc<AsyncMutex<()>>>>,
    /// 输入缓存归档/持久化串行锁。与发送锁分离，使用户等待发送期间的新输入能立即写入
    /// 新 revision，而不会等旧发送结束后才落盘。
    input_cache_update_locks: Mutex<HashMap<String, std::sync::Arc<AsyncMutex<()>>>>,
    /// 已成功投递的最新输入 revision，用于防止双击或迟到请求重复发送。
    delivered_input_revisions: Mutex<HashMap<String, u64>>,
    /// 前端冻结发送快照后、真正进入 send_message 前的附件租约。
    /// 该租约让输入缓存清理不会删掉已被本次发送冻结的归档路径。
    input_send_claims: Mutex<HashMap<String, InputSendClaim>>,
    /// 当前进程已明确丢弃/删除的输入缓存键，阻止迟到写入复活。
    discarded_input_caches: Mutex<HashSet<String>>,
    pub config: CoreConfigProvider,
    /// 内嵌 HTTP 的等待者按稳定用户消息 ID 绑定。终态由唯一的桌面流消费者完成，
    /// 不能按“当前最后一轮”唤醒，否则同会话排队消息会串答。
    remote_turn_waiters: Mutex<HashMap<(String, String), Vec<RemoteTurnWaiter>>>,
    /// 内嵌 HTTP 等待型轮次的会话所有权。watch 同时提供无丢唤醒的释放通知，
    /// 让同会话远端请求串行到终态，调度消息也不会插入正在等待的远端轮次。
    remote_turn_states: Mutex<HashMap<String, tokio::sync::watch::Sender<Option<String>>>>,
    embedded_server: Mutex<Option<tiangong_server::EmbeddedServerHandle>>,
    /// Tauri 应用句柄（browser/terminal 插件构造需要）。
    ///
    /// 由 setup 阶段经 [`Self::set_app_handle`] 注入（builder 链构造时尚无 handle）。
    /// 每次 `ensure_core` 创建 Core 时，经 [`crate::core_factory::DesktopCoreFactory`]
    /// 用此句柄现场构造全部插件实例。`Arc` 包裹以便与 factory 共享同一 cell。
    app_handle: std::sync::Arc<std::sync::OnceLock<tauri::AppHandle>>,
    /// 桌面端 Core 构造依赖（issue #245）：持有 app_handle/skill/mcp/config/
    /// storage_root,提供 `build_plugins()` 供 `ensure_core` 前构造插件集合。
    pub desktop_factory: std::sync::Arc<crate::core_factory::DesktopCoreFactory>,
    /// 插件变化上下文交接的状态容器（标记 / 首检 / 定档指纹）。
    pub(crate) config_handoff_store: crate::config_handoff::ConfigHandoffStore,
    /// 与 `TiangongState.core_manager` 共享同一实例的快捷句柄，供同步入口使用。
    pub core_manager: tiangong_core_manager::CoreManager,
    /// 中枢对话 Core（**直接持有，不经 CoreManager**）：中枢需要内置且唯一的
    /// 中枢插件（`crate::central::CentralPlugin`），CoreManager 的 runtime 聚合
    /// 桥插件构造与编排不适用；中枢的构造/投递/取消/关闭见下方方法组。
    pub(crate) central_core: Mutex<Option<tiangong_core::core::TiangongCore>>,
    /// 中枢派发执行器的请求通道（惰性启动；中枢插件经它派发后台工作）。
    pub(crate) central_dispatch_tx: std::sync::OnceLock<
        tokio::sync::mpsc::UnboundedSender<crate::central::CentralPluginMessage>,
    >,
    /// 已到达、尚未进入中枢对话的后台工作回报（暂存批量模式：随用户
    /// 下一条消息一并注入，或由中枢经 collect_work_reports 主动收集；
    /// 与中枢插件共享同一实例）。
    pub(crate) central_work_reports: std::sync::Arc<Mutex<Vec<String>>>,
    /// 在执行中的后台工作会话（dispatch 投递成功插入，终态移除）——
    /// 回报时机判定的依据：失败必达；成功且仍有任务在跑则暂存，
    /// 全部结束后汇总一轮（设计稿 §9「关键节点必达 / 适时沉默」）。
    pub(crate) central_running_works: Mutex<HashSet<String>>,
    /// 后台工作状态登记（前端「后台工作」状态条数据源；work_id 唯一）。
    pub(crate) central_work_registry: Mutex<Vec<crate::central::CentralWorkStatus>>,
    /// 排队依赖的挂起派发（after_work_id 未终态）：依赖终态回调 flush。
    pub(crate) central_pending_dispatches: Mutex<Vec<crate::central::CentralDispatchRequest>>,
    /// 中枢模型回退通知已发出（抖动控制：每进程一次）。
    pub(crate) central_model_fallback_notified: std::sync::atomic::AtomicBool,
    /// 激活问候已注入（每进程首次进入中枢模式时注入一次，§4.2/§10.4）。
    pub(crate) central_activation_greeted: std::sync::atomic::AtomicBool,
    /// 每会话的终态路由（R1：按 execution_id 路由 Worker turn 终态，
    /// 取代 session 级回调槽——快速完成暂存补发、投递失败显式取消）。
    pub(crate) execution_router: crate::central_host::ExecutionRouter,
    /// 授权票据注册表（R3：WorkspaceApproval / Decision 一次性票据，
    /// 仅宿主 UI 真实用户操作后签发；模型只能请求，不能生成）。
    pub(crate) ticket_registry: tiangong_central::TicketRegistry,
    /// 模式协调器（R4：中枢/普通切换的宿主状态机与普通上下文快照，
    /// 取代前端模块级变量作为事实源；后台工作不受切换影响）。
    pub(crate) mode_coordinator: crate::central_host::mode_coordinator::ModeCoordinator,
    /// 工具消息注入通道（插件作为生产者 push，app 消费者统一处理）。
    /// 插件通过 [`Self::tool_injection_tx`] 获取 sender，直接 push `ToolInjection`。
    /// 消费者任务由 [`Self::start_tool_injection_consumer`] 启动。
    tool_injection_tx: tokio::sync::mpsc::UnboundedSender<ToolInjection>,
    /// 消费者 receiver（Option：take 出来启动消费者任务后变 None）。
    tool_injection_rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<ToolInjection>>>,
    /// 会话空闲期暂存的页面状态快照（session_id → 最新一条），见
    /// [`ToolInjection::snapshot`]；下一条用户消息投递前注入。
    pending_snapshots: std::sync::Arc<Mutex<HashMap<String, ToolInjection>>>,
}

/// 工具消息注入请求（插件 → app 消费者）。
pub struct ToolInjection {
    /// 注入到哪个 session（None = 当前活跃 session）。
    pub session_id: Option<String>,
    /// 浏览器注入来源；消费前再次确认该页面仍有实际标签。
    pub browser_source: Option<(String, String)>,
    /// 页面状态快照（非事件）：会话空闲时由宿主暂存且只保留最新一条，
    /// 下一条用户消息投递前再注入，避免空闲期积压的过期页面被批量补入。
    pub snapshot: bool,
    /// 注入的工具数据。
    pub tool: Box<dyn tiangong_core::agent_input::ToolInput>,
}

/// 浏览器注入来源的页面是否仍有实际标签（无来源视为有效）。
fn browser_source_mounted(
    app_handle: &tauri::AppHandle,
    source: Option<&(String, String)>,
) -> bool {
    let Some((scope, tab_id)) = source else {
        return true;
    };
    app_handle
        .state::<crate::webview_host::WebviewHostState>()
        .registry
        .existing_session_state(scope)
        .map(crate::webview_host::manager::BrowserManager::from_state)
        .is_some_and(|manager| manager.is_tab_mounted(tab_id))
}

/// 暂存页面状态快照：同一会话只保留最新一条（新快照顶替旧快照）。
fn stash_snapshot(
    pending: &Mutex<HashMap<String, ToolInjection>>,
    session_id: String,
    injection: ToolInjection,
) {
    let mut guard = pending.lock().unwrap_or_else(|error| error.into_inner());
    guard.insert(session_id, injection);
}

/// 自制插件清单注入（插件变化事件的订阅者投递）。
///
/// 经工具注入通道追加到对话历史（append-only，KV cache 前缀不受影响）：
/// turn 活跃的会话在工具批次收敛后的安全点收到清单（PendingFinish 阶段
/// 会转 NeedModel 重新分析）；空闲会话由聚合桥的轮开始清单检查补上。
struct LocalPluginListInput;

impl tiangong_core::agent_input::ToolInput for LocalPluginListInput {
    fn tool_name(&self) -> &str {
        tiangong_plugin_runtime::LOCAL_PLUGIN_LIST_INJECTION
    }

    fn render(&self) -> serde_json::Value {
        tiangong_plugin_runtime::registry::local_plugin_inventory()
    }
}

#[derive(Debug, Clone)]
struct InputSendClaim {
    revision: u64,
    attachment_paths: Vec<String>,
}

/// 当前插件能力指纹（向 runtime 取值）。
///
/// 「插件是否发生变化」由 runtime 回答——它是插件注册表的所有者，启用
/// 判定、版本回落与摘要算法都属于它的内部知识。app 只消费这个值，据此
/// 决定后续如何处理（整理上下文、打标、定档），不重复实现判定逻辑。
pub(crate) fn execution_fingerprint() -> String {
    tiangong_plugin_runtime::registry::enabled_plugin_fingerprint()
}

impl TiangongApp {
    /// 构造应用状态。`app_handle` 由 setup 阶段经 [`Self::set_app_handle`] 注入。
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let (tool_injection_tx, tool_injection_rx) = tokio::sync::mpsc::unbounded_channel();

        let core_state = tiangong_app_state::app_state::TiangongState::new();
        let config = core_state.core_manager.config().clone();
        let storage_root = core_state.config.storage_root.clone();
        let core_manager = core_state.core_manager.clone();
        let state = std::sync::Arc::new(AsyncMutex::new(core_state));

        let app_handle = std::sync::Arc::new(std::sync::OnceLock::new());
        let desktop_factory = std::sync::Arc::new(crate::core_factory::DesktopCoreFactory {
            app_handle: app_handle.clone(),
            config: config.clone(),
            storage_root: storage_root.clone(),
        });
        Self {
            plugin_preload: tokio::sync::watch::channel(Some(Ok(
                tiangong_plugin_runtime::registry::StartupPluginReadiness::default(),
            )))
            .0,
            state,
            session_send_locks: Mutex::new(HashMap::new()),
            input_cache_update_locks: Mutex::new(HashMap::new()),
            delivered_input_revisions: Mutex::new(HashMap::new()),
            input_send_claims: Mutex::new(HashMap::new()),
            discarded_input_caches: Mutex::new(HashSet::new()),
            config,
            remote_turn_waiters: Mutex::new(HashMap::new()),
            remote_turn_states: Mutex::new(HashMap::new()),
            embedded_server: Mutex::new(None),
            app_handle,
            desktop_factory,
            config_handoff_store: crate::config_handoff::ConfigHandoffStore::new(
                storage_root.clone(),
            ),
            core_manager,
            central_core: Mutex::new(None),
            central_dispatch_tx: std::sync::OnceLock::new(),
            central_work_reports: std::sync::Arc::new(Mutex::new(Vec::new())),
            central_running_works: Mutex::new(HashSet::new()),
            central_work_registry: Mutex::new(Vec::new()),
            central_pending_dispatches: Mutex::new(Vec::new()),
            central_model_fallback_notified: std::sync::atomic::AtomicBool::new(false),
            central_activation_greeted: std::sync::atomic::AtomicBool::new(false),
            execution_router: crate::central_host::ExecutionRouter::new(),
            ticket_registry: tiangong_central::TicketRegistry::new(),
            mode_coordinator: crate::central_host::mode_coordinator::ModeCoordinator::new(),
            tool_injection_tx,
            tool_injection_rx: Mutex::new(Some(tool_injection_rx)),
            pending_snapshots: std::sync::Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn start_plugin_preload(&self) {
        self.plugin_preload.send_replace(None);
        self.spawn_plugin_preload();
    }

    /// 沙箱就绪后重试启动期失败的插件准备。failures 非空即触发：存在
    /// 永久损坏插件时每次调用都会重跑全量验证与常驻预热，当前调用点
    /// 仅启动与沙箱修复成功两处，频次可控；接入更频繁的调用点前需
    /// 先加退避或按插件粒度重试。
    pub(crate) fn retry_failed_plugin_preload(&self) {
        if self.plugin_preload.send_if_modified(|result| {
            let retry = match result {
                Some(Err(_)) => true,
                Some(Ok(readiness)) => !readiness.failures.is_empty(),
                _ => false,
            };
            if retry {
                *result = None;
            }
            retry
        }) {
            self.spawn_plugin_preload();
        }
    }

    fn spawn_plugin_preload(&self) {
        let completion = self.plugin_preload.clone();
        let storage_root = tiangong_config::io::storage_root();
        tauri::async_runtime::spawn(async move {
            let started = std::time::Instant::now();
            let result = tauri::async_runtime::spawn_blocking(move || {
                let _ = tiangong_plugin_runtime::launcher_update::launcher_status(&storage_root);
                tracing::info!("启动沙箱检查结束，开始后台加载插件");
                tiangong_plugin_runtime::registry::prepare_desktop_startup_plugins(&storage_root)
                    .map_err(|error| format!("{error:#}"))
            })
            .await
            .map_err(|error| format!("插件预加载失败：{error}"))
            .and_then(|result| result);
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                success = result.is_ok(),
                "桌面插件后台预加载结束"
            );
            completion.send_replace(Some(result));
        });
    }

    pub async fn wait_plugin_preload(
        &self,
    ) -> Result<tiangong_plugin_runtime::registry::StartupPluginReadiness, String> {
        let mut completion = self.plugin_preload.subscribe();
        let result = completion
            .wait_for(|result| result.is_some())
            .await
            .map_err(|_| "插件加载任务已关闭".to_string())?;
        result.as_ref().expect("已等待插件加载结果").clone()
    }

    /// 注入 Tauri 应用句柄（setup 阶段调用，仅一次）。
    /// 已注入的 app_handle（测试环境通常未注入 → None）。
    pub(crate) fn app_handle_read(&self) -> Option<tauri::AppHandle> {
        self.app_handle.get().cloned()
    }

    pub fn set_app_handle(&self, handle: tauri::AppHandle) {
        let _ = self.app_handle.set(handle);
    }

    /// 获取工具消息注入 channel sender。
    ///
    /// 插件持有此 sender 后可直接投递 `ToolInjection`，无需经过 emit/listen 事件中转。
    /// 消费者任务由 [`Self::start_tool_injection_consumer`] 启动后统一处理。
    pub fn tool_injection_tx(&self) -> tokio::sync::mpsc::UnboundedSender<ToolInjection> {
        self.tool_injection_tx.clone()
    }

    pub(crate) fn register_remote_turn_waiter(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> tokio::sync::oneshot::Receiver<RemoteTurnResult> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.remote_turn_waiters
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry((session_id.to_string(), message_id.to_string()))
            .or_default()
            .push(tx);
        rx
    }

    pub(crate) fn complete_remote_turn_waiters(
        &self,
        session_id: &str,
        message_id: &str,
        result: RemoteTurnResult,
    ) {
        let waiters = self
            .remote_turn_waiters
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&(session_id.to_string(), message_id.to_string()))
            .unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
    }

    pub(crate) fn fail_remote_session_waiters(&self, session_id: &str, message: &str) {
        let mut waiters = self
            .remote_turn_waiters
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let keys = waiters
            .keys()
            .filter(|(sid, _)| sid == session_id)
            .cloned()
            .collect::<Vec<_>>();
        let mut removed = Vec::new();
        for key in keys {
            removed.extend(waiters.remove(&key).unwrap_or_default());
        }
        drop(waiters);
        for waiter in removed {
            let _ = waiter.send(Err(message.to_string()));
        }
    }

    fn remote_turn_state_sender(
        &self,
        session_id: &str,
    ) -> tokio::sync::watch::Sender<Option<String>> {
        self.remote_turn_states
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry(session_id.to_string())
            .or_insert_with(|| tokio::sync::watch::channel(None).0)
            .clone()
    }

    pub(crate) fn remote_turn_owner(&self, session_id: &str) -> Option<String> {
        self.remote_turn_state_sender(session_id).borrow().clone()
    }

    pub(crate) fn begin_remote_turn(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<(), String> {
        let sender = self.remote_turn_state_sender(session_id);
        if let Some(owner) = sender.borrow().as_ref() {
            return Err(format!("会话已有远程轮次正在执行：{owner}"));
        }
        sender.send_replace(Some(message_id.to_string()));
        Ok(())
    }

    pub(crate) fn finish_remote_turn(&self, session_id: &str, message_id: &str) {
        let sender = self.remote_turn_state_sender(session_id);
        let owns_turn = sender.borrow().as_deref() == Some(message_id);
        if owns_turn {
            sender.send_replace(None);
        }
    }

    pub(crate) fn remote_turn_allows_message(&self, session_id: &str, message_id: &str) -> bool {
        self.remote_turn_owner(session_id)
            .is_none_or(|owner| owner == message_id)
    }

    pub(crate) async fn wait_for_remote_turn_release(&self, session_id: &str) {
        let mut receiver = self.remote_turn_state_sender(session_id).subscribe();
        loop {
            let active = receiver.borrow().is_some();
            if !active {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    /// 启动工具消息注入消费者任务（main.rs setup 阶段调用一次）。
    ///
    /// 循环接收插件 push 的 `ToolInjection`，统一处理注入到 session。
    /// 注入逻辑与 [`Self::inject_tool`] 相同，但支持指定 session_id。
    pub fn start_tool_injection_consumer(&self, app_handle: tauri::AppHandle) {
        let rx = {
            let mut guard = self.tool_injection_rx.lock().unwrap();
            guard.take()
        };
        let Some(mut rx) = rx else {
            tracing::warn!("工具消息注入消费者已启动，跳过重复启动");
            return;
        };

        // 持有 Arc<state> 让消费者任务独立存活
        let state = self.state.clone();
        let pending_snapshots = self.pending_snapshots.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(req) = rx.recv().await {
                let session_id = match req.session_id {
                    Some(ref id) => id.clone(),
                    None => {
                        let guard = state.lock().await;
                        guard.active_session_id.as_str().to_string()
                    }
                };

                let tool_name = req.tool.tool_name().to_string();

                if !browser_source_mounted(&app_handle, req.browser_source.as_ref()) {
                    tracing::debug!(session_id, "浏览器标签已关闭，跳过页面注入");
                    continue;
                }

                // 通过 app_handle 获取 TiangongApp
                let app_state = app_handle.state::<TiangongApp>();
                // 页面状态快照：会话空闲时不立即投递（否则进入会话延迟队列，
                // 多条过期快照会在下一轮一并补入），由宿主暂存最新一条，
                // 下一条用户消息投递前再注入。
                if req.snapshot && !app_state.core_manager.is_core_busy(&session_id) {
                    stash_snapshot(&pending_snapshots, session_id, req);
                    continue;
                }
                // 与发送、编辑和删除共享同一会话边界，覆盖快照读取、ensure、消费者
                // 绑定和最终 deliver，禁止在 take→删除/重建空窗中复活孤立 Core。
                let session_lock = app_state.session_send_lock(&session_id);
                let _send_guard = session_lock.lock_owned().await;
                // 会话存在性用 metadata 判定；ensure_core 需要的完整 session
                // 从磁盘 load（issue #245：真相源归磁盘）。
                //
                // 首条消息前的终端命令（terminal:user_command）会在此被跳过——但
                // terminal 插件的 on_turn_started 会在每轮 turn 启动时注入完整终端
                // 状态（terminal_data，含 recent_output），agent 下一轮自然能看到，
                // 不依赖这里的离散事件实时注入。
                let session_exists = state
                    .lock()
                    .await
                    .core_manager
                    .list_session_metadata()
                    .iter()
                    .any(|m| m.id == session_id);
                if !session_exists {
                    tracing::warn!(session_id, "消费者无法恢复 core：session 不存在");
                    continue;
                }
                use std::sync::mpsc;
                let (stream_tx, stream_rx) = mpsc::channel::<tiangong_types::StreamEvent>();
                let ensured = app_state
                    .ensure_core(&session_id, None, None, None, None, stream_tx)
                    .await;
                let ensured = match ensured {
                    Ok(ensured) => ensured,
                    Err(error) => {
                        tracing::warn!(%error, session_id, "插件未完成加载，取消自动恢复 Core");
                        continue;
                    }
                };
                if ensured.is_new {
                    crate::commands::start_stream_consumer(
                        app_handle.clone(),
                        ensured.session_id.clone(),
                        stream_rx,
                    );
                    tracing::info!(session_id, "消费者自动恢复 core");
                }

                if !browser_source_mounted(&app_handle, req.browser_source.as_ref()) {
                    tracing::debug!(session_id, "浏览器标签已关闭，跳过页面注入");
                    continue;
                }

                let core_sent = app_state
                    .core_manager
                    .deliver_to_core_if_live(&ensured.session_id, AgentInputKind::Tool(req.tool));

                if !core_sent {
                    tracing::warn!(session_id, tool_name, "deliver 失败（core 通道已关闭）");
                }

                tracing::debug!(session_id, tool_name, "工具消息注入完成");
            }
            tracing::info!("工具消息注入消费者任务结束");
        });
    }

    /// 启动插件自动升级任务（main.rs setup 阶段调用一次）。
    ///
    /// 启动后后台检查一次 OSS 插件目录的可更新状态，对已启用且有
    /// 新版本的插件直接触发升级。全程无弹窗，离线或失败仅记日志，
    /// 留待下次启动重试；插件的安装由首启推荐引导与插件市场负责。
    pub fn start_plugin_auto_updater(&self, app_handle: tauri::AppHandle) {
        tauri::async_runtime::spawn(async move {
            if let Err(error) = run_plugin_auto_upgrade(app_handle).await {
                tracing::warn!(%error, "插件自动升级任务异常结束");
            }
        });
    }

    /// 启动沙箱自动更新任务（main.rs setup 阶段调用一次）。
    ///
    /// 与插件自动升级一致：后台检查一次官方清单，本地已有可用 Sandbox
    /// 且远端版本更新时直接下载替换。全程无弹窗，离线或失败仅记日志，
    /// 留待下次启动重试；本地缺失或损坏仍由启动准备页负责安装。
    pub fn start_launcher_updater(&self, app_handle: tauri::AppHandle) {
        tauri::async_runtime::spawn(async move {
            run_launcher_auto_update(app_handle).await;
        });
    }

    /// 同步工具消息注入（供需要同步返回值的场景，如 browser:events 的 ack 判断）。
    ///
    /// core 不存在时返回 false。大多数场景应通过 [`Self::tool_injection_tx`] push 到 channel，
    /// 消费者会自动 ensure_core 恢复 core 后注入。
    pub async fn inject_tool(&self, tool: Box<dyn tiangong_core::agent_input::ToolInput>) -> bool {
        let tool_name = tool.tool_name().to_string();
        let session_id = {
            let guard = self.state.lock().await;
            guard.active_session_id.as_str().to_string()
        };
        let session_lock = self.session_send_lock(&session_id);
        let _send_guard = session_lock.lock_owned().await;
        if self.core_manager.has_live_core(&session_id) {
            tracing::info!(session_id, tool_name, "注入工具消息 via deliver");
            self.core_manager
                .deliver_to_core_if_live(&session_id, AgentInputKind::Tool(tool))
        } else {
            tracing::warn!(
                session_id,
                tool_name,
                "inject_tool: core 不存在，返回 false（应走 channel 消费者自动恢复）"
            );
            false
        }
    }

    pub fn session_send_lock(&self, session_id: &str) -> std::sync::Arc<AsyncMutex<()>> {
        let mut locks = match self.session_send_locks.lock() {
            Ok(guard) => guard,
            Err(err) => {
                warn!(error = %err, "session_send_locks 锁已污染，尝试恢复");
                err.into_inner()
            }
        };
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| std::sync::Arc::new(AsyncMutex::new(())))
            .clone()
    }

    pub fn remove_session_send_lock(&self, session_id: &str) {
        // 不从锁表删除 Arc：若旧 guard/等待者仍存活，新请求创建另一把锁
        // 会直接破坏互斥。这些小锁保留到进程结束。
        let mut delivered = match self.delivered_input_revisions.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        };
        delivered.remove(session_id);
        drop(delivered);
        let mut claims = match self.input_send_claims.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        };
        claims.remove(session_id);
        drop(claims);
        let mut discarded = match self.discarded_input_caches.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        };
        discarded.insert(session_id.to_string());
    }

    pub fn mark_input_cache_discarded(&self, cache_key: &str) {
        let mut discarded = match self.discarded_input_caches.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        };
        discarded.insert(cache_key.to_string());
    }

    pub fn input_cache_was_discarded(&self, cache_key: &str) -> bool {
        let discarded = match self.discarded_input_caches.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        };
        discarded.contains(cache_key)
    }

    pub fn input_revision_was_delivered(&self, cache_key: &str, revision: u64) -> bool {
        let delivered = match self.delivered_input_revisions.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        };
        delivered
            .get(cache_key)
            .is_some_and(|current| revision <= *current)
    }

    pub fn mark_input_revision_delivered(&self, cache_key: &str, revision: u64) {
        let mut delivered = match self.delivered_input_revisions.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        };
        delivered
            .entry(cache_key.to_string())
            .and_modify(|current| *current = (*current).max(revision))
            .or_insert(revision);
    }

    /// 冻结一版已归档输入用于发送。返回被更新 revision 替换的旧租约路径。
    pub fn register_input_send_claim(
        &self,
        session_id: &str,
        revision: u64,
        attachment_paths: Vec<String>,
    ) -> Result<Vec<String>, String> {
        let mut claims = match self.input_send_claims.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        };
        if let Some(existing) = claims.get(session_id) {
            if existing.revision > revision {
                return Err("输入框已有更新版本正在准备发送".to_string());
            }
            if existing.revision == revision {
                if existing.attachment_paths == attachment_paths {
                    return Ok(Vec::new());
                }
                return Err("同一输入版本的附件快照不一致".to_string());
            }
        }
        let replaced = claims
            .insert(
                session_id.to_string(),
                InputSendClaim {
                    revision,
                    attachment_paths,
                },
            )
            .map(|claim| claim.attachment_paths)
            .unwrap_or_default();
        Ok(replaced)
    }

    pub fn has_input_send_claim(&self, session_id: &str, revision: u64) -> bool {
        let claims = match self.input_send_claims.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        };
        claims
            .get(session_id)
            .is_some_and(|claim| claim.revision == revision)
    }

    pub fn release_input_send_claim(&self, session_id: &str, revision: u64) -> Vec<String> {
        let mut claims = match self.input_send_claims.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        };
        if claims
            .get(session_id)
            .is_some_and(|claim| claim.revision == revision)
        {
            return claims
                .remove(session_id)
                .map(|claim| claim.attachment_paths)
                .unwrap_or_default();
        }
        Vec::new()
    }

    pub fn release_any_input_send_claim(&self, session_id: &str) -> Vec<String> {
        let mut claims = match self.input_send_claims.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        };
        claims
            .remove(session_id)
            .map(|claim| claim.attachment_paths)
            .unwrap_or_default()
    }

    pub fn claimed_input_attachment_paths(&self) -> HashSet<String> {
        let claims = match self.input_send_claims.lock() {
            Ok(guard) => guard,
            Err(err) => err.into_inner(),
        };
        claims
            .values()
            .flat_map(|claim| claim.attachment_paths.iter().cloned())
            .collect()
    }

    pub fn input_cache_update_lock(&self, cache_key: &str) -> std::sync::Arc<AsyncMutex<()>> {
        let mut locks = match self.input_cache_update_locks.lock() {
            Ok(guard) => guard,
            Err(err) => {
                warn!(error = %err, "input_cache_update_locks 锁已污染，尝试恢复");
                err.into_inner()
            }
        };
        locks
            .entry(cache_key.to_string())
            .or_insert_with(|| std::sync::Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn lock_embedded_server(
        &self,
    ) -> std::sync::MutexGuard<'_, Option<tiangong_server::EmbeddedServerHandle>> {
        match self.embedded_server.lock() {
            Ok(guard) => guard,
            Err(err) => {
                warn!(error = %err, "embedded_server 锁已污染，尝试恢复");
                err.into_inner()
            }
        }
    }

    pub async fn sync_core_config_from_state(&self) -> Result<(), String> {
        // config registry 是唯一真相源（issue #245 整改方案）。
        // 不再从 app-state 反向同步到 registry——数据流单向：
        // tiangong-config → CoreConfig → CoreManager → Core
        let (app_config, agent_config) = self
            .with_state_read(|state| Ok((state.config.clone(), state.agent_config.clone())))
            .await?;
        let mut template = app_config.to_core_config();
        template.trust_mode = app_config.default_trust_mode;
        template.default_trust_mode = app_config.default_trust_mode;
        template.reasoning_effort = agent_config.reasoning_effort;
        let session_configs = self
            .core_manager
            .list_session_metadata()
            .iter()
            .map(|metadata| {
                let mut config = template.clone();
                config.trust_mode = metadata.trust_mode;
                config.reasoning_effort = metadata
                    .reasoning_effort
                    .unwrap_or(agent_config.reasoning_effort);
                (metadata.id.clone(), config)
            })
            .collect::<HashMap<_, _>>();
        // 总是热更存活 Core 的配置(replace_config + set_trust_mode)。
        // 能力集合变化时,存活 Core 的插件列表不变(构造时固定),
        // 但 endpoint/trust 等配置会热更——这是期望行为。
        self.core_manager.sync_config(template, &session_configs);
        Ok(())
    }

    pub async fn with_state<F, R>(&self, f: F) -> Result<R, String>
    where
        F: FnOnce(&mut tiangong_app_state::app_state::TiangongState) -> Result<R, anyhow::Error>,
    {
        let mut guard = self.state.lock().await;
        f(&mut guard).map_err(|e| e.to_string())
    }

    pub async fn with_state_read<F, R>(&self, f: F) -> Result<R, String>
    where
        F: FnOnce(&tiangong_app_state::app_state::TiangongState) -> Result<R, anyhow::Error>,
    {
        let guard = self.state.lock().await;
        f(&guard).map_err(|e| e.to_string())
    }

    /// 获取或创建会话对应的 TiangongCore。
    ///
    /// 已存在的 Core 直接复用并同步运行设置；不存在时调用 `create_core` 新建。
    ///
    /// Core 是会话级资源，空闲只表示当前没有 turn task，并不表示实例已经停止。
    /// 关闭和移除由删除会话、失败回滚等显式流程负责；此外 CoreManager 的空闲
    /// 回收会在 turn 结束后无交互达到保留时长（默认 1 小时）时自动销毁实例，
    /// 下次发送经本方法按需重建。
    /// 确保会话 Core 存在（issue #245：转走 core_manager）。
    ///
    /// 构建 per-session 配置 → 构造桌面插件集合 → 调
    /// `core_manager.ensure_core`。命中既有 Core 则刷新配置；否则新建。
    ///
    /// `initial_model_ref` 是本次发送携带的模型选择：新建 Core 时直接以该
    /// 模型初始化，避免新对话首条消息先按默认模型建 Core、投递时再触发一次
    /// 多余切换。为 `None` 时沿用 Session.model_ref / 路由默认。
    pub(crate) async fn ensure_core(
        &self,
        session_id: &str,
        workspace_dir: Option<String>,
        initial_trust_mode: Option<tiangong_types::TrustMode>,
        initial_reasoning_effort: Option<tiangong_llm::request::ReasoningEffort>,
        initial_model_ref: Option<&str>,
        stream_tx: std::sync::mpsc::Sender<tiangong_types::StreamEvent>,
    ) -> Result<tiangong_core_manager::EnsuredCore, String> {
        // 机制守卫（review S9，先于一切副作用）：中枢对话误经此入口会创建
        // 挂全插件的中枢 Core（破坏白名单隔离）——必须走 ensure_central_core。
        if crate::central::is_central_session(session_id) {
            return Err(format!(
                "中枢对话（{session_id}）必须经 ensure_central_core 构造，误入 ensure_core 已拦截"
            ));
        }
        self.wait_plugin_preload().await?;
        let (app_config, agent_config, default_workspace_dir) = self
            .with_state_read(|state| {
                Ok((
                    state.config.clone(),
                    state.agent_config.clone(),
                    state.workspace_dir.clone(),
                ))
            })
            .await
            .unwrap_or_default();
        let mut session_config = app_config.to_core_config();
        let existing_session = self.core_manager.load_session(session_id).ok();
        let workspace_dir = workspace_dir
            .filter(|cwd| !cwd.trim().is_empty())
            .unwrap_or(default_workspace_dir);
        if let Some(session) = existing_session {
            session_config.trust_mode = session.trust_mode;
            session_config.reasoning_effort = session
                .reasoning_effort
                .unwrap_or(agent_config.reasoning_effort);
        } else {
            session_config.trust_mode = initial_trust_mode.unwrap_or(app_config.default_trust_mode);
            session_config.reasoning_effort =
                initial_reasoning_effort.unwrap_or(agent_config.reasoning_effort);
        }
        // 桌面插件集合由 DesktopCoreFactory 构造（host 专属）。
        // 改为按需回调：只有 Core 不存在（需新建）时才会调用 build_plugins，
        // 避免每次发送消息都重复构造插件集合（桌面插件桥实例化）。
        //
        let factory = self.desktop_factory.clone();
        let models = app_config.models.clone();
        self.core_manager
            .ensure_core(
                session_id,
                session_config,
                workspace_dir,
                initial_model_ref,
                stream_tx,
                || factory.build_plugins_sync(session_id, models),
            )
            .await
    }

    // =========================================================================
    // 中枢对话 Core：直接构造与管理（不经 CoreManager）
    //
    // 中枢需要内置且唯一的中枢插件（crate::central::CentralPlugin：人设 +
    // §5.3 白名单工具集），CoreManager 的 runtime 聚合桥插件构造与会话
    // 编排不适用。中枢 Core 单实例常驻（进程生命周期），模型直接使用设置
    // 的主对话模型（§15.12；中枢 session.model_ref 优先）；压缩由 Core
    // 内部自治。
    // =========================================================================

    /// 测试构造：隔离 storage root 与工作区（host 集成测试用——避免读写
    /// 真实 ~/.tiangong；生产代码不得调用）。
    #[cfg(test)]
    pub(crate) fn new_for_test(storage_root: std::path::PathBuf, workspace_dir: String) -> Self {
        let config = tiangong_config::registry::init_from_dir(&storage_root);
        let core_config = config.to_core_config();
        let core_manager = tiangong_core_manager::CoreManager::new(
            tiangong_core::config::core::CoreConfigProvider::new(core_config.clone()),
            storage_root.clone(),
        );
        let core_state = tiangong_app_state::app_state::TiangongState {
            config,
            active_session_id: String::new(),
            workspace_dir: workspace_dir.clone(),
            input_caches: Default::default(),
            model_list: Vec::new(),
            agent_config: tiangong_core::config::agent::AgentConfig {
                trust_mode: core_config.trust_mode,
                default_trust_mode: core_config.default_trust_mode,
                reasoning_effort: core_config.reasoning_effort,
            },
            pending_turns: Default::default(),
            core_manager: core_manager.clone(),
        };
        let mut app = Self::new();
        app.state = std::sync::Arc::new(tokio::sync::Mutex::new(core_state));
        app.core_manager = core_manager;
        app.desktop_factory = std::sync::Arc::new(crate::core_factory::DesktopCoreFactory {
            app_handle: std::sync::Arc::new(std::sync::OnceLock::new()),
            config: tiangong_core::config::core::CoreConfigProvider::new(core_config),
            storage_root,
        });
        app
    }

    /// 插件集合/版本变化的变化点打标（runtime 插件变化事件的订阅者调用）：
    /// 给每个活跃会话标记待交接并后台立即处理（空闲当场压缩，忙留标记）。
    ///
    /// `fingerprint` 由事件携带（发布时刻快照），宿主不重算——回调内再取
    /// runtime 注册表锁可能与迁移路径的锁序冲突。不活跃会话（无 Core）
    /// 不打标：由首检兜底发现。
    pub fn mark_all_sessions_for_plugin_change(&self, fingerprint: &str) {
        crate::config_handoff::mark_all_sessions(
            &self.config_handoff_store,
            &self.core_manager,
            fingerprint,
        );
    }

    /// 自制插件能力变化：向全部活跃会话经注入通道下发最新清单。
    ///
    /// 分流判据（publisher == local）由订阅者检查；本方法只做投递。
    /// turn 活跃的会话经 `Command::InjectTool` 在工具批次收敛后的安全点
    /// 注入（PendingFinish 阶段自动转 NeedModel 重新分析——开发场景
    /// 「刚装完插件模型正要收尾」会重新考虑）；空闲会话本轮投递会因
    /// 无活跃 turn 被丢弃，由聚合桥的轮开始清单检查在下一条消息补上。
    pub fn broadcast_local_plugin_list(&self) {
        // 自制插件为空时不推送空清单（与轮开始检查一致）；清单有变化时照常推送。
        let empty = tiangong_plugin_runtime::registry::local_plugin_inventory()["plugins"]
            .as_array()
            .is_none_or(Vec::is_empty);
        if empty {
            return;
        }
        let session_ids: Vec<String> = self
            .core_manager
            .registry()
            .iter()
            .map(|(id, _)| id.to_string())
            .collect();
        for session_id in session_ids {
            let _ = self.tool_injection_tx.send(ToolInjection {
                session_id: Some(session_id),
                browser_source: None,
                snapshot: false,
                tool: Box::new(LocalPluginListInput),
            });
        }
    }

    /// 投递前的插件配置交接：待交接标记命中则执行，否则首检指纹兜底。
    ///
    /// 交接失败不阻断投递——消息照常发送，只是历史未经整理（失败已在
    /// `run_handoff` 内定档，不会反复重压）。会话忙时同样放行：本轮结束
    /// 后的下一条消息会在空闲窗口补做。
    async fn handoff_before_deliver(&self, session_id: &str) {
        use crate::config_handoff::ConfigHandoffOutcome;
        let outcome = crate::config_handoff::ensure_before_deliver(
            &self.config_handoff_store,
            &self.core_manager,
            session_id,
            execution_fingerprint,
        )
        .await;
        match outcome {
            ConfigHandoffOutcome::Aligned => {}
            ConfigHandoffOutcome::SkippedBusy => {
                tracing::debug!(session_id, "会话执行中，插件交接延后到下一条消息");
            }
            ConfigHandoffOutcome::Failed(reason) => {
                tracing::warn!(session_id, reason, "插件交接压缩未完成，上下文未整理即继续");
            }
        }
    }

    /// 用户消息投递前注入会话空闲期暂存的最新页面快照（见
    /// [`ToolInjection::snapshot`]）。调用方持有会话发送锁；快照来源页面
    /// 已关闭时丢弃。空闲 Core 会把注入放入会话延迟队列，随本条消息的
    /// 首次模型请求一并送达，此时队列中只有这一条最新快照。
    fn flush_pending_snapshot(&self, session_id: &str) {
        let injection = {
            let mut guard = self
                .pending_snapshots
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            guard.remove(session_id)
        };
        let Some(injection) = injection else {
            return;
        };
        let Some(app_handle) = self.app_handle.get() else {
            return;
        };
        if !browser_source_mounted(app_handle, injection.browser_source.as_ref()) {
            return;
        }
        if !self
            .core_manager
            .deliver_to_core_if_live(session_id, AgentInputKind::Tool(injection.tool))
        {
            tracing::debug!(session_id, "暂存页面快照投递失败（Core 不存在）");
        }
    }

    /// 向 Core 投递已准备好的用户消息（fire-and-forget，不等持久化确认）。
    ///
    /// app 层只做 host 专属的远端 turn 所有权检查与插件变化的上下文交接，
    /// 然后把消息交给 `CoreManager`：模型选择随消息携带（`with_model_ref`），
    /// 模型解析、切换前的上下文整理都是 Manager 投递路径的内部步骤。
    ///
    /// 插件变化的交接在此**同步完成后**再投递：调用方（发送/编辑重发/嵌入
    /// server）持有会话发送锁，交接与投递因此处在同一临界区内——既保证多条
    /// 消息的先后顺序，也让交接失败沿调用栈回报，不会出现「命令已返回但消息
    /// 还没送到」的中间态。
    ///
    /// 交接检查只挂在**用户消息投递**入口——ensure_core 的其余调用方
    /// （工具注入恢复、/clear、/compress、切换会话查看）不产生模型请求，
    /// 不应触发交接压缩。
    ///
    /// `model_ref` 为 `None` 表示跟随当前 Chat 默认模型。
    pub async fn deliver_prepared_if_live(
        &self,
        session_id: &str,
        message_id: String,
        prepared: Vec<tiangong_types::ContentBlock>,
        model_ref: Option<&str>,
        render: Option<tiangong_types::MessageRender>,
    ) -> Result<(), String> {
        if !self.remote_turn_allows_message(session_id, &message_id) {
            return Err("会话正在处理远端请求，拒绝插入其他用户消息".to_string());
        }
        self.handoff_before_deliver(session_id).await;
        self.flush_pending_snapshot(session_id);
        self.core_manager
            .deliver_user_message(
                session_id,
                AgentInputKind::prepared_with_id(message_id, prepared)
                    .with_model_ref(model_ref.map(str::to_string))
                    .with_render(render),
            )
            .await
    }

    /// 启动嵌入式 Server（共享 app 的 state 和 config）
    pub fn start_embedded_server(
        &self,
        host: &str,
        port: u16,
        token: Option<String>,
    ) -> Result<(), String> {
        let mut guard = self.lock_embedded_server();
        if guard.is_some() {
            return Err("Server 已在运行".to_string());
        }
        let app_handle = self
            .app_handle
            .get()
            .cloned()
            .ok_or_else(|| "Desktop 应用尚未完成初始化".to_string())?;
        let core_backend: std::sync::Arc<dyn tiangong_server::remote::backend::ServerCoreBackend> =
            crate::embedded_server::spawn_desktop_server_core_bridge(app_handle);
        let handle = tiangong_server::run_embedded(
            host,
            port,
            token,
            tiangong_server::EmbeddedServerDependencies {
                state: self.state.clone(),
                config: self.config.clone(),
                core_backend,
            },
        )
        .map_err(|e| e.to_string())?;
        *guard = Some(handle);
        Ok(())
    }

    /// 停止嵌入式 Server
    pub fn stop_embedded_server(&self) -> Result<(), String> {
        let mut guard = self.lock_embedded_server();
        if let Some(mut handle) = guard.take() {
            handle.stop();
            Ok(())
        } else {
            Err("Server 未在运行".to_string())
        }
    }

    /// 检查嵌入式 Server 是否在运行
    pub fn is_embedded_server_running(&self) -> bool {
        let guard = self.lock_embedded_server();
        guard.is_some()
    }
}

/// 插件自动升级一轮：拉取 OSS 目录获得各插件的可更新状态 →
/// 对已启用且有新版本的插件直接触发升级（复用手动升级的事务与
/// 回滚链路）→ 成功后广播 plugins_changed。禁用插件不动，可手动升级。
async fn run_plugin_auto_upgrade(app_handle: tauri::AppHandle) -> anyhow::Result<()> {
    use tauri::Emitter;
    use tiangong_plugin_runtime::artifacts;

    let state = app_handle.state::<TiangongApp>();
    let storage_root = state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await
        .map_err(|error| anyhow::anyhow!("读取存储根目录失败: {error}"))?;

    let repository = artifacts::PluginRepository::new()?;
    let available = match repository.list_available(&storage_root).await {
        Ok(list) => list,
        Err(error) => {
            // 离线或 OSS 不可达：静默跳过，下次启动再试。
            tracing::info!(%error, "插件目录拉取失败，跳过本次自动升级");
            return Ok(());
        }
    };
    let upgrades: Vec<String> = available
        .into_iter()
        .filter(|plugin| plugin.update_available && plugin.installed_enabled)
        .map(|plugin| plugin.id)
        .collect();
    if upgrades.is_empty() {
        tracing::debug!("插件无需自动升级");
        return Ok(());
    }
    tracing::info!(upgrades = ?upgrades, "开始插件自动升级");
    for plugin_id in &upgrades {
        match crate::commands::download_and_install_plugin(
            storage_root.clone(),
            plugin_id.clone(),
            app_handle.clone(),
        )
        .await
        {
            Ok(status) => {
                tracing::info!(
                    plugin_id,
                    version = %status.manifest_version,
                    "插件自动升级完成"
                );
                let _ = app_handle.emit("plugins_changed", &());
            }
            Err(error) => {
                tracing::warn!(plugin_id, %error, "插件自动升级失败，下次启动重试");
            }
        }
    }
    Ok(())
}

async fn run_launcher_auto_update(app_handle: tauri::AppHandle) {
    use tiangong_plugin_runtime::launcher_update::{AutoUpdateOutcome, LauncherUpdater};

    let state = app_handle.state::<TiangongApp>();
    let storage_root = match state
        .with_state_read(|core_state| Ok(core_state.config.storage_root.clone()))
        .await
    {
        Ok(root) => root,
        Err(error) => {
            tracing::warn!(%error, "读取存储根目录失败，跳过沙箱自动更新");
            return;
        }
    };
    match LauncherUpdater::new().update_if_newer(&storage_root).await {
        Ok(AutoUpdateOutcome::NotInstalled) => {
            tracing::info!("本地沙箱不可用，交由启动准备页安装，跳过自动更新");
        }
        Ok(AutoUpdateOutcome::UpToDate { current }) => {
            tracing::debug!(current, "沙箱已是最新版本");
        }
        Ok(AutoUpdateOutcome::Updated { previous, version }) => {
            tracing::info!(previous, version, "沙箱自动更新完成");
            tiangong_plugin_runtime::launcher_update::record_startup_prepare_failure(None);
        }
        Err(error) => {
            // 离线、清单不可达或替换失败：静默跳过，下次启动再试。
            tracing::warn!(error = %format!("{error:#}"), "沙箱自动更新失败，下次启动重试");
        }
    }
}

// =========================================================================
// 中枢派发执行器（自由函数）：中枢插件经 unbounded channel 提交后台工作
// 请求；执行器创建 Worker 会话（普通能力、全插件、指定模型）投递任务，
// 并在终态把结果注入中枢对话（事件驱动汇报的最小闭环）。
// =========================================================================

#[cfg(test)]
mod tests {
    use crate::central_host::dispatch::{
        build_worker_instruction, enqueue_central_dispatch, handle_central_dispatch,
        handle_work_terminal, plan_central_dispatch,
    };
    use crate::central_host::recovery::build_activation_summary;
    use crate::central_host::state_of;
    use central_state::{failure_looks_model_related, should_deliver_now, WorkOutcome};
    /// 回报时机判定：失败必达；成功仅在无其他在执行任务时触发汇总轮。
    #[test]
    fn work_report_timing_follows_design() {
        // 失败：无论是否还有任务在跑，立即汇报（关键节点必达）
        assert!(should_deliver_now(false, true));
        assert!(should_deliver_now(false, false));
        // 成功且仍有任务在执行：暂存等待汇总
        assert!(!should_deliver_now(true, true));
        // 成功且是最后一个：触发汇总轮
        assert!(should_deliver_now(true, false));
    }

    use super::*;
    use std::sync::atomic::Ordering;

    /// 模型/网络类失败识别：指导中枢换模型重派，而非把任务当失败关闭。
    #[test]
    fn model_related_failure_markers() {
        assert!(failure_looks_model_related(
            "请求超时（timeout after 300s）"
        ));
        assert!(failure_looks_model_related("429 Too Many Requests"));
        assert!(failure_looks_model_related("连接被对端重置"));
        assert!(failure_looks_model_related("API 返回 500"));
        assert!(failure_looks_model_related("internal server error"));
        assert!(!failure_looks_model_related("工作目录不存在：/tmp/missing"));
        assert!(!failure_looks_model_related("任务要求删除文件被拒绝"));
        // review：判定过宽——词边界收紧后的负例
        assert!(!failure_looks_model_related("已扫描 500 个文件，全部通过"));
        assert!(!failure_looks_model_related(
            "文件共 40110 行，超出行数预算"
        ));
        // UTF-8 安全（review P1：中文语境 + 状态码不得 panic，语境词命中）
        assert!(failure_looks_model_related("接口超时错误码429请稍后重试"));
        assert!(failure_looks_model_related(
            "上游服务器错误：500 服务不可用，任务终止"
        ));
        assert!(!failure_looks_model_related(
            "仓库共 500 个模块，已全部索引完成"
        ));
    }

    /// Worker 指令：决策协议（对话式）——禁交互工具 + 标记格式 + 答复回流。
    #[test]
    fn worker_instruction_carries_decision_protocol() {
        let instruction = build_worker_instruction("work-abc", "执行数据库迁移");
        assert!(instruction.contains("work-abc"));
        assert!(instruction.contains("执行数据库迁移"));
        assert!(instruction.contains("不要调用 request_user"));
        assert!(instruction.contains("【需要用户决策】"));
        assert!(instruction.contains("以下一条消息回到本会话"));
        assert!(instruction.contains("不要虚构结果"));
    }

    /// 阻塞识别：含【需要用户决策】→ WaitingUser（未终态），其余终态。
    #[test]
    fn work_outcome_classifies_waiting_user() {
        assert_eq!(
            central_state::classify_outcome("[后台工作完成] work-1\n结论：一切正常"),
            WorkOutcome::Finished
        );
        assert_eq!(
            central_state::classify_outcome("[后台工作失败] work-2\n失败原因：超时"),
            WorkOutcome::Finished
        );
        assert_eq!(
            central_state::classify_outcome(
                "[后台工作完成] work-3\n【需要用户决策】\n问题：是否继续？"
            ),
            WorkOutcome::WaitingUser
        );
    }

    #[test]
    fn stash_snapshot_keeps_only_latest_per_session() {
        let pending = Mutex::new(HashMap::new());
        let make = |tab: &str| ToolInjection {
            session_id: Some("s1".to_string()),
            browser_source: Some(("s1".to_string(), tab.to_string())),
            snapshot: true,
            tool: Box::new(LocalPluginListInput),
        };
        stash_snapshot(&pending, "s1".to_string(), make("tab-a"));
        stash_snapshot(&pending, "s1".to_string(), make("tab-b"));
        stash_snapshot(&pending, "s2".to_string(), make("tab-c"));

        let guard = pending.lock().unwrap();
        assert_eq!(guard.len(), 2);
        assert_eq!(
            guard["s1"]
                .browser_source
                .as_ref()
                .map(|(_, tab)| tab.as_str()),
            Some("tab-b")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn plugin_preload_wait_is_async_and_preserves_failure() {
        let app = TiangongApp::new();
        app.plugin_preload.send_replace(None);
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            app.wait_plugin_preload()
        )
        .await
        .is_err());
        app.plugin_preload.send_replace(Some(Ok(
            tiangong_plugin_runtime::registry::StartupPluginReadiness::default(),
        )));
        assert!(app.wait_plugin_preload().await.is_ok());
        app.plugin_preload
            .send_replace(Some(Err("preload failed".into())));
        assert_eq!(
            app.wait_plugin_preload().await.unwrap_err(),
            "preload failed"
        );
    }

    #[tokio::test]
    async fn same_session_core_creation_uses_one_serial_boundary() {
        // CoreManager.creation_lock 替代了原 CoreCreationLocks(issue #245)。
        use tiangong_core::config::core::{CoreConfig, CoreConfigProvider};
        let manager = tiangong_core_manager::CoreManager::new(
            CoreConfigProvider::new(CoreConfig::default()),
            std::path::PathBuf::from("/tmp"),
        );
        let first = manager.creation_lock("session-1");
        let second = manager.creation_lock("session-1");
        let other_session = manager.creation_lock("session-2");

        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert!(!std::sync::Arc::ptr_eq(&first, &other_session));

        let first_guard = first.lock_owned().await;
        let entered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let entered_in_task = std::sync::Arc::clone(&entered);
        let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
        let mut waiter = tokio::spawn(async move {
            let _ = waiting_tx.send(());
            let _guard = second.lock_owned().await;
            entered_in_task.store(true, Ordering::Release);
        });

        waiting_rx.await.expect("后继创建应开始等待同一把锁");
        tokio::task::yield_now().await;
        assert!(!entered.load(Ordering::Acquire));
        drop(first_guard);

        tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter)
            .await
            .expect("同会话后继创建应在前一创建离开后继续")
            .expect("创建锁等待任务不应失败");
        assert!(entered.load(Ordering::Acquire));
    }

    /// 空闲 Core 仍是可复用的会话级资源，只有从映射移除后才不再可投递。
    #[tokio::test]
    async fn has_live_core_tracks_core_instance_presence() {
        use std::sync::mpsc;
        use tiangong_core::config::core::{CoreConfig, CoreConfigProvider};

        let app = TiangongApp::new();
        let session_id = "session-lifecycle-test";
        assert!(!app.core_manager.has_live_core(session_id), "初始无 Core");

        let session = tiangong_core::session::Session::new(session_id);
        let (event_tx, _event_rx) = mpsc::channel();
        let storage_root = tempfile::tempdir().unwrap();
        let core = tiangong_core::core::TiangongCore::builder()
            .config(CoreConfigProvider::new(CoreConfig::default()))
            .session_id(session.id)
            .stream_tx(event_tx)
            .plugins(Vec::new())
            .storage_root(storage_root.path())
            .workspace_dir(storage_root.path().to_string_lossy())
            .trust_mode(session.trust_mode)
            .model_endpoint(tiangong_llm::ModelEndpoint {
                base_url: "http://test.invalid".to_string(),
                api_key: "test-key".to_string(),
                model: "test-model".to_string(),
                ..Default::default()
            })
            .build();
        assert!(core.is_stopped(), "新 Core 当前没有活跃 turn");
        // 直接经 core_manager registry 插入(issue #245:不再有 TiangongApp.lock_cores)。
        app.core_manager
            .registry()
            .insert(session_id.to_string(), core);

        assert!(
            app.core_manager.has_live_core(session_id),
            "空闲 Core 仍应可投递新消息"
        );
        let core = app
            .core_manager
            .take_core(session_id)
            .expect("应能取回 Core");
        tokio::task::spawn_blocking(move || core.shutdown_join())
            .await
            .unwrap()
            .unwrap();
        assert!(!app.core_manager.has_live_core(session_id), "移除后无 Core");
    }

    #[tokio::test]
    async fn remote_turn_ownership_serializes_until_exact_owner_finishes() {
        let app = std::sync::Arc::new(TiangongApp::new());
        app.begin_remote_turn("session-1", "message-1").unwrap();
        assert!(app.remote_turn_allows_message("session-1", "message-1"));
        assert!(!app.remote_turn_allows_message("session-1", "message-2"));

        let waiting_app = app.clone();
        let mut waiting = tokio::spawn(async move {
            waiting_app.wait_for_remote_turn_release("session-1").await;
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "所有者释放前同会话请求必须等待");

        app.finish_remote_turn("session-1", "wrong-message");
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "错误消息 ID 不能释放其他轮次");
        app.finish_remote_turn("session-1", "message-1");
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiting)
            .await
            .expect("正确所有者释放后等待方应继续")
            .expect("等待任务不应失败");
    }
    /// S9：ensure_core 拒绝中枢会话（白名单隔离的机制守卫，先于副作用）。
    #[tokio::test]
    async fn ensure_core_rejects_central_session_entry() {
        let app = TiangongApp::new();
        let (_tx, _rx) = std::sync::mpsc::channel();
        let result = app
            .ensure_core(
                crate::central::CENTRAL_SESSION_ID,
                None,
                None,
                None,
                None,
                _tx,
            )
            .await;
        let error = result.expect_err("中枢会话经 ensure_core 必须被拒绝");
        assert!(
            error.contains("ensure_central_core"),
            "错误信息须指向正确入口：{error}"
        );
        // 守卫先于副作用：未创建任何 Core
        assert!(
            !app.core_manager
                .has_live_core(crate::central::CENTRAL_SESSION_ID),
            "拦截必须发生在 Core 创建之前"
        );
    }
    /// 复用派发挂载的终态回调经 fire 链真实触发（挂载 → take → 执行 →
    /// drain 一次性），并覆盖 S6 一次性守卫与 T6 归属过滤的可构造部分。
    /// R1 执行路由（取代槽测试）：register → attach → fire 恰好触发一次，
    /// drain 一次性（重复 fire 不重复触发），并覆盖 S6 一次性守卫与
    /// 执行归属过滤的可构造部分。
    #[test]
    fn dispatch_reuse_fires_live_slot_callback() {
        let app = TiangongApp::new();
        let session = "s-live";
        // registry 预置：该会话当前条目即本执行（归属判定通过）
        app.central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(crate::central::CentralWorkStatus {
                work_id: "work-live".to_string(),
                session_id: session.to_string(),
                task: "复用任务".to_string(),
                model: None,
                dispatched_at: "2026-11-11 18:00:00".to_string(),
                state: central_state::WorkState::Running,
                reused_session: true,
                last_activity: None,
                cwd: None,
                execution_id: Some("exec-live".to_string()),
            });
        // 挂载（与 attach_work_finished_callback 同款闭包语义，去掉 app 依赖）
        {
            let fired = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let fired_clone = std::sync::Arc::clone(&fired);
            app.execution_router.register(session, "exec-live");
            app.execution_router.attach(
                session,
                "exec-live",
                Box::new(move |done: bool, reason: String| {
                    let mut log = fired_clone.lock().unwrap();
                    log.push(format!("work-live:{done}:{reason}"));
                }),
            );
            // fire（流消费者终态路径等价）：路由给活跃执行的回调
            app.execution_router.fire(session, true, String::new());
            assert_eq!(*fired.lock().unwrap(), vec!["work-live:true:".to_string()]);
            // drain 一次性：再 fire 无回调（重复终态不重复触发）
            app.execution_router.fire(session, true, String::new());
            assert_eq!(
                fired.lock().unwrap().len(),
                1,
                "fire 后回调必须已消费（一次性）"
            );
        }
        // 状态驱动幂等（P1-1 修正后的语义）：在途 → 可处理；已终态 → 跳过
        {
            let mut registry = app
                .central_work_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let entry = registry
                .iter_mut()
                .rev()
                .find(|e| e.work_id == "work-live")
                .unwrap();
            assert!(entry.state.is_inflight(), "running 在途可处理终态");
            entry.state = central_state::WorkState::ReportQueued;
            assert!(
                !entry.state.is_inflight(),
                "已终态的重复回调必须可被守卫识别跳过"
            );
        }
    }
    /// 激活摘要（§4.2）：有事实按事实组装（等待决策/中断/失败优先），
    /// 空闲态给能力介绍指引，不编造状态。
    #[test]
    fn activation_summary_builds_from_verifiable_facts() {
        let entries = vec![
            crate::central::CentralWorkStatus {
                work_id: "w1".into(),
                session_id: "s1".into(),
                task: "review 仓库".into(),
                model: None,
                dispatched_at: "2026-11-12 10:00:00".into(),
                state: central_state::WorkState::WaitingUser,
                reused_session: false,
                last_activity: None,
                cwd: None,
                execution_id: None,
            },
            crate::central::CentralWorkStatus {
                work_id: "w2".into(),
                session_id: "s2".into(),
                task: "调研".into(),
                model: None,
                dispatched_at: "2026-11-12 09:00:00".into(),
                state: central_state::WorkState::Interrupted,
                reused_session: false,
                last_activity: None,
                cwd: None,
                execution_id: None,
            },
        ];
        // R5 用户实测修正：只有需要用户动作的事实才注入（必达），
        // 且措辞自然（不说工具数/状态清单/能力介绍）
        let summary = build_activation_summary(&entries, &[]).expect("有等待决策/中断事实应注入");
        assert!(summary.contains("1 个工作在等你决策"), "{summary}");
        assert!(summary.contains("review 仓库"), "{summary}");
        assert!(summary.contains("1 个工作中断"), "{summary}");
        assert!(summary.contains("一两句自然"), "{summary}");
        assert!(!summary.contains("介绍你能做什么"), "{summary}");
        // 空闲与纯执行态：完全静默（适时沉默——机械问候无价值）
        assert!(build_activation_summary(&[], &[]).is_none(), "空闲不注入");
        let running_only = vec![crate::central::CentralWorkStatus {
            work_id: "w-run".into(),
            session_id: "s-run".into(),
            task: "跑测试".into(),
            model: None,
            dispatched_at: "2026-11-12 10:00:00".into(),
            state: central_state::WorkState::Running,
            reused_session: false,
            last_activity: None,
            cwd: None,
            execution_id: None,
        }];
        assert!(
            build_activation_summary(&running_only, &[]).is_none(),
            "纯执行态不注入（徽章可见，不打扰）"
        );
        // 有需用户动作的事实时，相关目标一并提供
        let goals = vec![crate::central::GoalRecord {
            goal_id: "g1".into(),
            title: "完成中枢模式".into(),
            description: String::new(),
            workspace: None,
            created_at: "2026-11-12".into(),
            status: "active".into(),
        }];
        let with_goal = build_activation_summary(&entries, &goals).expect("有事实时应注入");
        assert!(with_goal.contains("完成中枢模式"), "{with_goal}");
    }
    // ============================================================================
    // host 集成测试（W5，第十四轮 review 硬要求）：真实调用被修函数
    // （attach_work_finished_callback / execution_router /
    // on_central_work_finished / enqueue_central_dispatch），经 mock app +
    // 隔离 storage——「旧代码会红」的反向验证见各测试注释。
    // ============================================================================

    /// 构造隔离 storage 的 TiangongApp（emit 经未注入句柄静默跳过——host
    /// 集成测试无需 Wry 环境）。
    fn central_test_app() -> (TiangongApp, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ws_path = ws.clone();
        (
            TiangongApp::new_for_test(root.keep(), ws.to_string_lossy().to_string()),
            ws_path,
        )
    }

    fn dispatch_request(
        work_id: &str,
        session: &str,
        task: &str,
        reused: bool,
    ) -> crate::central::CentralDispatchRequest {
        crate::central::CentralDispatchRequest {
            work_id: work_id.to_string(),
            session_id: session.to_string(),
            task: task.to_string(),
            model: None,
            cwd: None,
            after_work_id: None,
            parallel: false,
            reused_session: reused,
            execution_id: "exec-test".to_string(),
        }
    }

    /// W5-T1（P1-1 真锁·第二 turn 回报）**真两 turn**：第一 turn（waiting_user
    /// 分流）→ 模拟答复恢复（registry 回 running）→ 第二 turn 终态。
    /// 反向验证留痕（第十五轮 review 要求）：恢复 central_finished_once 集合版，
    /// 第二次 insert(work_id) 返回 false → 回调 return → registry 保持
    /// running → `assert_eq!(entry.state, ReportQueued)` 红——已实际执行验证。
    #[tokio::test]
    async fn host_second_turn_after_resume_is_reported() {
        let (state, _ws) = central_test_app();
        let request = dispatch_request("work-t1", "s-t1", "任务一", true);
        // 会话文件：最后 assistant 回复含决策标记（build_work_notice 读取的
        // 真实数据源——标记必须出现在 Worker 最终答复里，不是 reason 参数）
        let sessions_dir = state.core_manager.storage_root().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("s-t1.json"),
            serde_json::json!({"id": "s-t1", "title": "会话", "created_at": "2026-11-12 12:00:00",
            "updated_at": "2026-11-12 12:00:00", "trust_mode": "supervised",
            "cwd_mode": "inherit", "cwd": _ws.to_string_lossy(), "messages": [
                {"id": "m1", "role": "assistant", "content": [{"type": "text",
                 "text": "【需要用户决策】\n问题：继续吗？"}], "created_at": "2026-11-12 12:00:00"}
            ]})
            .to_string(),
        )
        .unwrap();
        state
            .central_work_registry
            .lock()
            .unwrap()
            .push(crate::central::CentralWorkStatus {
                work_id: "work-t1".into(),
                session_id: "s-t1".into(),
                task: "任务一".into(),
                model: None,
                dispatched_at: "2026-11-12 12:00:00".into(),
                state: central_state::WorkState::Running,
                reused_session: true,
                last_activity: None,
                cwd: None,
                execution_id: None,
            });
        // 第一 turn：回报含决策标记 → waiting_user（未终态）
        handle_work_terminal(&state, &request, true, "");
        {
            let registry = state.central_work_registry.lock().unwrap();
            let entry = registry
                .iter()
                .rev()
                .find(|e| e.work_id == "work-t1")
                .unwrap();
            assert_eq!(
                entry.state,
                central_state::WorkState::WaitingUser,
                "第一 turn 分流 waiting_user"
            );
        }
        // 模拟答复送达（merged 路径把 registry 恢复 running）
        {
            let mut registry = state.central_work_registry.lock().unwrap();
            let entry = registry
                .iter_mut()
                .rev()
                .find(|e| e.work_id == "work-t1")
                .unwrap();
            entry.state = central_state::WorkState::Running;
        }
        // 第二 turn 前置：Worker 产出新回复（无标记——答复后正常完成），
        // 覆盖会话最后的 assistant 消息（真实链路的数据变化）
        std::fs::write(
            sessions_dir.join("s-t1.json"),
            serde_json::json!({"id": "s-t1", "title": "会话", "created_at": "2026-11-12 12:00:00",
            "updated_at": "2026-11-12 12:05:00", "trust_mode": "supervised",
            "cwd_mode": "inherit", "cwd": _ws.to_string_lossy(), "messages": [
                {"id": "m1", "role": "assistant", "content": [{"type": "text",
                 "text": "已按用户决定继续执行，任务完成。"}], "created_at": "2026-11-12 12:05:00"}
            ]})
            .to_string(),
        )
        .unwrap();
        // 第二 turn：终态必须被处理（旧集合版在此 return → 红）
        handle_work_terminal(&state, &request, true, "");
        {
            let registry = state.central_work_registry.lock().unwrap();
            let entry = registry
                .iter()
                .rev()
                .find(|e| e.work_id == "work-t1")
                .unwrap();
            assert_eq!(
                entry.state,
                central_state::WorkState::ReportQueued,
                "第二 turn 终态必须被处理（旧 central_finished_once 代码在此红）"
            );
        }
        let reports = state.central_work_reports.lock().unwrap().clone();
        assert!(
            reports.len() >= 2,
            "两 turn 的回报都入队（旧代码第二次丢在此红）"
        );
    }

    /// 旧 owner 回调被 T6 跳过；归属保持时正常执行——两条路径都验证。
    #[tokio::test]
    async fn host_owner_callback_respects_ownership_filter() {
        let (state, _ws) = central_test_app();
        // 路径 A：归属保持（owner 仍是当前条目）→ owner 回调执行
        let owner = dispatch_request("work-owner", "s-own", "原工作", true);
        state
            .central_work_registry
            .lock()
            .unwrap()
            .push(crate::central::CentralWorkStatus {
                work_id: "work-owner".into(),
                session_id: "s-own".into(),
                task: "原工作".into(),
                model: None,
                dispatched_at: "2026-11-12 12:00:00".into(),
                state: central_state::WorkState::Running,
                reused_session: false,
                last_activity: None,
                cwd: None,
                execution_id: None,
            });
        handle_work_terminal(&state, &owner, true, "");
        {
            let registry = state.central_work_registry.lock().unwrap();
            let entry = registry
                .iter()
                .rev()
                .find(|e| e.work_id == "work-owner")
                .unwrap();
            assert_eq!(
                entry.state,
                central_state::WorkState::ReportQueued,
                "归属保持时终态被处理"
            );
        }
        // 路径 B：归属被 steer 覆盖（旧行为模拟）→ owner 的迟到回调被跳过
        let mut registry = state.central_work_registry.lock().unwrap();
        let entry = registry
            .iter_mut()
            .rev()
            .find(|e| e.session_id == "s-own")
            .unwrap();
        entry.work_id = "steer-xxx".into(); // 模拟旧代码的 upsert 覆盖
        entry.state = central_state::WorkState::Running;
        drop(registry);
        let stale_owner = dispatch_request("work-owner", "s-own", "原工作", true);
        let before = state.central_work_reports.lock().unwrap().clone();
        handle_work_terminal(&state, &stale_owner, true, "");
        let after = state.central_work_reports.lock().unwrap().clone();
        assert_eq!(
            before.len(),
            after.len(),
            "归属已被覆盖时旧 owner 回调必须被 T6 跳过（不重复回报）"
        );
    }

    /// 保持 Running；会话空闲时排队条目正常登记 Queued。
    #[tokio::test]
    async fn host_enqueue_preserves_running_entry() {
        let (state, _ws) = central_test_app();
        // 会话空闲 → 排队登记 Queued
        let queued = dispatch_request("work-q1", "s-q1", "排队任务", false);
        enqueue_central_dispatch(&state, queued, "work-dep", "/ws").await;
        {
            let registry = state.central_work_registry.lock().unwrap();
            let entry = registry
                .iter()
                .rev()
                .find(|e| e.session_id == "s-q1")
                .unwrap();
            assert_eq!(
                entry.state,
                central_state::WorkState::Queued,
                "空闲会话排队正常登记"
            );
        }
        // 同会话在跑 → 排队不覆盖（W4：旧代码 retain+push 会改成 Queued）
        state
            .central_work_registry
            .lock()
            .unwrap()
            .push(crate::central::CentralWorkStatus {
                work_id: "work-run".into(),
                session_id: "s-run".into(),
                task: "在跑工作".into(),
                model: None,
                dispatched_at: "2026-11-12 12:00:00".into(),
                state: central_state::WorkState::Running,
                reused_session: false,
                last_activity: None,
                cwd: None,
                execution_id: None,
            });
        let queued2 = dispatch_request("work-q2", "s-run", "第二个排队", false);
        enqueue_central_dispatch(&state, queued2, "work-dep", "/ws").await;
        {
            let registry = state.central_work_registry.lock().unwrap();
            let running = registry
                .iter()
                .rev()
                .find(|e| e.work_id == "work-run")
                .unwrap();
            assert_eq!(
                running.state,
                central_state::WorkState::Running,
                "在跑工作不得被排队登记覆盖（W4 旧代码在此红）"
            );
        }
        // pending 队列两条都在
        let pending = state.central_pending_dispatches.lock().unwrap().clone();
        assert_eq!(pending.len(), 2, "排队请求必须入挂起队列");
    }

    /// 改成终态。
    #[tokio::test]
    async fn host_terminal_fallback_skips_queued_entry() {
        let (state, _ws) = central_test_app();
        state
            .central_work_registry
            .lock()
            .unwrap()
            .push(crate::central::CentralWorkStatus {
                work_id: "work-q9".into(),
                session_id: "s-mix".into(),
                task: "排队条目".into(),
                model: None,
                dispatched_at: "2026-11-12 12:00:00".into(),
                state: central_state::WorkState::Queued,
                reused_session: false,
                last_activity: None,
                cwd: None,
                execution_id: None,
            });
        // 未知 work_id + 命中 session 的排队条目 → 兜底不得写终态
        let request = dispatch_request("work-unknown", "s-mix", "未知工作", false);
        let notice = "[后台工作失败] work-unknown\n失败原因：超时".to_string();
        state.on_central_work_finished(&request, false, &notice);
        {
            let registry = state.central_work_registry.lock().unwrap();
            let entry = registry
                .iter()
                .rev()
                .find(|e| e.session_id == "s-mix")
                .unwrap();
            assert_eq!(
                entry.state,
                central_state::WorkState::Queued,
                "排队条目不得被 session 兜底串态（W4-b 旧代码在此红）"
            );
        }
    }

    /// 反向验证：把判定改回 is_inflight（R1 旧代码）→ 本测试红。
    #[tokio::test]
    async fn host_plan_resume_not_merged_but_steer_is() {
        let (state, ws) = central_test_app();
        // 会话元数据（self_session_cwd 与确认校验需要）
        let sessions_dir = state.core_manager.storage_root().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
        sessions_dir.join("s-resume.json"),
        serde_json::json!({"id": "s-resume", "title": "暂停的会话", "created_at": "2026-11-12 10:00:00",
            "updated_at": "2026-11-12 10:00:00", "trust_mode": "Supervised",
            "cwd": ws.to_string_lossy(), "messages": []}).to_string(),
    ).unwrap();
        // Paused 条目：resume 请求 → plan 判定非并入（恢复语义）
        state
            .central_work_registry
            .lock()
            .unwrap()
            .push(crate::central::CentralWorkStatus {
                work_id: "work-p".into(),
                session_id: "s-resume".into(),
                task: "暂停任务".into(),
                model: None,
                dispatched_at: "2026-11-12 10:00:00".into(),
                state: central_state::WorkState::Paused,
                reused_session: true,
                last_activity: None,
                cwd: Some(ws.to_string_lossy().to_string()),
                execution_id: None,
            });
        let resume = dispatch_request("work-p", "s-resume", "继续暂停的任务", true);
        let plan = plan_central_dispatch(&state, resume)
            .await
            .expect("plan 应成功");
        let steer_merged = match plan {
            crate::central_host::dispatch::DispatchPlan::Execute {
                steer_merged_into_running,
                ..
            } => steer_merged_into_running,
            crate::central_host::dispatch::DispatchPlan::Queued => {
                panic!("Paused 的 resume 不应排队")
            }
        };
        assert!(
            !steer_merged,
            "Paused 的 resume 不得并入（R1 旧代码 is_inflight 在此红）"
        );
        // Running 条目：steer 判为并入
        {
            let mut registry = state.central_work_registry.lock().unwrap();
            let entry = registry
                .iter_mut()
                .rev()
                .find(|e| e.session_id == "s-resume")
                .unwrap();
            entry.state = central_state::WorkState::Running;
        }
        let steer = dispatch_request("steer-x", "s-resume", "补充指令", true);
        let plan2 = plan_central_dispatch(&state, steer)
            .await
            .expect("plan 应成功");
        let steer_merged2 = match plan2 {
            crate::central_host::dispatch::DispatchPlan::Execute {
                steer_merged_into_running,
                ..
            } => steer_merged_into_running,
            crate::central_host::dispatch::DispatchPlan::Queued => {
                panic!("Running 会话的引导不应排队")
            }
        };
        assert!(steer_merged2, "Running 会话的引导应并入（归属保持原工作）");
    }

    /// 语义且无测试暴露）。依赖在途 → plan 必须返回 Queued 并完成登记。
    #[tokio::test]
    async fn host_plan_queue_decision_returns_queued() {
        let (state, ws) = central_test_app();
        let sessions_dir = state.core_manager.storage_root().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
        sessions_dir.join("s-dep.json"),
        serde_json::json!({"id": "s-dep", "title": "依赖会话", "created_at": "2026-11-12 10:00:00",
            "updated_at": "2026-11-12 10:00:00", "trust_mode": "Supervised",
            "cwd": ws.to_string_lossy(), "messages": []}).to_string(),
    ).unwrap();
        // 依赖在途
        state
            .central_work_registry
            .lock()
            .unwrap()
            .push(crate::central::CentralWorkStatus {
                work_id: "work-dep".into(),
                session_id: "s-dep".into(),
                task: "在跑依赖".into(),
                model: None,
                dispatched_at: "2026-11-12 10:00:00".into(),
                state: central_state::WorkState::Running,
                reused_session: false,
                last_activity: None,
                cwd: Some(ws.to_string_lossy().to_string()),
                execution_id: None,
            });
        let follow = crate::central::CentralDispatchRequest {
            work_id: "work-follow".into(),
            session_id: "s-new-follow".into(),
            task: "排队任务".into(),
            model: None,
            cwd: Some(ws.to_string_lossy().to_string()),
            after_work_id: Some("work-dep".into()),
            parallel: false,
            reused_session: false,
            execution_id: "exec-test".to_string(),
        };
        let plan = plan_central_dispatch(&state, follow)
            .await
            .expect("plan 应成功");
        assert!(
            matches!(plan, crate::central_host::dispatch::DispatchPlan::Queued),
            "依赖在途时必须返回 Queued（执行段不投递）"
        );
        // 排队登记与挂起队列已就绪
        let pending = state.central_pending_dispatches.lock().unwrap().clone();
        assert_eq!(pending.len(), 1, "排队请求必须入挂起队列");
    }
    // ============================================================================
    // 执行段集成测试（第十五轮 review 最小集）：真走 handle_central_dispatch
    //（mock_app 泛型 handle + 隔离 storage——WASM 插件早已淘汰，插件桥在
    // 测试环境可构造；preload 默认完成态不阻塞 ensure）。
    // ============================================================================

    /// P0 锁（执行段级）：排队决策 → dispatch 不得投递（无 Core 创建、无
    /// 回调挂载），挂起队列登记完整。
    #[tokio::test]
    async fn host_dispatch_queued_does_not_deliver() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let app = tauri::test::mock_app();
        let state_arc = {
            // manage 需要 TiangongApp 值；另取一份引用语义：manage 后用
            // handle 经 state_of 取回。
            app.manage(TiangongApp::new_for_test(
                root.keep(),
                ws.to_string_lossy().to_string(),
            ));
            app.handle()
        };
        std::fs::write(
            state_of(state_arc)
                .core_manager
                .storage_root()
                .join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let state = state_of(state_arc);
        // 依赖在途（另一会话）
        state
            .central_work_registry
            .lock()
            .unwrap()
            .push(crate::central::CentralWorkStatus {
                work_id: "work-dep".into(),
                session_id: "s-dep".into(),
                task: "依赖".into(),
                model: None,
                dispatched_at: "2026-11-12 10:00:00".into(),
                state: central_state::WorkState::Running,
                reused_session: false,
                last_activity: None,
                cwd: Some(ws.to_string_lossy().to_string()),
                execution_id: None,
            });
        let follow = crate::central::CentralDispatchRequest {
            work_id: "work-f".into(),
            session_id: "s-follow".into(),
            task: "后续".into(),
            model: None,
            cwd: Some(ws.to_string_lossy().to_string()),
            after_work_id: Some("work-dep".into()),
            parallel: false,
            reused_session: false,
            execution_id: "exec-test".to_string(),
        };
        handle_central_dispatch(state, state_arc, follow)
            .await
            .expect("排队 dispatch 应成功");
        // 未投递：无 Core、无回调、无 running 登记；挂起队列有
        assert!(
            !state.core_manager.has_live_core("s-follow"),
            "排队不得创建 Core"
        );
        assert!(
            state.execution_router.pending_handler_count("s-follow") == 0,
            "排队不得挂回调"
        );
        assert!(!state
            .central_running_works
            .lock()
            .unwrap()
            .contains("work-f"));
        assert_eq!(state.central_pending_dispatches.lock().unwrap().len(), 1);
        let registry = state.central_work_registry.lock().unwrap();
        let entry = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-f")
            .unwrap();
        assert_eq!(entry.state, central_state::WorkState::Queued);
    }

    /// W2 锁（执行段级）：steer 并入不改 registry 归属——先经 dispatch 建立
    /// owner（Core 活跃），再 dispatch steer，断言归属保持 + 回调只挂 owner。
    #[tokio::test]
    async fn host_steer_keeps_owner_registry() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let app = tauri::test::mock_app();
        let state = {
            let s = TiangongApp::new_for_test(root.keep(), ws.to_string_lossy().to_string());
            // 测试模型配置（新会话 ensure 的模型解析需要）
            std::fs::write(
            s.core_manager.storage_root().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            }).to_string(),
        ).unwrap();
            s
        };
        app.manage(state);
        let handle = app.handle();
        let state = state_of(handle);
        // 第一次 dispatch：新会话 owner（真建 Core + 投递 + 挂回调）
        let owner = crate::central::CentralDispatchRequest {
            work_id: "work-owner".into(),
            session_id: "s-own".into(),
            task: "原工作".into(),
            model: None,
            cwd: Some(ws.to_string_lossy().to_string()),
            after_work_id: None,
            parallel: false,
            reused_session: false,
            execution_id: "exec-test".to_string(),
        };
        handle_central_dispatch(state, handle, owner)
            .await
            .expect("owner dispatch 应成功");
        assert!(state.core_manager.has_live_core("s-own"), "owner 应建 Core");
        // 第二次 dispatch：steer（reused 同会话）——Running 并入
        let steer = crate::central::CentralDispatchRequest {
            work_id: "steer-x".into(),
            session_id: "s-own".into(),
            task: "补充指令".into(),
            model: None,
            cwd: None,
            after_work_id: None,
            parallel: false,
            reused_session: true,
            execution_id: "exec-test".to_string(),
        };
        handle_central_dispatch(state, handle, steer)
            .await
            .expect("steer dispatch 应成功");
        // 归属保持 owner（W2：旧代码 steer upsert 覆盖 → 此断言红）
        {
            let registry = state.central_work_registry.lock().unwrap();
            let entry = registry
                .iter()
                .rev()
                .find(|e| e.session_id == "s-own")
                .unwrap();
            assert_eq!(
                entry.work_id, "work-owner",
                "steer 并入不得覆盖归属（W2 旧代码红）"
            );
            assert_eq!(entry.state, central_state::WorkState::Running);
        }
        // 回调只挂 owner（steer 不挂）
        let handler_count = state.execution_router.pending_handler_count("s-own");
        assert_eq!(handler_count, 1, "只挂 owner 回调（steer 并入不挂）");
    }

    /// W3 锁（执行段级）：排队重放（cwd 还原）→ 投递成功清除 queued 标记。
    #[tokio::test]
    async fn host_replay_clears_marker_restores_cwd() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let app = tauri::test::mock_app();
        let state = {
            let s = TiangongApp::new_for_test(root.keep(), ws.to_string_lossy().to_string());
            std::fs::write(
            s.core_manager.storage_root().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            }).to_string(),
        ).unwrap();
            s
        };
        app.manage(state);
        let handle = app.handle();
        let state = state_of(handle);
        // 落盘排队记录（含 cwd——重放数据源）
        let record = crate::central::WorkItemRecord {
            work_id: "work-replay".into(),
            reused_session: false,
            session_id: "s-replay".into(),
            task: "重放任务".into(),
            model: None,
            dispatched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            finished_at: None,
            outcome: None,
            events: Vec::new(),
            queued_at: Some("2026-11-12 10:00:00".into()),
            after_work_id: Some("work-gone".into()), // 依赖已不在登记（视为终态）
            cwd: Some(ws.to_string_lossy().to_string()),
            execution_id: None,
        };
        let _ = crate::central::upsert_work_item(state.core_manager.storage_root(), &record);
        // 重放路径（recover 的重放等价）：按记录构造 request 走 dispatch
        let request = crate::central::CentralDispatchRequest {
            work_id: record.work_id.clone(),
            session_id: record.session_id.clone(),
            task: record.task.clone(),
            model: record.model.clone(),
            cwd: record.cwd.clone(),
            after_work_id: record.after_work_id.clone(),
            parallel: false,
            reused_session: record.reused_session,
            execution_id: "exec-test".to_string(),
        };
        handle_central_dispatch(state, handle, request)
            .await
            .expect("重放投递应成功");
        // queued 标记被清（不清 → 下次重启重复执行）
        let items = crate::central::load_work_items(state.core_manager.storage_root());
        let entry = items
            .iter()
            .rev()
            .find(|i| i.session_id == "s-replay")
            .unwrap();
        assert!(
            entry.queued_at.is_none(),
            "投递成功后 queued 标记必须清除（W3 红点）"
        );
        // cwd 还原（记录里的目录用于投递）
        assert!(
            state.core_manager.has_live_core("s-replay"),
            "重放应建 Core（cwd 生效）"
        );
    }

    /// R0 行为基线：派发后执行归属必须三处一致——登记条目、落盘记录与
    /// 事件时间线都携带本次 execution_id，任何后台工作可唯一定位
    /// （重构方案 §5.4 / 阶段 R0 完成标准）。
    #[tokio::test]
    async fn host_dispatch_records_execution_ownership() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let app = tauri::test::mock_app();
        let state = {
            let s = TiangongApp::new_for_test(root.keep(), ws.to_string_lossy().to_string());
            std::fs::write(
                s.core_manager.storage_root().join("models.json"),
                serde_json::json!({
                    "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                    "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                    "routing": {"chat": "k3"}
                })
                .to_string(),
            )
            .unwrap();
            s
        };
        app.manage(state);
        let handle = app.handle();
        let state = state_of(handle);
        let storage_root = state.core_manager.storage_root().to_path_buf();
        // 先落盘登记（生产链路中由中枢插件派发工具写入；此处模拟
        // resume / 重放场景：记录存在，执行归属待宿主投递后更新）。
        let record = crate::central::WorkItemRecord {
            work_id: "work-exec1".into(),
            reused_session: false,
            session_id: "s-exec".into(),
            task: "执行归属基线任务".into(),
            model: None,
            dispatched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            finished_at: None,
            outcome: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: Some(ws.to_string_lossy().to_string()),
            execution_id: None,
        };
        let _ = crate::central::upsert_work_item(&storage_root, &record);
        let request = crate::central::CentralDispatchRequest {
            work_id: "work-exec1".into(),
            execution_id: "exec-baseline-1".into(),
            session_id: "s-exec".into(),
            task: "执行归属基线任务".into(),
            model: None,
            cwd: Some(ws.to_string_lossy().to_string()),
            after_work_id: None,
            parallel: false,
            reused_session: false,
        };
        handle_central_dispatch(state, handle, request)
            .await
            .expect("派发应成功");
        // 1) 内存登记条目带本次执行归属
        {
            let registry = state.central_work_registry.lock().unwrap();
            let entry = registry
                .iter()
                .rev()
                .find(|e| e.session_id == "s-exec")
                .unwrap();
            assert_eq!(
                entry.execution_id.as_deref(),
                Some("exec-baseline-1"),
                "登记条目必须携带本次 execution_id"
            );
        }
        // 2) 落盘记录同步执行归属（事件回填的数据源）
        let items = crate::central::load_work_items(&storage_root);
        let record = items
            .iter()
            .rev()
            .find(|i| i.session_id == "s-exec")
            .unwrap();
        assert_eq!(
            record.execution_id.as_deref(),
            Some("exec-baseline-1"),
            "投递成功后落盘记录必须是本次 execution_id"
        );
        // 3) R2/R5 事件日志（唯一事实源）：派发事实入 events.jsonl 且带归属
        {
            let log = tiangong_central::EventLog::open(
                &storage_root.join("central").join("events.jsonl"),
            )
            .expect("事件日志应可打开");
            let entries = log.load_all().expect("事件日志应可读取");
            let started = entries
                .iter()
                .find(|logged| {
                    logged.event.event_type
                        == tiangong_central::event::WorkEventType::WorkItemStarted
                        && logged.event.work_item_id == "work-exec1"
                })
                .expect("派发事实必须已写入事件日志");
            assert_eq!(
                started.event.execution_id.as_deref(),
                Some("exec-baseline-1"),
                "事件日志中的派发事实必须带执行归属"
            );
            assert_eq!(
                started.event.worker_session_id.as_deref(),
                Some("s-exec"),
                "事件日志中的派发事实必须带 Worker 会话"
            );
        }
    }

    /// R3 行为基线：Workspace 授权票据双通道——未授权拒绝、指纹匹配
    /// 票据放行且一次性、指纹偷换拒绝（方案 §5.6 / §3.4）。
    #[tokio::test]
    async fn host_ticket_channel_authorizes_workspace_dispatch() {
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        // 未长期授权的目标目录（非默认工作区子树）
        let extra = tempfile::tempdir().unwrap();
        let extra_path = extra.path().to_string_lossy().to_string();
        let app = tauri::test::mock_app();
        let state = {
            let s = TiangongApp::new_for_test(root.keep(), ws.to_string_lossy().to_string());
            std::fs::write(
                s.core_manager.storage_root().join("models.json"),
                serde_json::json!({
                    "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                    "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                    "routing": {"chat": "k3"}
                })
                .to_string(),
            )
            .unwrap();
            s
        };
        app.manage(state);
        let handle = app.handle();
        let state = state_of(handle);
        let request = |task: &str| crate::central::CentralDispatchRequest {
            work_id: format!("work-{task}"),
            execution_id: tiangong_central::new_execution_id(),
            session_id: "s-ticket".to_string(),
            task: task.to_string(),
            model: None,
            cwd: Some(extra_path.clone()),
            after_work_id: None,
            parallel: false,
            reused_session: false,
        };
        // 1) 未授权：拒绝且指引等待用户授权
        let denied = handle_central_dispatch(state, handle, request("发布模块 v2"))
            .await
            .expect_err("未授权工作区必须拒绝");
        assert!(denied.contains("尚未经用户授权"), "拒绝信息：{denied}");
        // 2) 指纹偷换：签发「任务甲」票据，派发「发布模块 v2」→ 拒绝
        state.ticket_registry.issue_workspace_approval(
            &extra_path,
            "任务甲",
            tiangong_central::tickets::RiskLevel::High,
            600,
        );
        let swapped = handle_central_dispatch(state, handle, request("发布模块 v2"))
            .await
            .expect_err("任务指纹不一致必须拒绝（防偷换）");
        assert!(
            swapped.contains("尚未经用户授权"),
            "偷换拒绝信息：{swapped}"
        );
        // 3) 匹配票据：放行且一次性（同任务再派拒绝）
        handle_central_dispatch(state, handle, request("任务甲"))
            .await
            .expect("匹配票据应放行派发");
        let replay = handle_central_dispatch(state, handle, request("任务甲"))
            .await
            .expect_err("票据一次性：同任务重派需要新授权");
        assert!(replay.contains("尚未经用户授权"), "重放拒绝信息：{replay}");
    }

    /// R5：登记由事件日志投影重建（唯一事实源重放；快照仅旧数据回退）。
    #[test]
    fn host_registry_rebuilds_from_event_log() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("central");
        std::fs::create_dir_all(&storage).unwrap();
        let log = tiangong_central::EventLog::open(&storage.join("events.jsonl")).unwrap();
        // 派发事件携带登记字段
        let started = tiangong_central::WorkEvent::new(
            "work-r1",
            tiangong_central::event::WorkEventType::WorkItemStarted,
        )
        .with_execution("exec-r1")
        .with_worker_session("s-r1");
        let mut started = started;
        started.payload = serde_json::json!({
            "task": "重构登录模块", "model": "k3", "cwd": "/ws/proj", "reused_session": false,
        });
        log.append(started).unwrap();
        log.append(
            tiangong_central::WorkEvent::new(
                "work-r1",
                tiangong_central::event::WorkEventType::WorkItemCompleted,
            )
            .with_execution("exec-r1")
            .with_worker_session("s-r1"),
        )
        .unwrap();

        let registry = crate::central_host::central_events::rebuild_registry_from_log(root.path())
            .expect("日志非空应投影重建");
        assert_eq!(registry.len(), 1);
        let entry = &registry[0];
        assert_eq!(entry.work_id, "work-r1");
        assert_eq!(entry.session_id, "s-r1");
        assert_eq!(entry.task, "重构登录模块");
        assert_eq!(entry.model.as_deref(), Some("k3"));
        assert_eq!(entry.cwd.as_deref(), Some("/ws/proj"));
        assert_eq!(entry.execution_id.as_deref(), Some("exec-r1"));
        assert_eq!(
            entry.state,
            central_state::WorkState::Completed,
            "终态由事件投影"
        );

        // 日志为空 → None（调用方回退旧快照路径）
        let empty_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(empty_root.path().join("central")).unwrap();
        assert!(
            crate::central_host::central_events::rebuild_registry_from_log(empty_root.path())
                .is_none(),
            "空日志必须回退旧路径（R5 前数据兼容）"
        );
    }

    /// Review P1-2 具名锁 ①：同会话多次派发，投影重建取**最后**一条
    /// started（此前正序遍历错误保留最早条目——重派/恢复后徽章显示首轮）。
    #[test]
    fn host_registry_rebuild_keeps_latest_dispatch_per_session() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("central");
        std::fs::create_dir_all(&storage).unwrap();
        let log = tiangong_central::EventLog::open(&storage.join("events.jsonl")).unwrap();
        // 同会话两次派发（重派：不同任务、不同执行）
        for (task, execution) in [("首轮任务", "exec-1"), ("重派任务", "exec-2")] {
            let mut started = tiangong_central::WorkEvent::new(
                "work-r1",
                tiangong_central::event::WorkEventType::WorkItemStarted,
            )
            .with_execution(execution)
            .with_worker_session("s-r1");
            started.payload = serde_json::json!({ "task": task, "cwd": "/ws" });
            log.append(started).unwrap();
        }

        let registry = crate::central_host::central_events::rebuild_registry_from_log(root.path())
            .expect("应投影重建");
        assert_eq!(registry.len(), 1, "会话维度单条");
        assert_eq!(
            registry[0].task, "重派任务",
            "必须取最后一次派发（review P1-2：此前保留最早）"
        );
        assert_eq!(registry[0].execution_id.as_deref(), Some("exec-2"));
    }

    /// Review P1-2 具名锁 ②：旧时间线迁移后，投影重建字段完整
    /// （此前迁移事件缺登记 payload——任务空串/模型默认/cwd 丢失）。
    #[test]
    fn host_migration_preserves_registry_fields_for_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let storage_root = root.path();
        // R5 前旧数据：work-items.json（裸数组）含内嵌时间线
        let record = crate::central::WorkItemRecord {
            work_id: "work-old".into(),
            reused_session: false,
            session_id: "s-old".into(),
            task: "旧任务全文".into(),
            model: Some("k3".into()),
            dispatched_at: "2026-11-01 09:00:00".into(),
            finished_at: None,
            outcome: None,
            events: vec![crate::central::WorkItemEvent {
                at: "2026-11-01 09:00:00".into(),
                kind: "dispatched".into(),
                note: "派发执行".into(),
                execution_id: None,
            }],
            queued_at: None,
            after_work_id: None,
            cwd: Some("/ws/legacy".into()),
            execution_id: None,
        };
        let _ = crate::central::upsert_work_item(storage_root, &record);
        // 迁移（幂等导入）
        crate::central_host::central_events::migrate_legacy_timeline(storage_root);
        // 投影重建：字段必须完整（任务/模型/cwd/派发时间来自登记）
        let registry = crate::central_host::central_events::rebuild_registry_from_log(storage_root)
            .expect("迁移后应可投影重建");
        let entry = registry
            .iter()
            .find(|e| e.work_id == "work-old")
            .expect("迁移事件应重建登记");
        assert_eq!(entry.task, "旧任务全文", "任务名不得丢失（P1-2）");
        assert_eq!(entry.model.as_deref(), Some("k3"));
        assert_eq!(entry.cwd.as_deref(), Some("/ws/legacy"));
        assert_eq!(
            entry.dispatched_at, "2026-11-01 09:00:00",
            "派发时间取原值非迁移时刻"
        );
    }

    /// Review Z1 具名锁 B：投影重建输出按 dispatched_at 升序（顺序
    /// 不变量——retention「按位置取最旧」依赖此序；倒序输出曾使其
    /// 淘汰最新记录）。
    #[test]
    fn rebuild_keeps_chronological_order() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("central");
        std::fs::create_dir_all(&storage).unwrap();
        let log = tiangong_central::EventLog::open(&storage.join("events.jsonl")).unwrap();
        // 三条不同时间派发的工作（乱序写入事件日志）
        for (work, at) in [
            ("work-c", "2026-11-13 10:00:00"),
            ("work-a", "2026-11-13 08:00:00"),
            ("work-b", "2026-11-13 09:00:00"),
        ] {
            let session = format!("s-{work}");
            let mut started = tiangong_central::WorkEvent::new(
                work,
                tiangong_central::event::WorkEventType::WorkItemStarted,
            )
            .with_worker_session(&session);
            started.payload = serde_json::json!({ "task": "任务", "dispatched_at": at });
            started.created_at = at.to_string();
            log.append(started).unwrap();
        }
        let registry = crate::central_host::central_events::rebuild_registry_from_log(root.path())
            .expect("应投影重建");
        let order: Vec<&str> = registry.iter().map(|e| e.work_id.as_str()).collect();
        assert_eq!(
            order,
            vec!["work-a", "work-b", "work-c"],
            "输出必须按派发时间升序"
        );
    }

    /// Review Z1 具名锁 A：重建（升序）后保留策略保留最近 10 条
    /// completed、淘汰最旧 2 条（倒序时代曾反向淘汰最新）。
    #[test]
    fn retention_keeps_latest_ten_after_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("central");
        std::fs::create_dir_all(&storage).unwrap();
        let log = tiangong_central::EventLog::open(&storage.join("events.jsonl")).unwrap();
        // 12 条 completed，时间 00..11（编号即时间序，00 最旧）
        for i in 0..12 {
            let work = format!("work-{i:02}");
            let session = format!("s-{i:02}");
            let at = format!("2026-11-13 {:02}:00:00", i);
            let mut started = tiangong_central::WorkEvent::new(
                &work,
                tiangong_central::event::WorkEventType::WorkItemStarted,
            )
            .with_worker_session(&session);
            started.payload = serde_json::json!({ "task": "任务", "dispatched_at": at });
            started.created_at = at.clone();
            log.append(started).unwrap();
            let done = tiangong_central::WorkEvent::new(
                &work,
                tiangong_central::event::WorkEventType::WorkItemCompleted,
            )
            .with_worker_session(&session);
            log.append(done).unwrap();
        }
        let mut registry =
            crate::central_host::central_events::rebuild_registry_from_log(root.path())
                .expect("应投影重建");
        assert_eq!(registry.len(), 12);
        // 保留策略（宿主在回报清账时调用同款逻辑）：按位置移除最旧
        let removals = central_state::retention_removals(&registry, 10);
        let removed: Vec<&str> = removals
            .iter()
            .map(|&i| registry[i].work_id.as_str())
            .collect();
        assert_eq!(removed, vec!["work-00", "work-01"], "必须淘汰最旧的 2 条");
        for index in removals.iter().rev() {
            registry.remove(*index);
        }
        let kept: Vec<&str> = registry.iter().map(|e| e.work_id.as_str()).collect();
        assert_eq!(
            kept,
            vec![
                "work-02", "work-03", "work-04", "work-05", "work-06", "work-07", "work-08",
                "work-09", "work-10", "work-11"
            ],
            "保留最近 10 条（含最新 work-11）"
        );
    }

    /// W6 具名：中枢连发两条消息的 is_sending 配对——第一条 begin 占住后，
    /// 未 finish 的第二条被拒（当年「a 项」bug 的行为面：发送态收尾缺失会
    /// 让 is_sending 永真）；finish 释放并递增 revision 后第二条可发送。
    #[tokio::test]
    async fn central_two_messages_pair_input_send() {
        use crate::state_ops::{begin_input_send, finish_input_send};
        let (state, _ws) = central_test_app();
        let sid = crate::central::CENTRAL_SESSION_ID;
        // 第一条：占住发送位
        state
            .with_state(|cs| begin_input_send(cs, sid, 0).map(|_| ()))
            .await
            .expect("第一条 begin 应成功");
        // is_sending 占用期间：第二条被拒（互斥语义）
        let rejected = state
            .with_state(|cs| begin_input_send(cs, sid, 0).map(|_| ()))
            .await;
        assert!(rejected.is_err(), "发送位占用时第二条必须被拒");
        // 中枢发送路径的收尾（缺它 is_sending 永真——即历史 bug）
        state
            .with_state(|cs| finish_input_send(cs, sid, 0, true).map(|_| ()))
            .await
            .expect("finish 应成功");
        // 第二条：revision 递增后可发送
        state
            .with_state(|cs| begin_input_send(cs, sid, 1).map(|_| ()))
            .await
            .expect("释放后第二条 begin 应成功（is_sending 配对闭环）");
    }
}
