//! 中枢宿主编排层（独立 crate，R6 抽离）：派发执行器、执行路由、
//! Core 生命周期、模式协调、授权票据、事件广播与重启恢复。
//!
//! 边界约定（务实抽离，非严格抽象）：
//! - 本 crate 聚合并拥有 [`CentralHostState`]（原 `TiangongApp` 的中枢
//!   字段 + 共享句柄注入）；宿主 `tiangong-app` 持有 `Arc<CentralHostState>`
//!   并向 Tauri 注册同名 state，消费循环经 `state_of` 取回。
//! - `CoreManager` / `TiangongState` 是 Arc 共享句柄，与宿主共用同一实例
//!   （无复制语义）；流消费者启动经回调注入（宿主命令层的 spawn 细节
//!   不进本 crate）。
//! - 类型与持久化函数经本 crate 顶层 re-export（对齐原宿主
//!   `crate::central` 桥路径），host 模块内以 `crate::X` 引用。
pub mod host;

pub use host::execution_router::ExecutionRouter;

pub use host::recovery::recover_central_work_state;
// 模块顶层可见（宿主 `central_host::events` 等路径兼容——原 src-tauri
// 模块路径零改动）
pub use host::events::emit_central_work_updated;
pub use host::recovery::inject_central_activation_summary;
pub use host::{
    authorization, central_events, core_lifecycle, dispatch, events, mode_commands,
    mode_coordinator, recovery, reports, work_view,
};

// 原 `crate::central` 桥（宿主 central.rs 同款）：类型来自 central-plugin，
// 持久化函数来自 central-state——host 模块内 `crate::X` 引用保持稳定。
pub use central_plugin::*;
pub use central_state::{
    add_confirmed_workspace, canonical_path_string, clear_work_item_queued, interrupted_work_items,
    load_confirmed_workspaces, load_goals, load_reports, load_work_items, mark_work_item_finished,
    mark_work_item_finished_with_session, mark_work_item_queued, queued_work_items,
    recover_registry_entries, save_goals, save_reports, save_work_items, set_work_item_execution,
    set_work_item_outcome, upsert_work_item, workspace_confirmed, GoalRecord, ReportRecord,
    WorkItemEvent, WorkItemRecord,
};

/// 流消费者启动回调（宿主注入：spawn 细节属于应用装配层）。
pub type StreamConsumerStarter = std::sync::Arc<
    dyn Fn(String, std::sync::mpsc::Receiver<tiangong_types::StreamEvent>) + Send + Sync,
>;
/// 流终态回调（宿主流消费者执行完毕后触发；dispatch 挂载回报回调）。
pub type StreamFinishedHandler = Box<dyn Fn(bool, String) + Send>;

/// 投递 prepared 消息到会话（宿主 Core 基础设施：串行锁/交接/远端轮守卫
/// 都在宿主实现——中枢只表达「把消息送进会话」意图）。参数：
/// (session_id, message_id, prepared, model_ref)。
pub type DeliverPreparedFn = std::sync::Arc<
    dyn Fn(
            String,
            String,
            Vec<tiangong_types::ContentBlock>,
            Option<String>,
        ) -> futures_lite_shim::BoxResult
        + Send
        + Sync,
>;
/// 确保会话 Core 存在并绑定流通道（宿主 ensure_core 的窄化门面）。
/// 返回 is_new（true=本次创建；false=并发创建，调用方按错误处理）。
pub type EnsureCoreFn = std::sync::Arc<
    dyn Fn(
            String,
            Option<String>,
            Option<String>,
            std::sync::mpsc::Sender<tiangong_types::StreamEvent>,
        ) -> futures_lite_shim::BoxEnsure
        + Send
        + Sync,
>;
/// 闭包返回的 future 装箱（避免中枢 crate 依赖具体运行时类型别名）。
pub mod futures_lite_shim {
    pub type BoxResult =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;
    pub type BoxEnsure =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send>>;
}

