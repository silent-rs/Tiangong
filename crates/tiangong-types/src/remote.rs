use serde::{Deserialize, Serialize};

use crate::message::MediaAsset;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRole {
    #[default]
    Controller,
    Observer,
}

impl RemoteRole {
    pub fn can_send_message(&self) -> bool {
        matches!(self, Self::Controller)
    }

    pub fn can_manage_sessions(&self) -> bool {
        matches!(self, Self::Controller)
    }

    pub fn can_observe(&self) -> bool {
        true
    }

    pub fn can_cancel_task(&self) -> bool {
        matches!(self, Self::Controller)
    }

    pub fn display_name(&self) -> &str {
        match self {
            Self::Controller => "控制者",
            Self::Observer => "观察者",
        }
    }
}

impl std::fmt::Display for RemoteRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingMessage {
    pub id: String,
    pub connector: String,
    pub channel_id: String,
    pub sender_id: String,
    #[serde(default)]
    pub sender_role: RemoteRole,
    pub content: MessageContent,
    #[serde(default)]
    pub media: Vec<MediaAsset>,
    pub reply_to: Option<String>,
    pub timestamp: String,
    /// 正文之外的附加内容（模型专用指令、插件渲染声明）。
    #[serde(default, skip_serializing_if = "MessageAnnotations::is_empty")]
    pub annotations: MessageAnnotations,
}

/// 用户消息正文之外的附加内容。
///
/// - `instruction`：只给模型看的文本，保存为 `ContentBlock::ModelInstruction`，
///   界面不显示（如 Bot 回复通道说明）；
/// - `render`：只给界面用的插件渲染声明（见 [`crate::MessageMeta::render`]），
///   不进模型请求。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<crate::MessageRender>,
}

impl MessageAnnotations {
    pub fn is_empty(&self) -> bool {
        self.instruction.is_none() && self.render.is_none()
    }

    /// 规范化并校验：空白指令视为未设置；`render` 按 [`crate::MessageRender::validate`] 校验。
    pub fn normalized(self) -> Result<Self, String> {
        let instruction = self
            .instruction
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        if let Some(render) = &self.render {
            render.validate()?;
        }
        Ok(Self {
            instruction,
            render: self.render,
        })
    }

    /// 把模型专用指令追加到已准备好的内容块末尾。
    pub fn append_instruction(&self, content: &mut Vec<crate::ContentBlock>) {
        if let Some(instruction) = &self.instruction {
            content.push(crate::ContentBlock::model_instruction(instruction.clone()));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingMessage {
    pub content: MessageContent,
    /// 同一回复中的其余结构化内容；保留旧 content 作为首项以兼容现有消费者。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<MessageContent>,
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageContent {
    Text(String),
    Image {
        url: String,
        caption: Option<String>,
    },
    File {
        url: String,
        name: String,
    },
    Audio {
        url: String,
        duration: Option<u32>,
    },
    Video {
        url: String,
        caption: Option<String>,
    },
}
