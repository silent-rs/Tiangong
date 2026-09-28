//! Computer Use sidecar 业务服务：按操作名分发请求到平台后端。

use std::sync::RwLock;

use anyhow::{Context, Result};

use tiangong_plugin_computer_use_protocol::ops::{
    self, ActionRequest, ActionResponse, CAPABILITY, DESKTOP_ACTION_OPERATION,
    DESKTOP_FIND_OPERATION, DESKTOP_KEYBOARD_OPERATION, DESKTOP_LIST_WINDOWS_OPERATION,
    DESKTOP_MOUSE_OPERATION, DESKTOP_SNAPSHOT_OPERATION, DESKTOP_STATUS_OPERATION,
    DESKTOP_WAIT_OPERATION, DesktopStatusResponse, FindResponse, KeyboardResponse,
    ListWindowsResponse, MouseResponse, SET_ACCESS_OPERATION, SetAccessRequest, SnapshotResponse,
    VIRTUAL_CURSOR_OPERATION, VirtualCursorRequest, VirtualCursorResponse, WaitResponse,
};
use tiangong_plugin_computer_use_protocol::{
    Ack, COMPUTER_USE_PROTOCOL_VERSION, DesktopResult, PLUGIN_ID, PLUGIN_VERSION,
};
use tiangong_plugin_runtime::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request, Response,
    ServiceStatus,
};

use crate::backend::{self, Backend};

/// Computer Use sidecar 业务服务。
pub struct ComputerUseService {
    /// 平台无障碍后端。
    backend: Box<dyn Backend>,
    /// 当前会话是否完全信任模式（由 WASM 注入）。
    /// 注意：监督模式下的动作审批由宿主（Core）在工具调用层统一处理，
    /// sidecar 当前不做二次判断；此字段保留供未来在 sidecar 层增加细粒度限制。
    full_trust: RwLock<bool>,
}

impl ComputerUseService {
    /// 构造当前平台的后端实例。
    pub fn new() -> Result<Self> {
        Ok(Self {
            backend: backend::current_backend(),
            full_trust: RwLock::new(false),
        })
    }

