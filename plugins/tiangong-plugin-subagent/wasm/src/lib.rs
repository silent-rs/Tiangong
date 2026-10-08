//! Subagent 插件的 WASM 桥接组件。
//!
//! 职责（其余全部在 sidecar 进程内）：
//! - 工具规格声明与透传执行（sidecar 返回统一 ToolOutcome 形状）；
//! - `on_turn_finished` 生命周期钩子：提取本轮用户消息与最终回复，转发给
//!   sidecar 完成「天工会话后端」的运行归因（本轮由 Subagent 投递触发时，
//!   sidecar 把对应 Run 置完成并经 Hook 回报激活会话）。

mod bindings;
mod sidecar_client;
mod specs;

use bindings::exports::tiangong::plugin::plugin::{
    Guest, PluginDescriptor, PluginError, ToolCall, ToolResult, ToolSpec,
};
use bindings::exports::tiangong::plugin::plugin_ui::{
    Contribution, Guest as UiGuest, ResourceResponse, ViewMessageRequest, ViewMessageResponse,
    ViewResponse,
};
use serde_json::Value;
use tiangong_plugin_subagent_protocol::ops::{TOOL_SEND_AGENT_MESSAGE, TOOL_SUBMIT_AGENT_TASK};
use tiangong_plugin_subagent_protocol::{
    MENTION_CANDIDATES, PLUGIN_ID, PLUGIN_VERSION, SESSION_TURN_FINISHED, TOOL_OPERATIONS,
};

mod descriptor {
    pub const NAME: &str = "Subagent";
}

fn plugin_err(message: impl Into<String>) -> PluginError {
    PluginError::Message(message.into())
}

