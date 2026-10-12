//! Fs sidecar 业务服务：按操作名分发请求，承载文件读写与路径解析。
//!
//! 整合原进程内插件的 7 个工具执行 + set_workspace 钩子，全部经 IPC 暴露给
//! 运行时（host 侧 invoke_sidecar）与 WASM 桥接。无跨请求状态（按需进程随
//! 调用销毁）：工作区由每个请求的 access 上下文携带，读写范围由 OS 沙箱裁决。

use anyhow::{Context, Result, anyhow};

use tiangong_plugin_fs_protocol::tools::{
    APPLY_PATCH_OPERATION, ApplyPatchRequest, LIST_DIR_OPERATION, ListDirRequest,
    READ_FILE_OPERATION, REPLACE_IN_FILE_OPERATION, ReadFileRequest, ReplaceInFileRequest,
    SET_WORKSPACE_OPERATION, SetWorkspaceRequest, TREE_DIR_OPERATION, TreeDirRequest,
    WRITE_FILE_OPERATION, WriteFileRequest,
};
use tiangong_plugin_fs_protocol::{Ack, FS_PROTOCOL_VERSION, PLUGIN_ID, PLUGIN_VERSION};
use tiangong_plugin_runtime::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request, Response,
    ServiceStatus,
};

use crate::handlers;

/// Fs sidecar 业务服务。无跨请求状态（按需进程随调用销毁）：工作区
/// 由每个请求的 access 上下文携带。
pub struct FsService {}

impl FsService {
    /// 构造默认实例。
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }

    /// 按 sidecar 协议分发请求。
    ///
    /// 慢操作（文件 IO）经 `spawn_blocking` 在独立线程执行，避免在单线程
    /// runtime 上阻塞其他连接的请求与健康检查。
    pub async fn dispatch(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "Fs 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
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
                business_protocol: FS_PROTOCOL_VERSION,
                capabilities: vec!["fs".to_string()],
                instance_id: format!("fs-sidecar-{}", std::process::id()),
                status: ServiceStatus::Ready,
            })
            .with_context(|| "序列化 Fs 握手响应失败"),
            LIST_DIR_OPERATION => {
                let req: ListDirRequest =
                    serde_json::from_value(payload).with_context(|| "解析 list_dir 请求失败")?;
                let access = req.access.clone();
                let resp = tokio::task::spawn_blocking(move || handlers::handle_list_dir(req))
                    .await
                    .with_context(|| "list_dir 后台任务失败")?;
                let resp = handlers::annotate_workdir(resp, &access);
                serde_json::to_value(resp).with_context(|| "序列化 list_dir 响应失败")
            }
            TREE_DIR_OPERATION => {
                let req: TreeDirRequest =
                    serde_json::from_value(payload).with_context(|| "解析 tree_dir 请求失败")?;
                let access = req.access.clone();
                let resp = tokio::task::spawn_blocking(move || handlers::handle_tree_dir(req))
                    .await
                    .with_context(|| "tree_dir 后台任务失败")?;
                let resp = handlers::annotate_workdir(resp, &access);
                serde_json::to_value(resp).with_context(|| "序列化 tree_dir 响应失败")
            }
            READ_FILE_OPERATION => {
                let req: ReadFileRequest =
                    serde_json::from_value(payload).with_context(|| "解析 read_file 请求失败")?;
                let access = req.access.clone();
                let resp = tokio::task::spawn_blocking(move || handlers::handle_read_file(req))
                    .await
                    .with_context(|| "read_file 后台任务失败")?;
                let resp = handlers::annotate_workdir(resp, &access);
                serde_json::to_value(resp).with_context(|| "序列化 read_file 响应失败")
            }
            WRITE_FILE_OPERATION => {
                let req: WriteFileRequest =
                    serde_json::from_value(payload).with_context(|| "解析 write_file 请求失败")?;
                let access = req.access.clone();
                let resp = tokio::task::spawn_blocking(move || handlers::handle_write_file(req))
                    .await
                    .with_context(|| "write_file 后台任务失败")?;
                let resp = handlers::annotate_workdir(resp, &access);
                serde_json::to_value(resp).with_context(|| "序列化 write_file 响应失败")
            }
            REPLACE_IN_FILE_OPERATION => {
                let req: ReplaceInFileRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 replace_in_file 请求失败")?;
                let access = req.access.clone();
                let resp =
                    tokio::task::spawn_blocking(move || handlers::handle_replace_in_file(req))
                        .await
                        .with_context(|| "replace_in_file 后台任务失败")?;
                let resp = handlers::annotate_workdir(resp, &access);
                serde_json::to_value(resp).with_context(|| "序列化 replace_in_file 响应失败")
            }
            APPLY_PATCH_OPERATION => {
                let req: ApplyPatchRequest =
                    serde_json::from_value(payload).with_context(|| "解析 apply_patch 请求失败")?;
                let access = req.access.clone();
                let resp = tokio::task::spawn_blocking(move || handlers::handle_apply_patch(req))
                    .await
                    .with_context(|| "apply_patch 后台任务失败")?;
                let resp = handlers::annotate_workdir(resp, &access);
                serde_json::to_value(resp).with_context(|| "序列化 apply_patch 响应失败")
            }
            SET_WORKSPACE_OPERATION => {
                // 兼容保留：wasm 侧状态更新才是请求构造源；sidecar 无
                // 进程内状态（按需进程随调用销毁，工作区与信任模式由
                // 每个请求的 access 上下文携带）。
                let _req: SetWorkspaceRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 set_workspace 请求失败")?;
                serde_json::to_value(Ack {}).with_context(|| "序列化 set_workspace 响应失败")
            }
            operation => Err(anyhow!("不支持的 Fs 操作: {operation}")),
        }
    }
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for FsService {
    async fn dispatch(
        &self,
        request: tiangong_plugin_runtime::protocol::Request,
    ) -> tiangong_plugin_runtime::protocol::Response {
        FsService::dispatch(self, request).await
    }
}
