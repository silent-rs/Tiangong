//! 插件市场展示数据：默认插件集合、场景分类与首次启动引导标记。
//!
//! 这是**宿主产品数据**而非运行时机制：runtime 只负责目录检索、下载与安装，
//! 「哪些插件是基础能力、归入哪个分类、要不要弹首启引导」由宿主决定。
//! 前端契约（`AvailablePlugin` 的 `is_default` / `categories`）经
//! [`AvailablePluginView`] 补齐，字段名与顺序保持不变。

use std::path::Path;

use tiangong_plugin_runtime::artifacts::AvailablePlugin;

/// 首次启动引导完成标记文件名（位于存储根下）。
const FIRST_LAUNCH_MARKER: &str = ".first_launch_completed";

/// 默认插件 ID 列表。
///
/// 这些插件提供 Agent 的基础能力（系统提示词、文件操作、命令执行、网络获取、
/// 索引搜索、技能管理、MCP 工具桥接、审批征询、终端与浏览器），确保用户安装后
/// 即可获得基本体验。
pub const DEFAULT_PLUGIN_IDS: &[&str] = &[
    "prompt",
    "fs",
    "command",
    "fetch",
    "index",
    "skill",
    "mcp",
    "interaction",
    "terminal",
    "browser",
];

/// 场景分类常量：日常工作。
pub const CATEGORY_DAILY: &str = "daily";
/// 场景分类：编程开发。
pub const CATEGORY_CODING: &str = "coding";

/// 返回某插件的场景分类标签（多标签，可同时属于多个分类）。
///
/// 未在映射中声明的插件默认归入日常分类，保证插件市场不会出现无分类的孤立条目。
pub fn plugin_categories(id: &str) -> Vec<&'static str> {
    match id {
        // 基础能力：日常与编程通用。
        "prompt" | "fs" | "command" | "fetch" | "index" | "skill" | "mcp" | "memory"
        | "interaction" | "terminal" | "browser" => {
            vec![CATEGORY_DAILY, CATEGORY_CODING]
        }
        // 编程开发专属。
        "coding" => vec![CATEGORY_CODING],
        // 其余（定时任务、多媒体生成/分析）默认归入日常。
        _ => vec![CATEGORY_DAILY],
    }
}

/// 判断插件是否属于默认插件集合。
pub fn is_default_plugin(id: &str) -> bool {
    DEFAULT_PLUGIN_IDS.contains(&id)
}

/// 目录条目 + 宿主展示字段（前端契约与 `AvailablePlugin` 一致）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AvailablePluginView {
    #[serde(flatten)]
    pub plugin: AvailablePlugin,
    /// 是否为默认插件（基础能力，首次启动时会推荐安装）。
    pub is_default: bool,
    /// 场景分类标签（多标签，值为 `daily` / `coding` 的任意组合）。
    pub categories: Vec<&'static str>,
}

impl AvailablePluginView {
    pub fn new(plugin: AvailablePlugin) -> Self {
        Self {
            is_default: is_default_plugin(&plugin.id),
            categories: plugin_categories(&plugin.id),
            plugin,
        }
    }
}

/// 首次启动引导是否已完成（标记文件存在即视为完成）。
pub fn is_first_launch_completed(storage_root: &Path) -> bool {
    storage_root.join(FIRST_LAUNCH_MARKER).is_file()
}

/// 写入首次启动完成标记。内容为当前本地时间，便于排查。
pub fn mark_first_launch_completed(storage_root: &Path) -> std::io::Result<()> {
    let marker = storage_root.join(FIRST_LAUNCH_MARKER);
    std::fs::create_dir_all(storage_root)?;
    let timestamp = chrono::Local::now().naive_local().to_string();
    std::fs::write(&marker, timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认插件与分类映射() {
        assert!(is_default_plugin("fs"));
        assert!(!is_default_plugin("coding"));
        assert_eq!(plugin_categories("coding"), vec![CATEGORY_CODING]);
        assert_eq!(
            plugin_categories("fs"),
            vec![CATEGORY_DAILY, CATEGORY_CODING]
        );
        assert_eq!(plugin_categories("unknown"), vec![CATEGORY_DAILY]);
    }
}
