//! 测试基础设施：极简 HTTP mock 服务器与客户端构造（仅测试编译）。

// ── HTTP 层（本地 mock 服务器）─────────────────────
/// 极简同步 HTTP mock：按连接顺序弹出一个预置响应，记录收到的请求。
pub(crate) struct MockServer {
    pub(crate) addr: String,
    pub(crate) requests: std::sync::mpsc::Receiver<(String, Vec<u8>)>,
}
impl MockServer {
    pub(crate) fn start(responses: Vec<Vec<u8>>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut queue = responses.into_iter();
            for mut stream in listener.incoming().flatten() {
                let Some(response) = queue.next() else { break };
                if let Some((request_line, body)) = read_request(&stream) {
                    let _ = tx.send((request_line, body));
                    use std::io::Write;
                    let _ = stream.write_all(&response);
                }
            }
        });
        Self { addr, requests: rx }
    }
    pub(crate) fn request_line(&self) -> String {
        self.requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("mock 服务器应收到请求")
            .0
    }
    pub(crate) fn body(&self) -> Vec<u8> {
        self.requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("mock 服务器应收到请求")
            .1
    }
}
fn read_request(mut stream: &std::net::TcpStream) -> Option<(String, Vec<u8>)> {
    use std::io::Read;
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).ok()?;
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header_text = String::from_utf8_lossy(&header).to_string();
    let content_length = header_text
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body).ok()?;
    }
    let request_line = header_text.lines().next().unwrap_or_default().to_string();
    Some((request_line, body))
}
pub(crate) fn http_response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}
pub(crate) fn sse_response(events: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{events}"
    )
    .into_bytes()
}
pub(crate) fn mock_client(addr: &str) -> crate::client::DeepSeekClient {
    crate::DeepSeekClient::builder("test-key")
        .base_url(format!("http://{addr}"))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap()
}
