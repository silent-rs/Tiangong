//! 宿主上下文准备：图片读取/失败降级与会话规则在 Core；轻量类型映射在 LLM。
use crate::session::{ContentBlock, Message, MessageRole, StoredAsset};
use base64::{Engine as _, engine::general_purpose};
use std::path::Path;
use tiangong_llm::message::ImageContent;

pub const FALLBACK_USER_TEXT: &str = "请继续处理当前任务。";

/// 只处理本次拥有的消息副本，不修改 Session/历史，也不构造 LLM 请求。
pub fn prepare_messages(mut messages: Vec<Message>) -> Vec<Message> {
    for message in &mut messages {
        for block in &mut message.content {
            if let ContentBlock::Image { asset, data } = block {
                *block = match ready_image(asset, data.take()) {
                    Some(image) => ContentBlock::Image {
                        asset: asset.clone(),
                        data: Some(image.data),
                    },
                    None => ContentBlock::text(format!(
                        "[image unavailable] 图片文件已不可读（asset_id={}，path={}），内容不可用",
                        asset.asset_id,
                        escape_asset_text(&asset.local_path)
                    )),
                };
            }
        }
    }
    messages
}

/// Session 的上下文恢复规则，不应用到无会话的通用 LLM 入口。
pub fn prepare_context(messages: Vec<Message>) -> Vec<Message> {
    let mut messages = prepare_messages(messages);
    let first_user = messages.iter().position(|message| {
        message.role == MessageRole::User
            && message.content.iter().any(|block| match block {
                ContentBlock::Text { text } | ContentBlock::ModelInstruction { text } => {
                    !text.trim().is_empty()
                }
                ContentBlock::Image { .. } => true,
                _ => false,
            })
    });
    match first_user {
        Some(index) => {
            messages = messages
                .into_iter()
                .enumerate()
                .filter(|(i, message)| *i >= index || message.role == MessageRole::System)
                .map(|(_, message)| message)
                .collect();
        }
        None => {
            messages.retain(|message| message.role == MessageRole::System);
            messages.push(Message::new(MessageRole::User, FALLBACK_USER_TEXT));
        }
    }
    messages
}
fn ready_image(asset: &StoredAsset, runtime_data: Option<String>) -> Option<ImageContent> {
    if let Some(data) = runtime_data.filter(|data| !data.trim().is_empty()) {
        let data = if data.trim_start().starts_with("data:")
            || data.starts_with("https://")
            || data.starts_with("http://")
        {
            data
        } else {
            format!("data:{};base64,{data}", asset.mime_type)
        };
        return Some(ImageContent {
            mime_type: asset.mime_type.clone(),
            data,
        });
    }

    let trimmed = asset.local_path.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Some(ImageContent {
            mime_type: asset.mime_type.clone(),
            data: trimmed.to_string(),
        });
    }

    let bytes = std::fs::read(Path::new(trimmed)).ok()?;
    let mime_type = asset.mime_type.clone();
    let data = format!(
        "data:{mime_type};base64,{}",
        general_purpose::STANDARD.encode(bytes)
    );
    Some(ImageContent { mime_type, data })
}

/// 降级说明中资产路径的净化：压缩控制字符与 Unicode 行/双向控制符、按字符边界截断。
fn escape_asset_text(value: &str) -> String {
    fn is_suspicious(c: char) -> bool {
        c.is_control()
            || matches!(
                c,
                '\u{2028}' | '\u{2029}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}'
                    | '\u{2066}'..='\u{2069}'
            )
    }
    value
        .chars()
        .map(|c| if is_suspicious(c) { ' ' } else { c })
        .take(160)
        .collect()
}

