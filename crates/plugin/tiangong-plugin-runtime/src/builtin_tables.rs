//! 宿主内置的展示映射表（过渡期集中维护）。
//!
//! 插件普遍通过清单自行声明展示信息之前，宿主各端（Desktop 前端、CLI）
//! 需要的少量按工具名 / 参数名的映射只在这里维护，App 层一律经运行时
//! 查询、不再写死。插件声明后对应条目逐步删除，最终整体移除本模块。
//!
//! 约束：只放「名称 → 展示提示」的纯数据，不承载任何插件业务逻辑。

use serde::Serialize;

/// 工具图标内置表：工具原名 → 图标名（插件未在 `tool_icons` 声明时回落）。
pub(crate) const BUILTIN_TOOL_ICONS: &[(&str, &str)] = &[
    // 终端
    ("run_command", "square-terminal"),
    ("run_shell", "square-terminal"),
    ("terminal_send", "square-terminal"),
    ("terminal_open", "square-terminal"),
    ("terminal_close", "square-terminal"),
    ("terminal_status", "square-terminal"),
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

/// 内置图标表查询（按工具原名）。
pub(crate) fn builtin_tool_icon(tool: &str) -> Option<&'static str> {
    BUILTIN_TOOL_ICONS
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, icon)| *icon)
}

/// 终端字形表：图标名 → 终端可显示的字形（CLI 等无法渲染图标的端使用）。
const TERMINAL_GLYPHS: &[(&str, &str)] = &[
    ("square-terminal", "⚡"),
    ("search", "🔍"),
    ("globe", "🌐"),
    ("brain", "🧠"),
    ("image", "🎨"),
    ("video", "🎬"),
    ("volume-2", "🔊"),
    ("mic", "🎤"),
    ("scan-eye", "👁"),
    ("monitor", "🖥"),
    ("camera", "📷"),
    ("calendar-clock", "⏰"),
    ("book-open", "📖"),
    ("code", "💻"),
    ("bot", "🤖"),
    ("message-circle-question", "❓"),
    ("package", "📦"),
    ("puzzle", "🧩"),
    ("plug", "🔌"),
];

/// 无对应字形时的默认终端字形。
pub const DEFAULT_TERMINAL_GLYPH: &str = "🔧";

/// 图标名对应的终端字形；资源图标或未登记的图标名返回 None。
pub fn terminal_glyph(icon: &str) -> Option<&'static str> {
    TERMINAL_GLYPHS
        .iter()
        .find(|(name, _)| *name == icon)
        .map(|(_, glyph)| *glyph)
}

/// 通用参数摘要的展示提示（工具行「工具名 + 参数摘要」使用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolArgsPresentation {
    /// 摘要中排在最前、且只显示取值的参数（如按 action 分派的工具）。
    pub lead_args: &'static [&'static str],
    /// 摘要跳过的环境 / 配置类参数（不构成「这次调用做了什么」）。
    pub skip_args: &'static [&'static str],
    /// 位置参数中不展示的取值前缀（宿主混入的内部参数）。
    pub hidden_value_prefixes: &'static [&'static str],
}

/// 当前的参数摘要展示提示。
pub fn tool_args_presentation() -> ToolArgsPresentation {
    ToolArgsPresentation {
        lead_args: &["action"],
        skip_args: &[
            "cwd",
            "workdir",
            "working_dir",
            "timeout",
            "timeout_secs",
            "interactive",
            "encoding",
            "offset",
            "limit",
            "append",
            "dry_run",
            "force",
        ],
        hidden_value_prefixes: &["__tiangong_cwd="],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 内置图标均有合法图标名() {
        for (tool, icon) in BUILTIN_TOOL_ICONS {
            assert!(crate::tool_icons::is_valid_icon(icon), "{tool}: {icon}");
            assert_eq!(builtin_tool_icon(tool), Some(*icon));
        }
        assert_eq!(builtin_tool_icon("unknown_tool"), None);
    }

    #[test]
    fn 内置图标均有终端字形() {
        for (tool, icon) in BUILTIN_TOOL_ICONS {
            assert!(
                terminal_glyph(icon).is_some(),
                "{tool}: {icon} 缺少终端字形"
            );
        }
        assert_eq!(terminal_glyph("assets/tool.svg"), None);
    }

    #[test]
    fn 参数摘要提示序列化为_snake_case() {
        let json = serde_json::to_value(tool_args_presentation()).unwrap();
        assert_eq!(json["lead_args"], serde_json::json!(["action"]));
        assert!(
            json["skip_args"]
                .as_array()
                .unwrap()
                .contains(&"cwd".into())
        );
        assert_eq!(
            json["hidden_value_prefixes"],
            serde_json::json!(["__tiangong_cwd="])
        );
    }
}
