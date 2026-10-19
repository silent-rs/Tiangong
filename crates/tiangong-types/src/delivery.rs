//! 外部消息投递：随用户消息附带的标注，以及 Core 回复的结构化内容。

/// 用户消息正文之外的附加内容。
///
/// - `instruction`：只给模型看的文本，保存为 `ContentBlock::ModelInstruction`，
///   界面不显示（如 Bot 回复通道说明）；
/// - `render`：只给界面用的插件渲染声明（见 [`crate::MessageMeta::render`]），
///   不进模型请求；
/// - `workspace`：目标会话尚不存在时，新建会话使用的工作目录（绝对路径）；
///   已有会话忽略（会话工作目录在有对话后不再变更）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageAnnotations {
    pub instruction: Option<String>,
    pub render: Option<crate::MessageRender>,
    pub workspace: Option<String>,
}

impl MessageAnnotations {
    /// 规范化并校验：空白指令/工作区视为未设置；`render` 按
    /// [`crate::MessageRender::validate`] 校验；工作区须为绝对路径（是否为
    /// 实际目录由宿主在新建会话时校验）。
    pub fn normalized(self) -> Result<Self, String> {
        let instruction = self
            .instruction
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        if let Some(render) = &self.render {
            render.validate()?;
        }
        let workspace = self
            .workspace
            .map(|path| path.trim().to_string())
            .filter(|path| !path.is_empty());
        if let Some(path) = &workspace
            && !std::path::Path::new(path).is_absolute()
        {
            return Err(format!("workspace 必须是绝对路径：{path}"));
        }
        Ok(Self {
            instruction,
            render: self.render,
            workspace,
        })
    }

    /// 把模型专用指令追加到已准备好的内容块末尾。
    pub fn append_instruction(&self, content: &mut Vec<crate::ContentBlock>) {
        if let Some(instruction) = &self.instruction {
            content.push(crate::ContentBlock::model_instruction(instruction.clone()));
        }
    }
}

/// Core 对一条外部消息的回复。
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    pub content: MessageContent,
    /// 同一回复中的其余结构化内容；保留旧 content 作为首项以兼容现有消费者。
    pub attachments: Vec<MessageContent>,
}

/// 回复中的一项结构化内容。
#[derive(Debug, Clone)]
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