#[cfg(test)]
use crate::session::Role;
#[cfg(test)]
use tiangong_llm::{
    ModelRequest,
    message::{ChatMessage, MessageContent, MessageRole as LlmMessageRole, ThinkingContent},
    tool::{ToolCall, ToolResult, ToolResultContent},
};
#[cfg(test)]
pub trait IntoModelRequest {
    fn into_model_request(self) -> anyhow::Result<ModelRequest>;
}
#[cfg(test)]
impl IntoModelRequest for Vec<Message> {
    fn into_model_request(self) -> anyhow::Result<ModelRequest> {
        if !self
            .iter()
            .any(|message| message.role == MessageRole::System)
        {
            anyhow::bail!("模型上下文缺少 System 消息");
        }
        ModelRequest::from_messages(prepare_context(self))
    }
}
#[cfg(test)]
fn chat_message(message: Message) -> Option<ChatMessage> {
    let messages = prepare_messages(vec![Message::new(MessageRole::User, "start"), message]);
    ModelRequest::from_messages(messages)
        .ok()?
        .messages
        .into_iter()
        .nth(1)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{MediaKind, MessageToolCall, UserSource};

    fn assemble(messages: Vec<Message>) -> anyhow::Result<ModelRequest> {
        messages.into_model_request()
    }

    fn test_asset(kind: MediaKind, local_path: &str, mime_type: &str) -> StoredAsset {
        StoredAsset {
            asset_id: "asset-ready".to_string(),
            local_path: local_path.to_string(),
            original_name: "ready-resource".to_string(),
            mime_type: mime_type.to_string(),
            size: 4,
            kind,
        }
    }

    fn test_message(content: Vec<ContentBlock>) -> Message {
        Message {
            id: "msg-ready".to_string(),
            ..Message::with_role(Role::user(), content)
        }
    }

    fn system_message(text: &str) -> Message {
        Message::with_role(Role::System, vec![ContentBlock::text(text)])
    }

    fn user_message_with_ready_image(local_path: &str, data: Option<&str>) -> Message {
        test_message(vec![
            ContentBlock::text("处理图片"),
            ContentBlock::Image {
                asset: test_asset(MediaKind::Image, local_path, "image/png"),
                data: data.map(str::to_string),
            },
        ])
    }

    fn temp_image(prefix: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}.png",
            std::process::id(),
            scru128::new()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn images(message: &ChatMessage) -> Vec<&ImageContent> {
        message
            .content
            .iter()
            .filter_map(|content| match content {
                MessageContent::Image(image) => Some(image),
                _ => None,
            })
            .collect()
    }

    fn has_text(message: &ChatMessage, predicate: impl Fn(&str) -> bool) -> bool {
        message.content.iter().any(|content| match content {
            MessageContent::Text(text) => predicate(text),
            _ => false,
        })
    }

    #[test]
    fn fails_without_system_message() {
        // context 无 System 消息时必须 fail-fast，防止调用方遗漏 system prompt 注入。
        let error = assemble(vec![test_message(vec![ContentBlock::text("你好")])]).unwrap_err();
        assert!(
            error.to_string().contains("System"),
            "错误信息应指出缺少 System 消息，实际：{error}"
        );
    }

    #[test]
    fn system_texts_are_trimmed_and_joined_without_model_instructions() {
        let mut second = system_message("  第二段  ");
        second
            .content
            .push(ContentBlock::model_instruction("不进入 system"));
        let request = assemble(vec![
            system_message("  第一段\n"),
            system_message("   "),
            second,
            Message::new(MessageRole::User, "你好"),
        ])
        .unwrap();
        assert_eq!(request.system, "第一段\n第二段");
    }

    #[test]
    fn missing_user_falls_back_to_continue_text() {
        let assistant = Message::new(MessageRole::Assistant, "孤立回答");
        let request = assemble(vec![system_message("系统"), assistant]).unwrap();
        assert_eq!(
            request.messages,
            vec![ChatMessage::text(LlmMessageRole::User, FALLBACK_USER_TEXT)]
        );
    }

    #[test]
    fn leading_non_user_messages_are_skipped() {
        let request = assemble(vec![
            system_message("系统"),
            Message::new(MessageRole::Assistant, "截断前回答"),
            Message::tool_result("call_old", "read_file", "截断前结果", false),
            Message::new(MessageRole::User, "继续"),
        ])
        .unwrap();
        assert_eq!(
            request.messages,
            vec![ChatMessage::text(LlmMessageRole::User, "继续")]
        );
    }

    #[test]
    fn tool_result_keeps_call_id_and_error_flag() {
        let request = assemble(vec![
            system_message("系统"),
            Message::new(MessageRole::User, "开始"),
            Message::tool_result("call_1", "read_file", "结果", true),
        ])
        .unwrap();
        assert_eq!(
            request.messages[1],
            ChatMessage::new(
                LlmMessageRole::Tool,
                vec![MessageContent::ToolResult(ToolResult {
                    tool_call_id: "call_1".to_string(),
                    content: ToolResultContent::Text("结果".to_string()),
                    is_error: true,
                })],
            )
        );
    }

    /// 旧会话中无配对调用的 Tool 消息不进入请求。
    #[test]
    fn unpaired_legacy_tool_message_is_dropped() {
        assert!(
            chat_message(Message::tool_result("", "loop_guard", "旧运行轨迹", false)).is_none()
        );
    }

    #[test]
    fn assistant_keeps_reasoning_text_and_tool_calls_in_order() {
        let mut assistant = Message::with_reasoning(MessageRole::Assistant, "回答", "  思考\n")
            .with_tool_calls(vec![MessageToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "a"}),
            }]);
        if let Role::Assistant {
            reasoning_signature,
            ..
        } = &mut assistant.role
        {
            *reasoning_signature = Some("sig".to_string());
        }
        let message = chat_message(assistant).unwrap();
        assert_eq!(message.role, LlmMessageRole::Assistant);
        assert_eq!(
            message.content,
            vec![
                MessageContent::Thinking(ThinkingContent {
                    thinking: "  思考\n".to_string(),
                    signature: Some("sig".to_string()),
                }),
                MessageContent::Text("回答".to_string()),
                MessageContent::ToolCall(ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": "a"}),
                }),
            ]
        );
    }

    #[test]
    fn notice_and_empty_messages_are_dropped() {
        let notice = Message::with_role(
            Role::Notice { usage: None },
            vec![ContentBlock::text("通知")],
        );
        assert!(chat_message(notice).is_none());
        assert!(chat_message(Message::new(MessageRole::User, "  ")).is_none());
    }

    #[test]
    fn ready_image_prefers_current_data() {
        let message = user_message_with_ready_image(
            "/path/that/does/not/exist.png",
            Some("CURRENT_RUNTIME_BASE64"),
        );
        let result = chat_message(message).expect("应生成消息");
        assert_eq!(
            images(&result)[0].data,
            "data:image/png;base64,CURRENT_RUNTIME_BASE64"
        );
    }

    #[test]
    fn remote_image_url_is_passed_through_trimmed() {
        let message = user_message_with_ready_image("  https://example.com/a.png ", None);
        let result = chat_message(message).expect("应生成消息");
        assert_eq!(images(&result)[0].data, "https://example.com/a.png");
    }

    #[test]
    fn historical_ready_image_is_encoded_from_local_path() {
        let path = temp_image("tiangong-context-inline", &[1, 2, 3, 4]);
        let message = user_message_with_ready_image(path.to_str().unwrap(), None);
        let result = chat_message(message).expect("应生成消息");
        let image = images(&result)[0];
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.data, "data:image/png;base64,AQIDBA==");
        let _ = std::fs::remove_file(path);
    }

    /// 图片文件缺失的降级覆盖所有消息（不限 HostInjected）：历史图片被清理后，
    /// 请求仍可发出，图片位置换成文字说明，路径中的控制字符被净化。
    #[test]
    fn missing_image_degrades_to_sanitized_text() {
        let message = user_message_with_ready_image("/nonexistent/\u{202E}gnp.x\nline.png", None);
        let result = chat_message(message).expect("应生成消息");
        assert!(images(&result).is_empty());
        assert!(has_text(&result, |text| text
            == "[image unavailable] 图片文件已不可读（asset_id=asset-ready，path=/nonexistent/ gnp.x line.png），内容不可用"));
    }

    /// RFC 0017「看见而非知道」：HostInjected 注入消息（provenance + Image、
    /// data=None）必须携带原生图片内容，不得退化为文本提及路径。
    #[test]
    fn host_injected_message_carries_native_image() {
        let path = temp_image("tiangong-context-injected", &[9, 8, 7, 6]);
        let message = test_message(vec![
            ContentBlock::model_instruction(
                "[injected-images provenance]\n- 工具 desktop_screenshot",
            ),
            ContentBlock::Image {
                asset: test_asset(MediaKind::Image, path.to_str().unwrap(), "image/png"),
                data: None,
            },
        ])
        .with_source(UserSource::HostInjected);
        let result = chat_message(message).expect("应生成消息");
        assert!(has_text(&result, |text| text.contains("injected-images")));
        assert_eq!(images(&result)[0].data, "data:image/png;base64,CQgHBg==");
        let _ = std::fs::remove_file(path);
    }

    /// 完整工具轮序列 user → assistant(tool_calls) → tool 结果 → HostInjected
    /// 注入消息（provenance + Image）转换后，注入消息必须保留且位于 Tool 之后。
    #[test]
    fn host_injected_message_survives_full_request_conversion() {
        let path = temp_image("tiangong-context-seq", &[1, 2, 3, 4]);
        let assistant = Message::with_role(Role::assistant(), Vec::new()).with_tool_calls(vec![
            MessageToolCall {
                id: "call-1".to_string(),
                name: "desktop_screenshot".to_string(),
                arguments: serde_json::json!({}),
            },
        ]);
        let injected = test_message(vec![
            ContentBlock::model_instruction(
                "[injected-images provenance]\n- 工具 desktop_screenshot（call-1）",
            ),
            ContentBlock::Image {
                asset: test_asset(MediaKind::Image, path.to_str().unwrap(), "image/png"),
                data: None,
            },
        ])
        .with_source(UserSource::HostInjected);
        let request = assemble(vec![
            system_message("你是通用助手。"),
            test_message(vec![ContentBlock::text("看看屏幕")]),
            assistant,
            Message::tool_result("call-1", "desktop_screenshot", "{}", false),
            injected,
        ])
        .unwrap();
        let position = request
            .messages
            .iter()
            .position(|message| {
                message.role == LlmMessageRole::User
                    && has_text(message, |text| text.contains("injected-images"))
            })
            .expect("注入消息必须存在于转换后的请求");
        assert_eq!(images(&request.messages[position]).len(), 1);
        let tool_position = request
            .messages
            .iter()
            .position(|message| message.role == LlmMessageRole::Tool)
            .expect("工具结果消息必须存在");
        assert!(position > tool_position, "注入消息应位于工具结果之后");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn model_instruction_is_mapped_verbatim_and_assets_are_ignored() {
        let instruction = "  使用宿主已选择的资源能力处理 path=/tmp/report.pdf\n";
        let message = test_message(vec![
            ContentBlock::text("处理资源"),
            ContentBlock::AssetReference {
                asset: test_asset(MediaKind::File, "/tmp/report.pdf", "application/pdf"),
            },
            ContentBlock::model_instruction(instruction),
            ContentBlock::Media {
                kind: MediaKind::Image,
                url: "data:image/png;base64,SHOULD_NOT_INLINE".to_string(),
                mime_type: Some("image/png".to_string()),
                title: None,
            },
        ]);
        let result = chat_message(message).expect("应生成消息");
        assert_eq!(
            result.content,
            vec![
                MessageContent::Text("处理资源".to_string()),
                MessageContent::Text(instruction.to_string()),
            ]
        );
    }
}
