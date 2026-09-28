//! HTTP 服务（Silent）：daemon REST 接口与 `--config` 一次性配置页。
//!
//! - daemon：`/api/v1/*` 暴露与 MCP 相同的记忆能力，使用用户设置的 token 鉴权，
//!   地址写入 `<storage>/memory/daemon.json`（不含 token）供其他程序发现；
//! - config：仅监听回环地址，URL 携带一次性 token，复用天工插件配置页；
//!   页面点击"完成并关闭"后服务随之退出。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use silent::prelude::*;
use tiangong_plugin_memory_protocol::ui::{self as plugin_ui, UiRequest};
use tokio::sync::Notify;

use crate::daemon::{self, DaemonInfo};
use crate::mcp;
use crate::service::{MemoryService, ServiceError, parse_input};

const CONFIG_PAGE_TEMPLATE: &str = include_str!("../../wasm/src/memory.html");
const CONFIG_PAGE_CSS: &str = include_str!("../../wasm/src/memory.css");
const CONFIG_PAGE_JS: &str = include_str!("../../wasm/src/memory.js");
/// 独立配置页注入的宿主标记：memory.js 据此改走 HTTP 而非 postMessage。
const STANDALONE_SHIM: &str = "window.__MEMORY_STANDALONE__ = { token: __TOKEN__ };";
const TOKEN_HEADER: &str = "x-memory-token";

/// 路由共享状态。
#[derive(Clone)]
struct HttpState {
    service: MemoryService,
    token: Arc<String>,
    shutdown: Arc<Notify>,
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    Response::json(&serde_json::json!({ "error": message.into() })).with_status(status)
}

fn service_error(error: ServiceError) -> Response {
    let status = match &error {
        ServiceError::Invalid(_) => StatusCode::BAD_REQUEST,
        ServiceError::Disabled => StatusCode::SERVICE_UNAVAILABLE,
        ServiceError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    json_error(status, error.to_string())
}

fn state(req: &Request) -> Result<HttpState> {
    req.get_state::<HttpState>().cloned()
}

/// 常量时间比较，避免按字节短路泄露 token 前缀。
fn token_matches(expected: &str, actual: &str) -> bool {
    let (expected, actual) = (expected.as_bytes(), actual.as_bytes());
    if expected.len() != actual.len() {
        return false;
    }
    expected
        .iter()
        .zip(actual)
        .fold(0u8, |acc, (left, right)| acc | (left ^ right))
        == 0
}

/// 校验 `Authorization: Bearer <token>` 或 `X-Memory-Token`。
fn authorized(req: &Request, token: &str) -> bool {
    let headers = req.headers();
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    let custom = headers
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim);
    bearer
        .or(custom)
        .is_some_and(|actual| token_matches(token, actual))
}

async fn read_json(req: &mut Request) -> std::result::Result<serde_json::Value, ServiceError> {
    let has_body = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_none_or(|length| length > 0);
    if !has_body {
        return Ok(serde_json::json!({}));
    }
    match req.json_parse::<serde_json::Value>().await {
        Ok(value) => Ok(value),
        Err(SilentError::JsonEmpty) => Ok(serde_json::json!({})),
        Err(error) => Err(ServiceError::Invalid(format!(
            "请求体需为 JSON（Content-Type: application/json）：{error}"
        ))),
    }
}

// ── daemon REST API ────────────────────────────────────────────────

async fn health(_req: Request) -> Result<Response> {
    Ok(Response::json(&serde_json::json!({
        "status": "ok",
        "service": "tiangong-memory",
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

async fn list_tools(req: Request) -> Result<Response> {
    let state = state(&req)?;
    if !authorized(&req, &state.token) {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "未授权"));
    }
    let tools = mcp::tool_definitions()
        .into_iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema.as_ref(),
            })
        })
        .collect::<Vec<_>>();
    Ok(Response::json(&serde_json::json!({ "tools": tools })))
}

/// `POST /api/v1/tools/<name>`：通用调用入口，与 MCP 工具一一对应。
async fn call_tool(mut req: Request) -> Result<Response> {
    let state = state(&req)?;
    if !authorized(&req, &state.token) {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "未授权"));
    }
    let name: String = req.get_path_params("name")?;
    let arguments = match read_json(&mut req).await {
        Ok(value) => value,
        Err(error) => return Ok(service_error(error)),
    };
    Ok(
        match mcp::call_tool(&state.service, &name, arguments).await {
            None => json_error(StatusCode::NOT_FOUND, format!("未知工具：{name}")),
            Some(Ok(value)) => Response::json(&value),
            Some(Err(error)) => service_error(error),
        },
    )
}

