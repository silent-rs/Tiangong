//! sidecar 协议入口：版本校验、操作分发与请求级取消。

use std::io::Write;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use portable_pty::PtySize;
use tiangong_plugin_sidecar::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, PROTOCOL_VERSION, Request, RequestInvocationContext, Response,
};

use super::tools::tool_scope;
use super::types::{
    AcquireRequest, CloseRequest, ExecRequest, FindRequest, InstanceClosedRequest, InstanceEntry,
    ListByScopeRequest, ListInstancesRequest, ListInstancesResponse, OkResponse, ResizeRequest,
    ScreenUpdateRequest, SendRequest, SessionIdRequest, SpawnRequest, TerminalBindingRequest,
    TerminalGcRequest, WriteRequest,
};
use super::{SessionPhase, TerminalService};

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for TerminalService {
    async fn dispatch(&self, request: Request) -> Response {
        let context = tiangong_plugin_sidecar::invocation_context();
        self.dispatch_with_context(request, context).await
    }

    /// 请求级取消：宿主工具超时或会话取消经 Cancel 帧到达（stdio runner
    /// 已丢弃执行 future），从独立保留的映射定位具体 PTY，发送 Ctrl+C
    /// 中断正在执行的命令；经就绪探针确认登录 shell 重新接管后才放回
    /// 空闲池，确认失败直接关闭不放回（Ctrl+C 只送达信号，带子进程树的
    /// 长任务收尾需要时间，立即置 Idle 会让下一次调用在未收尾的终端上
    /// 握手，探针进入残留进程的 stdin，marker 永不回显）。
    async fn cancel(&self, request: &Request) -> Result<()> {
        let request_id = request.request_id.as_str();
        let active = self
            .active_tools
            .lock()
            .expect("活动工具表锁损坏")
            .remove(request_id);
        let Some(active) = active else {
            // 非执行型调用（send 的画面等待等）：等待 future 已被 runner
            // 丢弃，无 PTY 资源需要释放。
            return Ok(());
        };
        let terminal_id = active.terminal_id;
        // 与可能仍在丢弃收尾中的执行 future 串行化，避免写交错；future
        // 被 runner 丢弃后锁即释放，不会久等。
        let exec_lock =
            match self.with_session(&terminal_id, |session| Ok(Arc::clone(&session.exec_lock))) {
                Ok(exec_lock) => exec_lock,
                Err(error) => {
                    // 终端已退出（会话结束）等场景：无需中断，登记已清除。
                    tracing::debug!(
                        request_id,
                        terminal = %terminal_id,
                        %error,
                        "取消时终端已不可写"
                    );
                    return Ok(());
                }
            };
        let _guard = exec_lock.lock().await;
        // 按取消时刻的执行进度收尾，不能只凭请求的 interactive 参数：
        //
        // - Reserved：命令尚未写入 PTY（前端附着等待等阶段），终端干净，
        //   直接放回空闲池，无需中断确认——否则预留永远挂着不被复用；
        // - 交互程序已写入（interactive_started）：程序在前台且对用户
        //   可见，保留终端并转交互状态（探针进不了交互程序，确认流只会
        //   误杀）；由用户在标签内自行收尾；
        // - 更早进入的 Interactive：同上保持不动；
        // - 其余（命令执行中、交互调用的握手阶段）：走中断确认流。
        let state = self.with_session(&terminal_id, |session| {
            Ok((session.phase, session.interactive_started))
        });
        let (phase, interactive_started) = match state {
            Ok(state) => state,
            Err(error) => {
                // 拿到 exec_lock 后终端才被回收：无需中断，登记已清除。
                tracing::debug!(
                    request_id,
                    terminal = %terminal_id,
                    %error,
                    "取消时终端已不可用"
                );
                return Ok(());
            }
        };
        match phase {
            SessionPhase::Reserved => {
                let _ = self.with_session(&terminal_id, |session| {
                    session.phase = SessionPhase::Idle;
                    session.running_command = None;
                    Ok(())
                });
                tracing::info!(
                    request_id,
                    terminal = %terminal_id,
                    "取消发生在命令启动前，终端直接回到空闲"
                );
                return Ok(());
            }
            SessionPhase::Interactive => {
                tracing::info!(
                    request_id,
                    terminal = %terminal_id,
                    "取消交互调用：保留交互终端，不做中断确认"
                );
                return Ok(());
            }
            _ if active.interactive && interactive_started => {
                let _ = self.with_session(&terminal_id, |session| {
                    session.phase = SessionPhase::Interactive;
                    Ok(())
                });
                tracing::info!(
                    request_id,
                    terminal = %terminal_id,
                    "取消交互调用：程序保留在前台，终端转入交互状态"
                );
                return Ok(());
            }
            _ => {}
        }
        let interrupted = self.with_session(&terminal_id, |session| {
            session
                .writer
                .write_all(b"\x03")
                .context("取消写入中断输入失败")?;
            session.writer.flush().context("取消刷新中断输入失败")
        });
        if let Err(error) = interrupted {
            // 终端已退出（会话结束）等场景：无需中断，登记已清除。
            tracing::debug!(
                request_id,
                terminal = %terminal_id,
                %error,
                "取消时终端已不可写"
            );
            return Ok(());
        }
        // 就绪握手复用命令前置路径：shell_ready 终端会再发一次 Ctrl+C
        // 清理残留输入并用探针确认提示符已恢复。确认成功才放回复用池；
        // 失败时超时路径已把终端标记为 Unresponsive（终端仍存活，后台
        // 作业和用户输入不受影响），不放回池即可。
        match self.prepare_shell_for_command(&terminal_id).await {
            Ok(()) => {
                let _ = self.with_session(&terminal_id, |session| {
                    session.phase = SessionPhase::Idle;
                    session.running_command = None;
                    Ok(())
                });
                tracing::info!(
                    request_id,
                    terminal = %terminal_id,
                    "已按取消请求中断终端命令并确认 Shell 就绪"
                );
            }
            Err(error) => {
                tracing::warn!(
                    request_id,
                    terminal = %terminal_id,
                    %error,
                    "取消后 Shell 未在预算内回到提示符，终端保留但不再自动复用"
                );
            }
        }
        Ok(())
    }
}

