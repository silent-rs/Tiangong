//! 消息类型

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::StoredAsset;

const REDACTED_INLINE_DATA_REFERENCE: &str = "<inline-data-reference-unavailable>";

fn is_inline_data_reference(value: &str) -> bool {
    value
        .trim_start()
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
}

/// 消息角色标签（不带角色数据），用于比较、匹配与分派。
///
/// [`Notice`](MessageRole::Notice) 是系统发给用户的通知（如轮次失败原因），
/// 仅前端可见：按角色在上下文构建、压缩与 provider 转换处整体排除，
/// 不进模型上下文，也不与 [`System`](MessageRole::System)（系统提示通道）混用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
    Notice,
}

/// 单个对话轮次的执行状态，仅持久化到起轮的用户消息（turn 锚点）。
///
/// 起轮时写入 `Processing`，收尾改为终态；进程意外退出时会残留
/// `Processing`，下一条用户消息到来时由 Core 接续该轮。引导消息等
/// 非起轮的用户消息始终为 None。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnStatus {
    /// 执行中（已起轮、尚未收尾）。
    Processing,
    /// 正常完成（含总结阶段产出最终回复）。
    Success,
    /// 执行过程中出错。
    Failed,
    /// 用户主动取消。
    Cancelled,
}

impl TurnStatus {
    /// 是否为轮次终态（非 `Processing`）。
    pub fn is_terminal(self) -> bool {
        self != Self::Processing
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// 工具调用批次闭合前收到、等待安全边界注入的外部工具内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredToolInjection {
    pub tool_name: String,
    pub payload: Value,
}

/// 用户消息的来源：决定是否作为轮次锚点、能否被压缩等运行语义。
///
/// 前向兼容：未知值（更高版本写入的新变体）降级为 `Human`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UserSource {
    /// 用户真实输入（含 IM、Server API、定时任务等外部入口）。
    #[default]
    Human,
    /// 宿主注入的媒体消息（RFC 0017）：工具产物图片等以原生视觉部件发送给
    /// 模型。不是用户意图，不作轮次锚点；随压缩边界降级。
    HostInjected,
    /// 压缩后注入的「当前任务状态」恢复锚点：始终发送给模型、不再被压缩，
    /// 不作轮次锚点。
    CompressedResume,
}

impl<'de> Deserialize<'de> for UserSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "host_injected" => Self::HostInjected,
            "compressed_resume" => Self::CompressedResume,
            _ => Self::Human,
        })
    }
}

impl UserSource {
    /// 是否为用户真实输入（可作轮次锚点）。
    pub fn is_user_input(self) -> bool {
        self == Self::Human
    }
}

/// 角色及其必带字段。
///
/// 只放「某角色才有」的结构化字段；与角色无关、仅供界面使用的数据放
/// [`MessageMeta`]。JSON 形态：`{"type": "assistant", "tool_calls": [...], ...}`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Role {
    /// 系统提示通道。
    System,
    /// 用户消息。
    User {
        #[serde(default, skip_serializing_if = "is_default")]
        source: UserSource,
        /// 本轮执行状态（仅起轮锚点）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_status: Option<TurnStatus>,
        /// 本轮执行时长（毫秒，仅起轮锚点）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
        /// 本轮最终答复的消息 ID（成功完成时写入，失败回收时清空）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_reply: Option<String>,
    },
    /// 助手消息。
    Assistant {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reasoning_content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_signature: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<MessageToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Box<crate::token::MessageUsage>>,
        /// 思考阶段耗时（毫秒）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_elapsed_ms: Option<u64>,
        /// 正文生成阶段耗时（毫秒）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_elapsed_ms: Option<u64>,
    },
    /// 工具结果。`tool_call_id` 为空表示无配对调用的运行时上下文
    /// （provider 映射为 `<tool-context>` 用户文本）。
    Tool {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tool_call_id: String,
        #[serde(default)]
        tool_name: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
        /// 单次工具调用执行耗时（毫秒）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// 系统发给用户的通知（不进模型上下文）。
    Notice {
        /// 无正文模型调用的用量记录。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Box<crate::token::MessageUsage>>,
    },
}

impl Role {
    /// 以角色标签构造空角色数据。
    pub fn from_kind(kind: MessageRole) -> Self {
        match kind {
            MessageRole::System => Self::System,
            MessageRole::User => Self::user(),
            MessageRole::Assistant => Self::assistant(),
            MessageRole::Tool => Self::Tool {
                tool_call_id: String::new(),
                tool_name: String::new(),
                is_error: false,
                duration_ms: None,
            },
            MessageRole::Notice => Self::Notice { usage: None },
        }
    }

    pub fn user() -> Self {
        Self::User {
            source: UserSource::Human,
            turn_status: None,
            elapsed_ms: None,
            final_reply: None,
        }
    }

