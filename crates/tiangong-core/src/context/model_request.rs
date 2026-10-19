//! 会话上下文 → 模型请求：[`IntoModelRequest`] 把 `Session::context()` 返回的
//! 有序消息列表按值转换为 llm 层的 [`ModelRequest`]（`system` + `messages`），
//! 消息内容直接移动，不再额外拷贝。
//!
//! 消息的选取与顺序（system prompt + 摘要边界之后的对话，排除持久化 System
//! 与 Notice）已在 `Session::context()` 中确定，这里只做内容转换。宿主会话
//! 语义只在这里处理（issue #691），llm 层不理解宿主消息格式、不读取本地文件：
//! - system prompt 取自 `MessageRole::System` 消息，缺少即视为调用方错误；
//! - 没有配对调用（`tool_call_id` 为空）的 Tool 消息不进入请求（仅存在于 #77 之前的旧会话）；
//! - 模型指令与用户文本同样进入请求，附件引用与旧格式媒体不进入请求；
//! - 图片按当前请求数据 → 远程 URL → 本地文件的顺序取用，文件不可读时降级为文字说明；
//! - 截断导致的开头非 User 消息被跳过，没有 User 消息时补兜底用户文本。

use std::path::Path;

use anyhow::bail;
use base64::{Engine as _, engine::general_purpose};
use tiangong_llm::ModelRequest;
use tiangong_llm::message::{
    ChatMessage, ImageContent, MessageContent, MessageRole as LlmMessageRole, ThinkingContent,
};
use tiangong_llm::tool::{ToolCall, ToolResult, ToolResultContent};

use crate::session::{ContentBlock, Message, MessageRole, Role, StoredAsset};

/// 上下文中没有任何 User 消息时补发的兜底用户文本。
pub const FALLBACK_USER_TEXT: &str = "请继续处理当前任务。";

/// 把有序会话消息转换为模型请求。
///
/// `ModelRequest` 与 `Vec<Message>` 都不是 core 定义的类型，孤儿规则不允许在
/// core 中为它们实现 `TryFrom`；由 core 自定义该 trait 承载转换。
pub trait IntoModelRequest {
    /// 只填充 `system` 与 `messages`，其余请求参数（会话 ID、思考强度、输出
    /// 上限等）由调用方用结构体更新语法补充。
    fn into_model_request(self) -> anyhow::Result<ModelRequest>;
}

impl IntoModelRequest for Vec<Message> {
    fn into_model_request(self) -> anyhow::Result<ModelRequest> {
        // System prompt 必须由 core 显式注入（session.system_prompt_message）：
        // context 缺 System 角色消息视为调用方错误，fail-fast。文本允许为空
        //（core 不再生产 prompt 内容，无插件段落且无摘要时为空，请求即不带 system）。
        let mut system_texts: Vec<String> = Vec::new();
        let mut has_system_message = false;
        let mut messages = Vec::with_capacity(self.len());
        for message in self {
            if message.role == MessageRole::System {
                has_system_message = true;
                let text = plain_text(message.content);
                let text = text.trim();
                if !text.is_empty() {
                    system_texts.push(text.to_string());
                }
                continue;
            }
            if let Some(message) = chat_message(message) {
                messages.push(message);
            }
        }

        if !has_system_message {
            bail!(
                "模型上下文缺少 System 消息。system prompt 应由 core 显式注入（session.system_prompt_message）。"
            );
        }

        match messages
            .iter()
            .position(|message| message.role == LlmMessageRole::User)
        {
            // 跳过开头的非 User 消息（summary_up_to 截断可能导致以 Assistant/Tool 开头）
            Some(index) if index > 0 => {
                tracing::debug!(
                    skipped = index,
                    "跳过消息列表开头的非 User 消息（可能是 summary 截断导致）"
                );
                messages.drain(..index);
            }
            Some(_) => {}
            // 没有 User 消息时其余消息都无法构成合法请求，只保留兜底文本。
            None => {
                messages = vec![ChatMessage::text(LlmMessageRole::User, FALLBACK_USER_TEXT)];
            }
        }

        Ok(ModelRequest {
            system: system_texts.join("\n"),
            messages,
            ..Default::default()
        })
    }
}

/// 拼接 `Text` 内容块（不含模型指令、附件等其他内容块）。
fn plain_text(content: Vec<ContentBlock>) -> String {
    content
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect()
}

fn chat_message(message: Message) -> Option<ChatMessage> {
    let Message { role, content, .. } = message;
    let (role, reasoning, signature, tool_calls) = match role {
        Role::User { .. } => (LlmMessageRole::User, String::new(), None, Vec::new()),
        Role::Assistant {
            reasoning_content,
            reasoning_signature,
            tool_calls,
            ..
        } => (
            LlmMessageRole::Assistant,
            reasoning_content,
            reasoning_signature,
            tool_calls,
        ),
        Role::Tool {
            tool_call_id,
            is_error,
            ..
        } => {
            // 旧版本曾把运行时轨迹写成无配对调用的 Tool 消息（#77 起只记日志），
            // 它不是任何调用的结果，不进入请求。
            if tool_call_id.is_empty() {
                return None;
            }
            return Some(ChatMessage::new(
                LlmMessageRole::Tool,
                vec![MessageContent::ToolResult(ToolResult {
                    tool_call_id,
                    content: ToolResultContent::Text(plain_text(content)),
                    is_error,
                })],
            ));
        }
        Role::System => return None,
        // Notice 是系统发给用户的通知，任何路径都不得进入模型请求。
        Role::Notice { .. } => return None,
    };

    let mut parts = Vec::new();
    if !reasoning.trim().is_empty() {
        parts.push(MessageContent::Thinking(ThinkingContent {
            thinking: reasoning,
            signature,
        }));
    }

    for block in content {
        match block {
            ContentBlock::Text { text } | ContentBlock::ModelInstruction { text } => {
                if !text.trim().is_empty() {
                    parts.push(MessageContent::Text(text));
                }
            }
            ContentBlock::Image { asset, data } => parts.push(image_content(&asset, data)),
            ContentBlock::Media { .. } | ContentBlock::AssetReference { .. } => {}
        }
    }

    parts.extend(tool_calls.into_iter().map(|call| {
        MessageContent::ToolCall(ToolCall {
            id: call.id,
            name: call.name,
            arguments: call.arguments,
        })
    }));
    if parts.is_empty() {
        return None;
    }

    Some(ChatMessage::new(role, parts))
}

fn image_content(asset: &StoredAsset, runtime_data: Option<String>) -> MessageContent {
    match ready_image(asset, runtime_data) {
        Some(image) => MessageContent::Image(image),
        // 图片文件缺失（被清理/声明失效）时降级为文字说明，不让整个
        // 请求失败——否则该会话之后每一轮请求都会报错。路径可能来自
        // 工具可控字段，净化后拼接。
        None => MessageContent::Text(format!(
            "[image unavailable] 图片文件已不可读（asset_id={}，path={}），内容不可用",
            asset.asset_id,
            escape_asset_text(&asset.local_path)
        )),
    }
}

fn ready_image(asset: &StoredAsset, runtime_data: Option<String>) -> Option<ImageContent> {
    if let Some(data) = runtime_data.filter(|data| !data.trim().is_empty()) {
        let data = if data.trim_start().starts_with("data:") {
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
