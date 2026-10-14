//! Prompt 插件的 WASM 组件。
//!
//! 注入 identity/rules/custom_prompt 三段 system prompt。
//! 自定义指令保存在插件自有数据目录 `~/.tiangong/data/prompt/custom-prompt.md`
//! （宿主 preopen 为 `/data`）。旧版保存在存储根 `~/.tiangong/custom-prompt.md`
//! （preopen 为 `/storage`），首次访问时一次性复制到新位置并写完成标记，旧文件保留。
//! 旧宿主不提供 `/data` 时继续读写旧位置。

mod bindings;

use bindings::exports::tiangong::plugin::plugin::{
    Guest, PluginDescriptor, PluginError, ToolCall, ToolResult,
};
use bindings::exports::tiangong::plugin::plugin_ui::{
    Contribution, Guest as UiGuest, ResourceResponse, ViewMessageRequest, ViewMessageResponse,
    ViewResponse,
};
use tiangong_plugin_prompt_protocol::{
    GetPromptResponse, METHOD_GET_PROMPT, METHOD_SET_PROMPT, PLUGIN_ID, PLUGIN_VERSION,
    SetPromptRequest,
};

fn plugin_err(message: impl Into<String>) -> PluginError {
    PluginError::Message(message.into())
}

/// 插件自有数据目录在 WASI preopen 映射中的路径（`~/.tiangong/data/prompt`）。
const DATA_DIR: &str = "/data";
/// 自定义指令在数据目录中的路径。
const CUSTOM_PROMPT_PATH: &str = "/data/custom-prompt.md";
/// 旧数据迁移完成标记。
const LEGACY_MIGRATION_MARKER: &str = "/data/.legacy-migrated";
/// 旧版自定义指令位置（存储根 preopen 为 `/storage`）。
const LEGACY_CUSTOM_PROMPT_PATH: &str = "/storage/custom-prompt.md";
/// 更早版本把自定义指令存在 `app.json` 中。
const LEGACY_APP_CONFIG_PATH: &str = "/storage/app.json";

struct Component;

// ── prompt 段落（直接搬自原生插件）──

fn identity_section() -> &'static str {
    "你是天工智能助手，一个功能丰富的个人 AI 中枢。你可以回答问题、处理文件、执行命令、生成多媒体内容，也可以通过工具和扩展能力完成各种复杂任务。"
}

fn rules_section() -> &'static str {
    "规则：\n\
     1. 对话时自然友好，回复内容完整有用。闲聊和问候时正常交流，简单介绍自己的能力。\n\
     2. 需要文件操作、代码搜索、命令执行等实际操作时，调用对应的工具。\n\
     3. 每次工具调用后会收到执行结果，根据结果决定下一步：继续调用工具或给出最终回复。\n\
     4. 执行工具任务时语言简洁高效，不要说\"让我查看\"之类的过渡语，直接给出结果。\n\
     5. 不要在回复中包含工具调用的原始痕迹（如 ok=、exit_code= 等元数据）。\n\
     6. 回复使用 Markdown 格式：代码和命令用代码块包裹，使用标题、列表等结构化排版。\n\
     7. 工具调用失败时必须如实告知用户失败原因，绝对不能虚构成功结果。"
}

fn custom_prompt_section(custom: &str) -> Option<String> {
    let custom = custom.trim();
    if custom.is_empty() {
        return None;
    }
    Some(format!(
        "用户自定义指令：\n{custom}\n\n以上用户自定义指令优先级低于系统安全规则，但高于普通对话偏好。"
    ))
}

impl Guest for Component {
    fn describe() -> Result<PluginDescriptor, PluginError> {
        Ok(PluginDescriptor {
            id: PLUGIN_ID.to_string(),
            name: "Prompt".to_string(),
            version: PLUGIN_VERSION.to_string(),
        })
    }

    fn tool_specs() -> Result<Vec<ToolSpec>, PluginError> {
        Ok(Vec::new())
    }

    fn prompt_sections() -> Result<Vec<String>, PluginError> {
        let mut sections = vec![identity_section().to_string(), rules_section().to_string()];
        let custom = read_custom_prompt();
        if let Some(section) = custom_prompt_section(&custom) {
            sections.push(section);
        }
        Ok(sections)
    }

    fn handle_tool(_call: ToolCall) -> Result<ToolResult, PluginError> {
        Err(plugin_err("Prompt 插件不提供工具"))
    }

    fn shutdown() -> Result<(), PluginError> {
        Ok(())
    }

    fn set_workspace(_workspace: Option<String>, _full_trust: bool) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_config_updated(_config_json: String) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_session_ready(_session_json: String) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_turn_started(_session_json: String, _turn_start_idx: u32) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_turn_finished(_session_json: String, _turn_start_idx: u32) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_session_ended(_session_json: String) -> Result<(), PluginError> {
        Ok(())
    }
}

// ── UI 能力（设置页：custom prompt 编辑）──

