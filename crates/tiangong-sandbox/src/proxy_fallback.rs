//! 代理不可达时自动直连重试（#662）。
//!
//! reqwest 未启用 `system-proxy`，只读取进程环境变量 `HTTP(S)_PROXY` /
//! `ALL_PROXY`。VPN/代理工具开启时写入的环境变量会被桌面启动的天工继承，
//! 关闭 VPN 后代理端口不再监听，但运行中进程的环境不会变化，请求持续失败。
//!
//! 策略：请求经由环境变量代理且失败原因为连接阶段错误
//! （[`reqwest::Error::is_connect`]，即代理拒绝连接/连接超时）时，用禁用代理的
//! 客户端重放一次同一请求。HTTP 状态、TLS、校验等错误不重试；未配置代理
//! 环境变量时不重试；请求体不可克隆（流式上传）时不重试。重试只发生在
//! `send` 阶段，尚未读取任何响应体，不会造成重复下载。
//!
//! 本模块只依赖 `reqwest` 与 `tracing`，为避免跨 crate 依赖拖慢打包，以下
//! crate 各自持有一份内容相同的副本，修改时需同步：
//! `tiangong-sandbox`、`tiangong-plugin-runtime`、`tiangong-llm`。
//! `tiangong-anthropic` / `tiangong-deepseek` 作为通用 SDK 不内置该策略，只提供
//! `no_proxy` 配置与 `is_connect()` 判定，由 `tiangong-llm` 在其上实现回退。

use reqwest::{Client, RequestBuilder, Response, Url};

/// 读取环境变量的函数签名（测试注入固定值，避免修改进程环境）。
type EnvLookup = fn(&str) -> Option<String>;

fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn first_env(lookup: EnvLookup, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| lookup(key))
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

/// 按 reqwest（hyper-util）的环境变量规则，返回访问 `target` 时会使用的代理原始值。
///
/// 规则：https 目标取 `HTTPS_PROXY`，http 目标取 `HTTP_PROXY`（CGI 环境下忽略），
/// 均缺省时回退 `ALL_PROXY`；目标主机命中 `NO_PROXY` 时视为直连。
fn env_proxy_for(target: &Url, lookup: EnvLookup) -> Option<String> {
    let host = target.host_str()?;
    if let Some(no_proxy) = first_env(lookup, &["NO_PROXY", "no_proxy"])
        && no_proxy_matches(host, &no_proxy)
    {
        return None;
    }
    let specific = match target.scheme() {
        "https" => first_env(lookup, &["HTTPS_PROXY", "https_proxy"]),
        "http" if lookup("REQUEST_METHOD").is_none() => {
            first_env(lookup, &["HTTP_PROXY", "http_proxy"])
        }
        _ => None,
    };
    specific.or_else(|| first_env(lookup, &["ALL_PROXY", "all_proxy"]))
}

/// `NO_PROXY` 匹配：`*` 匹配全部；其余条目按主机名相等或域名后缀匹配
/// （前导点可省略）。IP 网段条目仅按字面相等比较——不精确时最多多一次
/// 直连尝试，不影响正确性。
fn no_proxy_matches(host: &str, no_proxy: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    no_proxy
        .split(',')
        .map(|entry| entry.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            entry == "*"
                || host == entry
                || host
                    .strip_suffix(&entry)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
}

/// 代理地址脱敏：去掉用户名/密码，只保留 `scheme://host:port`。
pub fn redact_proxy(raw: &str) -> String {
    let parsed = Url::parse(raw)
        .ok()
        .filter(|url| url.has_host())
        .or_else(|| Url::parse(&format!("http://{raw}")).ok());
    match parsed {
        Some(url) => {
            let host = url.host_str().unwrap_or_default();
            match url.port_or_known_default() {
                Some(port) => format!("{}://{host}:{port}", url.scheme()),
                None => format!("{}://{host}", url.scheme()),
            }
        }
        None => "<无法解析的代理地址>".to_string(),
    }
}

/// 拼接完整错误链（reqwest 的 Display 只含最外层，根因在 source 链中）。
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    text
}

