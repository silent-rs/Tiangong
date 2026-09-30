//! 外部与 Agent 交互的统一通道。
//!
//! 所有外部输入（用户消息、终端操作、浏览器注入等）都经此 trait 投递。
//! `TiangongCore` 实现此 trait，内部转为 `Command` 发送到 worker 通道。
//!
//! 按与 Agent 交互的语义分为三层：
//! - [`AgentInputKind::Message`]：对话消息（触发 turn）
//! - [`AgentInputKind::Tool`]：工具类输入（伪造 tool result 注入对话，不触发 turn）
//! - [`AgentInputKind::Command`]：控制指令

/// 外部与 Agent 交互的统一通道。
pub trait AgentInput: Send + Sync {
    /// 投递一个外部输入到 Agent。
    ///
    /// 输入被接受后返回 `Ok(())`；需要活跃任务但当前任务已结束时返回
    /// [`crate::core::CoreError::WorkerStopped`]，空闲期维护操作遇到忙碌 Core 时返回
    /// [`crate::core::CoreError::Busy`]。
    fn deliver(&self, input: AgentInputKind) -> Result<(), crate::core::CoreError>;
}

/// 外部输入的顶层分类，按交互语义分三层。
pub enum AgentInputKind {
    /// 对话消息层：用户消息等，会触发 Agent 执行一轮 turn。
    Message(MessageInput),
    /// 工具类输入层：伪造 tool result 注入对话（不触发 turn）。
    /// 用 `Box<dyn ToolInput>` trait object：插件可定义 struct 实现类型安全的注入，
    /// 也可通过 `AgentInputKind::tool(name, json)` 匿名注入。
    /// 渲染由 core 的 render_tool_output 统一处理。
    Tool(Box<dyn ToolInput>),
    /// 控制层：控制指令。
    Command(CommandInput),
}

impl AgentInputKind {
    /// 便捷构造：工具类注入（tool_name + JSON payload），无需定义 struct 实现 ToolInput。
    ///
    /// 适合匿名/一次性注入。有复用需求的工具类型建议在插件侧定义 struct
    /// 实现 `ToolInput` trait（类型更安全、render 逻辑内聚）。
    pub fn tool(tool_name: impl Into<String>, payload: serde_json::Value) -> Self {
        struct AnonymousToolInput {
            name: String,
            payload: serde_json::Value,
        }
        impl ToolInput for AnonymousToolInput {
            fn tool_name(&self) -> &str {
                &self.name
            }
            fn render(&self) -> serde_json::Value {
                self.payload.clone()
            }
        }
        AgentInputKind::Tool(Box::new(AnonymousToolInput {
            name: tool_name.into(),
            payload,
        }))
    }

    /// 便捷构造：用户消息（触发 turn）。
    pub fn message(content: impl Into<String>) -> Self {
        AgentInputKind::Message(MessageInput::UserMessage {
            prepared: vec![tiangong_types::ContentBlock::text(content)],
            message_id: None,
            model_ref: None,
        })
    }

    /// 便捷构造：已由宿主入口准备完成的用户消息。
    pub fn prepared(prepared: Vec<tiangong_types::ContentBlock>) -> Self {
        AgentInputKind::Message(MessageInput::UserMessage {
            prepared,
            message_id: None,
            model_ref: None,
        })
    }

    /// 便捷构造：带稳定消息 ID 的 Prepared 用户消息。
    pub fn prepared_with_id(
        message_id: impl Into<String>,
        prepared: Vec<tiangong_types::ContentBlock>,
    ) -> Self {
        AgentInputKind::Message(MessageInput::UserMessage {
            prepared,
            message_id: Some(message_id.into()),
            model_ref: None,
        })
    }

    /// 为用户消息附加模型选择（models 注册表 key；`None` 跟随默认）。
    ///
    /// 宿主把用户在界面上选的模型附在消息上即可，解析与切换由
    /// `CoreManager` 在投递路径内部完成。非用户消息不受影响。
    pub fn with_model_ref(mut self, model_ref: Option<String>) -> Self {
        if let AgentInputKind::Message(MessageInput::UserMessage {
            model_ref: slot, ..
        }) = &mut self
        {
            *slot = model_ref.filter(|key| !key.trim().is_empty());
        }
        self
    }

    /// 便捷构造：取消当前执行（cancel_flag 由 deliver 内部设置）。
    pub fn cancel() -> Self {
        AgentInputKind::Command(CommandInput::Cancel)
    }

    /// 便捷构造：在 Core 空闲时手动触发上下文压缩。
    pub fn compress_context() -> Self {
        AgentInputKind::Command(CommandInput::CompressContext)
    }

    /// 便捷构造：在 Core 空闲时重置上下文。
    pub fn reset_context() -> Self {
        AgentInputKind::Command(CommandInput::ResetContext)
    }
}

// ===== Message 层 =====

/// 对话消息层输入。
pub enum MessageInput {
    /// 用户消息（触发 Agent 执行一轮 turn）。
    UserMessage {
        prepared: Vec<tiangong_types::ContentBlock>,
        /// 前端预生成的消息 ID（用于流式复用），None 则由后端生成。
        message_id: Option<String>,
        /// 本条消息使用的模型（models 注册表 key）。
        ///
        /// `None` = 跟随当前 Chat 默认。由 `CoreManager` 在投递前解析成实际
        /// 端点并按需切换——宿主只需把用户的选择附在消息上，不参与编排。
        model_ref: Option<String>,
    },
}

// ===== Command 层 =====

/// 控制层输入。
pub enum CommandInput {
    /// 取消当前执行。
    Cancel,
    /// 运行时更新信任模式(即时生效到活跃 turn task)。
    SetTrustMode(crate::permission::TrustMode),
    /// 手动触发上下文压缩（仅允许 Core 空闲时执行）。
    CompressContext,
    /// 清理上下文（仅允许 Core 空闲时执行，LLM 下次只看到 system prompt）。
    ResetContext,
}

/// 工具类注入的统一协议。
///
/// core 定义此 trait，各插件（浏览器、终端等）在自己的 crate 里实现它，
/// 通过 `AgentInputKind::Tool(Box::new(XxxInput { ... }))` 投递。
/// core 的 worker 侧统一调用 `tool_name` + `render` 生成伪造的 tool result 消息，
/// 新增工具类型只需在插件侧实现此 trait，无需改动 core。
///
/// 对于一次性/匿名场景，可用 `AgentInputKind::tool(name, json)` 便捷构造。
pub trait ToolInput: Send + Sync {
    /// 工具名（伪造 tool_call 的 name 字段）。
    fn tool_name(&self) -> &str;

    /// 注入到对话的结构化内容（JSON）。
    ///
    /// 返回 JSON 而非文本，让 worker 侧根据 tool_name 决定呈现格式，
    /// 同时保留结构化数据供去重等逻辑使用。
    fn render(&self) -> serde_json::Value;
}
