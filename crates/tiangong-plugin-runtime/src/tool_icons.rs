//! 工具图标：界面工具行展示用的图标查询表。
//!
//! 优先级（高 → 低）：
//! 1. 插件在 `plugin.json` 的 `tool_icons` 中为该工具声明的图标；
//! 2. 插件在 `tool_icons` 中用 `*` 声明的默认图标；
//! 3. runtime 内置表（按工具原名，短期在此维护，插件普遍声明后逐步收缩）；
//! 4. 都没有时不给出，前端使用默认图标（工具交叉）。
//!
//! 图标值为宿主内置图标名（小写字母、数字与 `-`），或插件目录内 png/svg/jpeg
//! 相对路径。声明不合法时只跳过该条并告警，不影响插件装载与运行。

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::Serialize;
use tiangong_core::tools::extension::namespaced_tool_name;

/// `tool_icons` 中表示「本插件其余工具」的键。
pub const PLUGIN_DEFAULT_ICON_KEY: &str = "*";

/// 图标资源扩展名白名单（与 UI 贡献图标一致）。
const ICON_RESOURCE_EXTENSIONS: &[&str] = &["png", "svg", "jpg", "jpeg"];

/// runtime 内置图标表：工具原名 → 图标名。
const BUILTIN_TOOL_ICONS: &[(&str, &str)] = &[
    // 终端
    ("run_command", "square-terminal"),
    ("run_shell", "square-terminal"),
    ("terminal_send", "square-terminal"),
    ("terminal_open", "square-terminal"),
    ("terminal_close", "square-terminal"),
    ("terminal_status", "square-terminal"),
    // 文件
    ("read_file", "file-text"),
    ("list_dir", "folder-tree"),
    ("tree_dir", "folder-tree"),
    ("write_file", "file-pen-line"),
    ("replace_in_file", "file-pen-line"),
    ("apply_patch", "file-pen-line"),
    ("current_time", "clock"),
    // 检索
    ("index_search", "search"),
    ("search_code", "search"),
    ("grep", "search"),
    ("glob", "search"),
    ("search", "search"),
    // 网页与浏览器
    ("web_fetch", "globe"),
    ("web_search", "globe"),
    ("web_page_text", "globe"),
    ("web_query_dom", "globe"),
    ("web_click", "globe"),
    ("web_form_extract", "globe"),
    ("web_form_fill", "globe"),
    ("web_locate_element", "globe"),
    ("browser_open", "globe"),
    ("browser_navigate", "globe"),
    ("browser_close", "globe"),
    ("browser_eval", "globe"),
    // 记忆
    ("recall_memory", "brain"),
    // 媒体
    ("generate_image", "image"),
    ("generate_video", "video"),
    ("text_to_speech", "volume-2"),
    ("speech_to_text", "mic"),
    ("analyze_attachment", "scan-eye"),
    // 桌面操作
    ("desktop_app", "monitor"),
    ("desktop_screenshot", "camera"),
    ("desktop_input", "monitor"),
    ("desktop_ui", "monitor"),
    ("desktop_wait", "monitor"),
    // 定时任务
    ("scheduler_create_job", "calendar-clock"),
    ("scheduler_list_jobs", "calendar-clock"),
    ("scheduler_update_job", "calendar-clock"),
    ("scheduler_delete_job", "calendar-clock"),
    ("scheduler_trigger_job", "calendar-clock"),
    ("scheduler_get_job_runs", "calendar-clock"),
    // Skill 与编码工作流
    ("get_skill_detail", "book-open"),
    ("coding_project_context", "code"),
    ("coding_preflight", "code"),
    ("coding_checkpoint", "code"),
    ("coding_review", "code"),
    // Subagent
    ("create_agent", "bot"),
    ("list_agents", "bot"),
    ("get_agent", "bot"),
    ("activate_agent", "bot"),
    ("deactivate_agent", "bot"),
    ("list_active_agents", "bot"),
    ("send_agent_message", "bot"),
    ("submit_agent_task", "bot"),
    ("get_agent_task", "bot"),
    ("list_agent_tasks", "bot"),
    ("get_agent_run", "bot"),
    ("interrupt_agent_run", "bot"),
    ("cancel_agent_run", "bot"),
    ("list_agent_events", "bot"),
    ("get_agent_artifacts", "bot"),
    ("get_agent_memory", "bot"),
    ("append_agent_memory", "bot"),
    ("append_agent_instructions", "bot"),
    ("report_agent_result", "bot"),
    ("list_pending_work", "bot"),
    ("load_workspace_state", "bot"),
    ("update_workspace_state", "bot"),
    // 交互与插件
    ("request_user", "message-circle-question"),
    ("plugin_init", "package"),
    ("plugin_devkit", "package"),
    ("plugin_install", "package"),
    ("call_local_plugin", "puzzle"),
    ("list_local_plugins", "puzzle"),
    ("plugin_injection", "plug"),
];

