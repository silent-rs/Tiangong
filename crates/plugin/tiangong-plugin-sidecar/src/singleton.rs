//! 进程单例（淘汰制）。
//!
//! 淘汰制语义：已有健康 sidecar 实例时，新进程直接退出；不存在时才启动。
//! 不存在 Leader/Follower、心跳、选举等概念。
//!
//! 「已有健康实例」的判定 = 读 endpoint 文件 + TCP 端口可达性校验：
//! - endpoint 文件不存在 → 无实例 → 启动
//! - endpoint 文件存在但 TCP 连不上端口 → 实例已死 → 删除残留文件 → 启动
//! - endpoint 文件存在且 TCP 可达 → 已有实例 → 本进程退出
//!
//! 比「进程存活 + 心跳」可靠：直接验证 IPC 可达性，不依赖心跳刷新，
//! IPC 线程崩溃（端口关闭）能被立即发现。

use std::sync::Arc;

use anyhow::{Result, anyhow};

use crate::endpoint;
use crate::identity::SidecarConfig;
use crate::protocol::{Request, Response};
use crate::server::{self, IpcBridge};
use async_trait::async_trait;

/// 业务 sidecar 实现此 trait，提供请求分发。
///
/// `dispatch` 在 IPC server 的 tokio task 内 `await` 调用，慢操作应在内部用
/// `spawn_blocking` 等让出调度（参见 index sidecar）。
#[async_trait]
pub trait SidecarService: Send + Sync {
    async fn dispatch(&self, request: Request) -> Response;

    /// 请求级取消的资源清理钩子。普通异步任务由 stdio 运行器终止；持有
    /// 子进程、PTY 或外部句柄的服务应按目标请求释放资源。
    async fn cancel(&self, _request: &Request) -> Result<()> {
        Ok(())
    }
}

/// 单例守卫：持有 IPC bridge，drop 时停 server 并清理 endpoint 文件。
pub struct SingletonGuard {
    _bridge: Option<IpcBridge>,
}

impl Drop for SingletonGuard {
    fn drop(&mut self) {
        // IpcBridge::drop 会停 IPC server 线程；
        // IpcServer::drop 会删除 endpoint 文件（带 pid 校验，不误删新实例的）。
        self._bridge.take();
    }
}

/// 启动 sidecar（淘汰制单例）。
///
/// `service_factory` 仅在确认成为唯一实例后调用（被淘汰的候选不会构造 service）。
///
/// 返回值：
/// - `Ok(SingletonGuard)`：本进程成为唯一实例，持有期间经 IPC 暴露给运行时。
/// - `Err(SingletonError::AlreadyRunning)`：已有健康实例，调用方应优雅退出。
/// - `Err(其他)`：启动失败。
pub fn start<F>(config: &SidecarConfig, service_factory: F) -> Result<SingletonGuard>
where
    F: FnOnce() -> Result<Arc<dyn SidecarService>>,
{
    let service = &config.service;
    endpoint::ensure_runtime_dir(service)?;

    // 淘汰判定：已有健康实例则退出。
    if existing_instance_alive(service)? {
        return Err(anyhow!(SingletonError::AlreadyRunning));
    }

    // 确认成为唯一实例后才构造 service。
    let service_obj = service_factory()?;
    let bridge = server::spawn_bridge(config, service_obj)?;
    Ok(SingletonGuard {
        _bridge: Some(bridge),
    })
}

/// 判定是否已有健康的 sidecar 实例在运行。
///
/// 读 endpoint 文件 + TCP 端口可达性校验。endpoint 文件存在但端口连不上时，
/// 视为残留（实例已死），删除文件后返回 false（允许启动）。
fn existing_instance_alive(service: &str) -> Result<bool> {
    let path = endpoint::endpoint_path(service)?;
    if !path.exists() {
        return Ok(false);
    }
    let endpoint = match endpoint::read_endpoint(&path) {
        Ok(endpoint) => endpoint,
        Err(err) => {
            // endpoint 文件损坏，视为残留，删除后允许启动。
            tracing::warn!(service, error = %err, "endpoint 文件损坏，已删除并允许启动");
            let _ = std::fs::remove_file(&path);
            return Ok(false);
        }
    };
    if endpoint::tcp_probe(&endpoint.host, endpoint.port) {
        return Ok(true);
    }
    // endpoint 文件存在但端口不可达 = 实例已死，删除残留文件。
    tracing::info!(
        service,
        host = %endpoint.host,
        port = endpoint.port,
        "已有 sidecar endpoint 不可达，删除残留文件并启动新实例"
    );
    let _ = std::fs::remove_file(&path);
    Ok(false)
}

/// 单例错误。
#[derive(Debug)]
pub enum SingletonError {
    /// 已有健康实例在运行，本进程应退出。
    AlreadyRunning,
}

impl std::fmt::Display for SingletonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning => write!(f, "已有 sidecar 实例运行中，本进程退出"),
        }
    }
}

impl std::error::Error for SingletonError {}
