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

/// 消息角色
///
/// [`Notice`](MessageRole::Notice) 是系统发给用户的通知（如轮次失败原因），
/// 仅前端可见：按角色在上下文构建、压缩与 provider 转换处整体排除，
/// 不进模型上下文，也不与 [`System`](MessageRole::System)（系统提示通道）混用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
    Notice,
}

/// 单个对话轮次的最终执行状态，仅持久化到用户消息（turn 锚点）。
///
/// 向后兼容：旧 session 反序列化时缺失该字段默认为 None，前端不展示状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnStatus {
    /// 正常完成（含总结阶段产出最终回复）。
    Success,
    /// 执行过程中出错。
    Failed,
    /// 用户主动取消。
    Cancelled,
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

/// 消息所属的执行阶段。
///
/// 用于前端区分 ReAct 工具执行阶段的过程消息与总结阶段的最终回复，
/// 实现消息分层展示。向后兼容：旧 session 缺失该字段时默认为 `Normal`。
///
/// 前向兼容：未知阶段值（更高版本写入的新变体）降级为 `Normal`，
/// 而不是反序列化失败导致整个会话无法打开（曾发生于：开发版会话
/// 携带 `hostinjected` 消息，正式版无该变体，会话打不开）。序列化
/// 仍由 derive 生成小写标签；`Deserialize` 为手写实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MessagePhase {
    /// 默认值：旧消息或未标记阶段的消息；也是未知阶段值的降级目标。
    #[default]
    Normal,
    /// ReAct 工具执行阶段的消息（工具调用、工具结果、过程文本）。
    React,
    /// 总结阶段的最终回复（可复制）。
    Summary,
    /// 压缩后注入的「当前任务状态」合成消息。
    ///
    /// 上下文压缩隐藏早期模型上下文后，为避免正在进行的 turn 失忆，
    /// 由 core 持久化一条承载「最近用户提问 + 已完成结果 + 进行中工具」
    /// 的 User 消息。该消息始终发送给模型，前端不展示、搜索或编辑。
    CompressedResume,
    /// 宿主注入的媒体消息（RFC 0017）。
    ///
    /// 工具产物图片等宿主注入内容：以原生视觉部件发送给模型（像素直达，
    /// 而非文本提及路径），前端把它作为助手轮次内的过程片段在 assistant
    /// 侧展示（不进搜索、不可编辑），会话检查器可审计。它不是用户
    /// 意图，不作轮次锚点；与 CompressedResume 一样随压缩边界降级，
    /// 但不要求「始终发送」。
    HostInjected,
}

impl<'de> Deserialize<'de> for MessagePhase {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let phase = String::deserialize(deserializer)?;
        Ok(match phase.as_str() {
            "react" => Self::React,
            "summary" => Self::Summary,
            "compressedresume" => Self::CompressedResume,
            "hostinjected" => Self::HostInjected,
            _ => Self::Normal,
        })
    }
}

