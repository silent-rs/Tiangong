//! Analyze-Attachment 插件的 WASM 桥接组件。
//!
//! handle_tool 直接接收图片本地路径并转发到 sidecar 做多模态分析。

mod bindings;
mod sidecar_client;

use bindings::exports::tiangong::plugin::plugin::{
    Guest, PluginDescriptor, PluginError, ToolCall, ToolResult, ToolSpec,
};
use bindings::exports::tiangong::plugin::plugin_ui::{
    Contribution, Guest as UiGuest, ResourceResponse, ViewMessageRequest, ViewMessageResponse,
    ViewResponse,
};
use tiangong_plugin_analyze_attachment_protocol::{
    Analyze, AnalyzeRequest, AnalyzeResponse, TOOL_ANALYZE_ATTACHMENT,
};

mod descriptor {
    pub const ID: &str = tiangong_plugin_analyze_attachment_protocol::PLUGIN_ID;
    pub const NAME: &str = "Analyze-Attachment";
    pub const VERSION: &str = tiangong_plugin_analyze_attachment_protocol::PLUGIN_VERSION;
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

    // 通用图片分析工具：不随主模型能力增减，避免会话切换模型后工具集不一致。
    fn tool_specs() -> Result<Vec<ToolSpec>, PluginError> {
        Ok(vec![ToolSpec {
            name: TOOL_ANALYZE_ATTACHMENT.to_string(),
            description: "调用多模态模型分析本地图片。当需要了解图片内容、而当前对话中只有图片的本地路径（看不到图片本身）时使用；images 必须是对话中明确给出的图片本地路径，不要传消息编号或自行猜测路径。文档和其他文件应使用对应文件工具。".to_string(),
            input_schema: r#"{"type":"object","properties":{"instruction":{"type":"string","minLength":1,"description":"希望如何解析图片，例如提取文字、描述画面或回答与图片有关的问题"},"images":{"type":"array","minItems":1,"items":{"type":"string","minLength":1},"description":"待分析图片的本地完整路径列表，原样使用用户消息中的 path；多张图片按希望分析的顺序传入"}},"required":["instruction","images"]}"#
                .to_string(),
        }])
    }

    fn prompt_sections() -> Result<Vec<String>, PluginError> {
        Ok(vec![format!(
            "## 图片分析工具\n\
             当对话中只有图片的本地路径、看不到图片本身，且回答确实需要了解图片内容时，可调用 `{TOOL_ANALYZE_ATTACHMENT}`。\n\
             调用时将要分析的图片本地 `path` 原样传入 `images` 数组，并用 `instruction` 说明问题；不要用消息编号代替图片路径。\n\
             文档和其他文件应使用对应文件工具；普通文本对话、无需查看图片内容或消息未提供可分析图片时，不要调用此工具。"
        )])
    }

    fn handle_tool(call: ToolCall) -> Result<ToolResult, PluginError> {
        match call.name.as_str() {
            TOOL_ANALYZE_ATTACHMENT => handle_analyze(&call),
            other => Err(plugin_err(format!("未知的 Attachment 工具: {other}"))),
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

fn parse_analyze_request(arguments: &str) -> Result<AnalyzeRequest, PluginError> {
    let request: AnalyzeRequest = serde_json::from_str(arguments).map_err(|error| {
        plugin_err(format!(
            "附件参数无效，请提供 instruction 和 images 图片路径列表：{error}"
        ))
    })?;
    if request.instruction.trim().is_empty() {
        return Err(plugin_err("instruction 不能为空"));
    }
    if request.images.is_empty() || request.images.iter().any(|path| path.trim().is_empty()) {
        return Err(plugin_err("images 必须包含非空的图片本地路径"));
    }
    Ok(request)
}

fn handle_analyze(call: &ToolCall) -> Result<ToolResult, PluginError> {
    let request = parse_analyze_request(&call.arguments)?;
    let response: AnalyzeResponse = sidecar_client::invoke::<Analyze>(&request)
        .map_err(|e| plugin_err(format!("附件分析失败: {e}")))?;

    Ok(ToolResult {
        ok: true,
        summary: "附件解析完成".to_string(),
        stdout: response.text,
        stderr: String::new(),
        exit_code: 0,
        execution: None,
    })
}

impl UiGuest for Component {
    fn contributions() -> Result<Vec<Contribution>, PluginError> {
        Ok(Vec::new())
    }

    fn open_view(_contribution_id: String) -> Result<ViewResponse, PluginError> {
        Err(plugin_err("Attachment 插件无设置页"))
    }

    fn get_view_resource(_path: String) -> Result<ResourceResponse, PluginError> {
        Err(plugin_err("Attachment 插件无外部资源"))
    }

    fn handle_view_message(
        _request: ViewMessageRequest,
    ) -> Result<ViewMessageResponse, PluginError> {
        Err(plugin_err("Attachment 插件无设置页消息"))
    }
}
bindings::export!(Component with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 图片路径直接来自本次工具参数() {
        let paths = vec!["/tmp/旧图.png", "/tmp/新图 \"带引号\".png"];
        let request = parse_analyze_request(
            &json!({
                "instruction": "对比两张图片",
                "images": paths,
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(request.images, paths);
        assert_eq!(request.instruction, "对比两张图片");
        assert!(request.user_message_text.is_empty());
    }

    #[test]
    fn 缺少或无效路径明确失败而不回退旧消息() {
        for args in [
            json!({"instruction":"看图", "message_id":"old-message", "attachment_index":0}),
            json!({"instruction":"看图", "images":[]}),
            json!({"instruction":"看图", "images":[" "]}),
            json!({"instruction":"看图", "images":"/tmp/image.png"}),
            json!({"instruction":" ", "images":["/tmp/image.png"]}),
        ] {
            assert!(parse_analyze_request(&args.to_string()).is_err(), "{args}");
        }
    }

    #[test]
    fn 工具与提示段不随主模型能力变化() {
        for config in [
            r#"{"llm":{},"chat_capabilities":["chat","multimodal"]}"#,
            r#"{"llm":{},"chat_capabilities":["chat"]}"#,
        ] {
            Component::on_config_updated(config.to_string()).unwrap();
            assert_eq!(Component::tool_specs().unwrap().len(), 1, "{config}");
            assert_eq!(Component::prompt_sections().unwrap().len(), 1, "{config}");
        }
    }
}
