use serde::Serialize;

use crate::bindings::tiangong::plugin::sidecar;
use tiangong_plugin_volcengine_protocol::VolcengineOperation;

#[derive(Debug)]
pub enum ClientError {
    NotConfigured,
    Unavailable(String),
    Timeout,
    PermissionDenied,
    ProtocolMismatch(String),
    Internal(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => formatter.write_str("sidecar 未配置"),
            Self::Unavailable(message) => write!(formatter, "sidecar 不可用: {message}"),
            Self::Timeout => formatter.write_str("sidecar 请求超时"),
            Self::PermissionDenied => formatter.write_str("sidecar 权限不足"),
            Self::ProtocolMismatch(message) => {
                write!(formatter, "sidecar 协议不兼容: {message}")
            }
            Self::Internal(message) => write!(formatter, "{message}"),
        }
    }
}

pub fn invoke<O>(request: &O::Request) -> Result<O::Response, ClientError>
where
    O: VolcengineOperation,
    O::Request: Serialize,
{
    let payload = serde_json::to_string(request)
        .map_err(|error| ClientError::Internal(format!("序列化 {} 请求失败: {error}", O::NAME)))?;
    let response = sidecar::invoke(O::NAME, &payload).map_err(map_transport_error)?;
    serde_json::from_str(&response)
        .map_err(|error| ClientError::Internal(format!("解析 {} 响应失败: {error}", O::NAME)))
}

fn map_transport_error(error: sidecar::SidecarError) -> ClientError {
    match error {
        sidecar::SidecarError::NotConfigured => ClientError::NotConfigured,
        sidecar::SidecarError::Unavailable(message) => ClientError::Unavailable(message),
        sidecar::SidecarError::Timeout => ClientError::Timeout,
        sidecar::SidecarError::PermissionDenied => ClientError::PermissionDenied,
        sidecar::SidecarError::ProtocolMismatch(message) => ClientError::ProtocolMismatch(message),
        sidecar::SidecarError::Internal(message) => ClientError::Internal(message),
    }
}