/// 语义化 REST 别名：`POST /api/v1/<alias>` → 对应工具。
fn alias_route(path: &str, tool: &'static str) -> Route {
    Route::new(path).post(move |mut req: Request| async move {
        let state = state(&req)?;
        if !authorized(&req, &state.token) {
            return Ok(json_error(StatusCode::UNAUTHORIZED, "未授权"));
        }
        let arguments = match read_json(&mut req).await {
            Ok(value) => value,
            Err(error) => return Ok(service_error(error)),
        };
        Ok::<Response, SilentError>(
            match mcp::call_tool(&state.service, tool, arguments).await {
                Some(Ok(value)) => Response::json(&value),
                Some(Err(error)) => service_error(error),
                None => json_error(StatusCode::NOT_FOUND, format!("未知工具：{tool}")),
            },
        )
    })
}

fn daemon_routes() -> Route {
    Route::new("api/v1")
        .append(Route::new("health").get(health))
        .append(
            Route::new("tools")
                .get(list_tools)
                .append(Route::new("<name>").post(call_tool)),
        )
        .append(alias_route("recall", "memory_recall"))
        .append(alias_route("search", "memory_search"))
        .append(alias_route("memories", "memory_remember"))
        .append(alias_route("memories/list", "memory_list"))
        .append(alias_route("memories/forget", "memory_forget"))
        .append(alias_route("turns", "memory_record_turn"))
        .append(alias_route("sessions/end", "memory_end_session"))
        .append(alias_route("status", "memory_status"))
}

// ── 配置页 ────────────────────────────────────────────────────────

fn config_page_html(token: &str) -> String {
    // 自动生成的 token 只含 scru128 字符集，但 --token 允许用户自定义任意
    // 字符串，必须按 JS 字面量序列化再内联，避免破坏脚本或自注入。
    let shim = STANDALONE_SHIM.replace("__TOKEN__", &js_string_literal(token));
    CONFIG_PAGE_TEMPLATE
        .replace("/*__MEMORY_CSS__*/", CONFIG_PAGE_CSS)
        .replace("/*__MEMORY_JS__*/", &format!("{shim}\n{CONFIG_PAGE_JS}"))
}

