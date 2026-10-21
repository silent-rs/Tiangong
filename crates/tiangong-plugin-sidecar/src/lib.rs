//! 插件 sidecar 通用运行库。
//!
//! 业务中立的 sidecar 进程通用能力，以及宿主与 sidecar 双方共用的约定。
//! 依赖方向：插件与宿主侧 `tiangong-plugin-runtime`（加载 WASM / 启动连接
//! sidecar / 转发请求）都依赖本 crate，本 crate 不依赖宿主实现。
//!
//! # 包含
//! - [`protocol`]：宿主与 sidecar 共用的请求信封、握手与帧协议
//! - [`env`]：宿主注入 sidecar 进程的环境变量约定
//! - [`release`]：插件发布约定（官方目录、签名清单数据结构、信任根常量）
//! - [`singleton`]：进程单例（淘汰制：已有健康实例则退出，不存在时才启动）
//! - [`identity`]：sidecar 配置与实例身份
//! - [`endpoint`]：运行时目录解析、endpoint 文件发布/读取/清理、TCP 可达性探测
//! - [`server`]：通用 IPC server（TCP loopback + 动态端口 + token 鉴权 + JSON Lines 帧）
//! - [`shutdown`]：跨平台退出信号（Ctrl+C / SIGTERM）
//! - [`storage`]：插件存储根与自有数据目录 `data/<插件 id>` 定位
//!
//! # 不包含
//! 任何插件编号、业务操作名、tantivy/模型等业务依赖、App/CLI/Server 类型、
//! WASM guest 类型、某插件专属的数据目录规则。
//!
//! # 快速启动
//! ```no_run
//! # use std::sync::Arc;
//! # use async_trait::async_trait;
//! # use tiangong_plugin_sidecar::{SidecarConfig, SidecarService, run};
//! # use tiangong_plugin_sidecar::protocol::{Request, Response};
//! # struct MyService;
//! # #[async_trait]
//! # impl SidecarService for MyService {
//! #     async fn dispatch(&self, _: Request) -> Response { unimplemented!() }
//! # }
//! # async fn demo() -> anyhow::Result<()> {
//! let config = SidecarConfig::new("my-plugin");
//! run(config, || Ok(Arc::new(MyService))).await
//! # }
//! ```

pub mod endpoint;
pub mod env;
pub mod identity;
#[cfg(feature = "model")]
pub mod model;
pub mod protocol;
pub mod release;
pub mod server;
pub mod shutdown;
pub mod singleton;
pub mod stdio;
pub mod storage;

pub use identity::{SidecarConfig, SidecarIdentity};
#[cfg(feature = "model")]
pub use model::{ModelInfo, mask_sensitive};
pub use server::IpcBridge;
pub use singleton::{SidecarService, SingletonError, SingletonGuard, start};

/// 向当前 sidecar 请求发送进度或 Runtime 控制反馈，自动适配 TCP/stdio。
pub async fn emit_progress(message: impl Into<String>) {
    let message = message.into();
    server::emit_progress(message.clone()).await;
    stdio::emit_progress(message).await;
}

/// 获取当前工具请求的宿主权威上下文。TCP 与 stdio 一致；旧宿主调用返回 None。
pub fn invocation_context() -> Option<protocol::RequestInvocationContext> {
    server::invocation_context().or_else(stdio::invocation_context)
}

use std::sync::Arc;

use anyhow::Result;

/// 一键启动 sidecar（淘汰制单例）。
///
/// 完整流程：单例判定 → 成为唯一实例则起 IPC server，阻塞等终止信号；
/// 已有健康实例则返回 `Err(SingletonError::AlreadyRunning)`，调用方应优雅退出。
///
/// `service_factory` 仅在确认成为唯一实例后调用（被淘汰的候选不会构造 service，
/// 避免白白打开数据、占用资源）。
///
/// 各插件 main.rs 只需构造好 `SidecarConfig` + 业务 service 工厂，调本函数即可。
/// 若需在启动前做业务前置（如加载配置、恢复数据目录），在工厂闭包内完成。
pub async fn run<F>(config: SidecarConfig, service_factory: F) -> Result<()>
where
    F: FnOnce() -> Result<Arc<dyn SidecarService>>,
{
    // stdio 传输（RFC 0017 D16）：宿主直连管道，无单例与信号等待，EOF 即退。
    if stdio::stdio_requested() {
        return stdio::run_stdio(service_factory).await;
    }

    let guard = start(&config, service_factory).inspect_err(|err| {
        if err
            .downcast_ref::<SingletonError>()
            .is_some_and(|e| matches!(e, SingletonError::AlreadyRunning))
        {
            tracing::info!(
                "{} sidecar 已有实例运行中，本进程无需重复启动，退出",
                config.service
            );
        }
    })?;

    tracing::info!("{} sidecar 已成为唯一实例，开始服务", config.service);
    shutdown::wait_for_shutdown_signal().await?;
    tracing::info!("收到终止信号，{} sidecar 退出", config.service);
    drop(guard);
    Ok(())
}