/// 访问 `target` 时是否经由环境变量代理；是则返回脱敏后的代理地址。
///
/// 供无法拿到 `reqwest::Error` 的调用方（如经 SDK 错误类型转换后）自行判定。
pub fn env_proxy(target: &Url) -> Option<String> {
    env_proxy_for(target, process_env).map(|proxy| redact_proxy(&proxy))
}

/// 判断请求失败是否应直连重试：目标经由环境变量代理且为连接阶段错误。
///
/// 需要重试时返回脱敏后的代理地址（供日志使用）。
pub fn should_retry_direct(error: &reqwest::Error, target: &Url) -> Option<String> {
    should_retry_direct_with(error, target, process_env)
}

fn should_retry_direct_with(
    error: &reqwest::Error,
    target: &Url,
    lookup: EnvLookup,
) -> Option<String> {
    if !error.is_connect() {
        return None;
    }
    env_proxy_for(target, lookup).map(|proxy| redact_proxy(&proxy))
}

/// 发送请求；经由环境变量代理且连接失败时，用 `direct()` 构建的直连客户端
/// 重放一次同一请求。
///
/// `direct` 仅在需要回退时调用，应返回与原客户端配置一致（默认请求头、
/// 超时等）但调用了 `no_proxy()` 的客户端；构建失败时返回原始错误。
/// `label` 标明请求所属模块，写入日志。
pub async fn send_with_direct_fallback<F, E>(
    label: &'static str,
    builder: RequestBuilder,
    direct: F,
) -> reqwest::Result<Response>
where
    F: FnOnce() -> Result<Client, E>,
    E: std::fmt::Display,
{
    send_with_direct_fallback_with(label, builder, direct, process_env).await
}