    pub fn assistant() -> Self {
        Self::Assistant {
            reasoning_content: String::new(),
            reasoning_signature: None,
            tool_calls: Vec::new(),
            usage: None,
            reasoning_elapsed_ms: None,
            text_elapsed_ms: None,
        }
    }

    /// 角色标签。
    pub fn kind(&self) -> MessageRole {
        match self {
            Self::System => MessageRole::System,
            Self::User { .. } => MessageRole::User,
            Self::Assistant { .. } => MessageRole::Assistant,
            Self::Tool { .. } => MessageRole::Tool,
            Self::Notice { .. } => MessageRole::Notice,
        }
    }
}

/// 与角色无关的消息结构化字段。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageMeta {
    /// 表示从当前消息及以前的历史已被压缩摘要覆盖。
    #[serde(default, skip_serializing_if = "is_false")]
    pub compact: bool,
}

impl MessageMeta {
    pub fn is_empty(&self) -> bool {
        !self.compact
    }
}

/// 消息内容块
///
/// 统一表达消息中的文本、图片、视频、音频、文件等内容。
/// `Message.content` 为 `Vec<ContentBlock>`，支持多类型内容连续排列。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// 宿主准备好的模型指令。该内容会发送给模型，但不属于用户可见文本。
    ModelInstruction {
        text: String,
    },
    /// 旧格式或仅供展示的媒体块。新用户输入不得依赖 Provider 解释该块。
    Media {
        kind: MediaKind,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// 已由宿主决定直接进入模型请求的图片。
    Image {
        asset: StoredAsset,
        /// 仅供当前请求使用；会话和流事件序列化时始终省略。
        #[serde(default, skip_serializing)]
        data: Option<String>,
    },
    /// 供展示、插件或宿主使用的稳定资源引用；Provider 不解释该块。
    AssetReference {
        asset: StoredAsset,
    },
}

impl ContentBlock {
    pub fn text(content: impl Into<String>) -> Self {
        Self::Text {
            text: content.into(),
        }
    }

    pub fn model_instruction(content: impl Into<String>) -> Self {
        Self::ModelInstruction {
            text: content.into(),
        }
    }

    pub fn image(asset: StoredAsset, data: Option<String>) -> Self {
        Self::Image { asset, data }
    }

    pub fn asset_reference(asset: StoredAsset) -> Self {
        Self::AssetReference { asset }
    }

    pub fn is_text(&self) -> bool {
        matches!(self, Self::Text { .. } | Self::ModelInstruction { .. })
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text { text } | Self::ModelInstruction { text } => text.trim().is_empty(),
            Self::Media { .. } | Self::Image { .. } | Self::AssetReference { .. } => false,
        }
    }

    pub fn clear_transient_data(&mut self) {
        match self {
            Self::Image { asset, data } => {
                *data = None;
                asset.clear_inline_data_reference();
            }
            Self::AssetReference { asset } => asset.clear_inline_data_reference(),
            Self::Media { url, .. } if is_inline_data_reference(url) => {
                *url = REDACTED_INLINE_DATA_REFERENCE.to_string();
            }
            Self::Text { .. } | Self::ModelInstruction { .. } | Self::Media { .. } => {}
        }
    }

    /// 校验会进入持久化和流事件的资源引用不携带内联数据。
    pub fn validate_stable_reference(&self) -> Result<(), String> {
        let invalid = match self {
            Self::Image { asset, .. } | Self::AssetReference { asset } => {
                asset.has_inline_data_reference()
            }
            Self::Media { url, .. } => is_inline_data_reference(url),
            Self::Text { .. } | Self::ModelInstruction { .. } => false,
        };
        if invalid {
            Err("资源引用不能包含 data: 内联数据，请通过 Image.data 传递当前请求数据".to_string())
        } else {
            Ok(())
        }
    }

    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Media { url, .. } => Some(url),
            Self::Image { asset, .. } | Self::AssetReference { asset } => Some(&asset.local_path),
            Self::Text { .. } | Self::ModelInstruction { .. } => None,
        }
    }

    pub fn kind(&self) -> Option<MediaKind> {
        match self {
            Self::Media { kind, .. } => Some(*kind),
            Self::Image { asset, .. } | Self::AssetReference { asset } => Some(asset.kind),
            Self::Text { .. } | Self::ModelInstruction { .. } => None,
        }
    }
}

/// 媒体类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    File,
}

/// 会话消息中的结构化媒体资源（旧格式，保留用于反序列化兼容）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaAsset {
    pub kind: MediaKind,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
}

impl MediaAsset {
    /// 转换为 ContentBlock
    pub fn to_content_block(&self) -> ContentBlock {
        ContentBlock::Media {
            kind: self.kind,
            url: self.url.clone(),
            mime_type: self.mime_type.clone(),
            title: self.title.clone(),
        }
    }
}

