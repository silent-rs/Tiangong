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
}

/// 插件变化监听者。同步调用，实现方自行保证不阻塞迁移线程。
pub type PluginsChangedListener = Arc<dyn Fn(&PluginChangeEvent) + Send + Sync>;

fn listener_slot() -> &'static Mutex<Option<PluginsChangedListener>> {
    static SLOT: OnceLock<Mutex<Option<PluginsChangedListener>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
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
pub(crate) fn announce_plugin_change(kind: PluginChangeKind, plugin_id: &str) {
    let fingerprint = crate::registry::enabled_plugin_fingerprint();
    let event = PluginChangeEvent {
        kind,
        plugin_id: plugin_id.to_string(),
        fingerprint,
    };
    let listener = listener_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(listener) = listener {
        listener(&event);
    }
    crate::bridge::emit_plugins_changed();
}

/// 迁移结果包装：成功时广播事件，失败原样透传。
///
/// 事件语义是幂等事实快照：无实际变化的短路成功（如对已停用插件再次
/// 停用）也会发布——订阅者按 `fingerprint` 定档自然短路，无需 runtime
/// 区分「真迁移」与「重复意图」。
pub(crate) fn announced<T, E>(
    kind: PluginChangeKind,
    plugin_id: &str,
    result: Result<T, E>,
) -> Result<T, E> {
    if result.is_ok() {
        announce_plugin_change(kind, plugin_id);
    }
    result
}
