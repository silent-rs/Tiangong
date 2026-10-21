//! Skill 插件的 WASM 桥接组件。
//!
//! 本组件只做桥接：工具规格/工具执行/prompt 段落/管理操作全部转发到 Skill sidecar。
//! 重型原生依赖（文件扫描、skill.toml 读写、SKILL.md 加载、审计日志）全部在 sidecar
//! 进程内运行，WASM 沙箱仅负责参数解析与 IPC 转发。

mod bindings;
mod sidecar_client;

use bindings::exports::tiangong::plugin::plugin::{
    Guest, PluginDescriptor, PluginError, ToolCall, ToolResult, ToolSpec,
};
use bindings::exports::tiangong::plugin::plugin_ui::{
    Contribution, Guest as UiGuest, ResourceResponse, ViewMessageRequest, ViewMessageResponse,
    ViewResponse,
};
use serde_json::Value;
use tiangong_plugin_skill_protocol::{
    Empty, GetSkillDetail, GetSkillDetailRequest, GetSkillEnv, GetSkillEnvRequest, GetSkillSummary,
    ListSkills, ListSkillsResponse, RefreshSkills, RemoveSkill, RemoveSkillRequest, RevealSkillDir,
    RevealSkillDirRequest, SetSkillEnabled, SetSkillEnabledRequest, SetSkillEnv,
    SetSkillEnvRequest, SkillOperation, SkillSummaryResponse, TOOL_GET_SKILL_DETAIL, UpdateSkillMd,
    UpdateSkillMdRequest,
};

mod descriptor {
    pub const ID: &str = tiangong_plugin_skill_protocol::PLUGIN_ID;
    pub const NAME: &str = "Skill";
    pub const VERSION: &str = tiangong_plugin_skill_protocol::PLUGIN_VERSION;
}

fn plugin_err(message: impl Into<String>) -> PluginError {
    PluginError::Message(message.into())
}

struct Component;

impl Guest for Component {
    fn describe() -> Result<PluginDescriptor, PluginError> {
        Ok(PluginDescriptor {
            id: descriptor::ID.to_string(),
            name: descriptor::NAME.to_string(),
            version: descriptor::VERSION.to_string(),
        })
    }

    fn tool_specs() -> Result<Vec<ToolSpec>, PluginError> {
        // 工具声明固定；具体 Skill 是否存在或可用，在调用时检查。
        Ok(vec![ToolSpec {
            name: TOOL_GET_SKILL_DETAIL.to_string(),
            description: "获取已安装 Skill 的完整使用说明。".to_string(),
            input_schema: r#"{"type":"object","properties":{"skill_id":{"type":"string","description":"Skill ID"}},"required":["skill_id"]}"#
                .to_string(),
        }])
    }

    fn prompt_sections() -> Result<Vec<String>, PluginError> {
        build_prompt_sections()
    }

