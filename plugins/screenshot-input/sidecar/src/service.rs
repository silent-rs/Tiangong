use anyhow::Context;
use tiangong_plugin_screenshot_input_protocol::{
    CAPTURE_OPERATION, PLUGIN_ID, PLUGIN_VERSION, SCREENSHOT_PROTOCOL_VERSION,
};
use tiangong_plugin_sidecar::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, PROTOCOL_VERSION, Request, Response,
    ServiceStatus,
};

pub struct ScreenshotService;

#[async_trait::async_trait]
impl tiangong_plugin_sidecar::SidecarService for ScreenshotService {
    async fn dispatch(&self, request: Request) -> Response {
        let request_id = request.request_id.clone();
        if request.protocol_version != PROTOCOL_VERSION {
            return Response::error(
                &request_id,
                ErrorCode::ProtocolMismatch,
                format!(
                    "截图协议版本不匹配: expected={PROTOCOL_VERSION}, actual={}",
                    request.protocol_version
                ),
                false,
            );
        }

        let result = match request.operation.as_str() {
            HANDSHAKE_OPERATION => serde_json::to_value(HandshakeResponse {
                plugin_id: PLUGIN_ID.to_string(),
                plugin_version: PLUGIN_VERSION.to_string(),
                sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: PROTOCOL_VERSION.to_string(),
                business_protocol: SCREENSHOT_PROTOCOL_VERSION,
                capabilities: vec!["capture_region".to_string()],
                instance_id: format!("screenshot-input-sidecar-{}", std::process::id()),
                status: ServiceStatus::Ready,
            })
            .context("序列化截图握手响应失败"),
            CAPTURE_OPERATION => crate::capture::capture_region()
                .and_then(|response| serde_json::to_value(response).context("序列化截图响应失败")),
            other => Err(anyhow::anyhow!("未知的截图操作: {other}")),
        };

        match result {
            Ok(payload) => Response::success(&request_id, payload),
            Err(error) => Response::error(
                &request_id,
                ErrorCode::ServiceError,
                error.to_string(),
                false,
            ),
        }
    }
}
