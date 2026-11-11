use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

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
    central_core: Mutex<Option<tiangong_core::core::TiangongCore>>,
    /// 中枢派发执行器的请求通道（惰性启动；中枢插件经它派发后台工作）。
    central_dispatch_tx: std::sync::OnceLock<
        tokio::sync::mpsc::UnboundedSender<crate::central::CentralPluginMessage>,
    >,
    /// 已到达、尚未进入中枢对话的后台工作回报（暂存批量模式：随用户
    /// 下一条消息一并注入，或由中枢经 collect_work_reports 主动收集；
    /// 与中枢插件共享同一实例）。
    central_work_reports: std::sync::Arc<Mutex<Vec<String>>>,
    /// 在执行中的后台工作会话（dispatch 投递成功插入，终态移除）——
    /// 回报时机判定的依据：失败必达；成功且仍有任务在跑则暂存，
    /// 全部结束后汇总一轮（设计稿 §9「关键节点必达 / 适时沉默」）。
    central_running_works: Mutex<HashSet<String>>,
    /// 后台工作状态登记（前端「后台工作」状态条数据源；work_id 唯一）。
    central_work_registry: Mutex<Vec<crate::central::CentralWorkStatus>>,
    /// 每会话的终态回调槽（派发与引导共用消费者流，逐 work 挂载）。
    central_watch_slots: Mutex<HashMap<String, crate::commands::StreamFinishedSlot>>,
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
            central_watch_slots: Mutex::new(HashMap::new()),
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
        // 避免每次发送消息都重复构造插件集合（含 WASM 实例化）。
        //
        // 中枢对话不经此入口：中枢 Core 由 `ensure_central_core` 直接构造
        // （内置且唯一的中枢插件，见 `crate::central`）。
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

    /// 确保中枢 Core 存在：已存在则刷新配置复用，否则直接构造（挂中枢插件）。
    /// 返回是否新建（调用方据此启动流事件消费者）。
    pub(crate) async fn ensure_central_core(
        &self,
        app: &tauri::AppHandle,
        stream_tx: std::sync::mpsc::Sender<tiangong_types::StreamEvent>,
    ) -> Result<bool, String> {
        let (app_config, agent_config, workspace_dir) = self
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
        let existing_session = self
            .core_manager
            .load_session(crate::central::CENTRAL_SESSION_ID)
            .ok();
        if let Some(session) = &existing_session {
            session_config.trust_mode = session.trust_mode;
            session_config.reasoning_effort = session
                .reasoning_effort
                .unwrap_or(agent_config.reasoning_effort);
        }
        // 复用：刷新会话级运行配置（同 CoreManager ensure 命中分支语义）。
        {
            let guard = self.central_core.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(core) = guard.as_ref() {
                let _ = core.replace_config(session_config.clone());
                core.set_trust_mode(session_config.trust_mode);
                core.set_reasoning_effort(session_config.reasoning_effort);
                return Ok(false);
            }
        }
        // 新建：模型 = 中枢 session.model_ref ?? 设置的主对话模型（§15.12）。
        // 模型故障兜底（§15.12「中枢不掉线」）：会话指定模型失效（被删除/
        // 凭据缺失/能力不符）时不让中枢构建失败——回退默认 chat 路由；两者
        // 都不可用才报错。回退事实写入日志，首个可用机会注入中枢对话。
        let session_model_ref = existing_session.and_then(|session| session.model_ref);
        let model_endpoint = match self
            .core_manager
            .resolve_turn_model(session_model_ref.as_deref())
        {
            Ok(endpoint) => endpoint,
            Err(reason) => {
                tracing::warn!(
                    %reason,
                    "中枢会话模型不可用，回退默认对话模型（§15.12 中枢不掉线）"
                );
                let fallback =
                    self.core_manager
                        .resolve_turn_model(None)
                        .map_err(|fallback_error| {
                            format!(
                                "中枢模型不可用（{reason}），默认模型回退也失败：{fallback_error}"
                            )
                        })?;
                // 回退事实注入中枢回报队列：随下一条用户消息进入中枢对话，
                // 由中枢向用户说明（不再是只写日志的「首个可用机会」空承诺）。
                let model_note = format!(
                    "[系统中枢] 配置的中枢模型不可用（{reason}），本轮已自动回退默认模型；如需更换请调整设置。"
                );
                self.central_work_reports
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(model_note);
                fallback
            }
        };
        let core = tiangong_core::core::TiangongCore::builder()
            .session_id(crate::central::CENTRAL_SESSION_ID.to_string())
            .config(CoreConfigProvider::new(session_config.clone()))
            .trust_mode(session_config.trust_mode)
            .storage_root(self.core_manager.storage_root().to_path_buf())
            .workspace_dir(workspace_dir.clone())
            .stream_tx(stream_tx)
            // 征询功能暂缓（用户裁决 2026-11-11）：只挂中枢插件，不加
            // RuntimeCorePlugin（request_user 通道随之移除）。中枢需要用户
            // 决策时在对话文本中直接询问用户。
            .plugins(vec![Arc::new(crate::central::CentralPlugin::new(
                workspace_dir,
                self.core_manager.clone(),
                central_dispatch_sender(app),
                std::sync::Arc::clone(&self.central_work_reports),
            ))])
            .model_endpoint(model_endpoint)
            .build();
        let mut guard = self.central_core.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            // 并发双检：另一路径已创建，丢弃本次实例（其 stream_tx 消费者由先建者负责）。
            return Ok(false);
        }
        *guard = Some(core);
        Ok(true)
    }

    /// 向中枢 Core 投递用户消息（prepared blocks）。
    pub(crate) fn deliver_central_message(
        &self,
        message_id: String,
        prepared: Vec<tiangong_types::ContentBlock>,
        render: Option<tiangong_types::MessageRender>,
    ) -> Result<(), String> {
        use tiangong_core::agent_input::AgentInput;
        let core = self
            .central_core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| "中枢 Core 不存在（先 ensure_central_core）".to_string())?;
        core.deliver(AgentInputKind::prepared_with_id(message_id, prepared).with_render(render))
            .map_err(|error| format!("中枢消息投递失败：{error}"))
    }

    /// 取消中枢当前 turn。返回是否送达。
    pub(crate) fn cancel_central_turn(&self) -> bool {
        use tiangong_core::agent_input::{AgentInput, CommandInput};
        self.central_core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|core| {
                core.deliver(AgentInputKind::Command(CommandInput::Cancel))
                    .is_ok()
            })
    }

    /// 中枢 Core 是否存活（流事件消费者的存活判定用）。
    pub(crate) fn central_core_alive(&self) -> bool {
        self.central_core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// 关闭并等待中枢 Core 写盘结束（发送失败回滚 / 进程退出）。
    pub(crate) async fn shutdown_join_central_core(&self) {
        let core = self
            .central_core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(core) = core {
            let _ = tokio::task::spawn_blocking(|| core.shutdown_join()).await;
        }
    }

    /// 向中枢对话注入一条后台工作回报消息（完成/失败），触发中枢自动
    /// 转述（事件驱动汇报的最小闭环）。中枢空闲时开启新 turn；执行中
    /// 作为引导消息排队。
    /// 后台工作终态到达：登记移除 + 回报暂存 + **汇报时机判定**。
    ///
    /// 时机规则（设计稿 §9「关键节点必达 / 适时沉默」，意义与轻重的
    /// 区分由中枢模型在汇报轮内判断）：
    /// - **失败必达**：立即注入全部暂存并触发汇报轮（失败常需用户知晓）；
    /// - **成功且仍有任务在执行**：仅暂存——与后续任务汇总；
    /// - **成功且是最后一个（无在执行任务）**：注入全部暂存，触发一次
    ///   汇总轮，由中枢区分轻重统一回复。
    pub(crate) fn on_central_work_finished(
        &self,
        request: &crate::central::CentralDispatchRequest,
        done: bool,
        notice: &str,
        app: &tauri::AppHandle,
    ) {
        self.central_running_works
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request.work_id);
        self.central_work_reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(notice.to_string());
        // 状态登记：失败 → failed（与完成严格分态——review P1：失败必须
        // 如实呈现，不得混入 completed 绿色）；成功 → report_queued。
        {
            let mut registry = self
                .central_work_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = registry
                .iter_mut()
                .find(|entry| entry.work_id == request.work_id)
            {
                entry.state = if done {
                    "report_queued".to_string()
                } else {
                    "failed".to_string()
                };
            }
        }
        // 持久化终态与成败（重启后徽章按 outcome 重建 completed/failed 分组）
        crate::central::mark_work_item_finished_with_session(
            self.core_manager.storage_root(),
            &request.work_id,
            Some(&request.session_id),
            !done,
        );
        let has_running = !self
            .central_running_works
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        if should_deliver_work_reports_now(done, has_running) {
            self.deliver_pending_work_reports();
        }
        emit_central_work_updated(app);
    }

    /// 汇报已消费（注入或收集）：report_queued 转为 completed 保留
    /// （正常完成的工作留在清单可见），超出最近 10 个的旧 completed 移除。
    pub(crate) fn clear_queued_work_reports(&self) {
        let mut registry = self
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for entry in registry.iter_mut() {
            if entry.state == "report_queued" {
                entry.state = "completed".to_string();
            }
        }
        for state in ["completed", "failed"] {
            let count = registry.iter().filter(|e| e.state == state).count();
            if count > 10 {
                let mut to_remove = count - 10;
                registry.retain(|entry| {
                    if entry.state == state && to_remove > 0 {
                        to_remove -= 1;
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }

    /// 后台工作状态登记快照（前端状态条数据源）。
    pub(crate) fn central_work_status(&self) -> Vec<crate::central::CentralWorkStatus> {
        self.central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 注入全部暂存回报（第一条开启汇报轮，其余按 Core 原生引导排队）。
    pub(crate) fn deliver_pending_work_reports(&self) {
        use tiangong_core::agent_input::AgentInput;
        // 顺序（review k 项）：先确认 Core 存在再取走——Core 缺失时队列与
        // 状态登记都保持原样，由发送链路 flush 或 collect 兜底。
        let Some(core) = self
            .central_core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            tracing::warn!("中枢 Core 不存在，后台工作回报暂存待用户下一条消息");
            return;
        };
        let notices = self.take_central_work_reports();
        let mut failed: Vec<String> = Vec::new();
        for notice in notices {
            let prepared = vec![tiangong_types::ContentBlock::text(notice.clone())];
            if let Err(error) = core.deliver(AgentInputKind::prepared_with_id(
                scru128::new().to_string(),
                prepared,
            )) {
                tracing::warn!(%error, "后台工作回报注入中枢失败（放回队列）");
                failed.push(notice);
            }
        }
        // 投递失败的不丢弃：放回队列并保留待汇报登记（状态与队列一致）；
        // 全部成功才清除 report_queued 状态。
        if failed.is_empty() {
            self.clear_queued_work_reports();
        } else {
            self.central_work_reports
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(failed);
        }
    }

    /// 取走该会话全部已挂载的终态回调（消费者终态触发用；表取模式使
    /// 任意消费者实例都能触发挂载的回调——见 commands.rs N1 注释）。
    pub(crate) fn take_finished_handlers(
        &self,
        session_id: &str,
    ) -> Vec<crate::commands::StreamFinishedHandler> {
        self.central_watch_slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .map(|slot| std::mem::take(&mut *slot.lock().unwrap_or_else(|e| e.into_inner())))
            .unwrap_or_default()
    }

    /// 追加回报到暂存队列（投递失败的放回路径）。
    pub(crate) fn push_central_work_reports(&self, notices: Vec<String>) {
        self.central_work_reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(notices);
    }

    /// 取走全部暂存的后台工作回报（先进先出）。
    pub(crate) fn take_central_work_reports(&self) -> Vec<String> {
        let mut reports = self
            .central_work_reports
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *reports)
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

/// 从 AppHandle 取 TiangongApp（派发执行器/插件的共享状态入口）。
fn state_of(app: &tauri::AppHandle) -> &TiangongApp {
    use tauri::Manager;
    app.state::<TiangongApp>().inner()
}

/// 通知前端后台工作状态已变化（状态条刷新）。
fn emit_central_work_updated(app: &tauri::AppHandle) {
    use tauri::Emitter;
    let _ = app.emit("central-work-updated", ());
}

/// emit_central_work_updated 的跨模块入口（commands 复用）。
pub(crate) fn emit_central_work_updated_pub(app: &tauri::AppHandle) {
    emit_central_work_updated(app);
}

/// 重启恢复（§15.1 + 徽章「永不消失」）：从 work-items.json 全量重建
/// 状态登记（completed/failed 按 outcome、legacy 从中枢会话回报推断成败、
/// 未终态近期按 interrupted），并注入「离开期间」汇总回报——用户下一条
/// 消息时中枢能主动说明离开期间发生了什么（review P2：恢复后回报丢失
/// ——徽章显示已完成但中枢从未收到结果摘要）。
/// 幂等：registry 非空时跳过（防 setup 与 lazy 通道双跑重复注入）。
pub fn recover_central_work_state(app: &tauri::AppHandle) {
    use tauri::Manager;
    let state = app.state::<TiangongApp>();
    let entries =
        crate::central::recover_registry_entries(state.inner().core_manager.storage_root());
    if entries.is_empty() {
        return;
    }
    let mut registry = state
        .inner()
        .central_work_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !registry.is_empty() {
        return; // 已恢复过（双触发幂等）
    }
    let completed = entries.iter().filter(|e| e.state == "completed").count();
    let failed = entries.iter().filter(|e| e.state == "failed").count();
    let interrupted = entries.iter().filter(|e| e.state == "interrupted").count();
    for entry in &entries {
        tracing::info!(
            work_id = %entry.work_id,
            state = %entry.state,
            "后台工作状态已从登记重建（重启恢复）"
        );
    }
    registry.extend(entries);
    drop(registry);
    let summary = format!(
        "[系统中枢] 应用重启恢复（离开期间）：{completed} 个后台工作已完成、{failed} 个失败、{interrupted} 个中断。\n完成与失败工作的结果在其各自会话中；中断的工作可重派（复用原会话延续上下文）。收到用户消息后请按需转述本汇总。"
    );
    state
        .inner()
        .central_work_reports
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(summary);
    emit_central_work_updated(app);
}

/// 中枢派发执行器通道（惰性启动消费循环）。
fn central_dispatch_sender(
    app: &tauri::AppHandle,
) -> tokio::sync::mpsc::UnboundedSender<crate::central::CentralPluginMessage> {
    use tauri::Manager;
    let state = app.state::<TiangongApp>();
    state
        .inner()
        .central_dispatch_tx
        .get_or_init(|| {
            let (tx, mut rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::central::CentralPluginMessage>();
            let app = app.clone();
            tokio::spawn(async move {
                while let Some(message) = rx.recv().await {
                    match message {
                        crate::central::CentralPluginMessage::Dispatch(request) => {
                            if let Err(error) =
                                handle_central_dispatch(state_of(&app), &app, request.clone()).await
                            {
                                tracing::warn!(%error, work_id = %request.work_id, "中枢派发执行失败");
                                let notice = format!(
                                    "[后台工作失败] {}（模型 {}）\n任务：{}\n失败原因：{error}",
                                    request.work_id,
                                    request.model.as_deref().unwrap_or("（默认）"),
                                    request.task
                                );
                                // 派发即失败：无在执行登记，直接走失败必达路径
                                state_of(&app)
                                    .on_central_work_finished(&request, false, &notice, &app);
                            }
                        }
                        crate::central::CentralPluginMessage::Steer(request) => {
                            // 引导消息与派发同路径（登记/回调/回报时机一致），
                            // 仅语义不同：不新建会话、目标必须已存在（插件已校验）。
                            if let Err(error) =
                                handle_central_dispatch(state_of(&app), &app, request.clone()).await
                            {
                                tracing::warn!(%error, work_id = %request.work_id, "中枢引导执行失败");
                                let notice = format!(
                                    "[后台工作失败] {}（引导消息未送达）\n目标会话：{}\n内容：{}\n失败原因：{error}",
                                    request.work_id,
                                    request.session_id,
                                    request.task
                                );
                                state_of(&app)
                                    .on_central_work_finished(&request, false, &notice, &app);
                            }
                        }
                        crate::central::CentralPluginMessage::ReportsCollected => {
                            // collect_work_reports 已取走全部暂存回报：
                            // 清理状态登记并刷新前端。
                            let state = state_of(&app);
                            state.clear_queued_work_reports();
                            emit_central_work_updated(&app);
                        }
                    }
                }
            });
            tx
        })
        .clone()
}

/// 会话自身的工作目录（复用派发时沿用；会话不存在返回 None）。
fn self_session_cwd(state: &TiangongApp, session_id: &str) -> Option<String> {
    state
        .core_manager
        .load_session(session_id)
        .ok()
        .map(|session| session.cwd)
        .filter(|cwd| !cwd.trim().is_empty())
}

/// 执行中枢派发：创建 Worker 会话（普通能力、全插件、指定模型）投递任务，
/// 并挂终态 watcher 把结果注入中枢对话。
/// Worker 派发指令：身份 + 自主决策要求（后台会话无 UI，不得依赖交互
/// 工具；无法继续的关键阻塞如实写进最终结论，由中枢转述用户裁决）+ 任务。
fn build_worker_instruction(work_id: &str, task: &str) -> String {
    format!(
        "[中枢派发的后台工作 {}]\n你是被天工中枢派发的后台工作会话。请独立完成以下任务并给出结论；完成后你的最终答复会被中枢转述给用户。\n\n注意：你是后台会话，用户看不到你的界面——不要调用 request_user 等交互工具（无人应答，只会超时失败）。请自主决策完成任务；确因缺少用户决策无法继续时，在最终结论中如实说明阻塞点与建议选项，不要虚构结果。\n\n任务：\n{}",
        work_id, task
    )
}

async fn handle_central_dispatch(
    state: &TiangongApp,
    app: &tauri::AppHandle,
    request: crate::central::CentralDispatchRequest,
) -> Result<(), String> {
    let default_cwd = state
        .with_state_read(|core_state| Ok(core_state.workspace_dir.clone()))
        .await
        .unwrap_or_default();
    // 工作目录：请求指定 > 复用会话自身的 cwd > 全局默认
    let cwd = request
        .cwd
        .clone()
        .filter(|path| !path.trim().is_empty())
        .or_else(|| {
            // 复用已有会话：沿用该会话的工作目录（上下文连续的一部分）
            self_session_cwd(state, &request.session_id)
        })
        .unwrap_or(default_cwd);
    if !std::path::Path::new(&cwd).is_dir() {
        return Err(format!("工作目录不存在或不是目录：{cwd}"));
    }
    // Workspace 确认统一兜底（review P1-x2：Steer 绕过）——Dispatch/Steer
    // 殊途同归至此，最终 cwd（请求指定或复用会话自身）统一校验，插件侧
    // dispatch_work 的前置校验保留（错误信息更早更具体）。
    {
        let default_workspace = state
            .with_state_read(|core_state| Ok(core_state.workspace_dir.clone()))
            .await
            .unwrap_or_default();
        if !crate::central::workspace_confirmed(
            state.core_manager.storage_root(),
            &default_workspace,
            &cwd,
        ) {
            return Err(format!(
                "工作目录 {cwd} 尚未经用户确认（派发被拒绝）。请先在对话中向用户确认，\n                 同意后调用 confirm_workspace(path=\"{cwd}\") 登记，再重新派发。"
            ));
        }
    }
    // 回调槽（每会话一个，可多挂载）：派发与引导共用该会话既有的流消费者，
    // 逐 work 挂各自的终态回调——不再 retire+重建（消除 sidecar 重启与
    // prompt cache 失效；引导语义也要求保留在途执行）。
    let slot = {
        let mut slots = state
            .central_watch_slots
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        slots
            .entry(request.session_id.clone())
            .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(Vec::new())))
            .clone()
    };
    // 复用派发忽略 model 参数（review p 项）：透传会经 set_core_model_ref
    // 静默永久改写该会话的模型选择；复用/引导即沿用会话既有模型。
    let effective_model = if request.reused_session {
        None
    } else {
        request.model.as_deref()
    };

    let sid = request.session_id.clone();
    let instruction = build_worker_instruction(&request.work_id, &request.task);
    let prepared = vec![tiangong_types::ContentBlock::text(instruction)];

    if state.core_manager.has_live_core(&sid) {
        // Core 活跃（复用/引导/并发派发）：不重建，投递消息（忙→引导并入
        // 当前 turn；空闲→开新 turn，消费者已在线）。回调在投递成功后挂载
        // （review N5：先挂后投会在投递失败时残留回调，下次终态重复回报）。
        state
            .deliver_prepared_if_live(
                &sid,
                scru128::new().to_string(),
                prepared,
                effective_model,
                None,
            )
            .await?;
        attach_work_finished_callback(&slot, app, request.clone());
    } else {
        // Core 不存在（新建会话/重启后复用）：ensure 创建并启动流消费者
        // （is_new 守卫：命中复用说明并发创建，流通道会被丢弃，显式报错）。
        let (stream_tx, stream_rx) = std::sync::mpsc::channel::<tiangong_types::StreamEvent>();
        let ensured = state
            .ensure_core(&sid, Some(cwd), None, None, effective_model, stream_tx)
            .await?;
        if !ensured.is_new {
            return Err(format!(
                "会话 {} 的 Core 已被并发创建，本次流通道未绑定；请稍后重派",
                sid
            ));
        }
        crate::commands::start_stream_consumer(app.clone(), sid.clone(), stream_rx);
        state
            .deliver_prepared_if_live(
                &sid,
                scru128::new().to_string(),
                prepared,
                effective_model,
                None,
            )
            .await?;
        attach_work_finished_callback(&slot, app, request.clone());
    }
    tracing::info!(work_id = %request.work_id, session_id = %sid, "中枢后台工作已派发");
    // 给 Worker 会话写可识别标题（仅新建会话；复用/引导保留原标题）。
    if !request.reused_session {
        let title_preview: String = request.task.chars().take(24).collect();
        let title = format!("[后台工作] {title_preview}");
        if let Some(core) = state.core_manager.registry().get(&sid) {
            let _ = core.set_title(title, false);
        }
    }
    // 登记在执行集合与状态登记（running 按 work_id），并通知前端刷新
    {
        let mut running = state
            .central_running_works
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        running.insert(request.work_id.clone());
    }
    // 状态登记按**会话维度 upsert**（用户实测：同会话重派后旧 interrupted
    // 条目残留、多轮派发显示多条）——一个工作会话在徽章中始终是一条，
    // 重派/引导覆盖更新（work_id/task/时间取最新），中断旧条目自然消失。
    {
        let mut registry = state
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(position) = registry
            .iter()
            .position(|entry| entry.session_id == request.session_id)
        {
            registry.remove(position);
        }
        registry.push(crate::central::CentralWorkStatus {
            work_id: request.work_id.clone(),
            session_id: request.session_id.clone(),
            task: request.task.clone(),
            model: if request.reused_session {
                // 复用实际沿用会话既有模型（review M5：登记不得谎报请求模型）
                Some(format!(
                    "会话既有模型（已忽略 {}）",
                    request.model.as_deref().unwrap_or("默认")
                ))
            } else {
                request
                    .model
                    .clone()
                    .or_else(|| Some("路由默认".to_string()))
            },
            dispatched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            state: "running".to_string(),
            reused_session: request.reused_session,
        });
    }
    emit_central_work_updated(app);
    Ok(())
}

/// 向回调槽挂载单个 work 的终态回报回调。
fn attach_work_finished_callback(
    slot: &crate::commands::StreamFinishedSlot,
    app: &tauri::AppHandle,
    work: crate::central::CentralDispatchRequest,
) {
    let callback_app = app.clone();
    slot.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(Box::new(move |done: bool, reason: String| {
            let state = state_of(&callback_app);
            let notice = build_work_notice(state, &work, done, &reason);
            state.on_central_work_finished(&work, done, &notice, &callback_app);
        }));
}

/// 回报是否应立即注入汇报：失败必达；成功仅在无其他在执行任务时
/// 触发汇总轮（否则暂存等待汇总，或用户消息/询问兜底消费）。
fn should_deliver_work_reports_now(done: bool, has_running_work: bool) -> bool {
    !done || !has_running_work
}

/// 组装后台工作回报（读 Worker 会话最终答复，**不截断**）。
///
/// 不做长度截断的依据：完成汇报必须基于真实完整结果（设计稿 §2.3 /
/// §14.2「禁止推断完成」）——截断会制造残缺信息，转述反而误导；上下文
/// 安全由系统机制兜底（Core 自动压缩、模型上下文窗口、Worker 自身输出
/// 上限）。将来超长结果的正确形态是「结果落文件 + 注入摘要与引用」
/// （阶段 2 报告机制），而非拦腰截断。
fn build_work_notice(
    state: &TiangongApp,
    work: &crate::central::CentralDispatchRequest,
    done: bool,
    failed_reason: &str,
) -> String {
    let model_note = work.model.as_deref().unwrap_or("（默认）");
    if done {
        let summary = state
            .core_manager
            .load_session(&work.session_id)
            .ok()
            .and_then(|session| {
                session
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == tiangong_types::MessageRole::Assistant)
                    .map(|message| {
                        message
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                tiangong_types::ContentBlock::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<String>()
                    })
            })
            .unwrap_or_else(|| "（未能读取到执行结果）".to_string());
        format!(
            "[后台工作完成] {}（模型 {}）\n任务：{}\n\n执行结果：\n{}\n\n（请向用户完整转述上述结果的关键结论与依据，不要过度省略；如需细节可指引查看工作会话 {}）",
            work.work_id, model_note, work.task, summary, work.session_id
        )
    } else {
        let guidance = if failure_looks_model_related(failed_reason) {
            "\n\n（失败疑似模型/网络问题。处置：list_models 查询可用模型后，用              dispatch_work 带 session_id 与新 model 在原会话重派——上下文保留，             不要新建会话；确认无可用模型时再向用户说明。）"
        } else {
            ""
        };
        format!(
            "[后台工作失败] {}（模型 {}）\n任务：{}\n失败原因：{}{guidance}",
            work.work_id, model_note, work.task, failed_reason
        )
    }
}
/// 字节偏移回退到最近的 UTF-8 字符边界（不越过 0）。
fn char_safe_start(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// 字节偏移前进到最近的 UTF-8 字符边界（不超过文本长度）。
fn char_safe_end(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

/// 失败原因是否疑似模型/网络类（值得换模型重派，而非任务本身错误）。
/// 失败原因是否疑似模型/网络类（值得换模型重派，而非任务本身错误）。
///
/// 状态码按「词边界 + HTTP 语境窗口」匹配，避免「500 个文件」「401 行」
/// 这类纯数字文本误判（review：判定过宽）；词形标记保持子串匹配。
fn failure_looks_model_related(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    const MARKERS: &[&str] = &[
        "timeout",
        "超时",
        "rate limit",
        "rate_limit",
        "限流",
        "quota",
        "配额",
        "unauthorized",
        "authentication",
        "认证",
        "connection",
        "连接",
        "eof",
        "dns",
        "tls",
        "ssl",
        "网络",
        "provider error",
        "model not",
        "context length",
        "上下文长度",
        "overloaded",
        "internal server error",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
        "too many requests",
        "请求过于频繁",
        "服务器错误",
    ];
    if MARKERS.iter().any(|marker| lower.contains(marker)) {
        return true;
    }
    // 状态码：词边界（前后非数字）+ HTTP 语境（前后窗口含 http/api/status/
    // code/error/错误/状态码/服务器/请求 之一）——「已扫描 500 个文件」
    // 这类纯数字文本不再误判（review：判定过宽）。
    const CODES: &[&str] = &["429", "401", "403", "500", "502", "503", "504"];
    const CONTEXT: &[&str] = &[
        "http",
        "api",
        "status",
        "code",
        "error",
        "错误",
        "状态码",
        "服务器",
        "请求",
    ];
    CODES.iter().any(|code| {
        lower.match_indices(code).any(|(idx, matched)| {
            let before_ok = idx == 0 || !lower[..idx].ends_with(|c: char| c.is_ascii_digit());
            let end = idx + matched.len();
            let after_ok =
                end >= lower.len() || !lower[end..].starts_with(|c: char| c.is_ascii_digit());
            if !(before_ok && after_ok) {
                return false;
            }
            // UTF-8 安全窗口：字节偏移回退到最近字符边界再切片——
            // `&s[a..b]` 的 a/b 落在多字节字符中间会 panic（review P1：
            // 中文失败原因 + 状态码是常态输入，如「接口超时错误码429」）。
            let window_start = char_safe_start(&lower, idx.saturating_sub(16));
            let window_end = char_safe_end(&lower, (end + 16).min(lower.len()));
            let window = &lower[window_start..window_end];
            CONTEXT.iter().any(|context| window.contains(context))
        })
    })
}

#[cfg(test)]
mod tests {
    /// 回报时机判定：失败必达；成功仅在无其他在执行任务时触发汇总轮。
    #[test]
    fn work_report_timing_follows_design() {
        // 失败：无论是否还有任务在跑，立即汇报（关键节点必达）
        assert!(should_deliver_work_reports_now(false, true));
        assert!(should_deliver_work_reports_now(false, false));
        // 成功且仍有任务在执行：暂存等待汇总
        assert!(!should_deliver_work_reports_now(true, true));
        // 成功且是最后一个：触发汇总轮
        assert!(should_deliver_work_reports_now(true, false));
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

    /// Worker 指令：禁用交互工具（后台无 UI）+ 阻塞如实说明，不虚构结果。
    #[test]
    fn worker_instruction_forbids_interaction_tools() {
        let instruction = build_worker_instruction("work-abc", "执行数据库迁移");
        assert!(instruction.contains("work-abc"));
        assert!(instruction.contains("执行数据库迁移"));
        assert!(instruction.contains("不要调用 request_user"));
        assert!(instruction.contains("不要虚构结果"));
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
}