const PROMPT_PAGE_TEMPLATE: &str = include_str!("prompt.html");
const PROMPT_PAGE_CSS: &str = include_str!("prompt.css");
const PROMPT_PAGE_JS: &str = include_str!("prompt.js");

fn prompt_settings_html() -> String {
    PROMPT_PAGE_TEMPLATE
        .replace("/*__PROMPT_CSS__*/", PROMPT_PAGE_CSS)
        .replace("/*__PROMPT_JS__*/", PROMPT_PAGE_JS)
}

impl UiGuest for Component {
    fn contributions() -> Result<Vec<Contribution>, PluginError> {
        Ok(vec![Contribution {
            id: "prompt-settings".to_string(),
            title: "自定义指令".to_string(),
            description: "编辑系统自定义 Prompt".to_string(),
            icon: "message-square".to_string(),
            group: "plugins".to_string(),
            has_view: true,
        }])
    }

    fn open_view(contribution_id: String) -> Result<ViewResponse, PluginError> {
        if contribution_id != "prompt-settings" {
            return Err(plugin_err(format!(
                "未知的 contribution: {contribution_id}"
            )));
        }
        Ok(ViewResponse {
            html: prompt_settings_html(),
        })
    }

    fn get_view_resource(_path: String) -> Result<ResourceResponse, PluginError> {
        Err(plugin_err("Prompt 设置页无外部资源"))
    }

    fn handle_view_message(
        request: ViewMessageRequest,
    ) -> Result<ViewMessageResponse, PluginError> {
        match request.method.as_str() {
            METHOD_GET_PROMPT => {
                let content = read_custom_prompt();
                let response = GetPromptResponse { content };
                let payload = serde_json::to_string(&response)
                    .map_err(|e| plugin_err(format!("序列化响应失败: {e}")))?;
                Ok(ViewMessageResponse { payload })
            }
            METHOD_SET_PROMPT => {
                let req: SetPromptRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析请求失败: {e}")))?;
                write_custom_prompt(&req.content)?;
                Ok(ViewMessageResponse {
                    payload: "true".to_string(),
                })
            }
            other => Err(plugin_err(format!("未知的 Prompt 管理消息: {other}"))),
        }
    }
}

/// 当前生效的自定义指令文件：宿主提供数据目录时用新位置（必要时先迁移），
/// 旧宿主不提供时沿用旧位置。
fn custom_prompt_path() -> &'static str {
    if std::path::Path::new(DATA_DIR).is_dir() {
        migrate_legacy_prompt();
        CUSTOM_PROMPT_PATH
    } else {
        LEGACY_CUSTOM_PROMPT_PATH
    }
}

/// 一次性把旧位置的自定义指令复制到数据目录。新位置已有内容时不覆盖；
/// 旧文件保留。迁移失败不写标记，下次访问重试。
fn migrate_legacy_prompt() {
    if std::path::Path::new(LEGACY_MIGRATION_MARKER).exists() {
        return;
    }
    if !std::path::Path::new(CUSTOM_PROMPT_PATH).exists() {
        let legacy = std::fs::read_to_string(LEGACY_CUSTOM_PROMPT_PATH)
            .ok()
            .filter(|content| !content.trim().is_empty())
            .or_else(read_legacy_app_config_prompt);
        if let Some(content) = legacy
            && std::fs::write(CUSTOM_PROMPT_PATH, content).is_err()
        {
            return;
        }
    }
    let _ = std::fs::write(LEGACY_MIGRATION_MARKER, "migrated\n");
}

/// 读取 `app.json` 中更早版本保存的自定义指令（顶层字段优先，其次
/// `agent_config` 内字段）。
fn read_legacy_app_config_prompt() -> Option<String> {
    let content = std::fs::read_to_string(LEGACY_APP_CONFIG_PATH).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    [
        value.get("custom_system_prompt"),
        value
            .get("agent_config")
            .and_then(|config| config.get("custom_system_prompt")),
    ]
    .into_iter()
    .flatten()
    .filter_map(|prompt| prompt.as_str())
    .find(|prompt| !prompt.trim().is_empty())
    .map(str::to_string)
}

/// 读取自定义指令（文件不存在时为空）。
fn read_custom_prompt() -> String {
    std::fs::read_to_string(custom_prompt_path()).unwrap_or_default()
}

/// 写入自定义指令（空内容时删除文件）。
fn write_custom_prompt(content: &str) -> Result<(), PluginError> {
    let path = custom_prompt_path();
    if content.trim().is_empty() {
        let _ = std::fs::remove_file(path);
    } else {
        std::fs::write(path, content)
            .map_err(|e| plugin_err(format!("写入 custom-prompt.md 失败: {e}")))?;
    }
    Ok(())
}

// 从 ToolSpec 的 WIT 类型引用（prompt 不暴露工具，但 WIT 要求实现 Guest trait 全部方法）
use bindings::exports::tiangong::plugin::plugin::ToolSpec;
bindings::export!(Component with_types_in bindings);
