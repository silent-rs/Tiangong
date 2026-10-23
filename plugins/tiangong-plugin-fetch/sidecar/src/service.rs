//! Fetch sidecar 业务服务：承载 reqwest 阻塞抓取、SSRF 防护与 download 落盘。
//!
//! 无 WASM 桥接层（v2 清单直连）：宿主按清单 `tools` 把 `web_fetch` 调用
//! 直接送入本进程（operation 为工具名、payload 为调用参数对象），这里解析
//! 参数、执行抓取并返回 ToolOutcome 形状（ok/summary/stdout/stderr/exit_code）。
//! 工作目录取自宿主注入的请求上下文（download 落盘基准）。

use std::path::PathBuf;

use anyhow::{Context, Result};

use serde_json::Value;
use tiangong_plugin_fetch_protocol::web_fetch::{WebFetchRequest, WebFetchResponse};
use tiangong_plugin_fetch_protocol::{
    ExtractMode, FETCH_PROTOCOL_VERSION, FetchMode, PLUGIN_ID, PLUGIN_VERSION, TOOL_WEB_FETCH,
};
use tiangong_plugin_sidecar::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request,
    RequestInvocationContext, Response, ServiceStatus,
};

use crate::fetch;

/// 宿主直连的工具（握手能力 `tool:<名>` 与清单 `tools` 一致）。
const TOOLS: &[&str] = &[TOOL_WEB_FETCH];

/// Fetch sidecar 业务服务。
pub struct FetchService {}

impl FetchService {
    /// 构造默认实例。
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }

    /// 按 sidecar 协议分发请求。
    ///
    /// 慢操作（HTTP 抓取 + 落盘）经 `spawn_blocking` 在独立线程执行，
    /// 避免阻塞其他连接的请求与健康检查。
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
                    "Fetch 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
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
        match operation {
            HANDSHAKE_OPERATION => serde_json::to_value(HandshakeResponse {
                plugin_id: PLUGIN_ID.to_string(),
                plugin_version: PLUGIN_VERSION.to_string(),
                sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: PROTOCOL_VERSION.to_string(),
                business_protocol: FETCH_PROTOCOL_VERSION,
                capabilities: TOOLS.iter().map(|tool| format!("tool:{tool}")).collect(),
                instance_id: format!("fetch-sidecar-{}", std::process::id()),
                status: ServiceStatus::Ready,
            })
            .with_context(|| "序列化 Fetch 握手响应失败"),
            TOOL_WEB_FETCH => {
                let request = match parse_web_fetch_args(&payload) {
                    Ok(request) => request,
                    Err(message) => {
                        let text = format!("{message:#}");
                        return outcome(failure(&text, &text));
                    }
                };
                let base = workspace_of(context).unwrap_or_else(|| PathBuf::from("."));
                let response =
                    tokio::task::spawn_blocking(move || fetch::execute(request, &base, false))
                        .await
                        .with_context(|| "web_fetch 后台任务失败")?;
                outcome(response)
            }
            operation => Err(anyhow::anyhow!("不支持的 Fetch 操作: {operation}")),
        }
    }
}

/// 宿主注入的会话工作目录（download 落盘基准；缺失时退回当前目录）。
fn workspace_of(context: Option<&RequestInvocationContext>) -> Option<PathBuf> {
    context
        .map(|context| context.workspace.trim())
        .filter(|workspace| !workspace.is_empty())
        .map(PathBuf::from)
}

