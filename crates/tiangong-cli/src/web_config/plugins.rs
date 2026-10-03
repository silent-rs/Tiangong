//! 网页配置页的插件管理与插件配置页桥接。
//!
//! - 插件管理：列出已安装 / 可安装插件，安装、升级、启停、回滚、卸载、
//!   导入本地插件（服务器上的路径），管理可信发布者公钥；
//! - 插件配置：列出 `settings.plugin-page` 贡献（与桌面端设置页同一数据源），
//!   返回插件页面 HTML，并把页面内的 `plugin_call` 经宿主桥接转发给插件。
//!
//! 插件注册表是进程级单例：配置服务启动时预加载已安装插件，安装等变更
//! 由 runtime 自身更新注册表，页面刷新即可看到最新状态。

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use tiangong_plugin_runtime::registry::{self, RuntimeKind};

use super::api::{ApiContext, ApiError, ApiResult};

/// 插件配置页可用的桥接命名空间（与桌面 PluginIframe 一致）。
const PAGE_BRIDGE_NAMESPACES: &[&str] = &["plugin.", "storage.", "session.", "tool."];

fn bad(message: impl Into<String>) -> ApiError {
    ApiError {
        status: 400,
        message: message.into(),
    }
}

fn from_anyhow(error: anyhow::Error) -> ApiError {
    bad(format!("{error:#}"))
}

/// 在阻塞线程执行（插件注册表操作会做文件 IO、进程启停与验签）。
async fn blocking<T, F>(task: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|error| bad(format!("后台任务失败：{error}")))?
        .map_err(from_anyhow)
}

/// 配置服务启动时预加载已安装插件，供插件列表与配置页使用。
pub(crate) fn preload(dir: &Path) -> usize {
    registry::preload_installed_plugins(dir)
}

/// 配置服务退出时停止本进程拉起的 sidecar。
pub(crate) fn shutdown() {
    registry::shutdown_all_sidecars();
}

fn to_json<T: serde::Serialize>(value: T) -> ApiResult {
    serde_json::to_value(value).map_err(|error| bad(error.to_string()))
}

#[derive(Debug, Deserialize)]
struct PluginId {
    id: String,
}

