//! Fs sidecar 业务服务：宿主直连的工具入口。
//!
//! 宿主按清单 `tools` 把 `file_read` / `file_write` / `current_time` 的调用
//! 直接送到本进程（operation 为工具名、payload 为调用参数对象），这里解析
//! 参数、执行文件操作并返回 ToolOutcome 形状（ok/summary/stdout/stderr/
//! exit_code）。工作目录取自宿主注入的请求上下文（会话 cwd 真相源），读写
//! 范围由 OS 沙箱裁决。无跨请求状态（按需进程随调用销毁）。
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use tiangong_plugin_fs_protocol::tools::FsToolResponse;
use tiangong_plugin_fs_protocol::{
    FS_PROTOCOL_VERSION, FsAccessContext, PLUGIN_ID, PLUGIN_VERSION, TOOL_CURRENT_TIME,
    TOOL_FILE_READ, TOOL_FILE_WRITE,
};
use tiangong_plugin_sidecar::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request,
    RequestInvocationContext, Response, ServiceStatus,
};

use crate::dispatch::{self, DispatchError, FsCall};
use crate::{handlers, read_image};

/// 宿主直连的工具（握手能力 `tool:<名>` 与清单 `tools` 一致）。
const TOOLS: &[&str] = &[TOOL_FILE_READ, TOOL_FILE_WRITE, TOOL_CURRENT_TIME];

/// Fs sidecar 业务服务。
pub struct FsService {}

impl FsService {
    /// 构造默认实例。
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }

    /// 按 sidecar 协议分发请求。
    ///
    /// 文件 IO 经 `spawn_blocking` 在独立线程执行，避免阻塞运行时。
    pub async fn dispatch(
        &self,
        request: Request,
        context: Option<RequestInvocationContext>,
    ) -> Response {
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
        match self
            .dispatch_operation(&request.operation, request.payload, context.as_ref())
            .await
        {
            Ok(value) => Response::success(&request_id, value),
            Err(error) => Response::error(
                &request_id,
                ErrorCode::ServiceError,
                format!("{error:#}"),
                false,
            ),
        }
    }

    async fn dispatch_operation(
        &self,
        operation: &str,
        payload: Value,
        context: Option<&RequestInvocationContext>,
    ) -> Result<Value> {
        if operation == HANDSHAKE_OPERATION {
            return serde_json::to_value(HandshakeResponse {
                plugin_id: PLUGIN_ID.to_string(),
                plugin_version: PLUGIN_VERSION.to_string(),
                sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: PROTOCOL_VERSION.to_string(),
                business_protocol: FS_PROTOCOL_VERSION,
                capabilities: TOOLS.iter().map(|tool| format!("tool:{tool}")).collect(),
                instance_id: format!("fs-sidecar-{}", std::process::id()),
                status: ServiceStatus::Ready,
            })
            .context("序列化 Fs 握手响应失败");
        }
        if !TOOLS.contains(&operation) {
            return Err(anyhow!("不支持的 Fs 操作: {operation}"));
        }
        let access = access_of(context);
        let call = match dispatch::parse_call(operation, &payload, access.clone()) {
            Ok(call) => call,
            Err(DispatchError::InvalidArgument(message)) => {
                return outcome(invalid_argument(&message));
            }
            Err(DispatchError::UnknownTool(name)) => {
                return Err(anyhow!("未知的 Fs 工具: {name}"));
            }
        };
        if matches!(call, FsCall::CurrentTime) {
            return outcome(current_time());
        }
        let response = tokio::task::spawn_blocking(move || execute(call))
            .await
            .context("Fs 文件操作后台任务失败")?;
        emit_lock_events(&response).await;
        outcome(handlers::annotate_workdir(response, &access))
    }
}

/// 宿主注入的会话工作目录（缺失时由各操作报「工作目录未注入」）。
fn access_of(context: Option<&RequestInvocationContext>) -> FsAccessContext {
    FsAccessContext {
        workspace: context
            .map(|context| context.workspace.trim())
            .filter(|workspace| !workspace.is_empty())
            .map(String::from),
        full_trust: false,
    }
}

/// 执行一次文件操作（阻塞 IO）。
fn execute(call: FsCall) -> FsToolResponse {
    match call {
        FsCall::List(request) => handlers::handle_list_dir(request),
        FsCall::Tree(request) => handlers::handle_tree_dir(request),
        FsCall::Read(request) => handlers::handle_read_file(request),
        FsCall::Image(request) => read_image::handle_read_image(request),
        FsCall::Write(request) => handlers::handle_write_file(request),
        FsCall::Replace(request) => handlers::handle_replace_in_file(request),
        FsCall::Patch(request) => handlers::handle_apply_patch(request),
        FsCall::CurrentTime => current_time(),
    }
}

/// 写操作的加锁/解锁经进度通道上报 `file_lock_changed` 事件（宿主按
/// StreamEvent 解析转发，供锁面板显示）。辅助反馈，失败静默。
async fn emit_lock_events(response: &FsToolResponse) {
    for (paths, action) in [
        (&response.locked_paths, "locked"),
        (&response.unlocked_paths, "unlocked"),
    ] {
        for path in paths {
            let event = serde_json::json!({
                "type": "file_lock_changed",
                "path": path,
                "holder_agent_id": null,
                "holder_agent_label": null,
                "action": action,
            });
            tiangong_plugin_sidecar::emit_progress(event.to_string()).await;
        }
    }
}