    /// 按 sidecar 协议分发请求。
    pub async fn dispatch(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "Computer Use 协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
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
                business_protocol: COMPUTER_USE_PROTOCOL_VERSION,
                capabilities: vec![CAPABILITY.to_string()],
                instance_id: format!("computer-use-sidecar-{}", std::process::id()),
                status: ServiceStatus::Ready,
            })
            .with_context(|| "序列化 Computer Use 握手响应失败"),

            DESKTOP_STATUS_OPERATION => {
                let result = self.backend.status().await;
                serde_json::to_value(map_status(result, self.backend.platform()))
                    .with_context(|| "序列化 desktop_status 响应失败")
            }

            DESKTOP_LIST_WINDOWS_OPERATION => {
                let req: ops::ListWindowsRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 desktop_list_windows 请求失败")?;
                let result = self.backend.list_windows(&req).await;
                serde_json::to_value(map_list_windows(result))
                    .with_context(|| "序列化 desktop_list_windows 响应失败")
            }

            DESKTOP_SNAPSHOT_OPERATION => {
                let req: ops::SnapshotRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 desktop_snapshot 请求失败")?;
                let result = self.backend.snapshot(&req).await;
                serde_json::to_value(map_snapshot(result))
                    .with_context(|| "序列化 desktop_snapshot 响应失败")
            }

            DESKTOP_FIND_OPERATION => {
                let req: ops::FindRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 desktop_find 请求失败")?;
                let result = self.backend.find(&req).await;
                serde_json::to_value(map_find(result))
                    .with_context(|| "序列化 desktop_find 响应失败")
            }

            DESKTOP_ACTION_OPERATION => {
                let req: ActionRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 desktop_action 请求失败")?;
                let result = self.backend.action(&req).await;
                serde_json::to_value(map_action(result))
                    .with_context(|| "序列化 desktop_action 响应失败")
            }

            DESKTOP_WAIT_OPERATION => {
                let req: ops::WaitRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 desktop_wait 请求失败")?;
                let result = self.backend.wait(&req).await;
                serde_json::to_value(map_wait(result))
                    .with_context(|| "序列化 desktop_wait 响应失败")
            }

            ops::DESKTOP_SCREENSHOT_OPERATION => {
                let req: ops::ScreenshotRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 desktop_screenshot 请求失败")?;
                let result = self.backend.screenshot(&req).await;
                serde_json::to_value(result).with_context(|| "序列化 desktop_screenshot 响应失败")
            }

            ops::DESKTOP_OPEN_APP_OPERATION => {
                let req: ops::OpenAppRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 desktop_open_app 请求失败")?;
                let result = self.backend.open_app(&req).await;
                serde_json::to_value(result).with_context(|| "序列化 desktop_open_app 响应失败")
            }
            DESKTOP_MOUSE_OPERATION => {
                let req: ops::MouseRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 desktop_mouse 请求失败")?;
                let result = self.backend.mouse(&req).await;
                serde_json::to_value(map_mouse(result))
                    .with_context(|| "序列化 desktop_mouse 响应失败")
            }
            DESKTOP_KEYBOARD_OPERATION => {
                let req: ops::KeyboardRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 desktop_keyboard 请求失败")?;
                let result = self.backend.keyboard(&req).await;
                serde_json::to_value(map_keyboard(result))
                    .with_context(|| "序列化 desktop_keyboard 响应失败")
            }
            VIRTUAL_CURSOR_OPERATION => {
                let req: VirtualCursorRequest = serde_json::from_value(payload)
                    .with_context(|| "解析 virtual_cursor 请求失败")?;
                // 指针开关是纯 UI 状态：直接投递 overlay，立即回显生效值。
                apply_virtual_cursor(req.enabled)?;
                serde_json::to_value(DesktopResult::Ok(VirtualCursorResponse {
                    enabled: req.enabled,
                }))
                .with_context(|| "序列化 virtual_cursor 响应失败")
            }
            SET_ACCESS_OPERATION => {
                let req: SetAccessRequest =
                    serde_json::from_value(payload).with_context(|| "解析 set_access 请求失败")?;
                if let Ok(mut guard) = self.full_trust.write() {
                    *guard = req.full_trust;
                }
                serde_json::to_value(Ack {}).with_context(|| "序列化 set_access 响应失败")
            }

            ops::SPLIT_STATE_OPERATION => serde_json::to_value(crate::split::state())
                .with_context(|| "序列化 split_state 响应失败"),

            ops::SPLIT_TURN_OPERATION => {
                let req: ops::SplitTurnRequest =
                    serde_json::from_value(payload).with_context(|| "解析 split_turn 请求失败")?;
                let state = if req.finished {
                    crate::split::turn_finished(&req.session_id, req.used_split)
                } else {
                    crate::split::turn_started(&req.session_id)
                };
                serde_json::to_value(state).with_context(|| "序列化 split_turn 响应失败")
            }

            ops::RESTORE_HOST_WINDOW_OPERATION => {
                let response = match crate::split::saved_host() {
                    None => ops::RestoreHostWindowResponse {
                        restored: false,
                        detail: "天工窗口当前不在分屏布局，无需恢复".to_string(),
                        state: crate::split::clear(),
                    },
                    Some(saved) => match self.backend.restore_host_window(saved).await {
                        Ok(()) => ops::RestoreHostWindowResponse {
                            restored: true,
                            detail: "已恢复天工窗口".to_string(),
                            state: crate::split::clear(),
                        },
                        // 失败时保留状态，按钮继续显示，用户可重试。
                        Err(reason) => ops::RestoreHostWindowResponse {
                            restored: false,
                            detail: format!("恢复天工窗口失败：{reason}"),
                            state: crate::split::state(),
                        },
                    },
                };
                serde_json::to_value(response)
                    .with_context(|| "序列化 restore_host_window 响应失败")
            }

            operation => Err(anyhow::anyhow!("不支持的 Computer Use 操作: {operation}")),
        }
    }
}