impl MessagePhase {
    /// 是否为用户真实输入。
    ///
    /// 宿主注入的 role=User 消息（图片注入 `HostInjected`、压缩恢复锚点
    /// `CompressedResume`）是模型上下文的载体，不代表一次用户意图，
    /// 不得作为轮次锚点（`elapsed_ms` / `turn_status` 的落点）。
    /// UI 可见性与本判据正交：React/Summary 过程消息属于用户发起的
    /// 轮次，仍视为真实输入。
    pub fn is_user_input(&self) -> bool {
        !matches!(self, Self::HostInjected | Self::CompressedResume)
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

/// 对话消息
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: String,
    pub role: MessageRole,
    /// 消息内容，支持文本、图片、视频、音频、文件等多种类型混合排列。
    /// 向后兼容：旧格式 content 为 String 时自动包装为 `vec![ContentBlock::Text(string)]`。
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub reasoning_content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Box<crate::token::MessageUsage>>,
    /// 多 Worker 模式下标识消息所属 Worker
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<MessageToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub tool_result_is_error: bool,
    /// 表示从当前消息及以前的历史已被压缩摘要覆盖。
    #[serde(default, skip_serializing_if = "is_false")]
    pub compact: bool,
    /// 消息所属的执行阶段，用于前端消息分层展示。
    #[serde(default)]
    pub phase: MessagePhase,
    pub created_at: String,
    /// 该用户消息所属轮次的执行时长（毫秒）。仅持久化到用户消息（turn 锚点），
    /// 前端据此展示「执行总时长」，历史会话重新打开同样可见。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// 该轮次的最终状态。仅持久化到用户消息，便于前端直观区分成功/失败/取消。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_status: Option<TurnStatus>,
    /// 本次模型输出中思考（reasoning）阶段的耗时（毫秒）：首个 reasoning 增量
    /// 到最后一个增量的时长。仅持久化到 assistant 消息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_elapsed_ms: Option<u64>,
    /// 本次模型输出中正文生成阶段的耗时（毫秒）：首个文本增量到最后一个增量
    /// 的时长。仅持久化到 assistant 消息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_elapsed_ms: Option<u64>,
    /// 单次工具调用的执行耗时（毫秒）。仅持久化到 tool 结果消息，
    /// 历史 tool_calls 调用耗时展示；未测量（拒绝/重复跳过等）为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
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

/// 向后兼容的 Message 反序列化：
/// - content 同时支持旧 String 格式与新 content blocks 数组（见 deserialize_content）；
/// - 旧 session 的顶层 `media` 数组在反序列化时直接并入 content 末尾（不再保留为独立字段）。
impl<'de> Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // 与 origin/main 的 derive 行为保持一致：id/role/content/created_at 为必填，
        // 缺失时反序列化失败（而非静默生成空消息）。其余字段按需 default。
        #[derive(Deserialize)]
        struct MessageRaw {
            id: String,
            role: MessageRole,
            content: Value,
            #[serde(default)]
            reasoning_content: String,
            reasoning_signature: Option<String>,
            usage: Option<Box<crate::token::MessageUsage>>,
            worker_id: Option<String>,
            /// 旧格式 media 字段：反序列化时捕获，随即并入 content，不保留为结构字段。
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
            #[serde(default)]
            phase: MessagePhase,
            created_at: String,
            elapsed_ms: Option<u64>,
            turn_status: Option<TurnStatus>,
            #[serde(default)]
            reasoning_elapsed_ms: Option<u64>,
            #[serde(default)]
            text_elapsed_ms: Option<u64>,
            #[serde(default)]
            duration_ms: Option<u64>,
        }

        let raw = MessageRaw::deserialize(deserializer)?;
        let content = deserialize_message_content(raw.content, raw.role, &raw.id, raw.media)
            .map_err(de::Error::custom)?;

        Ok(Message {
            id: raw.id,
            role: raw.role,
            content,
            reasoning_content: raw.reasoning_content,
            reasoning_signature: raw.reasoning_signature,
            usage: raw.usage,
            worker_id: raw.worker_id,
            tool_calls: raw.tool_calls,
            tool_call_id: raw.tool_call_id,
            tool_name: raw.tool_name,
            tool_result_is_error: raw.tool_result_is_error,
            compact: raw.compact,
            phase: raw.phase,
            created_at: raw.created_at,
            elapsed_ms: raw.elapsed_ms,
            turn_status: raw.turn_status,
            reasoning_elapsed_ms: raw.reasoning_elapsed_ms,
            text_elapsed_ms: raw.text_elapsed_ms,
            duration_ms: raw.duration_ms,
        })
    }
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

    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            id: scru128::new().to_string(),
            role,
            content: vec![ContentBlock::text(content.into())],
            reasoning_content: String::new(),
            reasoning_signature: None,
            usage: None,
            worker_id: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_result_is_error: false,
            compact: false,
            phase: MessagePhase::Normal,
            created_at: now_text(),
            elapsed_ms: None,
            turn_status: None,
            reasoning_elapsed_ms: None,
            text_elapsed_ms: None,
            duration_ms: None,
        }
    }

    /// 构造带推理内容的消息。`reasoning` 通常仅对 Assistant 有意义。
    pub fn with_reasoning(
        role: MessageRole,
        content: impl Into<String>,
        reasoning: impl Into<String>,
    ) -> Self {
        Self {
            id: scru128::new().to_string(),
            role,
            content: vec![ContentBlock::text(content.into())],
            reasoning_content: reasoning.into(),
            reasoning_signature: None,
            usage: None,
            worker_id: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_result_is_error: false,
            compact: false,
            phase: MessagePhase::Normal,
            created_at: now_text(),
            elapsed_ms: None,
            turn_status: None,
            reasoning_elapsed_ms: None,
            text_elapsed_ms: None,
            duration_ms: None,
        }
    }

    /// 设置消息的执行阶段标记（链式调用）。
    pub fn with_phase(mut self, phase: MessagePhase) -> Self {
        self.phase = phase;
        self
    }

    /// 写入单次工具调用的执行耗时（链式调用）。仅对 tool 结果消息有意义。
    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    // ── 语义构造器：按角色表达专属字段，减少非法组合 ──

    /// 构造 Tool 结果消息，一次性写入 tool 专属字段。
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        let mut msg = Self::new(MessageRole::Tool, content);
        msg.tool_call_id = Some(tool_call_id.into());
        msg.tool_name = Some(tool_name.into());
        msg.tool_result_is_error = is_error;
        msg
    }

    /// 在 User 消息上写入该轮次的执行时长与最终状态（turn 锚点）。
    /// 仅 User 消息会写入；debug 构建下非 User 调用会触发断言，release 下为空操作。
    pub fn set_turn_result(&mut self, elapsed_ms: u64, status: TurnStatus) {
        debug_assert_eq!(
            self.role,
            MessageRole::User,
            "set_turn_result should only be called on user messages"
        );
        if self.role == MessageRole::User {
            self.elapsed_ms = Some(elapsed_ms);
            self.turn_status = Some(status);
        }
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
    ///
    /// `append_message_with_*_media` 把附件存进 `content` 的 `ContentBlock::Media`，
    /// 不再保留独立的 media 字段。任何需要 media 列表的地方都必须经此提取。
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
        for block in &mut stable.content {
            block.clear_transient_data();
        }
        stable
    }

    /// 清空当前消息中的瞬时图片数据。
    pub fn clear_transient_data(&mut self) {
        for block in &mut self.content {
            block.clear_transient_data();
        }
    }
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