    fn handle_tool(call: ToolCall) -> Result<ToolResult, PluginError> {
        match call.name.as_str() {
            TOOL_GET_SKILL_DETAIL => handle_get_skill_detail(&call),
            other => Err(plugin_err(format!("未知的 Skill 工具: {other}"))),
        }
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

// ── 工具实现 ────────────────────────────────────────────────────

fn handle_get_skill_detail(call: &ToolCall) -> Result<ToolResult, PluginError> {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let skill_id = args
        .get("skill_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if skill_id.is_empty() {
        return Err(plugin_err("缺少必填参数 skill_id"));
    }

    let request = GetSkillDetailRequest { id: skill_id };
    let response = sidecar_client::invoke::<GetSkillDetail>(&request)
        .map_err(|e| plugin_err(format!("get_skill_detail 执行失败: {e}")))?;

    if !response.detail.enabled {
        // 列出可用 skill 供 Agent 参考。
        let available = list_available_ids();
        return Ok(ToolResult {
            ok: false,
            summary: format!("Skill {} 已禁用", response.detail.name),
            stdout: String::new(),
            stderr: format!(
                "Skill {} 当前已禁用。可用的 skill：{}",
                response.detail.name, available
            ),
            exit_code: 1,
            execution: None,
        });
    }

    // {skill_dir} 占位符替换：用 storage_root/<id> 还原真实目录。
    let summary = load_summary().unwrap_or_default();
    let skill_dir = format!(
        "{}/{}",
        summary.storage_root.trim_end_matches('/'),
        response.detail.id
    );
    let readme = response.detail.readme.replace("{skill_dir}", &skill_dir);

    Ok(ToolResult {
        ok: true,
        summary: format!("Skill {} 的使用说明", response.detail.name),
        stdout: readme,
        stderr: String::new(),
        exit_code: 0,
        execution: None,
    })
}

// ── prompt 段落组装 ─────────────────────────────────────────────

/// 构造 3 段 system prompt：已安装 skill 摘要、允许文件操作目录、创建规范。
fn build_prompt_sections() -> Result<Vec<String>, PluginError> {
    let mut sections = Vec::new();

    // 段落 1：已安装 Skills 摘要（条件注入）。
    let summary = sidecar_client::invoke::<GetSkillSummary>(&Empty {})
        .map_err(|error| plugin_err(format!("读取 Skill 提示声明失败: {error}")))?;
    {
        if !summary.items.is_empty() {
            let mut lines = String::from(
                "已安装的 Skills（如果 Skill 能处理用户请求，优先调用 get_skill_detail 获取完整说明，然后按文档使用 run_command/run_shell 执行对应脚本）：\n",
            );
            for item in &summary.items {
                let desc = if item.description.is_empty() {
                    "无描述"
                } else {
                    &item.description
                };
                lines.push_str(&format!("- {} (id={}): {}\n", item.name, item.id, desc));
            }
            sections.push(lines);
        }
        // 段落 2：允许文件操作目录声明。
        if !summary.storage_root.is_empty() {
            sections.push(format!("额外允许文件操作目录：{}", summary.storage_root));
        }
        // 段落 3：Skill 创建规范。
        sections.push(skill_creation_guide(&summary.storage_root));
    }

    Ok(sections)
}

/// skill 创建规范模板（对齐原 prompt.rs 的 skill_creation_guide）。
fn skill_creation_guide(root: &str) -> String {
    let root = if root.is_empty() {
        "~/.tiangong/skills"
    } else {
        root
    };
    format!(
        r#"创建新 Skill 的规范：
- Skill 存储目录：{root}
- 每个 Skill 是一个子目录，目录名即 skill id，需含 `skill.toml` 与 `SKILL.md` 两个文件。
- 不要调用任何专用安装工具，直接用文件写入工具（fs 插件 file_write，action=write）在存储目录下创建文件即可。

创建步骤：
1. 在 `{root}` 下创建子目录（目录名即 skill id，只允许小写字母、数字、短横线）。
2. 写入 `skill.toml`（最小模板）：
   ```toml
   id = "<skill id>"
   name = "<显示名>"
   version = "0.1.0"
   entry = "SKILL.md"
   available = true

   [source]
   kind = "local"
   value = ""

   [requires]

   [permissions]
   ```
3. 写入 `SKILL.md`（使用说明，支持 `{{skill_dir}}` 占位符，运行时替换为 skill 目录绝对路径）。

创建后需刷新（重启或等待缓存失效）才能生效。"#,
        root = root
    )
}

// ── 辅助函数 ────────────────────────────────────────────────────

/// 调 sidecar 拿 skill 摘要（失败返回 None）。
fn load_summary() -> Option<SkillSummaryResponse> {
    sidecar_client::invoke::<GetSkillSummary>(&Empty {}).ok()
}

/// 列出所有 enabled skill id（供错误提示）。
fn list_available_ids() -> String {
    load_summary()
        .map(|s| {
            s.items
                .iter()
                .map(|i| i.id.clone())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

// ── UI 能力（plugin-ui 接口）──

/// 设置页模板（单文件内联，与 scheduler/memory/index 设置页同构）。
const SKILL_PAGE_TEMPLATE: &str = include_str!("skill.html");
const SKILL_PAGE_CSS: &str = include_str!("skill.css");
const SKILL_PAGE_JS: &str = include_str!("skill.js");

fn skill_settings_html() -> String {
    SKILL_PAGE_TEMPLATE
        .replace("/*__SKILL_CSS__*/", SKILL_PAGE_CSS)
        .replace("/*__SKILL_JS__*/", SKILL_PAGE_JS)
}

impl UiGuest for Component {
    fn contributions() -> Result<Vec<Contribution>, PluginError> {
        Ok(vec![Contribution {
            id: "skill-settings".to_string(),
            title: "Skills".to_string(),
            description: "管理已安装的 Skill".to_string(),
            icon: "sparkles".to_string(),
            group: "plugins".to_string(),
            has_view: true,
        }])
    }

    fn open_view(contribution_id: String) -> Result<ViewResponse, PluginError> {
        if contribution_id != "skill-settings" {
            return Err(plugin_err(format!(
                "未知的 contribution: {contribution_id}"
            )));
        }
        Ok(ViewResponse {
            html: skill_settings_html(),
        })
    }

    fn get_view_resource(_path: String) -> Result<ResourceResponse, PluginError> {
        Err(plugin_err("Skill 设置页无外部资源"))
    }

    fn handle_view_message(
        request: ViewMessageRequest,
    ) -> Result<ViewMessageResponse, PluginError> {
        let payload = match request.method.as_str() {
            "__tiangong.mention_candidates.v1" => skill_mention_candidates()?,
            "list" => invoke_for_ui::<ListSkills>(&Empty {})?,
            "detail" => {
                let req: GetSkillDetailRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析详情请求失败: {e}")))?;
                invoke_for_ui::<GetSkillDetail>(&req)?
            }
            "remove" => {
                let req: RemoveSkillRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析删除请求失败: {e}")))?;
                invoke_for_ui::<RemoveSkill>(&req)?
            }
            "toggle" => {
                let req: SetSkillEnabledRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析启停请求失败: {e}")))?;
                invoke_for_ui::<SetSkillEnabled>(&req)?
            }
            "refresh" => invoke_for_ui::<RefreshSkills>(&Empty {})?,
            "get_env" => {
                let req: GetSkillEnvRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析 env 请求失败: {e}")))?;
                invoke_for_ui::<GetSkillEnv>(&req)?
            }
            "set_env" => {
                let req: SetSkillEnvRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析保存 env 请求失败: {e}")))?;
                let _ = sidecar_client::invoke::<SetSkillEnv>(&req)
                    .map_err(|e| plugin_err(e.to_string()))?;
                "true".to_string()
            }
            "update_md" => {
                let req: UpdateSkillMdRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析更新说明请求失败: {e}")))?;
                let _ = sidecar_client::invoke::<UpdateSkillMd>(&req)
                    .map_err(|e| plugin_err(e.to_string()))?;
                "true".to_string()
            }
            "reveal" => {
                let req: RevealSkillDirRequest = serde_json::from_str(&request.payload)
                    .map_err(|e| plugin_err(format!("解析打开目录请求失败: {e}")))?;
                let _ = sidecar_client::invoke::<RevealSkillDir>(&req)
                    .map_err(|e| plugin_err(e.to_string()))?;
                "true".to_string()
            }
            other => return Err(plugin_err(format!("未知的 Skill 管理消息: {other}"))),
        };
        Ok(ViewMessageResponse { payload })
    }
}

fn skill_mention_candidates() -> Result<String, PluginError> {
    let response: ListSkillsResponse =
        sidecar_client::invoke::<ListSkills>(&Empty {}).map_err(|e| plugin_err(e.to_string()))?;
    let candidates = response
        .skills
        .into_iter()
        .filter(|skill| skill.enabled)
        .map(|skill| {
            serde_json::json!({
                "value": format!("@skill:{}", skill.id),
                "label": skill.name,
                "kind": "skill",
                "hint": if skill.description.is_empty() {
                    format!("v{}", skill.version)
                } else {
                    skill.description
                },
                "mark": "S",
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&candidates).map_err(|e| plugin_err(e.to_string()))
}

/// 通用 sidecar 转发器：调用操作 O 并把响应序列化成 JSON 字符串（供 iframe 消费）。
fn invoke_for_ui<O>(request: &O::Request) -> Result<String, PluginError>
where
    O: SkillOperation,
    O::Response: serde::Serialize,
{
    let response = sidecar_client::invoke::<O>(request).map_err(|e| plugin_err(e.to_string()))?;
    serde_json::to_string(&response).map_err(|e| plugin_err(e.to_string()))
}
bindings::export!(Component with_types_in bindings);
