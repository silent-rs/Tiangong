//! 已安装 WASM 插件的发现、加载、状态查询和动态热加载。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Serialize;
use sha2::Digest;
use tiangong_core::core::Plugin;

use crate::adapter::{WasmPluginAdapter, call_wasm_off_runtime};
use crate::config::PluginRuntimeConfig;
use crate::events::PluginChangeKind;
use crate::interpreter_env::{self, InterpreterKind};
use crate::loader::{
    Contribution, Descriptor, WasmPlugin, WasmPluginLoader, compile_component,
    instantiate_component,
};
use crate::manifest::{MANIFEST_FILE, PluginManifest, SidecarRuntime};
use crate::sidecar::{
    CONTENT_MANIFEST_FILE, EphemeralCommandConnection, InterpreterLaunch, ProcessSidecarConnection,
    SidecarConfig, SidecarConnection, StdioSidecarConnection,
};
use crate::signature::{SignedPluginRelease, verify_signed_release};
use crate::ts_plugin::TsPluginAdapter;

// ── 子模块划分 ──
//
// 本文件曾是 4500 行的单体：按职责边界拆分后，全局状态（statics 与其
// 直接操作）与共享数据结构留在本模块；各功能面见对应子模块，全部经
// `pub use` 重导出，外部路径 `crate::registry::*` 保持不变。
//
// - `inventory`：自制插件判据、对话内清单与能力指纹
// - `legacy`：旧插件 ID 迁移（合并保留目录、归档旧目录）
// - `discovery`：磁盘发现、预加载与插件记录构建
// - `loader`：查询视图、WASM 编译装载与 Core 适配器创建
// - `views`：slots/贡献/扩展页等 manifest 级视图
// - `migrations`：安装/启停/回滚/卸载与文件系统事务
// - `connections`：sidecar 连接池（复用/换代/临时连接）与停机清理

mod connections;
mod discovery;
mod inventory;
mod keepalive;
mod legacy;
mod loader;
mod migrations;
mod views;

use connections::stop_loaded_sidecar;
pub(crate) use connections::{
    ephemeral_sidecar_connection, ephemeral_sidecar_connection_with_workspace,
    refresh_stale_sidecar, sidecar_connection, sidecar_connection_with_workspace,
    stop_connection_for_directory,
};
pub use discovery::*;
pub use inventory::*;
pub use legacy::*;
pub use loader::*;
pub use migrations::*;
pub use views::*;

/// 本地信任标记（安装时原生确认后由宿主写入插件目录）。
const LOCAL_TRUST_FILE: &str = "local-trust.json";

static LOADED_PLUGINS: OnceLock<Mutex<HashMap<String, LoadedPlugin>>> = OnceLock::new();
/// 扫描发现但被忽略的无效插件（签名无效、沙箱越权、清单损坏）。
/// 随 `preload_installed_plugins` 全量刷新，供插件管理列表展示和清理。
static INVALID_PLUGINS: OnceLock<Mutex<Vec<InvalidPluginEntry>>> = OnceLock::new();
static SIDECAR_CONNECTIONS: OnceLock<
    Mutex<HashMap<SidecarConnectionKey, Arc<dyn SidecarConnection>>>,
> = OnceLock::new();
static LOAD_OPERATION: std::sync::RwLock<()> = std::sync::RwLock::new(());
static SHUTTING_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
pub(crate) fn persistent_grants_path(storage_root: &Path, plugin_id: &str) -> PathBuf {
    let key = hex::encode(sha2::Sha256::digest(plugin_id.as_bytes()));
    storage_root
        .join("sandbox")
        .join("grants")
        .join(format!("{key}.json"))
}

