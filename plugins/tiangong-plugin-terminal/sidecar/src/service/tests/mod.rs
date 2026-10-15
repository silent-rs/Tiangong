//! sidecar 服务测试：各职责模块的纯逻辑单测就近放在模块内，本目录收纳
//! 需要真实 PTY 与分发入口的跨模块集成测试。

#![allow(unused_imports)]

pub(super) use std::collections::HashMap;
pub(super) use std::sync::{Arc, Mutex};
pub(super) use std::time::{Duration, Instant};

pub(super) use tiangong_plugin_runtime::protocol::{
    PROTOCOL_VERSION, Request, RequestInvocationContext, Response,
};
pub(super) use tiangong_plugin_sidecar::SidecarService as _;

pub(super) use super::output::contains_marker;
pub(super) use super::types::{
    AcquireRequest, FindRequest, SessionIdRequest, SpawnRequest, TerminalBindingRequest,
    default_cols, default_rows,
};
pub(super) use super::{ActiveTool, SessionPhase, TerminalService};

mod cancel;
mod frontend;
mod send;
mod tools;

#[derive(Clone)]
pub(super) struct ScopedRequest {
    request: Request,
    context: Option<RequestInvocationContext>,
}

impl TerminalService {
    pub(super) async fn dispatch_test(&self, scoped: ScopedRequest) -> Response {
        self.dispatch_with_context(scoped.request, scoped.context)
            .await
    }
}

/// 构造带宿主权威上下文的工具请求（scope 为 None 即缺上下文场景）。
pub(super) fn tool_request(
    operation: &str,
    payload: serde_json::Value,
    scope: Option<(&str, &str)>,
) -> ScopedRequest {
    let request_id = format!("test-{}", scru128::new());
    ScopedRequest {
        request: Request {
            protocol_version: PROTOCOL_VERSION.to_string(),
            request_id: request_id.clone(),
            operation: operation.to_string(),
            payload,
        },
        context: scope.map(|(session_id, workspace)| RequestInvocationContext {
            session_id: session_id.to_string(),
            invocation_id: request_id,
            workspace: workspace.to_string(),
            actor_id: "test-agent".to_string(),
            deadline_ms: None,
        }),
    }
}

pub(super) fn outcome_of(response: Response) -> serde_json::Value {
    assert!(response.success, "工具调用失败: {response:?}");
    response.payload.expect("成功响应必须带 payload")
}