async fn send_with_direct_fallback_with<F, E>(
    label: &'static str,
    builder: RequestBuilder,
    direct: F,
    lookup: EnvLookup,
) -> reqwest::Result<Response>
where
    F: FnOnce() -> Result<Client, E>,
    E: std::fmt::Display,
{
    let (client, request) = builder.build_split();
    let request = request?;
    let replay = request.try_clone();
    let error = match client.execute(request).await {
        Ok(response) => return Ok(response),
        Err(error) => error,
    };
    let Some(replay) = replay else {
        return Err(error);
    };
    let Some(proxy) = should_retry_direct_with(&error, replay.url(), lookup) else {
        return Err(error);
    };
    let url = replay.url().to_string();
    tracing::warn!(
        label,
        proxy,
        url,
        error = error_chain(&error),
        "经代理请求连接失败，回退直连重试"
    );
    let client = match direct() {
        Ok(client) => client,
        Err(build_error) => {
            tracing::warn!(label, error = %build_error, "构建直连客户端失败，放弃重试");
            return Err(error);
        }
    };
    let result = client.execute(replay).await;
    match &result {
        Ok(response) => {
            tracing::info!(label, url, status = %response.status(), "直连重试成功");
        }
        Err(direct_error) => {
            tracing::warn!(
                label,
                url,
                error = error_chain(direct_error),
                "直连重试仍失败"
            );
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::time::Duration;

    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn https_proxy_env(key: &str) -> Option<String> {
        (key == "HTTPS_PROXY").then(|| "http://user:secret@127.0.0.1:7890".to_string())
    }

    fn all_proxy_with_no_proxy_env(key: &str) -> Option<String> {
        match key {
            "ALL_PROXY" => Some("socks5://127.0.0.1:1080".to_string()),
            "no_proxy" => Some("localhost, .example.com".to_string()),
            _ => None,
        }
    }

    /// 测试代理指向的环境变量值不参与实际连接，仅用于“是否经代理”的判定。
    fn http_proxy_env(key: &str) -> Option<String> {
        (key == "HTTP_PROXY").then(|| "http://127.0.0.1:9".to_string())
    }

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn 环境代理按协议与no_proxy选取() {
        assert_eq!(env_proxy_for(&url("https://a.com/x"), no_env), None);
        assert_eq!(
            env_proxy_for(&url("https://a.com/x"), https_proxy_env).as_deref(),
            Some("http://user:secret@127.0.0.1:7890")
        );
        // HTTPS_PROXY 不作用于 http 目标。
        assert_eq!(env_proxy_for(&url("http://a.com/x"), https_proxy_env), None);
        // ALL_PROXY 兜底，NO_PROXY 命中的主机直连。
        assert!(env_proxy_for(&url("http://a.com/x"), all_proxy_with_no_proxy_env).is_some());
        let oss = url("https://oss.example.com/x");
        assert_eq!(env_proxy_for(&oss, all_proxy_with_no_proxy_env), None);
        assert_eq!(
            env_proxy_for(&url("http://localhost:8080/"), all_proxy_with_no_proxy_env),
            None
        );
    }

    #[test]
    fn no_proxy匹配规则() {
        assert!(no_proxy_matches("a.com", "*"));
        assert!(no_proxy_matches("x.a.com", "a.com"));
        assert!(no_proxy_matches("x.a.com", ".a.com"));
        assert!(!no_proxy_matches("bada.com", "a.com"));
        assert!(no_proxy_matches("[::1]", "::1"));
        assert!(!no_proxy_matches("a.com", " , "));
    }

    #[test]
    fn 代理地址脱敏() {
        assert_eq!(
            redact_proxy("http://user:secret@127.0.0.1:7890"),
            "http://127.0.0.1:7890"
        );
        assert_eq!(redact_proxy("127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(redact_proxy("localhost:7890"), "http://localhost:7890");
        assert_eq!(redact_proxy("socks5://h:1080"), "socks5://h:1080");
        assert_eq!(redact_proxy("http://u:pw@[::1]:1"), "http://[::1]:1");
    }

    /// 绑定后立即释放的端口：连接会被拒绝，模拟已关闭的 VPN 代理。
    fn dead_proxy_client() -> Client {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        Client::builder()
            .proxy(reqwest::Proxy::all(proxy).unwrap())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    fn direct_client() -> reqwest::Result<Client> {
        Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
    }

    /// 只应答一次请求的本地 HTTP 服务，回显请求方法与请求体。
    fn serve_once(status: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2048];
            while let Ok(read) = stream.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let request = String::from_utf8_lossy(&request);
            let method = request.split_whitespace().next().unwrap_or_default();
            let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
            let reply = format!("{method} {body}");
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
        });
        (format!("http://{address}/api"), handle)
    }

    #[tokio::test]
    async fn 代理不可达时直连重放请求体() {
        let (target, server) = serve_once("200 OK");
        let builder = dead_proxy_client().post(&target).body("payload");
        let response =
            send_with_direct_fallback_with("test", builder, direct_client, http_proxy_env)
                .await
                .unwrap();
        assert!(response.status().is_success());
        assert_eq!(response.text().await.unwrap(), "POST payload");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn 未配置代理环境变量时不重试() {
        let builder = dead_proxy_client().get("http://127.0.0.1:9/x");
        let error = send_with_direct_fallback_with(
            "test",
            builder,
            || -> reqwest::Result<Client> {
                panic!("未配置代理时不应构建直连客户端")
            },
            no_env,
        )
        .await
        .unwrap_err();
        assert!(error.is_connect());
    }

    #[tokio::test]
    async fn http状态错误不重试() {
        let (target, server) = serve_once("503 Service Unavailable");
        let builder = direct_client().unwrap().get(&target);
        let response = send_with_direct_fallback_with(
            "test",
            builder,
            || -> reqwest::Result<Client> { panic!("HTTP 状态错误不应重试") },
            http_proxy_env,
        )
        .await
        .unwrap();
        assert_eq!(response.status().as_u16(), 503);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn 直连也失败时返回直连错误() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let target = format!("http://{}/x", listener.local_addr().unwrap());
        drop(listener);
        let builder = dead_proxy_client().get(&target);
        let error = send_with_direct_fallback_with("test", builder, direct_client, http_proxy_env)
            .await
            .unwrap_err();
        assert!(error.is_connect());
    }
}
