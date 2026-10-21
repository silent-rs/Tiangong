//! Fetch sidecar 业务服务：承载 reqwest 阻塞抓取、SSRF 防护与 download 落盘，
//! 按操作名分发请求。整合原进程内插件的工具执行与 set_workspace 钩子，全部经 IPC
//! 暴露给运行时（host 侧 invoke_sidecar）与 WASM 桥接。

use std::path::PathBuf;
use std::sync::RwLock;

use anyhow::{Context, Result, anyhow};

use tiangong_plugin_fetch_protocol::web_fetch::{
    SET_WORKSPACE_OPERATION, SetWorkspaceRequest, WEB_FETCH_OPERATION, WebFetchRequest,
};
use tiangong_plugin_fetch_protocol::{Ack, FETCH_PROTOCOL_VERSION, PLUGIN_ID, PLUGIN_VERSION};
use tiangong_plugin_sidecar::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request, Response,
    ServiceStatus,
};

use crate::fetch;

/// Fetch sidecar 业务服务。
pub struct FetchService {
    /// 当前会话工作目录（由 set_workspace 注入，download 落盘基准）。
    workspace: RwLock<Option<PathBuf>>,
    /// 是否完全信任模式（放宽工作区外路径校验，预留）。
    full_trust: RwLock<bool>,
}

impl FetchService {
    /// 构造默认实例。
    pub fn new() -> Result<Self> {
        Ok(Self {
            workspace: RwLock::new(None),
            full_trust: RwLock::new(false),
        })
    }

    /// 按 sidecar 协议分发请求。
    ///
    /// `async`：慢操作（HTTP 抓取 + 落盘）经 `spawn_blocking` 在独立线程执行，
    /// 避免在单线程 runtime 上阻塞其他连接的请求与健康检查。
    pub async fn dispatch(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "Fetch 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }

        let payload = match self
            .dispatch_operation(&request.operation, request.payload)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return Response::error(
                    &request_id,
                    ErrorCode::ServiceError,
                    error.to_string(),
                    false,
                );
            }
        };
        Response::success(&request_id, payload)
    }

    async fn dispatch_operation(
        &self,
        operation: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value> {
        match operation {
            HANDSHAKE_OPERATION => serde_json::to_value(HandshakeResponse {
                plugin_id: PLUGIN_ID.to_string(),
                plugin_version: PLUGIN_VERSION.to_string(),
                sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: PROTOCOL_VERSION.to_string(),
                business_protocol: FETCH_PROTOCOL_VERSION,
                capabilities: vec!["fetch".to_string()],
                instance_id: format!("fetch-sidecar-{}", std::process::id()),
                status: ServiceStatus::Ready,
            })
            .with_context(|| "序列化 Fetch 握手响应失败"),
            WEB_FETCH_OPERATION => {
                let req: WebFetchRequest =
                    serde_json::from_value(payload).with_context(|| "解析 web_fetch 请求失败")?;
                let (base, full_trust) = self.workspace_and_trust();
                let resp = tokio::task::spawn_blocking(move || {
                    let base = base.unwrap_or_else(|| PathBuf::from("."));
                    fetch::execute(req, &base, full_trust)
                })
                .await
                .with_context(|| "web_fetch 后台任务失败")?;
                serde_json::to_value(resp).with_context(|| "序列化 web_fetch 响应失败")
            }
            SET_WORKSPACE_OPERATION => {
                let req: SetWorkspaceRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 set_workspace 请求失败")?;
                self.handle_set_workspace(req)?;
                serde_json::to_value(Ack {}).with_context(|| "序列化 set_workspace 响应失败")
            }
            operation => Err(anyhow!("不支持的 Fetch 操作: {operation}")),
        }
    }

    // ── 生命周期 ─────────────────────────────────────────────

    fn handle_set_workspace(&self, req: SetWorkspaceRequest) -> Result<()> {
        if let Ok(mut guard) = self.workspace.write() {
            *guard = req.workspace.map(PathBuf::from);
        }
        Ok(())
    }

    /// 通知 sidecar 当前信任模式（由 WASM 经单独操作注入时使用，当前预留）。
    fn _set_full_trust(&self, full_trust: bool) {
        if let Ok(mut guard) = self.full_trust.write() {
            *guard = full_trust;
        }
    }

    // ── 辅助 ─────────────────────────────────────────────────────

    fn workspace_and_trust(&self) -> (Option<PathBuf>, bool) {
        let workspace = self.workspace.read().ok().and_then(|g| g.clone());
        let full_trust = self.full_trust.read().map(|g| *g).unwrap_or(false);
        (workspace, full_trust)
    }
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for FetchService {
    async fn dispatch(
        &self,
        request: tiangong_plugin_sidecar::protocol::Request,
    ) -> tiangong_plugin_sidecar::protocol::Response {
        FetchService::dispatch(self, request).await
    }
}
