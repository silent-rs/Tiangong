//! 网页配置页：`tiangong config` 与 CLI 内 `/config` 的唯一配置入口
//! （与 `tiangong-memory-sidecar --config` 同一交互方式）。
//!
//! - 缺省仅监听 127.0.0.1 随机端口，URL 携带一次性 token，并自动打开浏览器；
//! - `--host 0.0.0.0 --port <端口> --no-open` 用于服务器：在其他机器浏览器中打开
//!   打印的地址即可远程完成配置；
//! - 页面覆盖模型 / Server / 通用 / Prompt、插件管理与各插件自带的配置页；
//! - 页面点击"完成并关闭"或 Ctrl+C 后服务退出。

mod api;
mod plugins;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use silent::prelude::*;
use tokio::sync::Notify;

use self::api::ApiContext;

const PAGE_TEMPLATE: &str = include_str!("page.html");
const TOKEN_HEADER: &str = "x-tiangong-token";

#[derive(Clone)]
struct HttpState {
    api: Arc<ApiContext>,
    token: Arc<String>,
    shutdown: Arc<Notify>,
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    Response::json(&serde_json::json!({ "error": message.into() })).with_status(status)
}

fn state(req: &Request) -> silent::Result<HttpState> {
    req.get_state::<HttpState>().cloned()
}

/// 常量时间比较，避免按字节短路泄露 token 前缀。
fn token_matches(expected: &str, actual: &str) -> bool {
    let (expected, actual) = (expected.as_bytes(), actual.as_bytes());
    expected.len() == actual.len()
        && expected
            .iter()
            .zip(actual)
            .fold(0u8, |acc, (left, right)| acc | (left ^ right))
            == 0
}

fn authorized(req: &Request, token: &str) -> bool {
    req.headers()
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|actual| token_matches(token, actual.trim()))
}

/// 把字符串序列化为可安全内联进 `<script>` 的 JS 字面量。
fn js_string_literal(value: &str) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "\"\"".to_string())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

fn page_html(token: &str) -> String {
    PAGE_TEMPLATE.replace("\"__TOKEN__\"", &js_string_literal(token))
}

async fn page(mut req: Request) -> silent::Result<Response> {
    let state = state(&req)?;
    let token = req.params().get("token").cloned().unwrap_or_default();
    if !token_matches(&state.token, &token) {
        return Ok(Response::html(
            "<!doctype html><meta charset=\"utf-8\"><p>链接无效或已过期，请重新运行 tiangong config。</p>",
        )
        .with_status(StatusCode::UNAUTHORIZED));
    }
    let mut response = Response::html(&page_html(&state.token));
    response.set_header(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response.set_header(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    Ok(response)
}

async fn call(mut req: Request) -> silent::Result<Response> {
    let state = state(&req)?;
    if !authorized(&req, &state.token) {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "未授权"));
    }
    let action: String = req.get_path_params("action")?;
    let headers = req.headers();
    let chunked = headers.contains_key(header::TRANSFER_ENCODING);
    let has_body = chunked
        || headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > 0);
    let body = if !has_body {
        serde_json::json!({})
    } else {
        match req.json_parse::<serde_json::Value>().await {
            Ok(value) => value,
            Err(SilentError::JsonEmpty) => serde_json::json!({}),
            Err(error) => {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    format!("请求体需为 JSON：{error}"),
                ));
            }
        }
    };
    Ok(match api::dispatch(&state.api, &action, body).await {
        Ok(value) => Response::json(&value),
        Err(error) => json_error(
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST),
            error.message,
        ),
    })
}

async fn close(req: Request) -> silent::Result<Response> {
    let state = state(&req)?;
    if !authorized(&req, &state.token) {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "未授权"));
    }
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown.notify_one();
    });
    Ok(Response::json(&serde_json::json!({ "ok": true })))
}

fn routes() -> Route {
    Route::new_root()
        .append(Route::new("").get(page))
        .append(Route::new("api/<action>").post(call))
        .append(Route::new("close").post(close))
}

/// 解析监听地址：支持 IPv4、IPv6（可带方括号）与主机名。
async fn resolve_listen_addr(host: &str, port: u16) -> Result<SocketAddr> {
    let bare = host.trim().trim_start_matches('[').trim_end_matches(']');
    if bare.is_empty() {
        anyhow::bail!("监听地址不能为空");
    }
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    tokio::net::lookup_host((bare, port))
        .await
        .with_context(|| format!("解析监听地址失败：{host}"))?
        .next()
        .with_context(|| format!("监听地址没有可用的 IP：{host}"))
}

/// 浏览器访问用的地址：通配地址替换为本机回环。
fn browse_base(local: &SocketAddr) -> String {
    let ip = local.ip();
    let shown = if ip.is_unspecified() {
        if ip.is_ipv4() {
            "127.0.0.1".to_string()
        } else {
            "[::1]".to_string()
        }
    } else if ip.is_ipv6() {
        format!("[{ip}]")
    } else {
        ip.to_string()
    };
    format!("http://{shown}:{}", local.port())
}

