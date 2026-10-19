use serde::{Deserialize, Serialize};
use tiangong_types::{MediaAsset, MessageContent};

/// 外部 Bot / Connector 消息入口请求体
#[derive(Debug, Deserialize)]
pub struct ConnectorMessageRequest {
    /// Connector 名称，如 feishu-bot、telegram、webhook
    #[serde(default)]
    pub connector: Option<String>,
    /// 外部通道 ID，如飞书 chat_id
    pub channel_id: String,
    /// 外部消息 ID，不传时由 Server 生成
    #[serde(default)]
    pub message_id: Option<String>,
    /// 用户正文，只发附件时可省略或为空
    #[serde(default)]
    pub message: Option<String>,
    /// 已废弃：旧的单一类型正文，服务端在入口转为 `message` + `media`。
    /// 新调用方请只用 `message` + `media`；确认无外部调用方后删除（issue #689）。
    #[deprecated(note = "改用 message + media")]
    #[serde(default)]
    pub content: Option<ApiMessageContent>,
    /// 全部附件（图片、视频、音频、文件），按发送顺序排列
    #[serde(default)]
    pub media: Vec<MediaAsset>,
    /// 只给模型看的附加指令（保存为模型指令块，界面不显示），如 Bot 回复通道说明。
    #[serde(default)]
    pub instruction: Option<String>,
    /// 插件渲染声明（仅界面使用，不进模型请求）：由 `plugin` 的 `view` 贡献接管该消息的显示。
    #[serde(default)]
    pub render: Option<tiangong_types::MessageRender>,
    /// 目标会话尚不存在时新建会话使用的工作目录（绝对路径）；已有会话忽略。
    #[serde(default)]
    pub workspace: Option<String>,
}

/// 入口校验后的待投递消息（`/api/v1/messages` 内部使用）。
pub struct ConnectorMessage {
    pub connector: String,
    pub channel_id: String,
    pub message_id: String,
    /// 正文，可为空（只发附件时）
    pub text: String,
    /// 全部附件，按发送顺序排列
    pub media: Vec<MediaAsset>,
    pub annotations: tiangong_types::MessageAnnotations,
}

/// 外部 Bot / Connector 消息入口响应体
#[derive(Debug, Serialize)]
pub struct ConnectorMessageResponse {
    pub session_id: String,
    pub connector: String,
    pub channel_id: String,
    pub reply_to: Option<String>,
    pub message: String,
    pub content: ApiMessageContent,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ApiMessageContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiMessageContent {
    Text {
        text: String,
    },
    Image {
        url: String,
        #[serde(default)]
        caption: Option<String>,
    },
    File {
        url: String,
        name: String,
    },
    Audio {
        url: String,
        #[serde(default)]
        duration: Option<u32>,
    },
    Video {
        url: String,
        #[serde(default)]
        caption: Option<String>,
    },
}

impl ApiMessageContent {
    pub fn text(&self) -> String {
        match self {
            Self::Text { text } => text.clone(),
            Self::Image { caption, .. } | Self::Video { caption, .. } => {
                caption.clone().unwrap_or_default()
            }
            Self::File { .. } | Self::Audio { .. } => String::new(),
        }
    }
}

impl From<MessageContent> for ApiMessageContent {
    fn from(value: MessageContent) -> Self {
        match value {
            MessageContent::Text(text) => Self::Text { text },
            MessageContent::Image { url, caption } => Self::Image { url, caption },
            MessageContent::File { url, name } => Self::File { url, name },
            MessageContent::Audio { url, duration } => Self::Audio { url, duration },
            MessageContent::Video { url, caption } => Self::Video { url, caption },
        }
    }
}

/// 会话摘要（列表项）
#[derive(Debug, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub message_count: usize,
    pub created_at: String,
    pub updated_at: String,
}

/// 消息概要
#[derive(Debug, Serialize)]
pub struct MessageSummary {
    pub id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
}