#[derive(Debug, Deserialize)]
struct SetEnabled {
    id: String,
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct Uninstall {
    id: String,
    #[serde(default)]
    keep_data: bool,
}

#[derive(Debug, Deserialize)]
struct ImportLocal {
    path: String,
}

#[derive(Debug, Deserialize)]
struct ImportPublisher {
    publisher: String,
    public_key: String,
}

#[derive(Debug, Deserialize)]
struct Publisher {
    publisher: String,
}

#[derive(Debug, Deserialize)]
struct OpenPage {
    plugin_id: String,
    contribution_id: String,
}

#[derive(Debug, Deserialize)]
struct PageCall {
    plugin_id: String,
    method: String,
    #[serde(default)]
    payload: String,
}

fn parse<T: serde::de::DeserializeOwned>(body: Value) -> Result<T, ApiError> {
    serde_json::from_value(body).map_err(|error| bad(format!("请求参数无效：{error}")))
}

/// 插件相关操作入口；`action` 不属于插件命名空间时返回 None。
pub(crate) async fn dispatch(ctx: &ApiContext, action: &str, body: Value) -> Option<ApiResult> {
    let dir = ctx.dir().to_path_buf();
    let result = match action {
        "plugins.list" => list(dir).await,
        "plugins.available" => available(dir).await,
        "plugins.install" => match parse::<PluginId>(body) {
            Ok(req) => install(dir, req.id).await,
            Err(error) => Err(error),
        },
        "plugins.set_enabled" => match parse::<SetEnabled>(body) {
            Ok(req) => blocking(move || registry::set_plugin_enabled(&dir, &req.id, req.enabled))
                .await
                .and_then(to_json),
            Err(error) => Err(error),
        },
        "plugins.rollback" => match parse::<PluginId>(body) {
            Ok(req) => blocking(move || registry::rollback_plugin(&dir, &req.id))
                .await
                .and_then(to_json),
            Err(error) => Err(error),
        },
        "plugins.reload" => match parse::<PluginId>(body) {
            Ok(req) => blocking(move || registry::reload_plugin(&dir, &req.id))
                .await
                .and_then(to_json),
            Err(error) => Err(error),
        },
        "plugins.uninstall" => match parse::<Uninstall>(body) {
            Ok(req) => {
                let id = req.id.clone();
                blocking(move || registry::uninstall_plugin(&dir, &req.id, req.keep_data))
                    .await
                    .map(|()| json!({ "message": format!("已卸载插件 {id}") }))
            }
            Err(error) => Err(error),
        },
        "plugins.import_local" => match parse::<ImportLocal>(body) {
            Ok(req) => import_local(ctx, dir, req.path).await,
            Err(error) => Err(error),
        },
        "plugins.publishers" => publishers(dir).await,
        "plugins.publisher_import" => match parse::<ImportPublisher>(body) {
            Ok(req) => blocking(move || {
                tiangong_plugin_runtime::trust::import_trusted_publisher(
                    &dir,
                    req.publisher.trim(),
                    req.public_key.trim(),
                )
            })
            .await
            .and_then(to_json),
            Err(error) => Err(error),
        },
        "plugins.publisher_remove" => match parse::<Publisher>(body) {
            Ok(req) => blocking(move || {
                tiangong_plugin_runtime::trust::remove_trusted_publisher(&dir, &req.publisher)
            })
            .await
            .map(|removed| json!({ "removed": removed })),
            Err(error) => Err(error),
        },
        "plugins.pages" => Ok(pages()),
        "plugins.page_open" => match parse::<OpenPage>(body) {
            Ok(req) => page_open(req).await,
            Err(error) => Err(error),
        },
        "plugins.page_call" => match parse::<PageCall>(body) {
            Ok(req) => page_call(req).await,
            Err(error) => Err(error),
        },
        _ => return None,
    };
    Some(result)
}

async fn list(dir: PathBuf) -> ApiResult {
    let statuses = blocking(move || Ok(registry::list_plugins(&dir, RuntimeKind::Cli))).await?;
    to_json(statuses)
}

async fn available(dir: PathBuf) -> ApiResult {
    let repository =
        tiangong_plugin_runtime::artifacts::PluginRepository::new().map_err(from_anyhow)?;
    let list = repository
        .list_available(&dir)
        .await
        .map_err(|error| bad(format!("读取插件目录失败：{error:#}")))?;
    to_json(list)
}

async fn install(dir: PathBuf, id: String) -> ApiResult {
    let status =
        tiangong_plugin_runtime::artifacts::install_plugin_from_repository(&dir, &id, None)
            .await
            .map_err(from_anyhow)?;
    to_json(status)
}

/// 导入服务器本机上的插件目录或签名归档（`.tar.zst`）。
///
/// 远程配置时页面只能提交服务器路径；带 sidecar 的插件需在页面中确认权限
/// 后再调用本接口（页面负责展示清单中的权限）。
async fn import_local(ctx: &ApiContext, dir: PathBuf, path: String) -> ApiResult {
    let source = PathBuf::from(path.trim());
    if !source.is_absolute() {
        return Err(bad("请填写服务器上的绝对路径"));
    }
    if ctx.is_remote() {
        // 远程导入等同在服务器上安装可执行代码：仍要求签名校验（runtime 安装链保证），
        // 这里额外限制只能导入插件目录或归档，不跟随符号链接。
        let metadata = std::fs::symlink_metadata(&source)
            .map_err(|error| bad(format!("读取 {} 失败：{error}", source.display())))?;
        if metadata.file_type().is_symlink() {
            return Err(bad("不支持通过符号链接导入插件"));
        }
    }
    blocking(move || {
        let staged = if source
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("zst"))
        {
            tiangong_plugin_runtime::artifacts::stage_plugin_archive(&dir, &source)?
        } else {
            tiangong_plugin_runtime::artifacts::stage_local_plugin(&dir, &source)?
        };
        registry::import_staged_plugin(&dir, staged.path())
    })
    .await
    .and_then(to_json)
}

