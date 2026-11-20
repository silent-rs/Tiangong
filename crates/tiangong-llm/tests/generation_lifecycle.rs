//! 真实本地 HTTP 流：轻量请求、尾部用量、取消与超时契约。
use std::{sync::Arc, time::Duration};
use tiangong_llm::{ModelEndpoint, ModelRequest, SingleProviderClient, TextRequest};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn client(url: String) -> SingleProviderClient {
    SingleProviderClient::new(ModelEndpoint {
        base_url: url,
        api_key: "test".into(),
        model: "test".into(),
        timeout_ms: 100,
        ..Default::default()
    })
}

#[tokio::test]
async fn lightweight_title_is_streamed_once_without_session_history() {
    let server = MockServer::start().await;
    let events = [
        serde_json::json!({"choices":[{"index":0,"delta":{"content":"简短标题"},"finish_reason":"stop"}]}),
        serde_json::json!({"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
    ];
    let body = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let title = client(server.uri())
        .stream(
            TextRequest {
                instruction: "生成标题",
                input: "输入文本",
            },
            &ModelRequest {
                max_output_tokens: Some(20),
                ..Default::default()
            },
        )
        .unwrap()
        .collect_text()
        .await
        .unwrap();
    assert_eq!(title, "简短标题");
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 20);
    assert!(body.get("tools").is_none());
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
}

/// 服务端已发送 SSE 响应头/首事件，但剩余响应体阻塞；取消需关闭读取连接。
#[tokio::test]
async fn dropping_collector_closes_blocked_http_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let ready = started.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 8192];
        let mut received = Vec::new();
        loop {
            let n = socket.read(&mut request).await.unwrap();
            assert!(n > 0);
            received.extend_from_slice(&request[..n]);
            if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&received[..end]);
                let len = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|s| s.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if received.len() >= end + 4 + len {
                    break;
                }
            }
        }
        let first = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
        socket
            .write_all(format!("{:x}\r\n{first}\r\n", first.len()).as_bytes())
            .await
            .unwrap();
        ready.notify_one();
        let mut probe = [0; 1];
        let eof = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut probe))
            .await
            .expect("取消后客户端未关闭连接");
        assert!(matches!(eof, Ok(0) | Err(_)));
    });
    let client = client(format!("http://{addr}"));
    let task = tokio::spawn(async move {
        client
            .stream(
                TextRequest {
                    instruction: "标题",
                    input: "输入",
                },
                &ModelRequest::default(),
            )
            .unwrap()
            .collect_text()
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    server.await.unwrap();
}

#[tokio::test]
async fn response_header_timeout_is_bounded_and_does_not_succeed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;
    let result = tokio::time::timeout(
        Duration::from_secs(12),
        client(server.uri())
            .stream(
                TextRequest {
                    instruction: "标题",
                    input: "输入",
                },
                &ModelRequest {
                    timeout_ms: Some(20),
                    ..Default::default()
                },
            )
            .unwrap()
            .collect_text(),
    )
    .await;
    assert!(result.is_ok(), "重试必须有限");
    assert!(result.unwrap().is_err());
}