/// 从工具调用参数解析 `WebFetchRequest`。
///
/// 与原 WASM 桥接层的宽松解析保持一致：数字字段接受字符串形式；未知
/// `mode` / `extract_mode` 回退默认值；`url` 缺失时返回错误文案。
fn parse_web_fetch_args(args: &Value) -> Result<WebFetchRequest> {
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if url.is_empty() {
        anyhow::bail!("web_fetch 缺少 url 参数");
    }
    let mode = match args.get("mode").and_then(Value::as_str) {
        Some("download") => FetchMode::Download,
        _ => FetchMode::Text,
    };
    let extract_mode = match args.get("extract_mode").and_then(Value::as_str) {
        Some("text") => ExtractMode::Text,
        Some("raw") => ExtractMode::Raw,
        _ => ExtractMode::Auto,
    };
    Ok(WebFetchRequest {
        url,
        mode,
        max_chars: opt_usize(args, "max_chars").unwrap_or(12_000),
        output_path: args
            .get("output_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
        overwrite: args
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        timeout_ms: opt_u64(args, "timeout_ms").unwrap_or(15_000),
        follow_redirects: args
            .get("follow_redirects")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        extract_mode,
    })
}

/// 读取可选数值参数（数字或数字字符串，与原 WASM 桥接一致）。
fn opt_usize(args: &Value, key: &str) -> Option<usize> {
    opt_u64(args, key).map(|value| value as usize)
}

fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    let value = args.get(key)?;
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

/// 工具结果（ToolOutcome 形状，与 `WebFetchResponse` 字段一一对应）。
fn outcome(response: WebFetchResponse) -> Result<Value> {
    serde_json::to_value(response).with_context(|| "序列化 web_fetch 响应失败")
}

/// 构造失败的工具结果。
fn failure(summary: &str, stderr: &str) -> WebFetchResponse {
    WebFetchResponse {
        ok: false,
        summary: summary.to_string(),
        stderr: stderr.to_string(),
        exit_code: 1,
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for FetchService {
    async fn dispatch(
        &self,
        request: tiangong_plugin_sidecar::protocol::Request,
    ) -> tiangong_plugin_sidecar::protocol::Response {
        let context = tiangong_plugin_sidecar::invocation_context();
        FetchService::dispatch(self, request, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiangong_plugin_sidecar::protocol::HANDSHAKE_OPERATION;

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
        FetchService::new()
            .unwrap()
            .dispatch(Request::new(operation, payload), ctx)
            .await
    }

    #[tokio::test]
    async fn 握手声明工具能力() {
        let response = call(HANDSHAKE_OPERATION, Value::Null, None).await;
        let capabilities = &response.payload.unwrap()["capabilities"];
        assert_eq!(capabilities, &serde_json::json!(["tool:web_fetch"]));
    }

    #[tokio::test]
    async fn 缺少_url_返回失败结果() {
        let response = call("web_fetch", serde_json::json!({}), None).await;
        let payload = response.payload.unwrap();
        assert_eq!(payload["ok"], serde_json::json!(false));
        assert!(payload["stderr"].as_str().unwrap().contains("url"));
    }

    #[test]
    fn 解析参数接受字符串数字与默认值() {
        let request = parse_web_fetch_args(
            &serde_json::json!({"url": "https://example.com", "max_chars": "8000"}),
        )
        .unwrap();
        assert_eq!(request.url, "https://example.com");
        assert_eq!(request.max_chars, 8000);
        assert_eq!(request.mode, FetchMode::Text);
        assert_eq!(request.extract_mode, ExtractMode::Auto);
        assert_eq!(request.timeout_ms, 15_000);
        assert!(request.follow_redirects);

        let request = parse_web_fetch_args(&serde_json::json!({
            "url": "https://example.com",
            "mode": "download",
            "extract_mode": "raw",
            "overwrite": true
        }))
        .unwrap();
        assert_eq!(request.mode, FetchMode::Download);
        assert_eq!(request.extract_mode, ExtractMode::Raw);
        assert!(request.overwrite);
    }

    #[test]
    fn 工作目录取自请求上下文() {
        assert_eq!(
            workspace_of(Some(&context("/tmp/demo"))),
            Some(PathBuf::from("/tmp/demo"))
        );
        assert_eq!(workspace_of(Some(&context("  "))), None);
        assert_eq!(workspace_of(None), None);
    }
}