/// 配置撤权时先停用旧连接，再撤销已登记的文件身份并按新配置重建。
#[cfg(windows)]
pub fn invalidate_persistent_grants(storage_root: &Path) -> Result<()> {
    let _operation = LOAD_OPERATION
        .write()
        .map_err(|_| anyhow::anyhow!("插件操作锁已损坏"))?;
    let directory = storage_root.join("sandbox").join("grants");
    if !directory.is_dir() {
        return Ok(());
    }
    let targets = loaded_plugins()
        .lock()
        .map_err(|_| anyhow::anyhow!("插件注册表已损坏"))?
        .values()
        .filter(|loaded| loaded.manifest.should_preload_sidecar())
        .map(|loaded| (loaded.manifest.id.clone(), loaded.directory.clone()))
        .collect::<Vec<_>>();
    for (id, path) in &targets {
        stop_loaded_sidecar(id)?;
        stop_connection_for_directory(path)?;
    }
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_some_and(|value| value == "json")
            && path
                .file_stem()
                .and_then(|value| value.to_str())
                .is_some_and(|value| {
                    value.len() == 64 && value.bytes().all(|value| value.is_ascii_hexdigit())
                })
        {
            tiangong_sandbox::sandbox::windows::revoke_persistent_grants(&path)?;
        }
    }
    let mut failures = Vec::new();
    for (id, _) in targets {
        if let Err(error) = find_installed_plugin(storage_root, &id)
            .and_then(|installed| reload_plugin_inner(storage_root, &installed))
        {
            set_runtime_error(&id, format!("{error:#}"));
            failures.push(format!("{id}: {error:#}"));
        }
    }
    if !failures.is_empty() {
        bail!(failures.join("; "));
    }
    Ok(())
}

pub fn sidecars_shutting_down() -> bool {
    SHUTTING_DOWN.load(std::sync::atomic::Ordering::Acquire)
}

pub fn begin_sidecar_shutdown() {
    SHUTTING_DOWN.store(true, std::sync::atomic::Ordering::Release);
}

pub(crate) fn background_sidecar_operation() -> Option<std::sync::RwLockWriteGuard<'static, ()>> {
    let guard = LOAD_OPERATION.write().ok()?;
    (!sidecars_shutting_down()).then_some(guard)
}

/// 按需启动 sidecar 的沙箱策略在进程启动后不可扩权，因此连接缓存必须
/// 同时按安装目录和会话工作区区分，不能让默认连接遮蔽当前对话的可写域。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SidecarConnectionKey {
    directory: PathBuf,
    workspace: Option<PathBuf>,
}

/// 全局 server 连接信息（可覆盖更新），供需要回调 host 的 sidecar 使用。
///
/// Server 启停、端口或令牌变化时，入口层调 [`set_server_endpoint`] /
/// [`clear_server_endpoint`] 更新此值，并重启依赖 server 的 sidecar。
static SERVER_ENDPOINT: Mutex<Option<(String, Option<String>)>> = Mutex::new(None);

/// 设置或更新本机 server 的连接信息。
///
/// 与上一次值不同时重启依赖 server 的 sidecar（见 [`restarts_on_server_change`]），
/// 让它们用新的地址/令牌重新连接。
pub fn set_server_endpoint(url: String, token: Option<String>) {
    replace_server_endpoint(Some((url, token)));
}

/// 清除本机 server 的连接信息（嵌入式 server 停止时调用）。
///
/// 依赖 server 的 sidecar 以无 server 环境重启：需要 server 的调用由插件
/// 自行报错，用户重新开启 Server 后再次重启即可恢复。
pub fn clear_server_endpoint() {
    replace_server_endpoint(None);
}

fn replace_server_endpoint(endpoint: Option<(String, Option<String>)>) {
    let restart_needed = {
        let Ok(mut guard) = SERVER_ENDPOINT.lock() else {
            tracing::error!("SERVER_ENDPOINT 锁损坏，无法更新 server 连接信息");
            return;
        };
        let changed = *guard != endpoint;
        *guard = endpoint;
        changed
    };
    if restart_needed {
        restart_server_dependent_sidecars();
    }
}