/// 一次性访问令牌（两段 scru128，共 160 位随机量）。
fn one_time_token() -> String {
    format!("{}{}", scru128::new(), scru128::new()).to_lowercase()
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {},
    }
    Ok(())
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}

/// 配置页启动参数。
#[derive(Debug, Clone)]
pub struct WebConfigOptions {
    /// 监听地址（缺省 127.0.0.1）。
    pub host: String,
    /// 监听端口（None 为随机）。
    pub port: Option<u16>,
    /// 是否自动打开浏览器。
    pub open_browser: bool,
    /// 固定访问令牌（None 时随机生成一次性令牌）。
    pub token: Option<String>,
    /// 初始打开的分区（如 `providers`、`plugins`）。
    pub initial_tab: Option<String>,
}

impl Default for WebConfigOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: None,
            open_browser: true,
            token: None,
            initial_tab: None,
        }
    }
}

/// 打开配置页并阻塞到页面关闭。
///
/// 在独立线程中创建运行时，调用方（如已进入 tokio 运行时的 REPL）无需关心
/// 嵌套运行时问题。
pub fn run(options: WebConfigOptions) -> Result<()> {
    std::thread::Builder::new()
        .name("tiangong-web-config".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .context("初始化异步运行时失败")?;
            let result = runtime.block_on(serve(options));
            // 配置页拉起的插件 sidecar（插件配置页 / 插件管理）随服务退出一并停止。
            plugins::shutdown();
            result
        })
        .context("启动配置页线程失败")?
        .join()
        .map_err(|_| anyhow::anyhow!("配置页线程异常退出"))?
}

async fn serve(options: WebConfigOptions) -> Result<()> {
    let token = match options.token {
        Some(token) if token.trim().chars().count() >= 8 => token.trim().to_string(),
        Some(_) => anyhow::bail!("--token 至少 8 个字符"),
        None => one_time_token(),
    };
    let addr = resolve_listen_addr(&options.host, options.port.unwrap_or(0)).await?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("监听 {addr} 失败"))?;
    let local = listener.local_addr()?;
    let remote = !local.ip().is_loopback();
    let dir = tiangong_config::io::storage_root();
    // 插件运行时（WASM 配置注入、模型能力过滤）读取进程级配置单例。
    tiangong_config::registry::init();
    let api = Arc::new(ApiContext::new(dir.clone(), remote));
    let preload_dir = dir.clone();
    let loaded = tokio::task::spawn_blocking(move || plugins::preload(&preload_dir))
        .await
        .unwrap_or(0);
    tracing::info!(loaded, "配置页已预加载插件");
    let shutdown = Arc::new(Notify::new());
    let route = routes().with_state(HttpState {
        api,
        token: Arc::new(token.clone()),
        shutdown: shutdown.clone(),
    });

    let fragment = options
        .initial_tab
        .as_deref()
        .map(|tab| format!("#{tab}"))
        .unwrap_or_default();
    let url = format!("{}/?token={token}{fragment}", browse_base(&local));
    eprintln!("天工配置页：{url}");
    eprintln!("配置目录：  {}", dir.display());
    if remote {
        if local.ip().is_unspecified() {
            eprintln!(
                "已监听所有网卡（端口 {}）。远程访问请把地址中的主机换成服务器 IP 或域名：",
                local.port()
            );
            eprintln!(
                "  http://<服务器地址>:{}/?token={token}{fragment}",
                local.port()
            );
        }
        eprintln!("注意：远程配置使用明文 HTTP，访问令牌与页面中填写的 API Key 会在网络上传输；");
        eprintln!(
            "      请仅在可信网络中使用，或改用 SSH 隧道：ssh -L {0}:127.0.0.1:{0} <服务器>",
            local.port()
        );
    }
    eprintln!("配置完成后在页面点击\"完成并关闭\"，或按 Ctrl+C 退出。");
    if options.open_browser
        && let Err(error) = open::that_detached(&url)
    {
        eprintln!("无法自动打开浏览器（{error}），请手动访问上面的地址。");
    }

    let server = Server::new()
        .listen(Listener::from(listener))
        .with_shutdown(Duration::from_secs(2));
    tokio::select! {
        _ = server.serve(route) => {}
        _ = shutdown.notified() => {}
        result = wait_for_shutdown_signal() => result?,
    }
    eprintln!("配置页已关闭。");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_and_page() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("abc", ""));
        let html = page_html("a\"</script>");
        assert!(!html.contains("\"__TOKEN__\""));
        assert!(!html.contains("a\"</script>"));
        assert!(html.contains("\\u003c/script\\u003e"));
        assert_ne!(one_time_token(), one_time_token());
    }

    #[test]
    fn browse_url() {
        let any: SocketAddr = "0.0.0.0:8800".parse().unwrap();
        assert_eq!(browse_base(&any), "http://127.0.0.1:8800");
        let v6: SocketAddr = "[::1]:9".parse().unwrap();
        assert_eq!(browse_base(&v6), "http://[::1]:9");
    }
}
