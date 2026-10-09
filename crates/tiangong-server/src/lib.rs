pub mod api;
pub mod auth;
pub mod config;
pub mod daemon;
pub mod remote;
pub mod webhook;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use silent::prelude::*;
use tiangong_core::permission::TrustMode;
use tokio::sync::Mutex;

use self::api::{ServerAppContext, SharedState, build_routes};
use self::remote::backend::ServerCoreBackend;
use self::remote::event::EventBus;

/// 启动 Server 模式（前台运行，阻塞）
pub fn run_server(host: &str, port: u16, token: Option<String>) -> Result<()> {
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    // 注入 server 连接信息给所有插件 sidecar（定时任务等需经 HTTP 回调本机 server）。
    tiangong_plugin_runtime::registry::set_server_endpoint(
        format!("http://{host}:{port}"),
        token.clone(),
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    tracing::info!("正在初始化应用状态...");
    let mut app_state = tiangong_app_state::app_state::TiangongState::new();
    let storage_root = app_state.config.storage_root.clone();
    // 与桌面端/CLI 一致：已安装插件由 runtime 统一预加载（后台进行，不阻塞
    // 监听）。会话 Core 的 runtime 聚合桥每次调用都按注册表取插件，加载
    // 完成后的请求即可用到全部插件工具。
    {
        let storage_root = storage_root.clone();
        std::thread::spawn(move || {
            let loaded =
                tiangong_plugin_runtime::registry::preload_installed_plugins(&storage_root);
            tracing::info!(loaded, "已预加载插件");
        });
    }
    app_state.config.default_trust_mode = TrustMode::FullTrust;
    let core_manager = app_state.core_manager.clone();
    core_manager
        .config()
        .replace(app_state.config.to_core_config());
    app_state.workspace_dir = std::env::current_dir()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();
    app_state.agent_config.trust_mode = TrustMode::FullTrust;
    let state: SharedState = Arc::new(Mutex::new(app_state));

    let event_bus = Arc::new(EventBus::default());
    let app = Arc::new(ServerAppContext::new(
        state,
        core_manager,
        event_bus.clone(),
        storage_root,
    ));

    tracing::info!("构建路由...");
    let (api_routes, configs) = build_routes(app.clone(), token, event_bus);

    let mut route = Route::new_root().append(api_routes);
    route.set_state(Some(configs));

    tracing::info!("Server 启动：http://{addr}");

    // 自建 runtime 并真正用 block_on 驱动：HTTP 服务跑在这个 runtime 上。
    // 定时任务的 cron 调度已下沉到 scheduler sidecar，本进程不再恢复 cron job。
    runtime.block_on(async move {
        Server::new().bind(addr).serve(route).await;
    });

    Ok(())
}

/// 后台运行 Server
pub fn run_daemon(host: &str, port: u16, token: Option<String>) -> Result<()> {
    daemon::run_daemon(host, port, token)
}

/// 停止后台 Server
pub fn stop_daemon() -> Result<()> {
    daemon::stop_daemon()
}

// ── 嵌入式 Server（Desktop App 内部运行）──────────────────────────

/// 嵌入式 Server 的关闭句柄
pub struct EmbeddedServerHandle {
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EmbeddedServerHandle {
    /// 停止嵌入式 Server
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        // 等待 server 线程退出
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 启动嵌入式 Server（非阻塞，在独立线程中运行）
///
/// 创建独立的 tokio runtime 运行 HTTP，但 Core 和 MCP 管理实例均由 Desktop 注入。
/// HTTP runtime 不拥有会话写入器；定时任务的 cron 调度由 scheduler sidecar 负责。
pub struct EmbeddedServerDependencies {
    pub state: SharedState,
    pub config: tiangong_core::config::core::CoreConfigProvider,
    pub core_backend: Arc<dyn ServerCoreBackend>,
    pub event_bus: Arc<EventBus>,
}

pub fn run_embedded(
    host: &str,
    port: u16,
    token: Option<String>,
    dependencies: EmbeddedServerDependencies,
) -> Result<EmbeddedServerHandle> {
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    // 注入 server 连接信息给所有插件 sidecar（定时任务等需经 HTTP 回调本机 server）。
    tiangong_plugin_runtime::registry::set_server_endpoint(
        format!("http://{host}:{port}"),
        token.clone(),
    );

    let EmbeddedServerDependencies {
        state,
        config,
        core_backend,
        event_bus,
    } = dependencies;
    let app = Arc::new(ServerAppContext::with_backend(
        state,
        config,
        event_bus.clone(),
        core_backend,
    ));

    tracing::info!("构建嵌入式 Server 路由...");
    let (api_routes, configs) = build_routes(app.clone(), token, event_bus);

    let mut route = Route::new_root().append(api_routes);
    route.set_state(Some(configs));

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tracing::info!("嵌入式 Server 启动：http://{addr}");
    let thread = std::thread::Builder::new()
        .name("tiangong-embedded-server".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build embedded server runtime");
            rt.block_on(async move {
                let server = Server::new()
                    .bind(addr)
                    .with_shutdown(std::time::Duration::from_secs(5));
                tokio::select! {
                    _ = server.serve(route) => {}
                    _ = shutdown_rx => {
                        tracing::info!("嵌入式 Server 收到关闭信号");
                    }
                }
            });
        })?;

    Ok(EmbeddedServerHandle {
        shutdown_tx: Some(shutdown_tx),
        thread: Some(thread),
    })
}
