//! 插件生命周期变化事件。
//!
//! runtime 是插件状态机的唯一所有者：安装/升级/回滚/重载/启停/卸载的
//! 迁移在本 crate 内完成，成功即广播事实（[`PluginChangeEvent`]）。宿主
//! （app / server / cli）经 [`set_plugins_changed_listener`] 注册订阅者
//! 各自响应——UI 刷新、上下文交接打标等编排不再散落在调用方成功点。
//!
//! 事件回调在迁移函数的注册表锁全部释放后同步调用；回调内不得再回调
//! 本 crate 的迁移 API（避免重入持有操作写锁）。`fingerprint` 为发布
//! 时刻的启用插件指纹，订阅者免重算。

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

/// 一次插件生命周期迁移的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginChangeKind {
    /// 全新安装（此前不在注册表）。
    Installed,
    /// 同 ID 换代：升级 / 导入替换 / 回滚 / 重载。
    Upgraded,
    /// 启用。
    Enabled,
    /// 停用。
    Disabled,
    /// 卸载（含无效插件清理）。
    Uninstalled,
    /// 动态 prompt 段落内容变化（sidecar 经 `runtime.prompt_updated` 通知
    /// 重取后内容有变）：能力指纹随段落摘要变化，订阅者按指纹触发上下文
    /// 交接（压缩 message），与升级/启停同链路。
    PromptSectionsChanged,
    /// sidecar 动态工具声明变化（`runtime.tools_updated` 通知重取后有变）：
    /// core 每轮经适配器 `tool_specs()` 读取，快照更新后下一轮工具装配
    /// 自动生效；工具清单不进能力指纹（与 WASM 时代行为一致）。
    ToolsChanged,
}

/// 插件生命周期变化事件：迁移成功后由 runtime 发布。
#[derive(Debug, Clone)]
pub struct PluginChangeEvent {
    /// 迁移种类。
    pub kind: PluginChangeKind,
    /// 变化涉及的插件 id。
    pub plugin_id: String,
    /// 发布时刻的启用插件指纹（id@version），订阅者免重算。
    pub fingerprint: String,
    /// 发布时刻的存储根（部分事件由 sidecar 通知触发，拿不到时为 `None`）。
    /// 卸载收尾据此定位沙箱授权记录，订阅者无需自行推导。
    pub storage_root: Option<std::path::PathBuf>,
}

/// 插件变化监听者。同步调用，实现方自行保证不阻塞迁移线程。
pub type PluginsChangedListener = Arc<dyn Fn(&PluginChangeEvent) + Send + Sync>;

fn listener_slot() -> &'static Mutex<Option<PluginsChangedListener>> {
    static SLOT: OnceLock<Mutex<Option<PluginsChangedListener>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// crate 内部订阅者：runtime 各域在迁移事实发布时自行释放资源。
///
/// 安装事务（D3）只负责制品与磁盘状态；挂起的工具调用、桥接订阅这类
/// 运行时资源由拥有它们的域按事件收尾，安装代码不再直接调用它们。
fn internal_listener_slot() -> &'static Mutex<Vec<PluginsChangedListener>> {
    static SLOT: OnceLock<Mutex<Vec<PluginsChangedListener>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Vec::new()))
}

/// 注册 crate 内部订阅者（仅本 crate 可见）。
///
/// 注册一次即可：调用方用 `OnceLock` 之类的守卫保证幂等，重复注册会导致
/// 同一事件被通知多次。
pub(crate) fn add_plugins_changed_listener(listener: PluginsChangedListener) {
    internal_listener_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(listener);
}

/// 插件集变更的页面广播出口。
///
/// 主前端的宿主事件到达不了插件沙箱，订阅了 `plugins.changed` 的插件页面
/// 需要一条单独的推送通道；具体怎么推（桥接事件、测试记录器）由注入方决定，
/// 状态机只负责在迁移成功后调用。
pub type PluginChangeBroadcaster = Arc<dyn Fn() + Send + Sync>;
fn broadcaster_slot() -> &'static OnceLock<PluginChangeBroadcaster> {
    static SLOT: OnceLock<PluginChangeBroadcaster> = OnceLock::new();
    &SLOT
}
/// 注册页面广播出口（宿主启动时调用一次，重复调用以首次为准）。
pub fn set_plugin_change_broadcaster(broadcaster: PluginChangeBroadcaster) {
    let _ = broadcaster_slot().set(broadcaster);
}
fn plugin_change_broadcaster() -> Option<PluginChangeBroadcaster> {
    broadcaster_slot().get().cloned()
}
/// 注册插件变化监听者（重复调用覆盖旧值；传 `None` 注销）。
///
/// 未注册时事件静默丢弃——server/cli 等无宿主 UI 的入口不注册即可。
pub fn set_plugins_changed_listener(listener: Option<PluginsChangedListener>) {
    *listener_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = listener;
}

/// 发布一次插件变化事件。
///
/// 必须在本 crate 所有注册表锁释放后调用（指纹计算内部会短暂持锁）。
/// 同时把变更下发给订阅了 `plugins.changed` 的插件页面——主前端的宿主
/// 事件到达不了插件沙箱，插件页面经桥接订阅后自行刷新。
pub(crate) fn announce_plugin_change(
    kind: PluginChangeKind,
    plugin_id: &str,
    storage_root: Option<&Path>,
) {
    let fingerprint = crate::registry::enabled_plugin_fingerprint();
    let event = PluginChangeEvent {
        kind,
        plugin_id: plugin_id.to_string(),
        fingerprint,
        storage_root: storage_root.map(Path::to_path_buf),
    };
    release_plugin_resources(&event);
    let listener = listener_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(listener) = listener {
        listener(&event);
    }
    let internal = internal_listener_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    for listener in internal {
        listener(&event);
    }
    if let Some(broadcast) = plugin_change_broadcaster() {
        broadcast();
    }
}

/// 迁移事实发布前的资源收尾：停用、卸载、换代之后，该插件挂起的工具
/// 调用与桥接订阅都不再有意义。
///
/// 释放动作放在事件发布方（本域）而不是安装事务里：安装代码只负责制品
/// 与磁盘状态，运行时资源由拥有它的域按同一份事实各自收尾，避免安装路径
/// 反向依赖调用层与桥接层。桥接订阅由 `bridge::install_runtime_feedback`
/// 注册的内部订阅者清理（D7 订阅 D4，方向与依赖矩阵一致）。
fn release_plugin_resources(event: &PluginChangeEvent) {
    if matches!(
        event.kind,
        PluginChangeKind::Disabled | PluginChangeKind::Uninstalled | PluginChangeKind::Upgraded
    ) {
        crate::tools::cancel_plugin_calls(&event.plugin_id);
    }
    // 卸载后撤销该插件的沙箱持久授权（Windows AppContainer 记录）。
    if event.kind == PluginChangeKind::Uninstalled
        && let Some(root) = event.storage_root.as_deref()
    {
        crate::launcher_update::revoke_plugin_grants(root, &event.plugin_id);
    }
}

/// 迁移结果包装：成功时广播事件，失败原样透传。
///
/// 事件语义是幂等事实快照：无实际变化的短路成功（如对已停用插件再次
/// 停用）也会发布——订阅者按 `fingerprint` 定档自然短路，无需 runtime
/// 区分「真迁移」与「重复意图」。
pub(crate) fn announced<T, E>(
    kind: PluginChangeKind,
    plugin_id: &str,
    storage_root: Option<&Path>,
    result: Result<T, E>,
) -> Result<T, E> {
    if result.is_ok() {
        announce_plugin_change(kind, plugin_id, storage_root);
    }
    result
}