// 缓存的会话消息（thread-local，生命周期钩子注入）——用于向 Subagent
// 派活/发消息时自动携带本轮用户消息的附件（模型不知道附件本地路径）。
thread_local! {
    static SESSION_MESSAGES: std::cell::RefCell<Vec<Value>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// 工具级失败的 ToolResult（区别于 PluginError：后者会被宿主映射为
/// 「未注册的工具」，丢失真实原因）。
fn tool_failure(summary: impl Into<String>) -> ToolResult {
    ToolResult {
        ok: false,
        summary: summary.into(),
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 1,
        execution: None,
    }
}

struct Component;

impl Guest for Component {
    fn describe() -> Result<PluginDescriptor, PluginError> {
        Ok(PluginDescriptor {
            id: PLUGIN_ID.to_string(),
            name: descriptor::NAME.to_string(),
            version: PLUGIN_VERSION.to_string(),
        })
    }

    fn tool_specs() -> Result<Vec<ToolSpec>, PluginError> {
        Ok(specs::TOOL_SPECS
            .iter()
            .map(|(name, description, schema)| ToolSpec {
                name: (*name).to_string(),
                description: (*description).to_string(),
                input_schema: (*schema).to_string(),
            })
            .collect())
    }

    fn prompt_sections() -> Result<Vec<String>, PluginError> {
        Ok(vec![specs::PROMPT_SECTION.to_string()])
    }

    fn handle_tool(call: ToolCall) -> Result<ToolResult, PluginError> {
        // 失败一律映射为 ok:false 的 ToolResult：PluginError 会被宿主吞成
        // 「未注册的工具」（adapter 对 Err 返回 None），模型将收到误导信息。
        if !TOOL_OPERATIONS.contains(&call.name.as_str()) {
            return Ok(tool_failure(format!("未知的 Subagent 工具: {}", call.name)));
        }
        // 派活/发消息自动携带本轮用户消息的附件：模型无法得知附件本地
        // 路径（多模态看得到图、非多模态只有文字标注），由插件从会话
        // 消息中提取并注入工具参数，随投递传给成员（成员间协作同链路）。
        let arguments = if matches!(
            call.name.as_str(),
            TOOL_SEND_AGENT_MESSAGE | TOOL_SUBMIT_AGENT_TASK
        ) {
            inject_attachments(&call.arguments)
        } else {
            call.arguments
        };
        // sidecar 对全部工具操作返回 ToolOutcome 形状，直接映射。
        let response = match sidecar_client::invoke_raw(&call.name, &arguments) {
            Ok(response) => response,
            Err(error) => {
                return Ok(tool_failure(format!(
                    "{} 执行失败（sidecar 通道）: {error}",
                    call.name
                )));
            }
        };
        let outcome: Value = match serde_json::from_str(&response) {
            Ok(outcome) => outcome,
            Err(error) => {
                return Ok(tool_failure(format!(
                    "解析 {} 响应失败: {error}",
                    call.name
                )));
            }
        };
        Ok(ToolResult {
            ok: outcome.get("ok").and_then(Value::as_bool).unwrap_or(false),
            summary: outcome
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            stdout: outcome
                .get("stdout")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            stderr: String::new(),
            exit_code: outcome.get("exit_code").and_then(Value::as_i64).unwrap_or(
                if outcome.get("ok").and_then(Value::as_bool) == Some(true) {
                    0
                } else {
                    1
                },
            ) as i32,
            execution: None,
        })
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

    fn on_session_ready(session_json: String) -> Result<(), PluginError> {
        cache_session(&session_json);
        Ok(())
    }

    fn on_turn_started(session_json: String, _turn_start_idx: u32) -> Result<(), PluginError> {
        cache_session(&session_json);
        Ok(())
    }

    fn on_turn_finished(session_json: String, turn_start_idx: u32) -> Result<(), PluginError> {
        forward_turn_finished(&session_json, turn_start_idx);
        Ok(())
    }

    fn on_session_ended(_session_json: String) -> Result<(), PluginError> {
        SESSION_MESSAGES.with(|m| m.borrow_mut().clear());
        Ok(())
    }
}

impl UiGuest for Component {
    fn contributions() -> Result<Vec<Contribution>, PluginError> {
        Ok(Vec::new())
    }

    fn open_view(_contribution_id: String) -> Result<ViewResponse, PluginError> {
        Err(plugin_err("本插件无设置页视图"))
    }

    fn get_view_resource(_path: String) -> Result<ResourceResponse, PluginError> {
        Err(plugin_err("本插件无视图资源"))
    }

    fn handle_view_message(
        request: ViewMessageRequest,
    ) -> Result<ViewMessageResponse, PluginError> {
        // mention 候选经此通道下发（loader 以专用方法名探测）。
        if request.method == "__tiangong.mention_candidates.v1" {
            // UI 辅助能力：sidecar 不可用时降级为空列表，不阻塞输入补全。
            let payload =
                sidecar_client::invoke_raw(MENTION_CANDIDATES, "{}").unwrap_or_else(|error| {
                    eprintln!("[subagent] 提及候选获取失败，降级为空: {error}");
                    "[]".to_string()
                });
            return Ok(ViewMessageResponse { payload });
        }
        Err(plugin_err("本插件无视图消息通道"))
    }
}

/// 缓存 session JSON 中的消息列表。
fn cache_session(session_json: &str) {
    let session: Value = serde_json::from_str(session_json).unwrap_or(Value::Null);
    let messages = session
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    SESSION_MESSAGES.with(|m| {
        *m.borrow_mut() = messages;
    });
}

/// 把最近一条用户消息的附件注入工具参数（无附件时原样返回）。
///
/// 附件提取只认 image / asset_reference 块的 asset.local_path（Server 端
/// 附件归档的统一形状）；本轮轮次内不会再有新用户消息进来，最后一条
/// user 消息即触发本次工具调用的消息。
fn inject_attachments(arguments: &str) -> String {
    let attachments = latest_user_attachments();
    if attachments.is_empty() {
        return arguments.to_string();
    }
    let mut value: Value = match serde_json::from_str(arguments) {
        Ok(value) => value,
        Err(_) => return arguments.to_string(),
    };
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "attachments".to_string(),
            serde_json::Value::Array(attachments),
        );
    }
    serde_json::to_string(&value).unwrap_or_else(|_| arguments.to_string())
}

