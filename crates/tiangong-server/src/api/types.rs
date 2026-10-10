use serde::{Deserialize, Serialize};
use tiangong_types::{MediaAsset, MessageContent};

/// POST /api/v1/chat 请求体
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// 可选的会话 ID，为空时使用当前活跃会话
    pub session_id: Option<String>,
    /// 用户消息内容
    pub message: String,
}

/// POST /api/v1/chat 响应体
#[derive(Debug, Serialize)]
pub struct ChatResponse {
    pub session_id: String,
    pub response: String,
}

/// 外部 Bot / Connector 消息入口请求体
#[derive(Debug, Deserialize)]
pub struct ConnectorMessageRequest {
    /// Connector 名称，如 feishu-bot、telegram、webhook
    #[serde(default)]
    pub connector: Option<String>,
    /// 外部通道 ID，如飞书 chat_id
    pub channel_id: String,
    /// 外部发送者 ID
    #[serde(default)]
    pub sender_id: Option<String>,
    /// 外部消息 ID，不传时由 Server 生成
    #[serde(default)]
    pub message_id: Option<String>,
    /// 文本消息快捷字段
    #[serde(default)]
    pub message: Option<String>,
    /// 结构化消息内容
    #[serde(default)]
    pub content: Option<ApiMessageContent>,
    /// 附加媒体资源（图片、视频等）
    #[serde(default)]
    pub media: Vec<MediaAsset>,
    #[serde(default)]
    pub reply_to: Option<String>,
    /// 只给模型看的附加指令（保存为模型指令块，界面不显示），如 Bot 回复通道说明。
    #[serde(default)]
    pub instruction: Option<String>,
    /// 插件渲染声明（仅界面使用，不进模型请求）：由 `plugin` 的 `view` 贡献接管该消息的显示。
    #[serde(default)]
    pub render: Option<tiangong_types::MessageRender>,
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

impl From<ApiMessageContent> for MessageContent {
    fn from(value: ApiMessageContent) -> Self {
        match value {
            ApiMessageContent::Text { text } => Self::Text(text),
            ApiMessageContent::Image { url, caption } => Self::Image { url, caption },
            ApiMessageContent::File { url, name } => Self::File { url, name },
            ApiMessageContent::Audio { url, duration } => Self::Audio { url, duration },
            ApiMessageContent::Video { url, caption } => Self::Video { url, caption },
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
