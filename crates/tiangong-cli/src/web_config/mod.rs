//! 浏览器配置页入口：`tiangong config` 与 CLI 内 `/config` 共用。
//!
//! 页面复用桌面端设置组件，资源随 Tauri 构建嵌入桌面二进制，HTTP 服务也在
//! 桌面 crate（`tiangong-app`）中实现。本模块只定义启动参数与启动器注册点：
//! 桌面二进制启动时调用 [`set_launcher`] 注入实现，CLI 与 entry 经 [`run`] 调用。

use std::sync::OnceLock;

use anyhow::Result;

/// 配置页启动参数。
#[derive(Debug, Clone)]
pub struct WebConfigOptions {
    /// 监听地址（缺省 127.0.0.1）。
    pub host: String,
    /// 监听端口（None 为随机）。
    pub port: Option<u16>,
    /// 是否自动打开浏览器。
    pub open_browser: bool,
    /// 初始打开的分区（如 `models`、`plugins`）。
    pub initial_tab: Option<String>,
}

impl Default for WebConfigOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: None,
            open_browser: true,
            initial_tab: None,
        }
    }
}

/// 配置页启动器：打开配置页并阻塞到页面关闭。
pub type WebConfigLauncher = Box<dyn Fn(WebConfigOptions) -> Result<()> + Send + Sync>;

static LAUNCHER: OnceLock<WebConfigLauncher> = OnceLock::new();

/// 注册配置页启动器（桌面二进制启动时调用，重复注册忽略）。
pub fn set_launcher(launcher: WebConfigLauncher) {
    let _ = LAUNCHER.set(launcher);
}

/// 打开配置页并阻塞到页面关闭。
pub fn run(options: WebConfigOptions) -> Result<()> {
    match LAUNCHER.get() {
        Some(launcher) => launcher(options),
        None => anyhow::bail!("当前程序未包含配置页，请使用天工桌面应用提供的 tiangong 命令"),
    }
}