/// 对话消息。
///
/// - `role`：角色及其必带字段（见 [`Role`]）；
/// - `content`：可直接提交给模型的内容块；
/// - `meta`：与角色无关的结构化字段（压缩标记、插件渲染）。
///
/// 序列化为新格式；反序列化同时接受旧的扁平格式（`role` 为字符串、角色
/// 字段平铺在顶层），读入后按新结构归位，下次保存即写为新格式。
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: String,
    pub created_at: String,
    pub role: Role,
    /// 消息内容，支持文本、图片、视频、音频、文件等多种类型混合排列。
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "MessageMeta::is_empty")]
    pub meta: MessageMeta,
}

impl PartialEq<MessageRole> for Role {
    fn eq(&self, other: &MessageRole) -> bool {
        self.kind() == *other
    }
}

impl PartialEq<Role> for MessageRole {
    fn eq(&self, other: &Role) -> bool {
        *self == other.kind()
    }
}

#[derive(Deserialize)]
struct LegacyResourceBlock {
    asset_id: String,
    local_path: String,
    original_name: String,
    mime_type: String,
    size: u64,
    kind: MediaKind,
    handling_mode: String,
}

fn deserialize_message_content(
    value: Value,
    role: MessageRole,
    message_id: &str,
    legacy_media: Vec<MediaAsset>,
) -> Result<Vec<ContentBlock>, String> {
    let values = match value {
        Value::String(text) => vec![serde_json::json!({"type": "text", "text": text})],
        Value::Array(values) => values,
        _ => return Err("content 必须是字符串或内容块数组".to_string()),
    };

    let mut content = Vec::new();
    let mut runtime_images = std::collections::HashMap::<String, String>::new();
    let mut resource_index = 0usize;

    for value in values {
        let block_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match block_type {
            "attachment" => {
                let attachment = value
                    .get("attachment")
                    .cloned()
                    .ok_or_else(|| "旧 attachment 块缺少 attachment 字段".to_string())?;
                let attachment: LegacyResourceBlock = serde_json::from_value(attachment)
                    .map_err(|error| format!("旧 attachment 块无效：{error}"))?;
                let inline_data = is_inline_data_reference(&attachment.local_path);
                let asset_id = if attachment.asset_id.len() > 128
                    || is_inline_data_reference(&attachment.asset_id)
                {
                    legacy_hashed_asset_id(&attachment.local_path)
                } else {
                    attachment.asset_id
                };
                let asset = StoredAsset {
                    asset_id,
                    local_path: if inline_data {
                        "<legacy-inline-data-unavailable>".to_string()
                    } else {
                        attachment.local_path
                    },
                    original_name: attachment.original_name,
                    mime_type: attachment.mime_type,
                    size: attachment.size,
                    kind: attachment.kind,
                };
                if attachment.handling_mode == "inline_image" && !inline_data {
                    content.push(ContentBlock::Image { asset, data: None });
                } else {
                    let instruction =
                        legacy_resource_instruction(message_id, resource_index, &asset);
                    content.push(ContentBlock::AssetReference { asset });
                    content.push(instruction);
                }
                resource_index += 1;
            }
            "runtime_inline_image" => {
                let asset_id = value
                    .get("asset_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let data = value
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !asset_id.is_empty() && !data.is_empty() {
                    runtime_images.insert(asset_id.to_string(), data.to_string());
                }
            }
            "media" if role == MessageRole::User => {
                let block: ContentBlock = serde_json::from_value(value)
                    .map_err(|error| format!("旧 media 块无效：{error}"))?;
                if let ContentBlock::Media {
                    kind,
                    url,
                    mime_type,
                    title,
                } = block
                {
                    let asset = stored_asset_from_legacy_media(kind, url, mime_type, title);
                    push_legacy_resource(&mut content, message_id, resource_index, asset);
                    resource_index += 1;
                }
            }
            _ => {
                let block: ContentBlock = serde_json::from_value(value)
                    .map_err(|error| format!("内容块无效：{error}"))?;
                if matches!(
                    block,
                    ContentBlock::Media { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::AssetReference { .. }
                ) {
                    resource_index += 1;
                }
                content.push(block);
            }
        }
    }

    for media in legacy_media {
        if role == MessageRole::User {
            let asset =
                stored_asset_from_legacy_media(media.kind, media.url, media.mime_type, media.title);
            push_legacy_resource(&mut content, message_id, resource_index, asset);
            resource_index += 1;
        } else {
            content.push(media.to_content_block());
        }
    }

    if !runtime_images.is_empty() {
        for block in &mut content {
            if let ContentBlock::Image { asset, data } = block
                && let Some(runtime_data) = runtime_images.remove(&asset.asset_id)
            {
                *data = Some(runtime_data);
            }
        }
    }

    // 无论消息角色或历史格式如何，反序列化边界都只保留稳定引用。
    for block in &mut content {
        block.clear_transient_data();
    }

    Ok(content)
}

fn push_legacy_resource(
    content: &mut Vec<ContentBlock>,
    message_id: &str,
    resource_index: usize,
    asset: StoredAsset,
) {
    if asset.kind == MediaKind::Image && asset.local_path != "<legacy-inline-data-unavailable>" {
        content.push(ContentBlock::Image { asset, data: None });
    } else {
        let instruction = legacy_resource_instruction(message_id, resource_index, &asset);
        content.push(ContentBlock::AssetReference { asset });
        content.push(instruction);
    }
}

fn stored_asset_from_legacy_media(
    kind: MediaKind,
    url: String,
    mime_type: Option<String>,
    title: Option<String>,
) -> StoredAsset {
    const UNAVAILABLE_INLINE_PATH: &str = "<legacy-inline-data-unavailable>";
    let is_inline_data = is_inline_data_reference(&url);
    let original_name = title.unwrap_or_else(|| legacy_resource_name(&url, is_inline_data));
    let mime_type = mime_type.unwrap_or_else(|| legacy_resource_mime(kind, &url));
    StoredAsset {
        asset_id: legacy_hashed_asset_id(&url),
        local_path: if is_inline_data {
            UNAVAILABLE_INLINE_PATH.to_string()
        } else {
            url
        },
        original_name,
        mime_type,
        size: 0,
        kind,
    }
}

fn legacy_hashed_asset_id(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("legacy-{:016x}", hasher.finish())
}

fn legacy_resource_name(value: &str, is_inline_data: bool) -> String {
    if is_inline_data {
        return "legacy-inline-resource".to_string();
    }
    value
        .split(['?', '#'])
        .next()
        .unwrap_or(value)
        .rsplit(['/', '\\'])
        .find(|part| !part.trim().is_empty())
        .unwrap_or("legacy-resource")
        .to_string()
}

fn legacy_resource_mime(kind: MediaKind, value: &str) -> String {
    let trimmed = value.trim_start();
    if is_inline_data_reference(trimmed)
        && let Some(mime) = trimmed[5..]
            .split_once(';')
            .map(|(mime, _)| mime)
            .filter(|mime| mime.contains('/'))
    {
        return mime.to_string();
    }

    let path = value
        .split(['?', '#'])
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase();
    match kind {
        MediaKind::Image if path.ends_with(".jpg") || path.ends_with(".jpeg") => "image/jpeg",
        MediaKind::Image if path.ends_with(".webp") => "image/webp",
        MediaKind::Image if path.ends_with(".gif") => "image/gif",
        MediaKind::Image => "image/png",
        MediaKind::Video if path.ends_with(".webm") => "video/webm",
        MediaKind::Video => "video/mp4",
        MediaKind::Audio if path.ends_with(".wav") => "audio/wav",
        MediaKind::Audio if path.ends_with(".ogg") => "audio/ogg",
        MediaKind::Audio => "audio/mpeg",
        MediaKind::File if path.ends_with(".pdf") => "application/pdf",
        MediaKind::File if path.ends_with(".txt") => "text/plain",
        MediaKind::File => "application/octet-stream",
    }
    .to_string()
}

fn legacy_resource_instruction(
    message_id: &str,
    index: usize,
    asset: &StoredAsset,
) -> ContentBlock {
    if asset.local_path == "<legacy-inline-data-unavailable>" {
        return ContentBlock::model_instruction(format!(
            "本条历史用户消息包含未归档的内联资源，内容无法安全恢复。请明确告知用户重新上传；不得把旧内联数据写回会话或模型上下文。\n- attachment_index={index} asset_id={} kind={:?} name={} mime_type={}",
            asset.asset_id, asset.kind, asset.original_name, asset.mime_type,
        ));
    }
    ContentBlock::model_instruction(format!(
        "本条用户消息包含一个已保存资源。需要读取内容时，请使用当前可用的资源处理能力，直接传入下列本地 path。来源：message_id={message_id}、attachment_index={index}。\n- asset_id={} kind={:?} name={} mime_type={} size={} path={}",
        asset.asset_id,
        asset.kind,
        asset.original_name,
        asset.mime_type,
        asset.size,
        asset.local_path,
    ))
}

/// 消息反序列化的原始形态：同时容纳新格式（`role` 为对象、`meta`）与旧的
/// 扁平格式（`role` 为字符串、角色字段平铺在顶层），由 [`MessageRaw::decode`]
/// 归位到新结构。
#[derive(Deserialize)]
pub struct MessageRaw {
    id: String,
    role: Value,
    content: Value,
    created_at: String,
    #[serde(default)]
    meta: Option<MessageMeta>,
    // ── 以下为旧扁平格式字段 ──
    #[serde(default)]
    reasoning_content: String,
    reasoning_signature: Option<String>,
    usage: Option<Box<crate::token::MessageUsage>>,
    worker_id: Option<String>,
    /// 更早格式的顶层 media：并入 content。
    #[serde(default)]
    media: Vec<MediaAsset>,
    #[serde(default)]
    tool_calls: Vec<MessageToolCall>,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    #[serde(default)]
    tool_result_is_error: bool,
    #[serde(default)]
    compact: bool,
    phase: Option<String>,
    elapsed_ms: Option<u64>,
    turn_status: Option<TurnStatus>,
    reasoning_elapsed_ms: Option<u64>,
    text_elapsed_ms: Option<u64>,
    duration_ms: Option<u64>,
}

/// 旧格式助手消息的阶段，供会话级迁移推导 `final_reply`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyAssistantPhase {
    Normal,
    React,
    Summary,
}

/// 旧格式迁移所需、但不属于新结构的信息。
#[derive(Debug, Clone, Copy)]
struct LegacyHint {
    assistant_phase: Option<LegacyAssistantPhase>,
    from_worker: bool,
}

impl MessageRaw {
    /// 归位为新结构；旧格式返回迁移提示。
    fn decode(self) -> Result<(Message, Option<LegacyHint>), String> {
        if !self.role.is_string() {
            let role: Role = serde_json::from_value(self.role)
                .map_err(|error| format!("消息 role 无效：{error}"))?;
            let content =
                deserialize_message_content(self.content, role.kind(), &self.id, self.media)?;
            let message = Message {
                id: self.id,
                created_at: self.created_at,
                role,
                content,
                meta: self.meta.unwrap_or_default(),
            };
            return Ok((message, None));
        }

        let kind: MessageRole = serde_json::from_value(self.role)
            .map_err(|error| format!("消息 role 无效：{error}"))?;
        let content = deserialize_message_content(self.content, kind, &self.id, self.media)?;
        let phase = self.phase.as_deref().unwrap_or("normal");
        let from_worker = self.worker_id.is_some();
        let mut assistant_phase = None;
        let role = match kind {
            MessageRole::System => Role::System,
            MessageRole::User => Role::User {
                source: match phase {
                    "hostinjected" => UserSource::HostInjected,
                    "compressedresume" => UserSource::CompressedResume,
                    _ => UserSource::Human,
                },
                turn_status: self.turn_status,
                elapsed_ms: self.elapsed_ms,
                final_reply: None,
            },
            MessageRole::Assistant => {
                assistant_phase = Some(match phase {
                    "summary" => LegacyAssistantPhase::Summary,
                    "react" => LegacyAssistantPhase::React,
                    _ => LegacyAssistantPhase::Normal,
                });
                Role::Assistant {
                    reasoning_content: self.reasoning_content,
                    reasoning_signature: self.reasoning_signature,
                    tool_calls: self.tool_calls,
                    usage: self.usage,
                    reasoning_elapsed_ms: self.reasoning_elapsed_ms,
                    text_elapsed_ms: self.text_elapsed_ms,
                }
            }
            MessageRole::Tool => Role::Tool {
                tool_call_id: self.tool_call_id.unwrap_or_default(),
                tool_name: self.tool_name.unwrap_or_default(),
                is_error: self.tool_result_is_error,
                duration_ms: self.duration_ms,
            },
            MessageRole::Notice => Role::Notice { usage: self.usage },
        };
        let message = Message {
            id: self.id,
            created_at: self.created_at,
            role,
            content,
            meta: MessageMeta {
                compact: self.compact,
            },
        };
        Ok((
            message,
            Some(LegacyHint {
                assistant_phase,
                from_worker,
            }),
        ))
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = MessageRaw::deserialize(deserializer)?;
        raw.decode()
            .map(|(message, _)| message)
            .map_err(de::Error::custom)
    }
}

/// 会话消息列表的反序列化：逐条归位，并为旧格式补齐轮次级信息。
///
/// 旧格式以助手消息 `phase=summary` 标记最终答复（更早的会话没有 phase，
/// 以非过程的助手正文作最终答复）；新结构把最终答复记在本轮起轮的用户
/// 消息 `final_reply` 上。供 `#[serde(deserialize_with)]` 使用。
pub fn deserialize_messages<'de, D>(deserializer: D) -> Result<Vec<Message>, D::Error>
where
    D: Deserializer<'de>,
{
    let raws = Vec::<MessageRaw>::deserialize(deserializer)?;
    let mut messages = Vec::with_capacity(raws.len());
    let mut hints = Vec::with_capacity(raws.len());
    for raw in raws {
        let (message, hint) = raw.decode().map_err(de::Error::custom)?;
        messages.push(message);
        hints.push(hint);
    }
    assign_legacy_final_replies(&mut messages, &hints);
    Ok(messages)
}

fn assign_legacy_final_replies(messages: &mut [Message], hints: &[Option<LegacyHint>]) {
    fn commit(messages: &mut [Message], anchor: Option<usize>, reply: Option<String>) {
        if let (Some(anchor), Some(reply)) = (anchor, reply)
            && let Role::User { final_reply, .. } = &mut messages[anchor].role
            && final_reply.is_none()
        {
            *final_reply = Some(reply);
        }
    }

    let mut anchor = None;
    let mut summary = None;
    let mut normal = None;
    for index in 0..messages.len() {
        if messages[index].is_user_input() {
            commit(messages, anchor, summary.take().or(normal.take()));
            anchor = Some(index);
            continue;
        }
        let Some(hint) = hints[index] else {
            continue;
        };
        if hint.from_worker {
            continue;
        }
        match hint.assistant_phase {
            Some(LegacyAssistantPhase::Summary) => summary = Some(messages[index].id.clone()),
            Some(LegacyAssistantPhase::Normal)
                if messages[index].tool_calls().is_empty()
                    && !messages[index].text_content().trim().is_empty() =>
            {
                normal = Some(messages[index].id.clone());
            }
            _ => {}
        }
    }
    commit(messages, anchor, summary.or(normal));
}

/// 以旧的扁平格式序列化消息列表（插件会话快照边界使用）。
///
/// 已安装的旧版 WASM 插件按扁平格式解析 `PluginSession.messages`；新版插件
/// 的反序列化两种格式都接受。供 `#[serde(serialize_with)]` 使用。
pub fn serialize_messages_flat<S>(messages: &[Message], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeSeq;
    let mut seq = serializer.serialize_seq(Some(messages.len()))?;
    for message in messages {
        seq.serialize_element(&message.to_flat_value())?;
    }
    seq.end()
}

impl Message {
    /// 构造一条系统留痕消息：Notice 角色 + 统一的 `[类别] 内容` 形态。
    ///
    /// 供各类系统通知复用（上下文管理、插件管理等）：前端按 `[类别] `
    /// 前缀剥离后展示正文；Notice 角色本身被排除出模型上下文与压缩
    /// 范围，文案不影响 KV cache 前缀。
    pub fn notice(category: &str, content: impl std::fmt::Display) -> Self {
        Self::new(MessageRole::Notice, format!("[{category}] {content}"))
    }

    /// 以角色标签和文本构造消息（角色字段取空值）。
    pub fn new(kind: MessageRole, content: impl Into<String>) -> Self {
        Self::with_role(
            Role::from_kind(kind),
            vec![ContentBlock::text(content.into())],
        )
    }

    /// 以完整角色数据和内容块构造消息。
    pub fn with_role(role: Role, content: Vec<ContentBlock>) -> Self {
        Self {
            id: scru128::new().to_string(),
            created_at: now_text(),
            role,
            content,
            meta: MessageMeta::default(),
        }
    }

    /// 构造带推理内容的消息。`reasoning` 仅对 Assistant 生效。
    pub fn with_reasoning(
        kind: MessageRole,
        content: impl Into<String>,
        reasoning: impl Into<String>,
    ) -> Self {
        let mut message = Self::new(kind, content);
        if let Role::Assistant {
            reasoning_content, ..
        } = &mut message.role
        {
            *reasoning_content = reasoning.into();
        }
        message
    }

    /// 构造宿主准备好的用户消息（稳定 ID + 内容块）。
    pub fn user_prepared(id: impl Into<String>, content: Vec<ContentBlock>) -> Self {
        let mut message = Self::with_role(Role::user(), content);
        message.id = id.into();
        message
    }

    /// 构造 Tool 结果消息，一次性写入 tool 专属字段。
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::with_role(
            Role::Tool {
                tool_call_id: tool_call_id.into(),
                tool_name: tool_name.into(),
                is_error,
                duration_ms: None,
            },
            vec![ContentBlock::text(content.into())],
        )
    }

    /// 设置用户消息来源（链式调用）；非用户消息不受影响。
    pub fn with_source(mut self, source: UserSource) -> Self {
        if let Role::User { source: slot, .. } = &mut self.role {
            *slot = source;
        }
        self
    }

    /// 写入单次工具调用的执行耗时（链式调用）；非工具消息不受影响。
    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        if let Role::Tool {
            duration_ms: slot, ..
        } = &mut self.role
        {
            *slot = Some(duration_ms);
        }
        self
    }

    /// 写入助手发起的工具调用（链式调用）；非助手消息不受影响。
    pub fn with_tool_calls(mut self, calls: Vec<MessageToolCall>) -> Self {
        if let Role::Assistant { tool_calls, .. } = &mut self.role {
            *tool_calls = calls;
        }
        self
    }

    /// 在 User 消息上写入该轮次的执行时长与最终状态（turn 锚点）。
    /// 非 User 消息为空操作（debug 构建下触发断言）。
    pub fn set_turn_result(&mut self, elapsed: u64, status: TurnStatus) {
        debug_assert_eq!(
            self.role.kind(),
            MessageRole::User,
            "set_turn_result should only be called on user messages"
        );
        if let Role::User {
            turn_status,
            elapsed_ms,
            ..
        } = &mut self.role
        {
            *elapsed_ms = Some(elapsed);
            *turn_status = Some(status);
        }
    }

    /// 设置轮次状态（仅用户消息生效）。
    pub fn set_turn_status(&mut self, status: TurnStatus) {
        if let Role::User { turn_status, .. } = &mut self.role {
            *turn_status = Some(status);
        }
    }

    /// 设置本轮最终答复（仅用户锚点消息生效）。
    pub fn set_final_reply(&mut self, reply: Option<String>) {
        if let Role::User { final_reply, .. } = &mut self.role {
            *final_reply = reply;
        }
    }

    /// 记录模型调用用量（Assistant / Notice 生效）。
    pub fn set_usage(&mut self, record: crate::token::MessageUsage) {
        match &mut self.role {
            Role::Assistant { usage, .. } | Role::Notice { usage } => {
                *usage = Some(Box::new(record));
            }
            _ => {}
        }
    }

    // ── 只读访问：按角色取字段，其他角色返回空值 ──

    pub fn kind(&self) -> MessageRole {
        self.role.kind()
    }

    pub fn reasoning_content(&self) -> &str {
        match &self.role {
            Role::Assistant {
                reasoning_content, ..
            } => reasoning_content,
            _ => "",
        }
    }

    pub fn reasoning_signature(&self) -> Option<&str> {
        match &self.role {
            Role::Assistant {
                reasoning_signature,
                ..
            } => reasoning_signature.as_deref(),
            _ => None,
        }
    }

    pub fn tool_calls(&self) -> &[MessageToolCall] {
        match &self.role {
            Role::Assistant { tool_calls, .. } => tool_calls,
            _ => &[],
        }
    }

    /// 工具结果配对的调用 ID（无配对调用或非工具消息时为 None）。
    pub fn tool_call_id(&self) -> Option<&str> {
        match &self.role {
            Role::Tool { tool_call_id, .. } if !tool_call_id.is_empty() => Some(tool_call_id),
            _ => None,
        }
    }

    pub fn tool_name(&self) -> Option<&str> {
        match &self.role {
            Role::Tool { tool_name, .. } if !tool_name.is_empty() => Some(tool_name),
            _ => None,
        }
    }

    pub fn tool_is_error(&self) -> bool {
        matches!(self.role, Role::Tool { is_error: true, .. })
    }

    pub fn duration_ms(&self) -> Option<u64> {
        match &self.role {
            Role::Tool { duration_ms, .. } => *duration_ms,
            _ => None,
        }
    }

    pub fn usage(&self) -> Option<&crate::token::MessageUsage> {
        match &self.role {
            Role::Assistant { usage, .. } | Role::Notice { usage } => usage.as_deref(),
            _ => None,
        }
    }

    pub fn reasoning_elapsed_ms(&self) -> Option<u64> {
        match &self.role {
            Role::Assistant {
                reasoning_elapsed_ms,
                ..
            } => *reasoning_elapsed_ms,
            _ => None,
        }
    }

    pub fn text_elapsed_ms(&self) -> Option<u64> {
        match &self.role {
            Role::Assistant {
                text_elapsed_ms, ..
            } => *text_elapsed_ms,
            _ => None,
        }
    }

    pub fn turn_status(&self) -> Option<TurnStatus> {
        match &self.role {
            Role::User { turn_status, .. } => *turn_status,
            _ => None,
        }
    }

    pub fn elapsed_ms(&self) -> Option<u64> {
        match &self.role {
            Role::User { elapsed_ms, .. } => *elapsed_ms,
            _ => None,
        }
    }

    pub fn final_reply(&self) -> Option<&str> {
        match &self.role {
            Role::User { final_reply, .. } => final_reply.as_deref(),
            _ => None,
        }
    }

    /// 用户消息来源；非用户消息为 None。
    pub fn user_source(&self) -> Option<UserSource> {
        match &self.role {
            Role::User { source, .. } => Some(*source),
            _ => None,
        }
    }

    /// 是否为用户真实输入（轮次锚点候选）。
    pub fn is_user_input(&self) -> bool {
        self.user_source().is_some_and(UserSource::is_user_input)
    }

    /// 获取纯文本内容（拼接所有 Text 块）
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("")
    }

    /// 是否包含非文本内容块
    pub fn has_media(&self) -> bool {
        self.content.iter().any(|b| !b.is_text())
    }

    /// 提取稳定资源，保持 content block 原始顺序。
    pub fn extract_stored_assets(&self) -> Vec<StoredAsset> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Image { asset, .. } | ContentBlock::AssetReference { asset } => {
                    Some(asset.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// 从 content blocks 提取媒体资产（content blocks 是媒体的唯一真相源）。
    pub fn extract_media_assets(&self) -> Vec<MediaAsset> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Media {
                    kind,
                    url,
                    mime_type,
                    title,
                } => Some(MediaAsset {
                    kind: *kind,
                    url: url.clone(),
                    mime_type: mime_type.clone(),
                    title: title.clone(),
                    capability: None,
                }),
                ContentBlock::Image { asset, .. } | ContentBlock::AssetReference { asset } => {
                    Some(MediaAsset {
                        kind: asset.kind,
                        url: asset.local_path.clone(),
                        mime_type: Some(asset.mime_type.clone()),
                        title: Some(asset.original_name.clone()),
                        capability: None,
                    })
                }
                ContentBlock::Text { .. } | ContentBlock::ModelInstruction { .. } => None,
            })
            .collect()
    }

    /// 返回移除瞬时图片数据后的稳定消息副本。
    pub fn stable(&self) -> Self {
        let mut stable = self.clone();
        stable.clear_transient_data();
        stable
    }

    /// 清空当前消息中的瞬时图片数据。
    pub fn clear_transient_data(&mut self) {
        for block in &mut self.content {
            block.clear_transient_data();
        }
    }

    /// 旧的扁平 JSON 形态（插件会话快照兼容边界使用）。
    pub fn to_flat_value(&self) -> Value {
        let mut value = serde_json::json!({
            "id": self.id,
            "role": self.role.kind(),
            "content": self.content,
            "created_at": self.created_at,
        });
        let object = value.as_object_mut().expect("json! 对象");
        let mut put = |key: &str, field: Value| {
            if !field.is_null() {
                object.insert(key.to_string(), field);
            }
        };
        let phase = match &self.role {
            Role::User { source, .. } => match source {
                UserSource::HostInjected => "hostinjected",
                UserSource::CompressedResume => "compressedresume",
                UserSource::Human => "normal",
            },
            Role::Assistant { .. } | Role::Tool { .. } => "react",
            Role::System | Role::Notice { .. } => "normal",
        };
        put("phase", Value::from(phase));
        if self.meta.compact {
            put("compact", Value::Bool(true));
        }
        match &self.role {
            Role::System => {}
            Role::User {
                turn_status,
                elapsed_ms,
                ..
            } => {
                put(
                    "turn_status",
                    serde_json::to_value(turn_status).unwrap_or_default(),
                );
                put(
                    "elapsed_ms",
                    serde_json::to_value(elapsed_ms).unwrap_or_default(),
                );
            }
            Role::Assistant {
                reasoning_content,
                reasoning_signature,
                tool_calls,
                usage,
                reasoning_elapsed_ms,
                text_elapsed_ms,
            } => {
                put("reasoning_content", Value::from(reasoning_content.as_str()));
                put(
                    "reasoning_signature",
                    serde_json::to_value(reasoning_signature).unwrap_or_default(),
                );
                if !tool_calls.is_empty() {
                    put(
                        "tool_calls",
                        serde_json::to_value(tool_calls).unwrap_or_default(),
                    );
                }
                put("usage", serde_json::to_value(usage).unwrap_or_default());
                put(
                    "reasoning_elapsed_ms",
                    serde_json::to_value(reasoning_elapsed_ms).unwrap_or_default(),
                );
                put(
                    "text_elapsed_ms",
                    serde_json::to_value(text_elapsed_ms).unwrap_or_default(),
                );
            }
            Role::Tool {
                tool_call_id,
                tool_name,
                is_error,
                duration_ms,
            } => {
                if !tool_call_id.is_empty() {
                    put("tool_call_id", Value::from(tool_call_id.as_str()));
                }
                if !tool_name.is_empty() {
                    put("tool_name", Value::from(tool_name.as_str()));
                }
                if *is_error {
                    put("tool_result_is_error", Value::Bool(true));
                }
                put(
                    "duration_ms",
                    serde_json::to_value(duration_ms).unwrap_or_default(),
                );
            }
            Role::Notice { usage } => {
                put("usage", serde_json::to_value(usage).unwrap_or_default());
            }
        }
        value
    }
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// 当前本地时间文本
pub fn now_text() -> String {
    chrono::Local::now().naive_local().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 系统留痕统一形态：`[类别] 内容` + Notice 角色。
    #[test]
    fn notice构造统一类别前缀() {
        let message = Message::notice("上下文管理", "已切换模型：test-model");
        assert_eq!(message.role, MessageRole::Notice);
        assert_eq!(
            message.text_content(),
            "[上下文管理] 已切换模型：test-model"
        );
        // 其他类别同一形态，适配后续不同类型的系统通知。
        let other = Message::notice("插件管理", "已安装 demo");
        assert_eq!(other.text_content(), "[插件管理] 已安装 demo");
    }
}