/// 虚拟指针开关（RFC 0018）：macOS / Windows 投递 overlay；Linux 尚无
/// overlay，返回明确错误（wasm 侧轮次结束收起时忽略该错误）。按平台拆成
/// 多个实现，避免在同一函数体内 `bail!` 后留下不可达代码（-D warnings）。
#[cfg(target_os = "macos")]
fn apply_virtual_cursor(enabled: bool) -> anyhow::Result<()> {
    crate::backend::overlay::set_enabled(enabled);
    Ok(())
}

#[cfg(target_os = "windows")]
fn apply_virtual_cursor(enabled: bool) -> anyhow::Result<()> {
    crate::backend::win_overlay::set_enabled(enabled);
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn apply_virtual_cursor(_enabled: bool) -> anyhow::Result<()> {
    anyhow::bail!("virtual_cursor 仅 macOS / Windows 支持")
}

// ── backend Info → 协议 Response 的映射 ────────────────────────

fn map_status(
    result: DesktopResult<crate::backend::StatusInfo>,
    platform: tiangong_plugin_computer_use_protocol::Platform,
) -> DesktopResult<DesktopStatusResponse> {
    match result {
        DesktopResult::Ok(info) => DesktopResult::Ok(DesktopStatusResponse {
            platform,
            session: info.session,
            accessibility: info.accessibility,
            supported_actions: info.supported_actions,
        }),
        DesktopResult::Err(error) => DesktopResult::Err(error),
    }
}

fn map_list_windows(
    result: DesktopResult<tiangong_plugin_computer_use_protocol::ListWindowsResponse>,
) -> DesktopResult<ListWindowsResponse> {
    // backend 返回的 ListWindowsResponse 与协议 ListWindowsResponse 同类型。
    result
}

fn map_snapshot(
    result: DesktopResult<crate::backend::SnapshotInfo>,
) -> DesktopResult<SnapshotResponse> {
    match result {
        DesktopResult::Ok(info) => DesktopResult::Ok(SnapshotResponse {
            snapshot: info.snapshot,
            nodes: info.nodes,
            truncated: info.truncated,
            warnings: info.warnings,
        }),
        DesktopResult::Err(error) => DesktopResult::Err(error),
    }
}

fn map_find(result: DesktopResult<crate::backend::FindInfo>) -> DesktopResult<FindResponse> {
    match result {
        DesktopResult::Ok(info) => DesktopResult::Ok(FindResponse {
            matches: info.matches,
            snapshot: info.snapshot,
            ambiguous: info.ambiguous,
        }),
        DesktopResult::Err(error) => DesktopResult::Err(error),
    }
}

fn map_action(
    result: DesktopResult<crate::backend::ActionResult>,
) -> DesktopResult<ActionResponse> {
    match result {
        DesktopResult::Ok(info) => DesktopResult::Ok(ActionResponse {
            performed: info.performed,
            summary: info.summary,
            new_window: info.new_window,
        }),
        DesktopResult::Err(error) => DesktopResult::Err(error),
    }
}

fn map_mouse(result: DesktopResult<crate::backend::MouseResult>) -> DesktopResult<MouseResponse> {
    match result {
        DesktopResult::Ok(info) => DesktopResult::Ok(MouseResponse {
            performed: info.performed,
            summary: info.summary,
        }),
        DesktopResult::Err(error) => DesktopResult::Err(error),
    }
}

fn map_keyboard(
    result: DesktopResult<crate::backend::KeyboardResult>,
) -> DesktopResult<KeyboardResponse> {
    match result {
        DesktopResult::Ok(info) => DesktopResult::Ok(KeyboardResponse {
            performed: info.performed,
            summary: info.summary,
        }),
        DesktopResult::Err(error) => DesktopResult::Err(error),
    }
}
fn map_wait(result: DesktopResult<crate::backend::WaitResult>) -> DesktopResult<WaitResponse> {
    match result {
        DesktopResult::Ok(info) => DesktopResult::Ok(WaitResponse {
            satisfied: info.satisfied,
            waited_ms: info.waited_ms,
            matched_element: info.matched_element,
            detail: info.detail,
        }),
        DesktopResult::Err(error) => DesktopResult::Err(error),
    }
}

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for ComputerUseService {
    async fn dispatch(&self, request: Request) -> Response {
        ComputerUseService::dispatch(self, request).await
    }
}