async fn dispatch_operation(
    service: &TerminalService,
    request_id: &str,
    operation: &str,
    payload: serde_json::Value,
    context: Option<&RequestInvocationContext>,
) -> Result<serde_json::Value> {
    // Agent 工具入口（宿主直连）：会话归属来自宿主注入的请求上下文，
    // 缺失即拒绝——不允许脱离会话真相源执行终端操作。
    if matches!(
        operation,
        "run_command"
            | "run_shell"
            | "terminal_open"
            | "terminal_send"
            | "terminal_close"
            | "terminal_status"
    ) {
        let scope = tool_scope(request_id, context)?;
        let outcome = match operation {
            "run_command" => service.tool_run_command(&scope, false, payload).await,
            "run_shell" => service.tool_run_command(&scope, true, payload).await,
            "terminal_open" => service.tool_terminal_open(&scope).await,
            "terminal_send" => service.tool_terminal_send(&scope, payload).await,
            "terminal_status" => service.tool_terminal_status(&scope, payload),
            _ => service.tool_terminal_close(&scope, payload).await,
        };
        return Ok(serde_json::to_value(outcome)?);
    }
    match operation {
        HANDSHAKE_OPERATION => Ok(serde_json::json!({
            "plugin_id": "terminal",
            "plugin_version": env!("CARGO_PKG_VERSION"),
            "sidecar_version": env!("CARGO_PKG_VERSION"),
            "protocol_version": PROTOCOL_VERSION,
            "business_protocol": 1,
            "capabilities": [
                "tool:run_command",
                "tool:run_shell",
                "tool:terminal_open",
                "tool:terminal_send",
                "tool:terminal_close",
                "tool:terminal_status",
            ],
            "instance_id": format!("terminal-sidecar-{}", std::process::id()),
            "status": "ready",
        })),
        "terminalSpawn" => {
            let request: SpawnRequest =
                serde_json::from_value(payload).context("terminalSpawn 参数无效")?;
            Ok(serde_json::to_value(service.spawn_session(request)?)?)
        }
        "terminalAcquire" => {
            let request: AcquireRequest =
                serde_json::from_value(payload).context("terminalAcquire 参数无效")?;
            Ok(serde_json::to_value(service.acquire_session(request))?)
        }
        "terminalRelease" => {
            let request: SessionIdRequest =
                serde_json::from_value(payload).context("terminalRelease 参数无效")?;
            Ok(serde_json::to_value(service.release_session(request)?)?)
        }
        "terminalExec" => {
            let request: ExecRequest =
                serde_json::from_value(payload).context("terminalExec 参数无效")?;
            Ok(serde_json::to_value(
                service.exec_in_session(request).await?,
            )?)
        }
        "terminalWrite" => {
            let request: WriteRequest =
                serde_json::from_value(payload).context("terminalWrite 参数无效")?;
            service.with_session(&request.session_id, |session| {
                session
                    .writer
                    .write_all(request.data.as_bytes())
                    .context("写入 PTY 失败")?;
                session.writer.flush().context("刷新 PTY 失败")
            })?;
            Ok(serde_json::to_value(OkResponse { ok: true })?)
        }
        "terminalSend" => {
            let request: SendRequest =
                serde_json::from_value(payload).context("terminalSend 参数无效")?;
            Ok(serde_json::to_value(
                service.send_to_session(request).await?,
            )?)
        }
        "terminalAttach" => {
            let request: TerminalBindingRequest =
                serde_json::from_value(payload).context("终端附着状态参数无效")?;
            Ok(serde_json::to_value(service.attach_frontend(request)?)?)
        }
        "terminalScreenUpdate" => {
            let request: ScreenUpdateRequest =
                serde_json::from_value(payload).context("terminalScreenUpdate 参数无效")?;
            Ok(serde_json::to_value(service.update_screen(request)?)?)
        }
        "terminalResize" => {
            let request: ResizeRequest =
                serde_json::from_value(payload).context("terminalResize 参数无效")?;
            service.with_session(&request.session_id, |session| {
                session
                    .master
                    .resize(PtySize {
                        rows: request.rows,
                        cols: request.cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .context("调整 PTY 尺寸失败")
            })?;
            Ok(serde_json::to_value(OkResponse { ok: true })?)
        }
        "terminalKill" => {
            let request: SessionIdRequest =
                serde_json::from_value(payload).context("terminalKill 参数无效")?;
            Ok(serde_json::to_value(service.kill_session(request)?)?)
        }
        "terminalClose" => {
            let request: CloseRequest =
                serde_json::from_value(payload).context("terminalClose 参数无效")?;
            Ok(serde_json::to_value(service.close_session(request)?)?)
        }
        "terminalGc" => {
            let request: TerminalGcRequest =
                serde_json::from_value(payload).context("terminalGc 参数无效")?;
            Ok(serde_json::to_value(service.gc_terminals(request)?)?)
        }
        // 宿主统一实例生命周期：标签移除后宿主保证发出一次（页面未就绪或
        // 已卸载时 beforeClose 无人接收，也由此回收）。幂等，归属校验同
        // terminalClose——实例编号与终端编号一一对应。
        "instanceClosed" => {
            let request: InstanceClosedRequest =
                serde_json::from_value(payload).context("instanceClosed 参数无效")?;
            let scope_id = request.session_id.trim().to_string();
            let instance_id = request.instance_id.trim().to_string();
            if scope_id.is_empty() || instance_id.is_empty() {
                bail!("instanceClosed 需要有效的 session_id 与 instance_id");
            }
            Ok(serde_json::to_value(service.close_session(
                CloseRequest {
                    session_id: Some(instance_id),
                    scope_id,
                },
            )?)?)
        }
        // 宿主统一实例核查：列出会话下持有资源的实例（切换会话恢复标签、
        // 核查多余资源、逻辑删除会话时释放）。
        "listInstances" => {
            let request: ListInstancesRequest =
                serde_json::from_value(payload).context("listInstances 参数无效")?;
            let listed = service.list_by_scope(&ListByScopeRequest {
                scope_id: request.session_id,
            })?;
            Ok(serde_json::to_value(ListInstancesResponse {
                instances: listed
                    .terminals
                    .into_iter()
                    .map(|entry| InstanceEntry {
                        instance_id: entry.session_id,
                    })
                    .collect(),
            })?)
        }
        "terminalFind" => {
            let request: FindRequest =
                serde_json::from_value(payload).context("terminalFind 参数无效")?;
            Ok(serde_json::to_value(service.find_by_scope(&request))?)
        }
        "terminalListByScope" => {
            let request: ListByScopeRequest =
                serde_json::from_value(payload).context("terminalListByScope 参数无效")?;
            Ok(serde_json::to_value(service.list_by_scope(&request)?)?)
        }
        other => bail!("未知操作: {other}"),
    }
}

impl TerminalService {
    pub(super) async fn dispatch_with_context(
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
                    "terminal 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }
        if request.operation != HANDSHAKE_OPERATION {
            self.mark_activity();
        }
        let payload = match dispatch_operation(
            self,
            &request_id,
            &request.operation,
            request.payload.clone(),
            context.as_ref(),
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return Response::error(
                    &request_id,
                    ErrorCode::ServiceError,
                    format!("{error:#}"),
                    false,
                );
            }
        };
        Response::success(&request_id, payload)
    }
}