async fn publishers(dir: PathBuf) -> ApiResult {
    let (list, fingerprint) = blocking(move || {
        let list = tiangong_plugin_runtime::trust::list_trusted_publishers(&dir)?;
        let fingerprint = tiangong_plugin_runtime::trust::user_public_key_b64(&dir)
            .ok()
            .and_then(|key| tiangong_plugin_runtime::trust::publisher_fingerprint(&key).ok());
        Ok((list, fingerprint))
    })
    .await?;
    Ok(json!({ "publishers": list, "user_key_fingerprint": fingerprint }))
}

/// 已启用插件提供的配置页（`settings.plugin-page`，与桌面端设置页一致）。
fn pages() -> Value {
    let pages: Vec<Value> = registry::list_slot_contributions("settings.plugin-page")
        .into_iter()
        .filter(|entry| entry.has_view)
        .map(|entry| {
            json!({
                "plugin_id": entry.plugin_id,
                "contribution_id": entry.contribution_id,
                "title": entry.title,
                "description": entry.description,
                "source": entry.source,
            })
        })
        .collect();
    json!({ "pages": pages })
}

async fn page_open(req: OpenPage) -> ApiResult {
    let entry = registry::list_slot_contributions("settings.plugin-page")
        .into_iter()
        .find(|entry| {
            entry.plugin_id == req.plugin_id && entry.contribution_id == req.contribution_id
        })
        .ok_or_else(|| bad(format!("插件 {} 没有该配置页", req.plugin_id)))?;
    let source = entry.source;
    let html = blocking(move || match source {
        registry::ContributionSource::Manifest => {
            registry::open_manifest_view(&req.plugin_id, &req.contribution_id)
        }
        registry::ContributionSource::Wasm => {
            registry::open_view(&req.plugin_id, &req.contribution_id)
                .ok_or_else(|| anyhow::anyhow!("插件 {} 未加载或无页面", req.plugin_id))
        }
    })
    .await?;
    Ok(json!({ "html": html }))
}

/// 旧协议裸方法名补 `plugin.` 前缀，其余命名空间按白名单透传。
fn normalize_page_method(method: &str) -> Result<String, ApiError> {
    if PAGE_BRIDGE_NAMESPACES
        .iter()
        .any(|prefix| method.starts_with(prefix))
    {
        return Ok(method.to_string());
    }
    if method.contains('.') {
        return Err(bad(format!("配置页不支持调用 {method}")));
    }
    Ok(format!("plugin.{method}"))
}

async fn page_call(req: PageCall) -> ApiResult {
    // 只能调用拥有配置页的已启用插件，防止借配置页令牌调用任意插件桥接。
    let has_page = registry::list_slot_contributions("settings.plugin-page")
        .iter()
        .any(|entry| entry.plugin_id == req.plugin_id && entry.has_view);
    if !has_page {
        return Err(bad(format!("插件 {} 没有配置页", req.plugin_id)));
    }
    if req.method.len() > 100 || req.payload.len() > 2_000_000 {
        return Err(bad("请求过大"));
    }
    let method = normalize_page_method(&req.method)?;
    let result = blocking(move || {
        tiangong_plugin_runtime::bridge_call(&req.plugin_id, &method, &req.payload)
    })
    .await?;
    Ok(json!({ "result": result }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_methods_are_normalized() {
        assert_eq!(
            normalize_page_method("bootstrap").unwrap(),
            "plugin.bootstrap"
        );
        assert_eq!(
            normalize_page_method("plugin.save_config").unwrap(),
            "plugin.save_config"
        );
        assert_eq!(
            normalize_page_method("storage.read").unwrap(),
            "storage.read"
        );
        assert!(normalize_page_method("sidecar.invoke").is_err());
        assert!(normalize_page_method("plugin-dev.build").is_err());
        assert!(normalize_page_method("terminal.open").is_err());
    }
}
