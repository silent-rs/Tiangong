//! 宿主消息到协议中间模型的纯数据转换：不读文件、不理解 Session、不修改历史 ID。
use crate::message::{
    ChatMessage, ImageContent, MessageContent, MessageRole as LlmMessageRole, ThinkingContent,
};
use crate::tool::{ToolCall, ToolResult, ToolResultContent};
use anyhow::{Result, bail};
use tiangong_types::{ContentBlock, Message, Role, StoredAsset};

pub(crate) fn convert_messages(input: Vec<Message>) -> Result<(Option<String>, Vec<ChatMessage>)> {
    let mut system = Vec::new();
    let mut messages = Vec::with_capacity(input.len());
    for message in input {
        if message.role == tiangong_types::MessageRole::Notice {
            continue;
        }
        for block in &message.content {
            if let ContentBlock::Image { asset, data } = block
                && !data
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
                && !asset.local_path.trim().starts_with("https://")
                && !asset.local_path.trim().starts_with("http://")
            {
                bail!("图片尚未准备：请在调用 LLM 前准备媒体数据");
            }
        }
        if message.role == tiangong_types::MessageRole::System {
            let text = plain_text(message.content);
            if !text.trim().is_empty() {
                system.push(text.trim().to_owned());
            }
        } else if let Some(message) = chat_message(message) {
            messages.push(message);
        }
    }
    if messages.is_empty() {
        bail!("模型请求缺少有效消息");
    }
    let system = system.join("\n");
    Ok(((!system.is_empty()).then_some(system), messages))
}

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
            ContentBlock::Image { asset, data } => parts.push(image_content(&asset, data)?),
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

fn image_content(asset: &StoredAsset, data: Option<String>) -> Option<MessageContent> {
    let data = data.filter(|value| !value.trim().is_empty()).or_else(|| {
        let path = asset.local_path.trim();
        (path.starts_with("https://") || path.starts_with("http://")).then(|| path.to_owned())
    })?;
    let data =
        if data.starts_with("data:") || data.starts_with("http://") || data.starts_with("https://")
        {
            data
        } else {
            format!("data:{};base64,{data}", asset.mime_type)
        };
    Some(MessageContent::Image(ImageContent {
        mime_type: asset.mime_type.clone(),
        data,
    }))
}
