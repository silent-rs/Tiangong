//! 浏览器配置页：`tiangong config` 与 CLI 内 `/config` 的配置入口。
//!
//! 页面复用桌面端设置组件（前端 `config.html` 入口，随 Tauri 构建嵌入二进制），
//! 前端 `invoke` 在浏览器中改走 `POST api/invoke/<命令名>`，由 [`commands`] 分发。
//!
//! 配置页由用户本人临时打开：缺省监听 127.0.0.1 随机端口并自动打开浏览器；
//! 服务器上可用 `--host 0.0.0.0 --port <端口> --no-open` 在其他机器浏览器中打开。
//! 页面点击"完成并关闭"或 Ctrl+C 后服务退出。

mod bots;
mod commands;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use http_body_util::BodyExt;
use silent::prelude::*;
use tiangong_cli::web_config::WebConfigOptions;
use tokio::sync::Notify;

use self::commands::WebConfigContext;

const PAGE_ASSET: &str = "config.html";

/// 资源读取器：参数为相对路径（如 `config.html`、`assets/config-xxx.js`），返回内容。
pub type WebAssets = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

/// 从 Tauri 上下文取出前端构建产物（`frontendDist`，即 `frontend/dist`）。
///
/// 正式与开发模式使用同一份构建产物、同一套读取方式，区别只在来源：
/// - 正式构建（`cargo tauri build`，启用 `custom-protocol`）：产物在编译期嵌入二进制，
///   安装后的 App 直接从自身读取；
/// - `cargo tauri dev`：Tauri 不嵌入前端，读取磁盘上的 `frontendDist` 目录，
///   不依赖 vite 开发服务器。产物由 `beforeDevCommand` 在启动 vite 前先构建一次。
pub fn assets_from_context<R: tauri::Runtime>(context: &mut tauri::Context<R>) -> WebAssets {
    if tauri::is_dev() {
        if let Some(dir) = dev_frontend_dist(context.config()) {
            return Arc::new(move |path: &str| read_dist_file(&dir, path));
        }
    }
    let assets = context.set_assets(Box::new(NoAssets));
    Arc::new(move |path: &str| {
        assets
            .get(&tauri::utils::assets::AssetKey::from(path))
            .map(|bytes| bytes.into_owned())
    })
}

/// 开发模式下 `frontendDist` 的磁盘目录（相对 `tauri.conf.json` 所在的 crate 目录）。
fn dev_frontend_dist(config: &tauri::Config) -> Option<std::path::PathBuf> {
    match config.build.frontend_dist.as_ref()? {
        tauri::utils::config::FrontendDist::Directory(dir) => {
            Some(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir))
        }
        _ => None,
    }
}

/// 读取构建产物目录中的文件；只接受目录内的普通相对路径。
fn read_dist_file(dir: &std::path::Path, path: &str) -> Option<Vec<u8>> {
    let relative = std::path::Path::new(path.trim_start_matches('/'));
    if !relative
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    std::fs::read(dir.join(relative)).ok()
}

/// 取走嵌入资源后留在上下文中的空占位（该上下文只用于 CLI，不再启动 GUI）。
struct NoAssets;

impl<R: tauri::Runtime> tauri::Assets<R> for NoAssets {
    fn get(&self, _key: &tauri::utils::assets::AssetKey) -> Option<std::borrow::Cow<'_, [u8]>> {
        None
    }

    fn iter(&self) -> Box<tauri::utils::assets::AssetsIter<'_>> {
        Box::new(std::iter::empty())
    }

    fn csp_hashes(
        &self,
        _html_path: &tauri::utils::assets::AssetKey,
    ) -> Box<dyn Iterator<Item = tauri::utils::assets::CspHash<'_>> + '_> {
        Box::new(std::iter::empty())
    }
}

#[derive(Clone)]
struct HttpState {
    ctx: Arc<WebConfigContext>,
    assets: WebAssets,
    remote: bool,
    shutdown: Arc<Notify>,
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    Response::json(&serde_json::json!({ "error": message.into() })).with_status(status)
}