/// 把任意字符串序列化为可安全内联进 `<script>` 的 JS 字面量。
///
/// JSON 字符串是合法的 JS 字符串字面量；额外把 `<`、`>`、`&` 转成
/// Unicode 转义，防止内容里出现 `</script>` 提前结束脚本块。
fn js_string_literal(value: &str) -> String {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
    json.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

async fn config_page(mut req: Request) -> Result<Response> {
    let state = state(&req)?;
    let token = req.params().get("token").cloned().unwrap_or_default();
    if !token_matches(&state.token, &token) {
        return Ok(Response::html(
            "<!doctype html><meta charset=\"utf-8\"><p>链接无效或已过期，请重新运行 --config。</p>",
        )
        .with_status(StatusCode::UNAUTHORIZED));
    }
    let mut response = Response::html(&config_page_html(&state.token));
    response.set_header(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct ViewCall {
    #[serde(default)]
    payload: String,
}

fn parse_view_payload<T: for<'de> Deserialize<'de>>(
    payload: &str,
) -> std::result::Result<T, ServiceError> {
    serde_json::from_str(payload)
        .map_err(|error| ServiceError::Invalid(format!("解析页面请求失败：{error}")))
}

fn to_json<T: Serialize>(value: T) -> std::result::Result<serde_json::Value, ServiceError> {
    serde_json::to_value(value).map_err(|error| ServiceError::Internal(error.into()))
}

/// 页面 `memory_request` 消息 → 插件协议操作名与载荷（与 WASM 桥一致）。
fn ui_request_operation(
    request: UiRequest,
) -> std::result::Result<(&'static str, serde_json::Value), ServiceError> {
    use tiangong_plugin_memory_protocol::recall;
    Ok(match request {
        UiRequest::ListNodes { query } => (
            plugin_ui::LIST_NODES_OPERATION,
            to_json(plugin_ui::ListNodesRequest { query })?,
        ),
        UiRequest::CountNodes { query } => (
            plugin_ui::COUNT_NODES_OPERATION,
            to_json(plugin_ui::CountNodesRequest { query })?,
        ),
        UiRequest::ListRelations { node_id } => (
            plugin_ui::LIST_RELATIONS_OPERATION,
            to_json(plugin_ui::ListRelationsRequest { node_id })?,
        ),
        UiRequest::ListRelationsBatch { node_ids } => (
            plugin_ui::LIST_RELATIONS_BATCH_OPERATION,
            to_json(plugin_ui::ListRelationsBatchRequest { node_ids })?,
        ),
        UiRequest::UpsertManualMemory { draft } => (
            plugin_ui::UPSERT_MANUAL_MEMORY_OPERATION,
            to_json(plugin_ui::UpsertManualMemoryRequest { draft })?,
        ),
        UiRequest::SetNodeStatus { node_id, status } => (
            plugin_ui::SET_NODE_STATUS_OPERATION,
            to_json(plugin_ui::SetNodeStatusRequest { node_id, status })?,
        ),
        UiRequest::UpsertRelation { draft } => (
            plugin_ui::UPSERT_RELATION_OPERATION,
            to_json(plugin_ui::UpsertRelationRequest { draft })?,
        ),
        UiRequest::DeleteRelation { relation_id } => (
            plugin_ui::DELETE_RELATION_OPERATION,
            to_json(plugin_ui::DeleteRelationRequest { relation_id })?,
        ),
        UiRequest::Recall { anchors, limit } => (
            recall::RECALL_OPERATION,
            to_json(recall::RecallRequest { anchors, limit })?,
        ),
    })
}

/// 页面方法 → 插件协议操作（对应 WASM `handle_view_message`）。
fn view_operation(
    method: &str,
    payload: &str,
) -> std::result::Result<(&'static str, serde_json::Value), ServiceError> {
    Ok(match method {
        "bootstrap" => (plugin_ui::CONFIG_GET_OPERATION, serde_json::json!({})),
        "save_config" => {
            let selection: plugin_ui::MemorySelection = parse_view_payload(payload)?;
            (plugin_ui::CONFIG_SET_OPERATION, to_json(selection)?)
        }
        "probe_config" => {
            let probe: plugin_ui::ProbeRequest = parse_view_payload(payload)?;
            (plugin_ui::CONFIG_PROBE_OPERATION, to_json(probe)?)
        }
        "memory_request" => ui_request_operation(parse_view_payload(payload)?)?,
        other => return Err(ServiceError::Invalid(format!("未知页面消息：{other}"))),
    })
}

async fn view_call(mut req: Request) -> Result<Response> {
    let state = state(&req)?;
    if !authorized(&req, &state.token) {
        return Ok(json_error(StatusCode::UNAUTHORIZED, "未授权"));
    }
    let method: String = req.get_path_params("method")?;
    let call: ViewCall = match read_json(&mut req).await {
        Ok(value) => match parse_input(value) {
            Ok(call) => call,
            Err(error) => return Ok(service_error(error)),
        },
        Err(error) => return Ok(service_error(error)),
    };
    let (operation, payload) = match view_operation(&method, &call.payload) {
        Ok(result) => result,
        Err(error) => return Ok(service_error(error)),
    };
    Ok(match state.service.dispatch(operation, payload).await {
        Ok(value) => Response::json(&value),
        Err(error) => service_error(error),
    })
}

/// 页面"完成并关闭"：先返回响应，再延迟通知服务退出。
async fn close_config(req: Request) -> Result<Response> {
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

fn config_routes() -> Route {
    Route::new_root()
        .append(Route::new("").get(config_page))
        .append(Route::new("view/<method>").post(view_call))
        .append(Route::new("close").post(close_config))
}

// ── 启动 ──────────────────────────────────────────────────────────

async fn bind(addr: SocketAddr) -> anyhow::Result<(tokio::net::TcpListener, SocketAddr)> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("监听 {addr} 失败"))?;
    let local = listener.local_addr()?;
    Ok((listener, local))
}

/// 运行 HTTP 服务直到 `shutdown` 被通知或收到终止信号。
async fn serve_until(
    listener: tokio::net::TcpListener,
    route: Route,
    shutdown: Arc<Notify>,
) -> anyhow::Result<()> {
    let server = Server::new()
        .listen(Listener::from(listener))
        .with_shutdown(Duration::from_secs(2));
    tokio::select! {
        _ = server.serve(route) => {}
        _ = shutdown.notified() => {}
        result = crate::wait_for_shutdown_signal() => result?,
    }
    Ok(())
}

pub struct DaemonOptions {
    pub host: String,
    pub port: u16,
    pub token: String,
}

/// daemon 前台服务主体（后台模式下由分离的子进程执行）。
pub async fn run_daemon(service: MemoryService, options: DaemonOptions) -> anyhow::Result<()> {
    let addr = resolve_listen_addr(&options.host, options.port).await?;
    let (listener, local) = bind(addr).await?;
    let url = browse_base(&local);
    let info_path = daemon::info_path();
    let info = DaemonInfo::current(url.clone());
    write_private_json(&info_path, &info)?;
    let _guard = DaemonInfoGuard(info_path.clone());

    let shutdown = Arc::new(Notify::new());
    let route = Route::new_root()
        .with_state(HttpState {
            service,
            token: Arc::new(options.token),
            shutdown: shutdown.clone(),
        })
        .append(daemon_routes());
    eprintln!("tiangong-memory daemon 已启动：{url}/api/v1");
    if !is_loopback(&local) {
        // 与 --config 保持一致的风险提示：daemon 同样是明文 HTTP，token 与
        // 请求体（含记忆内容）会在网络上传输。
        print_plaintext_warning(&local, "daemon");
    }
    tracing::info!(%url, "memory daemon 已启动");
    serve_until(listener, route, shutdown).await
}

pub struct ConfigOptions {
    pub host: String,
    pub port: u16,
    pub open_browser: bool,
    /// 访问令牌；缺省随机生成（一次性）。
    pub token: Option<String>,
}

/// 解析监听地址：支持 IPv4、IPv6（可带方括号）与主机名。
async fn resolve_listen_addr(host: &str, port: u16) -> anyhow::Result<SocketAddr> {
    let host = host.trim();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
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

/// 地址是否只在本机可达。
fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// 浏览器访问用的地址：通配地址替换为本机回环，便于本机直接打开。
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

/// 生成一次性访问令牌（两段 scru128，共 160 位随机量）。
fn one_time_token() -> String {
    format!("{}{}", scru128::new(), scru128::new()).to_lowercase()
}

/// `--config`：打开浏览器配置页，页面关闭后退出。
///
/// 缺省仅监听本机；`--host 0.0.0.0` 等非回环地址用于远程配置，此时页面与
/// 其中填写的 API Key 以明文 HTTP 传输，只应在可信网络或 SSH 隧道中使用。
pub async fn run_config(service: MemoryService, options: ConfigOptions) -> anyhow::Result<()> {
    let token = match options.token {
        Some(token) => daemon::require_token(Some(token))?,
        None => one_time_token(),
    };
    let addr = resolve_listen_addr(&options.host, options.port).await?;
    let (listener, local) = bind(addr).await?;
    let remote = !is_loopback(&local);
    let url = format!("{}/?token={token}", browse_base(&local));
    let shutdown = Arc::new(Notify::new());
    let route = config_routes().with_state(HttpState {
        service,
        token: Arc::new(token.clone()),
        shutdown: shutdown.clone(),
    });
    eprintln!("Memory 配置页：{url}");
    if remote {
        if local.ip().is_unspecified() {
            eprintln!(
                "已监听所有网卡（端口 {}）。远程访问请把地址中的主机换成本机 IP 或域名：",
                local.port()
            );
            eprintln!("  http://<本机地址>:{}/?token={token}", local.port());
        }
        print_plaintext_warning(&local, "配置");
    }
    eprintln!("配置完成后在页面点击\"完成并关闭\"，或按 Ctrl+C 退出。");
    if options.open_browser
        && let Err(error) = open::that_detached(&url)
    {
        eprintln!("无法自动打开浏览器（{error}），请手动访问上面的地址。");
    }
    serve_until(listener, route, shutdown).await?;
    eprintln!("配置页已关闭。");
    Ok(())
}

/// 非回环监听时的明文传输提示（配置页与 daemon 共用）。
fn print_plaintext_warning(local: &SocketAddr, scene: &str) {
    // 远程会话的调用者只持有一个令牌，不应能让后端解析任意环境变量。
    tiangong_memory::config::restrict_env_refs();
    eprintln!("注意：远程{scene}使用明文 HTTP，访问令牌与请求内容会在网络上传输；");
    eprintln!(
        "      请仅在可信网络中使用，或改用 SSH 隧道：ssh -L {0}:127.0.0.1:{0} <主机>",
        local.port()
    );
}

fn write_private_json<T: Serialize>(path: &std::path::Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败：{}", parent.display()))?;
    }
    let body = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("写入 {} 失败", tmp.display()))?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(())
}

/// 退出时删除仍属于本进程的发现文件。
struct DaemonInfoGuard(PathBuf);

impl Drop for DaemonInfoGuard {
    fn drop(&mut self) {
        let owned = std::fs::read(&self.0)
            .ok()
            .and_then(|body| serde_json::from_slice::<DaemonInfo>(&body).ok())
            .is_some_and(|info| info.pid == std::process::id());
        if owned {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_address_and_browse_url() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let v4 = resolve_listen_addr("0.0.0.0", 8800).await.unwrap();
            assert!(v4.ip().is_unspecified() && !is_loopback(&v4));
            assert_eq!(browse_base(&v4), "http://127.0.0.1:8800");
            let v6 = resolve_listen_addr("[::1]", 9).await.unwrap();
            assert!(is_loopback(&v6));
            assert_eq!(browse_base(&v6), "http://[::1]:9");
            let named = resolve_listen_addr("localhost", 1).await.unwrap();
            assert!(is_loopback(&named));
            // 空地址无法解析（不依赖外部 DNS 对不存在域名的行为）。
            assert!(resolve_listen_addr("", 1).await.is_err());
        });
        let lan: SocketAddr = "192.168.1.8:7717".parse().unwrap();
        assert!(!is_loopback(&lan));
        assert_eq!(browse_base(&lan), "http://192.168.1.8:7717");
        assert_ne!(one_time_token(), one_time_token());
        assert!(one_time_token().len() >= 50);
    }

    #[test]
    fn token_comparison_requires_exact_match() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("abc", "ab"));
        assert!(!token_matches("abc", ""));
    }

    #[test]
    fn config_page_injects_standalone_shim() {
        let html = config_page_html("tok123");
        assert!(html.contains("__MEMORY_STANDALONE__ = { token: \"tok123\" }"));
        assert!(!html.contains("/*__MEMORY_JS__*/"));
        assert!(!html.contains("/*__MEMORY_CSS__*/"));
    }

    #[test]
    fn view_operations_map_to_plugin_protocol() {
        let (operation, _) = view_operation("bootstrap", "").expect("bootstrap");
        assert_eq!(operation, plugin_ui::CONFIG_GET_OPERATION);
        let (operation, payload) = view_operation(
            "memory_request",
            r#"{"method":"list_relations","node_id":"n1"}"#,
        )
        .expect("memory_request");
        assert_eq!(operation, plugin_ui::LIST_RELATIONS_OPERATION);
        assert_eq!(payload["node_id"], "n1");
        assert!(view_operation("unknown", "").is_err());
        assert!(view_operation("save_config", "not json").is_err());
    }
}

#[cfg(test)]
mod config_page_tests {
    use super::*;

    #[test]
    fn custom_token_is_escaped_into_js_literal() {
        // 自定义 token 里的引号与标签都必须被转义，脚本结构保持完整。
        let html = config_page_html("a'b\"</script><img src=x>");
        assert!(
            !html.contains("</script><img"),
            "不得原样内联标签：{html:?}"
        );
        assert!(html.contains("\\u003c/script\\u003e"), "`<`/`>` 应转义");
        assert!(
            html.contains("window.__MEMORY_STANDALONE__ = { token: \""),
            "shim 仍应是合法赋值"
        );
    }

    #[test]
    fn plain_token_round_trips() {
        let html = config_page_html("0abcdef0123456789abcdefg");
        assert!(html.contains("token: \"0abcdef0123456789abcdefg\" }"));
    }
}