/// 工具结果（ToolOutcome 形状，锁路径只用于事件，不进入结果）。
fn outcome(response: FsToolResponse) -> Result<Value> {
    Ok(serde_json::json!({
        "ok": response.ok,
        "summary": response.summary,
        "stdout": response.stdout,
        "stderr": response.stderr,
        "exit_code": response.exit_code,
    }))
}

/// 参数校验失败：失败的工具结果（不执行操作）。
fn invalid_argument(message: &str) -> FsToolResponse {
    FsToolResponse {
        ok: false,
        summary: message.to_string(),
        stderr: message.to_string(),
        exit_code: 1,
        ..Default::default()
    }
}

/// current_time：本机本地时间（含时区偏移）。
fn current_time() -> FsToolResponse {
    let now = chrono::Local::now();
    let output = serde_json::json!({
        "unix_timestamp": now.timestamp(),
        "rfc3339": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        "local_time": now.format("%Y-%m-%d %H:%M:%S").to_string(),
        "timezone_offset": now.format("%:z").to_string(),
    });
    FsToolResponse {
        ok: true,
        summary: format!(
            "当前本地时间：{}（UTC{}）",
            now.format("%Y-%m-%d %H:%M:%S"),
            now.format("%:z")
        ),
        stdout: serde_json::to_string_pretty(&output).unwrap_or_default(),
        exit_code: 0,
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for FsService {
    async fn dispatch(&self, request: Request) -> Response {
        let context = tiangong_plugin_sidecar::invocation_context();
        FsService::dispatch(self, request, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(workspace: &str) -> RequestInvocationContext {
        RequestInvocationContext {
            session_id: "s".into(),
            invocation_id: "i".into(),
            workspace: workspace.into(),
            actor_id: String::new(),
            deadline_ms: None,
        }
    }

    async fn call(operation: &str, payload: Value, workspace: Option<&str>) -> Response {
        let ctx = workspace.map(context);
        FsService::new()
            .unwrap()
            .dispatch(Request::new(operation, payload), ctx)
            .await
    }

    #[tokio::test]
    async fn 握手声明三个工具能力() {
        let response = call(HANDSHAKE_OPERATION, Value::Null, None).await;
        let capabilities = &response.payload.unwrap()["capabilities"];
        assert_eq!(
            capabilities,
            &serde_json::json!(["tool:file_read", "tool:file_write", "tool:current_time"])
        );
    }

    #[tokio::test]
    async fn 工具调用按宿主上下文的工作目录执行并返回结果形状() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let workspace = dir.path().to_str().unwrap();
        let response = call(
            TOOL_FILE_READ,
            serde_json::json!({"action":"read","path":"a.txt"}),
            Some(workspace),
        )
        .await;
        assert!(response.success);
        let payload = response.payload.unwrap();
        assert_eq!(payload["ok"], true);
        assert!(payload["stdout"].as_str().unwrap().contains("hello"));
        assert!(
            payload["summary"]
                .as_str()
                .unwrap()
                .starts_with("工作目录：")
        );
        assert!(
            payload.get("locked_paths").is_none(),
            "锁路径不进入工具结果"
        );

        let written = call(
            TOOL_FILE_WRITE,
            serde_json::json!({"action":"write","path":"b.txt","content":"x"}),
            Some(workspace),
        )
        .await
        .payload
        .unwrap();
        assert_eq!(written["ok"], true);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "x"
        );
    }

    #[tokio::test]
    async fn 缺少工作目录或参数错误时返回失败结果() {
        let missing = call(TOOL_FILE_READ, serde_json::json!({"action":"list"}), None)
            .await
            .payload
            .unwrap();
        assert_eq!(missing["ok"], false);
        assert!(
            missing["summary"]
                .as_str()
                .unwrap()
                .contains("工作目录未注入")
        );

        let invalid = call(TOOL_FILE_READ, serde_json::json!({}), Some("/tmp"))
            .await
            .payload
            .unwrap();
        assert_eq!(invalid["ok"], false);
        assert!(
            invalid["summary"]
                .as_str()
                .unwrap()
                .contains("缺少必填参数 action")
        );
    }

    #[tokio::test]
    async fn current_time_返回本地时间与偏移() {
        let payload = call(TOOL_CURRENT_TIME, Value::Null, None)
            .await
            .payload
            .unwrap();
        assert_eq!(payload["ok"], true);
        let stdout: Value = serde_json::from_str(payload["stdout"].as_str().unwrap()).unwrap();
        assert!(stdout["unix_timestamp"].as_i64().unwrap() > 0);
        assert!(stdout["timezone_offset"].as_str().unwrap().contains(':'));
    }

    #[tokio::test]
    async fn 未知操作报错() {
        let response = call("fs.read_file", Value::Null, Some("/tmp")).await;
        assert!(!response.success);
    }
}
