//! 桌面端 Core 构造依赖（issue #245）。
//!
//! host 专属状态（Tauri app_handle、CoreConfigProvider 的 generation）留在本结构。
//! Core 的插件列表只持有一个 [`RuntimeCorePlugin`] 聚合桥（编译期唯一成员）：
//! 对 Core 而言插件集合构造后永不变化，已安装插件的能力（工具/prompt/钩子）
//! 由桥在被调用时向 runtime 注册表聚合——新装插件下一轮自然出现，启停/升级/
//! 卸载由 runtime 经适配器就地生效，Core 与 app 都不需要"被通知"。

use std::sync::Arc;

use tauri::AppHandle;
use tiangong_core::config::core::CoreConfigProvider;
use tiangong_core::core::Plugin;

/// 桌面端 Core 构造依赖。
///
/// 与 `TiangongApp` 共享以下句柄（同一实例，dual-ownership 语义不变）：
/// - `app_handle`：Tauri 句柄（setup 阶段注入；构造时尚未就绪）
/// - `config`：全局 CoreConfigProvider
/// - `storage_root`：会话文件根
#[derive(Clone)]
pub struct DesktopCoreFactory {
    pub app_handle: Arc<std::sync::OnceLock<AppHandle>>,
    pub config: CoreConfigProvider,
    pub storage_root: std::path::PathBuf,
}

impl DesktopCoreFactory {
    /// 构造桌面端 Core 的插件集合（issue #245；`ensure_core` 按需回调）。
    ///
    /// 返回单元素列表：为该会话实例化的 runtime 聚合桥。每个 Core 持有独立
    /// 桥实例（交付表互不共享），与既有「各 Core 适配器隔离 per-session 状态」
    /// 语义一致。
    pub fn build_plugins_sync(
        &self,
        session_id: &str,
        _models: tiangong_llm::models_config::ModelsConfig,
    ) -> Vec<Arc<dyn Plugin>> {
        if self.app_handle.get().is_none() {
            tracing::warn!("app_handle 尚未注入，桌面插件构造中止");
            return Vec::new();
        }
        vec![tiangong_plugin_runtime::RuntimeCorePlugin::new(
            tiangong_plugin_runtime::registry::RuntimeKind::Desktop,
            self.storage_root.clone(),
            session_id,
        )]
    }
}
