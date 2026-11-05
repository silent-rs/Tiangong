//! 天工插件运行时。
//!
//! 提供清单声明插件的装载与 sidecar 直连/桌面 TS 工具桥接，以及配套
//! sidecar 的通用协议和运行管理。
//!
//! # 公共中立约束
//!
//! 本 crate 必须始终保持插件无关，不得包含任何具体插件的 ID、操作名、协议类型、
//! 数据路径、业务事件或条件分支。单个插件需要的行为必须放在该插件的私有协议
//! 或 sidecar 中。只有能被任意插件复用、且无需理解业务负载的机制，才能进入
//! 运行时。完整约束见 `docs/plugin-development.md` 的“公共中立约束”。
//! 唯一的过渡期例外是 [`builtin_tables`]：宿主展示用的工具名映射纯数据集中
//! 于此，插件自行声明后逐条删除。

// ── 域目录（物理布局与 docs/plugin-runtime-module-boundaries.md 九域一致）──
pub mod bridge;
mod contract;
mod distribution;
pub mod host_policy;
mod invocation;
pub mod plugin_dev;
pub mod registry;
#[doc(hidden)]
pub mod test_support;
mod transport;
pub mod trust;
// 旧顶层模块移入域目录后，经重导出保持 crate 内与对外的旧路径全部可达。
pub use bridge::core_bridge;
pub use bridge::{
    BRIDGE_NAMESPACES, BridgeNamespace, BridgeNamespaceHandler, EVENT_NAMESPACE_PREFIXES,
    NativeServiceHandler, bridge_call, bridge_call_with_workspace, bridge_emit, bridge_subscribe,
    bridge_unsubscribe, emit_plugins_changed, invoke_plugin_sidecar, register_bridge_namespace,
    release_session_plugin_resources, request_app_open, set_app_handler, set_browser_handler,
    set_dialog_handler, set_event_emitter, set_session_input_handler, set_terminal_handler,
    set_webview_handler, unregister_bridge_namespace,
};
pub use bridge::{SidecarResultObserver, set_sidecar_result_observer};
pub use contract::{launch_policy, manifest, slots, storage_layout, tool_key};
pub use core_bridge::{
    CALL_LOCAL_PLUGIN_TOOL, LIST_LOCAL_PLUGINS_TOOL, LOCAL_PLUGIN_LIST_INJECTION, RuntimeCorePlugin,
};
pub use distribution::artifacts;
pub use events::{
    PluginChangeEvent, PluginChangeKind, PluginsChangedListener, set_plugins_changed_listener,
};
pub use interpreter_env::ensure_interpreter_env;
pub use invocation::runtime_feedback;
pub(crate) use invocation::{execution, ts_plugin, ts_tools};
pub use plugin_dev::{
    ImportSummary, InstallerIdentity, TrustedInstallerHandler, content_manifest_fingerprint,
    note_trusted_build, set_plugin_dev_import_confirm, set_plugin_dev_trusted_installer,
    trusted_build_fingerprint,
};
pub use registry::{builtin_tables, events, tool_icons, tool_titles};
pub use sidecar::{
    ProcessSidecarConnection, SidecarConfig, SidecarConnection, SidecarNotificationForwarder,
    set_sidecar_notification_forwarder,
};
pub use slots::{
    BUILTIN_SLOTS, OPEN_MODE_SLOT, OpenMode, SandboxKind, SlotContextKey, SlotDescriptor,
    SlotInstances, SlotRegistry, UiContribution,
};
/// 宿主与 sidecar 共用的通信协议（定义在 `tiangong-plugin-sidecar`）。
pub use tiangong_plugin_sidecar::protocol;
pub use transport::{interpreter_env, launcher_update, platform, sidecar};
pub use trust::{
    LOCAL_PUBLISHER, OFFICIAL_PUBLISHER, TrustedPublisher, ensure_user_signing_key,
    import_trusted_publisher, list_trusted_publishers, publisher_fingerprint,
    remove_trusted_publisher,
};
pub use trust::{content_manifest, signature, verification};

/// Runtime 启动 Sidecar 时始终屏蔽的精确环境变量名。
pub const BUILTIN_DENIED_ENV_KEYS: &[&str] = &[
    "BASH_ENV",
    "ENV",
    "PS4",
    "NODE_OPTIONS",
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "PERL5OPT",
    "RUBYOPT",
    "RUBYLIB",
    "JAVA_TOOL_OPTIONS",
    "ZDOTDIR",
];

/// Runtime 启动 Sidecar 时始终屏蔽的环境变量名前缀。
pub const BUILTIN_DENIED_ENV_PREFIXES: &[&str] = &["LD_", "DYLD_"];

/// 内置注入类屏蔽的精确变量名清单（匹配大小写不敏感）。
pub fn builtin_denied_env_keys() -> &'static [&'static str] {
    BUILTIN_DENIED_ENV_KEYS
}

/// 内置屏蔽的变量名前缀清单（匹配大小写不敏感）。
pub fn builtin_denied_env_prefixes() -> &'static [&'static str] {
    BUILTIN_DENIED_ENV_PREFIXES
}