fn state(req: &Request) -> silent::Result<HttpState> {
    req.get_state::<HttpState>().cloned()
}

/// 在 `config.html` 的 `<head>` 起始处注入宿主标记（先于模块脚本执行）。
fn inject_host(html: &str, remote: bool) -> String {
    let script = format!("<script>window.__TIANGONG_WEB__={{\"remote\":{remote}}};</script>");
    match html.find("<head>") {
        Some(index) => {
            let at = index + "<head>".len();
            format!("{}{script}{}", &html[..at], &html[at..])
        }
        None => format!("{script}{html}"),
    }
}

fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or_default();
    match ext.to_ascii_lowercase().as_str() {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

async fn page(req: Request) -> silent::Result<Response> {
    let state = state(&req)?;
    let Some(html) = (state.assets)(PAGE_ASSET) else {
        return Ok(json_error(StatusCode::NOT_FOUND, "缺少配置页资源"));
    };
    let mut response = Response::html(&inject_host(&String::from_utf8_lossy(&html), state.remote));
    response.set_header(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

/// 前端静态资源（`frontend/dist` 构建产物）。
async fn asset(req: Request) -> silent::Result<Response> {
    let state = state(&req)?;
    let path: String = req.get_path_params("path")?;
    let key = path.trim_start_matches('/');
    if key.split('/').any(|segment| segment == "..") {
        return Ok(json_error(StatusCode::NOT_FOUND, "Not Found"));
    }
    let Some(bytes) = (state.assets)(key) else {
        return Ok(json_error(StatusCode::NOT_FOUND, "Not Found"));
    };
    let mut response = Response::empty();
    response.set_body(full(bytes));
    response.set_header(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type(key)),
    );
    Ok(response)
}

async fn invoke(mut req: Request) -> silent::Result<Response> {
    let state = state(&req)?;
    let command: String = req.get_path_params("command")?;
    // 直接读取请求体（不要求 Content-Type），空体按 `{}` 处理。
    let bytes = match req.take_body() {
        ReqBody::Empty => Default::default(),
        body => match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(error) => {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    format!("读取请求体失败：{error}"),
                ));
            }
        },
    };
    let args = if bytes.iter().all(u8::is_ascii_whitespace) {
        serde_json::json!({})
    } else {
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(serde_json::Value::Null) => serde_json::json!({}),
            Ok(value) => value,
            Err(error) => {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    format!("请求体需为 JSON：{error}"),
                ));
            }
        }
    };
    Ok(match commands::dispatch(&state.ctx, &command, args).await {
        // 统一包一层，避免 `null` 与空响应体歧义。
        Ok(value) => Response::json(&serde_json::json!({ "result": value })),
        Err(error) => json_error(
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST),
            error.message,
        ),
    })
}

async fn close(req: Request) -> silent::Result<Response> {
    let shutdown = state(&req)?.shutdown;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown.notify_one();
    });
    Ok(Response::json(&serde_json::json!({ "ok": true })))
}

fn routes() -> Route {
    Route::new_root()
        .append(Route::new("").get(page))
        .append(Route::new("api/invoke/<command>").post(invoke))
        .append(Route::new("api/close").post(close))
        .append(Route::new("<path:**>").get(asset))
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

#[cfg(unix)]
async fn wait_for_shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
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

/// 打开配置页并阻塞到页面关闭。
///
/// 在独立线程中创建运行时，调用方（如已进入 tokio 运行时的 REPL）无需关心
/// 嵌套运行时问题。
pub fn run(options: WebConfigOptions, assets: WebAssets) -> Result<()> {
    std::thread::Builder::new()
        .name("tiangong-web-config".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .context("初始化异步运行时失败")?;
            let result = runtime.block_on(serve(options, assets));
            // 配置页拉起的插件 sidecar（插件配置页 / 插件管理）随服务退出一并停止。
            tiangong_plugin_runtime::registry::shutdown_all_sidecars();
            result
        })
        .context("启动配置页线程失败")?
        .join()
        .map_err(|_| anyhow::anyhow!("配置页线程异常退出"))?
}

