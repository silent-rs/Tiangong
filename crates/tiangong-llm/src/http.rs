//! HTTP 发送：环境变量代理不可达时直连重试（#662）。

use reqwest::{Client, RequestBuilder, Response};

/// 发送请求；连接阶段失败时改用 `direct()` 构建的直连客户端重发一次。
///
/// VPN 关闭后进程环境中的 `HTTP(S)_PROXY` / `ALL_PROXY` 仍指向已失效的代理，
/// reqwest 只读环境变量代理，请求会在连接阶段失败。此时请求尚未送达服务端，
/// 重发不会造成重复处理；HTTP 状态等其他错误、请求体不可克隆时原样返回。
pub(crate) async fn send_with_direct_retry<E: std::fmt::Display>(
    request: RequestBuilder,
    direct: impl FnOnce() -> Result<Client, E>,
) -> reqwest::Result<Response> {
    let (client, request) = request.build_split();
    let request = request?;
    let retry = request.try_clone();
    let error = match client.execute(request).await {
        Err(error) if error.is_connect() => error,
        result => return result,
    };
    let Some(retry) = retry else {
        return Err(error);
    };
    match direct() {
        Ok(direct) => {
            tracing::warn!(error = %error, "连接失败，改用直连重试");
            direct.execute(retry).await
        }
        Err(build_error) => {
            tracing::warn!(error = %build_error, "构建直连客户端失败，放弃重试");
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// 端口 9（discard）通常无人监听，模拟 VPN 关闭后失效的代理。
    fn dead_proxy_client() -> Client {
        Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap()
    }

    fn direct_client() -> reqwest::Result<Client> {
        Client::builder().no_proxy().build()
    }

    #[tokio::test]
    async fn 代理不可达时直连重发同一请求() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string("payload"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(1)
            .mount(&server)
            .await;
        let request = dead_proxy_client().post(server.uri()).body("payload");
        let response = send_with_direct_retry(request, direct_client)
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn http状态错误不重试() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        let request = direct_client().unwrap().get(server.uri());
        let response = send_with_direct_retry(request, || -> reqwest::Result<Client> {
            panic!("HTTP 状态错误不应重试")
        })
        .await
        .unwrap();
        assert_eq!(response.status(), 503);
    }

    #[tokio::test]
    async fn 直连也失败时返回直连错误() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let request = dead_proxy_client().get(format!("http://{address}/"));
        let error = send_with_direct_retry(request, direct_client)
            .await
            .unwrap_err();
        assert!(error.is_connect(), "{error}");
    }
}
