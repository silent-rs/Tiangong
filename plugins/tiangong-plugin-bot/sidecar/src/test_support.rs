use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

pub async fn spawn_delayed_json_response(
    delay: Duration,
    body: String,
) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定长时 Mock 服务失败");
    let address = listener.local_addr().expect("读取长时 Mock 地址失败");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("接收 Mock 请求失败");
        let mut request = [0_u8; 8192];
        let size = stream.read(&mut request).await.expect("读取 Mock 请求失败");
        assert!(size > 0, "Mock 服务必须收到请求后再开始等待");

        tokio::time::sleep(delay).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("写入 Mock 响应失败");
    });
    (format!("http://{address}"), server)
}