/// 提取最近一条用户消息的全部附件（AttachmentPayload JSON 形状）。
fn latest_user_attachments() -> Vec<Value> {
    SESSION_MESSAGES.with(|m| {
        let messages = m.borrow();
        let Some(last_user) = messages
            .iter()
            .rev()
            .find(|msg| msg.get("role").and_then(Value::as_str) == Some("user"))
        else {
            return Vec::new();
        };
        last_user
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|block| {
                        matches!(
                            block.get("type").and_then(Value::as_str),
                            Some("image") | Some("asset_reference")
                        )
                    })
                    .filter_map(|block| {
                        let asset = block.get("asset")?;
                        let path = asset.get("local_path").and_then(Value::as_str)?;
                        if path.is_empty() {
                            return None;
                        }
                        Some(serde_json::json!({
                            "path": path,
                            "kind": asset.get("kind").and_then(Value::as_str).unwrap_or("file"),
                            "mime_type": asset.get("mime_type").and_then(Value::as_str),
                            "name": asset.get("original_name").and_then(Value::as_str),
                        }))
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// 提取本轮信息并转发 sidecar（天工会话后端的完成回报）。
fn forward_turn_finished(session_json: &str, turn_start_idx: u32) {
    let Ok(session) = serde_json::from_str::<Value>(session_json) else {
        return;
    };
    let Some(session_id) = session.get("id").and_then(Value::as_str) else {
        return;
    };
    let messages = session
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 本轮用户锚点：宿主快照只含本轮消息（turn_start_idx 为 0，锚点即首条）；
    // 旧宿主传完整历史与本轮起点位置，按位置定位兼容。
    let anchor = messages.get(turn_start_idx as usize);
    let (user_text, turn_status, anchor_idx) = match anchor {
        Some(anchor) => {
            let idx = messages
                .iter()
                .position(|message| std::ptr::eq(message, anchor))
                .unwrap_or(0);
            (
                message_text(anchor),
                anchor
                    .get("turn_status")
                    .and_then(Value::as_str)
                    .unwrap_or("success")
                    .to_string(),
                idx,
            )
        }
        None => (String::new(), "success".to_string(), 0),
    };
    // 最终回复：本轮（锚点之后）倒序第一条 assistant（优先 summary 阶段，
    // 过滤空文本）。切片从锚点之后开始——锚点已排除，倒序第一条即本轮
    // 最后一条消息（最终答案），不得再跳过。
    let mut assistant_text = String::new();
    for phase in ["summary", "normal"] {
        for message in messages[(anchor_idx + 1).min(messages.len())..]
            .iter()
            .rev()
        {
            if message.get("role").and_then(Value::as_str) == Some("assistant")
                && message
                    .get("phase")
                    .and_then(Value::as_str)
                    .unwrap_or("normal")
                    == phase
            {
                let text = message_text(message);
                if !text.trim().is_empty() {
                    assistant_text = text;
                    break;
                }
            }
        }
        if !assistant_text.is_empty() {
            break;
        }
    }
    let payload = serde_json::json!({
        "session_id": session_id,
        "user_text": user_text,
        "assistant_text": assistant_text,
        "turn_status": turn_status,
    });
    if let Ok(body) = serde_json::to_string(&payload)
        && let Err(error) = sidecar_client::invoke_raw(SESSION_TURN_FINISHED, &body)
    {
        // 通知型钩子：失败不阻断会话，sidecar 侧有孤儿运行恢复兜底。
        eprintln!("[subagent] 转发 turn 完成失败: {error}");
    }
}

fn message_text(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

bindings::export!(Component with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;

    fn user_message_with_attachments(id: &str, text: &str, attachments: &str) -> Value {
        serde_json::from_str(&format!(
            r#"{{
                "id": "{id}",
                "role": "user",
                "content": [
                    {{ "type": "text", "text": "{text}" }},
                    {attachments}
                ]
            }}"#
        ))
        .unwrap()
    }

    fn cache(messages: Vec<Value>) {
        SESSION_MESSAGES.with(|m| *m.borrow_mut() = messages);
    }

    #[test]
    fn 提取最近用户消息的图片与文件附件() {
        cache(vec![
            user_message_with_attachments(
                "msg-1",
                "第一张",
                r#"{ "type": "image", "asset": { "local_path": "/tmp/a.png", "kind": "image", "mime_type": "image/png", "original_name": "a.png" } }"#,
            ),
            serde_json::json!({
                "id": "msg-2", "role": "assistant",
                "content": [{ "type": "text", "text": "收到" }]
            }),
            user_message_with_attachments(
                "msg-3",
                "看文件",
                r#"{ "type": "asset_reference", "asset": { "local_path": "/tmp/b.pdf", "kind": "file", "mime_type": null, "original_name": "b.pdf" } }"#,
            ),
        ]);
        let attachments = latest_user_attachments();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0]["path"], "/tmp/b.pdf");
        assert_eq!(attachments[0]["kind"], "file");
        assert_eq!(attachments[0]["name"], "b.pdf");
    }

    #[test]
    fn 最近用户消息无附件时不携带历史附件() {
        cache(vec![
            user_message_with_attachments(
                "msg-1",
                "带图",
                r#"{ "type": "image", "asset": { "local_path": "/tmp/a.png", "kind": "image", "mime_type": "image/png", "original_name": "a.png" } }"#,
            ),
            serde_json::json!({
                "id": "msg-2", "role": "user",
                "content": [{ "type": "text", "text": "纯文本追问" }]
            }),
        ]);
        assert!(latest_user_attachments().is_empty());
    }

    #[test]
    fn 注入附件到工具参数且无附件时原样返回() {
        cache(vec![user_message_with_attachments(
            "msg-1",
            "带图",
            r#"{ "type": "image", "asset": { "local_path": "/tmp/a.png", "kind": "image", "mime_type": "image/png", "original_name": "a.png" } }"#,
        )]);
        let injected = inject_attachments(r#"{"agent_id":"x","content":"hi"}"#);
        let value: Value = serde_json::from_str(&injected).unwrap();
        assert_eq!(value["attachments"][0]["path"], "/tmp/a.png");
        // 原有参数保留。
        assert_eq!(value["agent_id"], "x");

        cache(Vec::new());
        let untouched = inject_attachments(r#"{"agent_id":"x","content":"hi"}"#);
        assert_eq!(untouched, r#"{"agent_id":"x","content":"hi"}"#);
    }
}