/// 查询表中的一项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolIcon {
    /// 图标名，或插件目录内的图标资源相对路径。
    pub icon: String,
    /// 图标来自的插件；资源图标据此读取文件，内置表条目为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
}

/// 参与建表的插件信息：插件 id、`tool_icons` 声明与插件声明的工具原名。
pub struct PluginToolIcons<'a> {
    pub plugin_id: &'a str,
    pub declared: Option<&'a BTreeMap<String, String>>,
    pub tools: Vec<String>,
}

/// 内置表查询（按工具原名）。
pub fn builtin_tool_icon(tool: &str) -> Option<&'static str> {
    BUILTIN_TOOL_ICONS
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, icon)| *icon)
}

/// 图标值是否为资源路径形态（含 `/` 或 `.`）。
pub fn is_icon_resource(icon: &str) -> bool {
    icon.contains('/') || icon.contains('.')
}

/// 图标值是否合法：内置图标名（小写字母、数字、`-`）或安全的白名单资源路径。
pub fn is_valid_icon(icon: &str) -> bool {
    let icon = icon.trim();
    if icon.is_empty() || icon.len() > 256 {
        return false;
    }
    if !is_icon_resource(icon) {
        return icon
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
    }
    let path = Path::new(icon);
    let safe = path.components().all(|component| {
        !matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    safe && ICON_RESOURCE_EXTENSIONS.contains(&extension.as_str())
}

/// 取插件声明中的合法图标；不合法时告警并视为未声明。
fn declared_icon(
    plugin_id: &str,
    declared: Option<&BTreeMap<String, String>>,
    key: &str,
) -> Option<String> {
    let icon = declared?.get(key)?.trim();
    if is_valid_icon(icon) {
        Some(icon.to_string())
    } else {
        tracing::warn!(plugin = %plugin_id, tool = %key, icon = %icon, "插件声明的工具图标不合法，已忽略");
        None
    }
}

/// 解析单个插件工具的图标（插件声明优先，其次内置表）。
pub fn resolve_plugin_tool_icon(
    plugin_id: &str,
    declared: Option<&BTreeMap<String, String>>,
    tool: &str,
) -> Option<ToolIcon> {
    if let Some(icon) = declared_icon(plugin_id, declared, tool)
        .or_else(|| declared_icon(plugin_id, declared, PLUGIN_DEFAULT_ICON_KEY))
    {
        return Some(ToolIcon {
            icon,
            plugin_id: Some(plugin_id.to_string()),
        });
    }
    builtin_tool_icon(tool).map(|icon| ToolIcon {
        icon: icon.to_string(),
        plugin_id: None,
    })
}

/// 建立工具图标查询表：键为工具对外名 `{插件id}__{工具名}` 与原名（旧会话历史）。
///
/// 先放入内置表（覆盖无插件归属的工具，如 core 内置注入工具），再按插件
/// 覆盖；查不到的工具不在表中，由前端使用默认图标。
pub fn build_tool_icon_table<'a>(
    plugins: impl IntoIterator<Item = PluginToolIcons<'a>>,
) -> BTreeMap<String, ToolIcon> {
    let mut table: BTreeMap<String, ToolIcon> = BUILTIN_TOOL_ICONS
        .iter()
        .map(|(name, icon)| {
            (
                (*name).to_string(),
                ToolIcon {
                    icon: (*icon).to_string(),
                    plugin_id: None,
                },
            )
        })
        .collect();
    for plugin in plugins {
        for tool in &plugin.tools {
            let Some(icon) = resolve_plugin_tool_icon(plugin.plugin_id, plugin.declared, tool)
            else {
                continue;
            };
            if let Some(exposed) = namespaced_tool_name(plugin.plugin_id, tool) {
                table.insert(exposed, icon.clone());
            }
            table.insert(tool.clone(), icon);
        }
    }
    table
}

/// 资源图标是否确为该插件 `tool_icons` 中声明的值（读取文件前的校验）。
pub fn declares_icon_resource(declared: Option<&BTreeMap<String, String>>, icon: &str) -> bool {
    is_icon_resource(icon)
        && is_valid_icon(icon)
        && declared.is_some_and(|map| map.values().any(|value| value.trim() == icon))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn 插件声明优先于内置表() {
        let icons = declared(&[("generate_image", "palette")]);
        let icon = resolve_plugin_tool_icon("volcengine", Some(&icons), "generate_image").unwrap();
        assert_eq!(icon.icon, "palette");
        assert_eq!(icon.plugin_id.as_deref(), Some("volcengine"));
    }

    #[test]
    fn 插件默认图标优先于内置表_未声明时回落内置表() {
        let icons = declared(&[("*", "flame")]);
        assert_eq!(
            resolve_plugin_tool_icon("volcengine", Some(&icons), "generate_image")
                .unwrap()
                .icon,
            "flame"
        );
        let builtin = resolve_plugin_tool_icon("fs", None, "read_file").unwrap();
        assert_eq!(builtin.icon, "file-text");
        assert_eq!(builtin.plugin_id, None);
        assert_eq!(resolve_plugin_tool_icon("demo", None, "unknown_tool"), None);
    }

    #[test]
    fn 不合法声明被忽略且不影响其余解析() {
        let icons = declared(&[
            ("generate_image", "../../etc/passwd.png"),
            ("generate_video", "Bad Name"),
            ("speak", "icons/speak.gif"),
            ("*", ""),
        ]);
        assert_eq!(
            resolve_plugin_tool_icon("p", Some(&icons), "generate_image")
                .unwrap()
                .icon,
            "image",
            "逃逸路径被忽略，回落内置表"
        );
        assert_eq!(
            resolve_plugin_tool_icon("p", Some(&icons), "generate_video")
                .unwrap()
                .icon,
            "video"
        );
        assert_eq!(
            resolve_plugin_tool_icon("p", Some(&icons), "speak"),
            None,
            "扩展名不在白名单，且无内置项"
        );
    }

    #[test]
    fn 资源图标合法性() {
        assert!(is_valid_icon("icons/tool.svg"));
        assert!(is_valid_icon("tool.PNG"));
        assert!(is_valid_icon("square-terminal"));
        assert!(!is_valid_icon("/abs/tool.svg"));
        assert!(!is_valid_icon("../tool.svg"));
        assert!(!is_valid_icon("tool.exe"));
        assert!(!is_valid_icon("Tool"));
    }

    #[test]
    fn 查询表同时以原名与插件前缀名为键() {
        let volc = declared(&[("generate_image", "icons/seedream.svg")]);
        let table = build_tool_icon_table([
            PluginToolIcons {
                plugin_id: "generate-image-openai",
                declared: None,
                tools: vec!["generate_image".to_string()],
            },
            PluginToolIcons {
                plugin_id: "volcengine",
                declared: Some(&volc),
                tools: vec!["generate_image".to_string(), "generate_video".to_string()],
            },
        ]);
        assert_eq!(
            table["generate-image-openai__generate_image"].icon, "image",
            "未声明的插件回落内置表"
        );
        assert_eq!(
            table["volcengine__generate_image"].icon,
            "icons/seedream.svg"
        );
        assert_eq!(
            table["volcengine__generate_image"].plugin_id.as_deref(),
            Some("volcengine")
        );
        assert_eq!(table["generate_video"].icon, "video");
        assert_eq!(
            table["plugin_injection"].icon, "plug",
            "无插件归属的工具用内置表"
        );
        assert!(!table.contains_key("unknown_tool"));
    }

    #[test]
    fn 只能读取插件声明过的资源图标() {
        let icons = declared(&[("speak", "icons/speak.svg"), ("*", "flame")]);
        assert!(declares_icon_resource(Some(&icons), "icons/speak.svg"));
        assert!(!declares_icon_resource(Some(&icons), "icons/other.svg"));
        assert!(!declares_icon_resource(Some(&icons), "flame"));
        assert!(!declares_icon_resource(None, "icons/speak.svg"));
    }
}