/// 中枢宿主状态聚合（原 `TiangongApp` 的 11 个中枢字段 + 共享句柄）。
///
/// 不实现 `Clone`：内部 Mutex/OnceLock 是**共享语义**（克隆会复制数据制造
/// 第二事实源）。跨线程/跨 Tauri state 传递用 `Arc<CentralHostState>`。
pub struct CentralHostState {
    /// 中枢独立 Core（单实例常驻；构造见 core_lifecycle）。
    pub central_core: std::sync::Mutex<Option<tiangong_core::core::TiangongCore>>,
    pub central_dispatch_tx: std::sync::OnceLock<
        tokio::sync::mpsc::UnboundedSender<central_plugin::CentralPluginMessage>,
    >,
    /// 与中枢插件共享的回报暂存（Arc：插件构造入参）。
    pub central_work_reports: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    pub central_running_works: std::sync::Mutex<std::collections::HashSet<String>>,
    pub central_work_registry: std::sync::Mutex<Vec<central_plugin::CentralWorkStatus>>,
    pub central_pending_dispatches: std::sync::Mutex<Vec<central_plugin::CentralDispatchRequest>>,
    pub central_model_fallback_notified: std::sync::atomic::AtomicBool,
    pub central_activation_greeted: std::sync::atomic::AtomicBool,
    pub execution_router: host::ExecutionRouter,
    pub ticket_registry: tiangong_central::TicketRegistry,
    pub mode_coordinator: host::mode_coordinator::ModeCoordinator,
    // —— 共享句柄注入（与宿主同一实例）——
    pub core_manager: tiangong_core_manager::CoreManager,
    pub app_state: std::sync::Arc<tokio::sync::Mutex<tiangong_app_state::app_state::TiangongState>>,
    pub app_handle: std::sync::Arc<std::sync::OnceLock<tauri::AppHandle>>,
    pub stream_consumer_starter: StreamConsumerStarter,
    pub deliver_prepared: DeliverPreparedFn,
    pub ensure_core_fn: EnsureCoreFn,
}

impl CentralHostState {
    pub fn new(
        core_manager: tiangong_core_manager::CoreManager,
        app_state: std::sync::Arc<tokio::sync::Mutex<tiangong_app_state::app_state::TiangongState>>,
        app_handle: std::sync::Arc<std::sync::OnceLock<tauri::AppHandle>>,
        stream_consumer_starter: StreamConsumerStarter,
        deliver_prepared: DeliverPreparedFn,
        ensure_core_fn: EnsureCoreFn,
    ) -> Self {
        Self {
            central_core: std::sync::Mutex::new(None),
            central_dispatch_tx: std::sync::OnceLock::new(),
            central_work_reports: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            central_running_works: std::sync::Mutex::new(std::collections::HashSet::new()),
            central_work_registry: std::sync::Mutex::new(Vec::new()),
            central_pending_dispatches: std::sync::Mutex::new(Vec::new()),
            central_model_fallback_notified: std::sync::atomic::AtomicBool::new(false),
            central_activation_greeted: std::sync::atomic::AtomicBool::new(false),
            execution_router: host::execution_router::ExecutionRouter::new(),
            ticket_registry: tiangong_central::TicketRegistry::default(),
            mode_coordinator: host::mode_coordinator::ModeCoordinator::new(),
            core_manager,
            app_state,
            app_handle,
            stream_consumer_starter,
            deliver_prepared,
            ensure_core_fn,
        }
    }

    /// 读应用共享状态（原宿主 `with_state_read` 的中枢版）。
    pub async fn with_state_read<T>(
        &self,
        reader: impl FnOnce(&tiangong_app_state::app_state::TiangongState) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let state = self.app_state.lock().await;
        reader(&state)
    }

    pub fn app_handle_read(&self) -> Option<tauri::AppHandle> {
        self.app_handle.get().cloned()
    }
}

/// 从 AppHandle 取 `Arc<CentralHostState>`（宿主在 setup 中 manage 同一份）。
pub fn state_of_central<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> std::sync::Arc<CentralHostState> {
    use tauri::Manager;
    app.state::<std::sync::Arc<CentralHostState>>()
        .inner()
        .clone()
}