/// 插件安装目录（`<storage_root>/plugins/<id>`）反推存储根。
fn storage_root_of(directory: &Path) -> Option<PathBuf> {
    directory
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

/// 取当前已设置的 server 连接信息（未设置返回 None）。
fn current_server_endpoint() -> Option<(String, Option<String>)> {
    SERVER_ENDPOINT.lock().ok().and_then(|guard| guard.clone())
}

/// server 连接信息变化时是否需要重启该插件的 sidecar。
///
/// spawn 时注入的 server env 是快照：清单声明 `require_server`、旧版
/// scheduler（经 HTTP 回调 server，清单未声明），以及常驻 sidecar（如 Bot
/// 的 IM 长连接，可能先于 server 启动）都需重启拿到新值。terminal 等按需
/// 或会话驱动的 sidecar 不受影响，避免中断用户终端。
pub(crate) fn restarts_on_server_change(manifest: &crate::manifest::PluginManifest) -> bool {
    manifest.sidecar.is_some()
        && (manifest.require_server
            || SERVER_DEPENDENT_PLUGINS.contains(&manifest.id.as_str())
            || manifest.should_preload_sidecar())
}

/// 重启依赖 server 的已加载插件 sidecar（见 [`restarts_on_server_change`]）。
///
/// Server 地址/令牌变化（含启停）后，旧 sidecar 进程持有的 env 已过期，必须
/// 重启才能拿到新值。停止后常驻 sidecar 立即在后台按新配置重新拉起，按需
/// sidecar 在下次调用时启动。
fn restart_server_dependent_sidecars() {
    if sidecars_shutting_down() {
        return;
    }
    let targets = loaded_plugins()
        .lock()
        .map(|plugins| {
            plugins
                .values()
                .filter(|loaded| restarts_on_server_change(&loaded.manifest))
                .map(|loaded| {
                    (
                        loaded.manifest.id.clone(),
                        loaded.directory.clone(),
                        loaded.enabled && loaded.manifest.should_preload_sidecar(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for (plugin_id, directory, resident) in targets {
        if let Err(error) =
            stop_loaded_sidecar(&plugin_id).and_then(|()| stop_connection_for_directory(&directory))
        {
            tracing::warn!(plugin_id, %error, "重启 server 依赖 sidecar 时停止失败");
            continue;
        }
        tracing::info!(
            plugin_id,
            "server 连接信息变化，已停止 sidecar，将以新配置重启"
        );
        if resident && let Some(storage_root) = storage_root_of(&directory) {
            prewarm_plugin_sidecar(&storage_root, &plugin_id);
        }
    }
}

/// 列出声明 `require_server` 且已启用的插件（id, name）：宿主在关闭
/// Server 时据此提示用户受影响的插件。声明只描述依赖，不触发重启
/// 或状态广播。
pub fn server_dependent_enabled_plugins() -> Vec<(String, String)> {
    let Ok(plugins) = loaded_plugins().lock() else {
        return Vec::new();
    };
    plugins
        .iter()
        .filter(|(_, loaded)| loaded.enabled && loaded.manifest.require_server)
        .map(|(id, loaded)| {
            let name = loaded
                .descriptor
                .as_ref()
                .map(|value| value.name.clone())
                .unwrap_or_else(|| id.clone());
            (id.clone(), name)
        })
        .collect()
}

/// 未在清单声明 `require_server`、但会经 HTTP 回调本机 server 的旧版插件。
const SERVER_DEPENDENT_PLUGINS: &[&str] = &["scheduler"];

const DISABLED_MARKER: &str = ".disabled";
const ROLLBACK_DIR: &str = ".rollback";

/// 天工运行入口类型。
///
/// 插件清单可声明 `entrypoints` 限定适用入口；runtime 据此过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    Desktop,
    Cli,
    Server,
}

impl RuntimeKind {
    pub fn key(&self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Cli => "cli",
            Self::Server => "server",
        }
    }
}

/// 获取当前已配置的模型能力列表（snake_case），供过滤判断。
fn configured_model_capabilities() -> Vec<String> {
    let models = tiangong_config::registry::models();
    tiangong_llm::ModelCapability::all()
        .iter()
        .filter(|cap| models.resolve_for_capability(**cap).is_some())
        .map(|cap| cap.key().to_string())
        .collect()
}

/// 判断插件是否可在当前入口注册为 Core 工具。
///
/// 返回 `Some(reason)` 表示不可注册（reason 为跳过原因，供日志/管理页展示）。
fn check_plugin_availability(
    manifest: &PluginManifest,
    runtime: RuntimeKind,
    configured: &[String],
) -> Option<String> {
    // 入口过滤
    if !manifest.available_at(runtime.key()) {
        return Some(format!("当前入口 {} 不在插件声明的入口列表", runtime.key()));
    }
    // 必需模型能力过滤
    let configured_strs: Vec<&str> = configured.iter().map(String::as_str).collect();
    let missing = manifest.missing_capabilities(&configured_strs);
    if !missing.is_empty() {
        return Some(format!("缺少必需模型能力：{}", missing.join(", ")));
    }
    None
}

#[derive(Clone)]
pub(crate) struct InstalledPlugin {
    pub(crate) directory: PathBuf,
    pub(crate) manifest: PluginManifest,
    pub(crate) enabled: bool,
    pub(crate) signed_release: Option<SignedPluginRelease>,
}

struct LoadedPlugin {
    directory: PathBuf,
    manifest: PluginManifest,
    /// 安装时验定的签名发布信息（None=未签名/本地信任）。
    /// `publisher == local` 是自制插件动态调用通道的分流判据，
    /// 安装时确定、终身不变。
    signed_release: Option<SignedPluginRelease>,
    wasm_bytes: Option<Arc<Vec<u8>>>,
    component: Option<Arc<wasmtime::component::Component>>,
    ui_plugin: Option<Arc<Mutex<WasmPlugin>>>,
    descriptor: Option<Descriptor>,
    generation: u64,
    instances: Vec<Weak<WasmPluginAdapter>>,
    ts_instances: Vec<Weak<TsPluginAdapter>>,
    sidecar: Option<Arc<dyn SidecarConnection>>,
    /// 安装阶段验证记录中仍有效的 sidecar 能力（None=记录缺失或失效）。
    verified_sidecar: Option<Vec<String>>,
    /// 数据/制品/逻辑层加载错误（解释器不可发现、WASM 无法编译等）：
    /// 插件不满足安装条件，安装路径据此回滚。
    load_error: Option<String>,
    /// 启动/握手/运行检查错误（常驻 sidecar 起不来、验证失败、记录保存
    /// 失败等）：插件保留安装，插件管理显示启动异常，重试验证成功后清除。
    runtime_error: Option<String>,
    enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginStatus {
    pub id: String,
    pub name: String,
    /// 插件描述（manifest.description）；None 表示未声明。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub manifest_version: String,
    pub loaded_version: Option<String>,
    pub state: String,
    pub generation: u64,
    pub enabled: bool,
    pub can_rollback: bool,
    pub has_sidecar: bool,
    pub sidecar_running: bool,
    pub last_error: Option<String>,
    /// 插件未注册为可调用工具的原因（如缺少模型能力、入口不匹配）。
    /// None 表示插件当前可调用；Some 表示不可调用及原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

/// 扫描时被忽略的无效插件记录。
///
/// `id` 为插件目录名（可能与清单 ID 不同，如部署残留的 `.terminal-staging-*`），
/// 是清理操作的唯一标识；清单可读时附带名称与版本，便于界面展示。
#[derive(Debug, Clone, Serialize)]
pub struct InvalidPluginEntry {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_version: Option<String>,
    pub reason: String,
}

fn loaded_plugins() -> &'static Mutex<HashMap<String, LoadedPlugin>> {
    LOADED_PLUGINS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 测试辅助：读取已加载插件的验证能力快照，供安装/升级后内存状态
/// 立即可见性的断言使用（安装后运行检查语义）。
#[cfg(test)]
pub(crate) fn loaded_verified_sidecar(plugin_id: &str) -> Option<Vec<String>> {
    loaded_plugins()
        .lock()
        .ok()
        .and_then(|plugins| {
            plugins
                .get(plugin_id)
                .map(|loaded| loaded.verified_sidecar.clone())
        })
        .flatten()
}

/// 测试辅助：读取已加载插件的运行异常信息（启动/运行检查失败时登记）。
#[cfg(test)]
pub(crate) fn loaded_runtime_error(plugin_id: &str) -> Option<String> {
    loaded_plugins().lock().ok().and_then(|plugins| {
        plugins
            .get(plugin_id)
            .and_then(|loaded| loaded.runtime_error.clone())
    })
}

fn invalid_plugins() -> &'static Mutex<Vec<InvalidPluginEntry>> {
    INVALID_PLUGINS.get_or_init(|| Mutex::new(Vec::new()))
}

fn sidecar_connections() -> &'static Mutex<HashMap<SidecarConnectionKey, Arc<dyn SidecarConnection>>>
{
    SIDECAR_CONNECTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 当前用户全局沙箱开关。tiangong-config 注册表是唯一真相源；
/// 页面直调 terminal 与工具调用均在每次 spawn 时读取，配置未初始化时
/// 按开启处理（disabled=false，fail-safe）。
pub fn sandbox_disabled() -> bool {
    tiangong_config::registry::try_sandbox_disabled().unwrap_or(false)
}

/// 用户切换沙箱设置后停止现有按需进程。配置值已由调用方写入唯一真相源；
/// 下一次 terminal/command/解释器/按需插件调用会自主读取并按新状态重建。
pub fn on_sandbox_setting_changed() {
    restart_on_demand_sidecars_for_sandbox_switch();
}

/// 宿主退出时逐个停止所有已启动的 sidecar。
///
/// 先拒绝新的启动并等待正在进行的加载、预热和补验证，再收集连接清理，
/// 避免后台任务在清理快照之后才注册进程而漏过 stop。
pub fn shutdown_all_sidecars() {
    begin_sidecar_shutdown();
    let _operation = LOAD_OPERATION.write().ok();
    crate::ts_tools::cancel_all_calls();
    let connections = sidecar_connections()
        .lock()
        .map(|connections| connections.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let total = connections.len();
    let mut stopped = 0;
    for connection in connections {
        let plugin_id = connection.plugin_id().to_string();
        match connection.stop() {
            Ok(()) => {
                stopped += 1;
                tracing::info!(plugin_id = %plugin_id, "sidecar 已停止");
            }
            Err(error) => {
                // endpoint 文件不存在说明 sidecar 未启动或已退出，属正常情况。
                tracing::debug!(plugin_id = %plugin_id, %error, "停止 sidecar 时无需操作（可能未运行）");
            }
        }
    }
    tracing::info!(total, stopped, "sidecar 关闭完成");
}

/// 用户切换全局沙箱开关后，停止所有首次实际使用才启动的 sidecar。
///
/// OS 沙箱在进程创建时固化，运行中不能原地添加或移除。这里终止当前
/// terminal/command/解释器/按需插件进程并保留连接对象；下一次调用由
/// `spawn` 读取最新开关重建。随 App 预加载的常驻服务不受影响。
fn restart_on_demand_sidecars_for_sandbox_switch() {
    let on_demand_ids = loaded_plugins()
        .lock()
        .map(|plugins| {
            plugins
                .iter()
                .filter(|(_, loaded)| !loaded.manifest.should_preload_sidecar())
                .map(|(id, _)| id.clone())
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();
    if on_demand_ids.is_empty() {
        return;
    }
    let connections = sidecar_connections()
        .lock()
        .map(|connections| {
            connections
                .values()
                .filter(|connection| on_demand_ids.contains(connection.plugin_id()))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for connection in &connections {
        connection.cancel_current();
    }
    tracing::info!(
        sidecars = connections.len(),
        "沙箱开关已切换：按需 sidecar 当前进程已停止，后续调用将按新策略重建"
    );
}
#[cfg(test)]
mod tests;
