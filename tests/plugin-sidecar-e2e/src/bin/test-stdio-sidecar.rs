//! 测试用 echo sidecar：stdio 端到端测试的协议对端。
//!
//! dispatch 规则：`echo` 操作原样回显 payload；`crash` 操作直接退出进程
//! （验证宿主侧换代重启）；`handshake_count` 返回本进程握手次数（验证
//! 常驻进程只握手一次）；Unix 下 `hang` 启动后台进程后保持请求运行。
//! 环境变量 `TIANGONG_TEST_STDIO_MUTE=1` 时进入协议沉默（验证宿主侧
//! 握手有限时限）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use tiangong_plugin_sidecar::protocol::{
    ErrorCode, HANDSHAKE_OPERATION, HandshakeResponse, Request, Response, ServiceStatus,
};
use tiangong_plugin_sidecar::{SidecarConfig, SidecarService, run};

static HANDSHAKE_COUNT: AtomicU32 = AtomicU32::new(0);

struct EchoService;

#[async_trait]
impl SidecarService for EchoService {
    async fn dispatch(&self, request: Request) -> Response {
        match request.operation.as_str() {
            HANDSHAKE_OPERATION => {
                HANDSHAKE_COUNT.fetch_add(1, Ordering::SeqCst);
                Response::success(
                    &request.request_id,
                    serde_json::to_value(HandshakeResponse {
                        plugin_id: "test-stdio".to_string(),
                        plugin_version: "0.0.0".to_string(),
                        sidecar_version: env!("CARGO_PKG_VERSION").to_string(),
                        protocol_version: tiangong_plugin_sidecar::protocol::PROTOCOL_VERSION
                            .to_string(),
                        business_protocol: 0,
                        capabilities: vec!["echo".to_string()],
                        instance_id: format!("test-stdio-{}", std::process::id()),
                        status: ServiceStatus::Ready,
                    })
                    .expect("序列化握手响应失败"),
                )
            }
            "echo" => Response::success(&request.request_id, request.payload),
            "handshake_count" => Response::success(
                &request.request_id,
                serde_json::json!({"count": HANDSHAKE_COUNT.load(Ordering::SeqCst)}),
            ),
            "crash" => {
                std::process::exit(86);
            }
            "delay" => {
                let millis = request
                    .payload
                    .get("millis")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(100);
                tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
                Response::success(&request.request_id, request.payload)
            }
            "hang" => hang_with_child(request).await,
            other => Response::error(
                &request.request_id,
                ErrorCode::BadRequest,
                format!("未知操作: {other}"),
                false,
            ),
        }
    }
}

#[cfg(unix)]
async fn hang_with_child(request: Request) -> Response {
    let sidecar_pid_file = request
        .payload
        .get("sidecar_pid_file")
        .and_then(serde_json::Value::as_str);
    let child_pid_file = request
        .payload
        .get("child_pid_file")
        .and_then(serde_json::Value::as_str);
    let (Some(sidecar_pid_file), Some(child_pid_file)) = (sidecar_pid_file, child_pid_file) else {
        return Response::error(
            &request.request_id,
            ErrorCode::BadRequest,
            "hang 缺少 PID 文件路径",
            false,
        );
    };
    let mut child = match std::process::Command::new("/bin/sleep").arg("300").spawn() {
        Ok(child) => child,
        Err(error) => {
            return Response::error(
                &request.request_id,
                ErrorCode::ServiceError,
                format!("启动测试后台进程失败: {error}"),
                false,
            );
        }
    };
    let write_result = std::fs::write(sidecar_pid_file, std::process::id().to_string())
        .and_then(|_| std::fs::write(child_pid_file, child.id().to_string()));
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Response::error(
            &request.request_id,
            ErrorCode::ServiceError,
            format!("写入测试 PID 失败: {error}"),
            false,
        );
    }
    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    let _ = child.kill();
    let _ = child.wait();
    Response::success(&request.request_id, serde_json::Value::Null)
}

#[cfg(not(unix))]
async fn hang_with_child(request: Request) -> Response {
    Response::error(
        &request.request_id,
        ErrorCode::BadRequest,
        "hang 仅用于 Unix 生命周期验证",
        false,
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::var_os("TIANGONG_TEST_STDIO_MUTE").is_some() {
        // 协议沉默模式：进程存活但从不读写 stdio，验证宿主侧握手时限。
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    let config = SidecarConfig::new("test-stdio");
    run(config, || Ok(Arc::new(EchoService))).await
}