async fn serve(options: WebConfigOptions, assets: WebAssets) -> Result<()> {
    if (assets)(PAGE_ASSET).is_none() {
        anyhow::bail!(if tauri::is_dev() {
            format!(
                "未找到前端构建产物中的 {PAGE_ASSET}，请通过 `cargo tauri dev`（会先构建前端）启动，或手动执行 `yarn --cwd frontend build`"
            )
        } else {
            format!("当前二进制未包含配置页资源（{PAGE_ASSET}），请使用 Tauri 构建的天工应用")
        });
    }
    let addr = resolve_listen_addr(&options.host, options.port.unwrap_or(0)).await?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("监听 {addr} 失败"))?;
    let local = listener.local_addr()?;
    let remote = !local.ip().is_loopback();
    let dir = tiangong_config::io::storage_root();
    // 插件运行时（WASM 配置注入、模型能力过滤）读取进程级配置单例。
    tiangong_config::registry::init();
    let preload_dir = dir.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        tiangong_plugin_runtime::registry::preload_installed_plugins(&preload_dir)
    })
    .await
    .unwrap_or(0);
    tracing::info!(loaded, "配置页已预加载插件");
    let shutdown = Arc::new(Notify::new());
    let route = routes().with_state(HttpState {
        ctx: Arc::new(WebConfigContext::new(dir.clone(), remote)),
        assets,
        remote,
        shutdown: shutdown.clone(),
    });

    let fragment = options
        .initial_tab
        .as_deref()
        .map(|tab| format!("#{tab}"))
        .unwrap_or_default();
    let url = format!("{}/{fragment}", browse_base(&local));
    eprintln!("天工配置页：{url}");
    eprintln!("配置目录：  {}", dir.display());
    if local.ip().is_unspecified() {
        eprintln!(
            "已监听所有网卡，其他机器请访问 http://<本机地址>:{}/",
            local.port()
        );
    }
    eprintln!("配置完成后在页面点击\"完成并关闭\"，或按 Ctrl+C 退出。");
    if options.open_browser {
        if let Err(error) = open::that_detached(&url) {
            eprintln!("无法自动打开浏览器（{error}），请手动访问上面的地址。");
        }
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
    fn page_injection() {
        let html = inject_host(
            "<!doctype html><html><head><title>x</title></head></html>",
            true,
        );
        assert!(html
            .contains("<head><script>window.__TIANGONG_WEB__={\"remote\":true};</script><title>"));
        assert!(inject_host("<p>x</p>", false).starts_with("<script>"));
    }

    #[test]
    fn dist_reader_stays_inside_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("config.html"), "<html>").unwrap();
        std::fs::write(dir.path().join("assets/a.js"), "js").unwrap();
        assert_eq!(
            read_dist_file(dir.path(), "config.html").unwrap(),
            b"<html>"
        );
        assert_eq!(read_dist_file(dir.path(), "/assets/a.js").unwrap(), b"js");
        assert!(read_dist_file(dir.path(), "assets/../config.html").is_none());
        assert!(read_dist_file(dir.path(), "../outside").is_none());
        assert!(read_dist_file(dir.path(), "missing.js").is_none());
    }

    #[test]
    fn browse_url_and_mime() {
        let any: SocketAddr = "0.0.0.0:8800".parse().unwrap();
        assert_eq!(browse_base(&any), "http://127.0.0.1:8800");
        let v6: SocketAddr = "[::1]:9".parse().unwrap();
        assert_eq!(browse_base(&v6), "http://[::1]:9");
        assert_eq!(
            content_type("assets/a.JS"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("x"), "application/octet-stream");
    }
}
