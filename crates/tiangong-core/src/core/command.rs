//! Agent 命令类型

/// 用户命令
pub enum Command {
    /// 取消当前执行
    Cancel,
    /// 运行时切换信任模式(即时生效到活跃 turn task)
    SetTrustMode(crate::permission::TrustMode),
    /// 运行时切换思考强度（下一次尚未发出的模型请求生效）。
    SetReasoningEffort(tiangong_llm::ReasoningEffort),
    /// 更新会话标题。
    SetTitle {
        title: String,
        only_if_default: bool,
    },
    /// 工具类内容自动注入（浏览器页面、终端用户操作等，不触发 turn）。
    InjectTool {
        tool_name: String,
        payload: serde_json::Value,
    },
    /// 运行中注入用户消息。
    InjectUserMessage {
        message_id: String,
        content: Vec<tiangong_types::ContentBlock>,
    },
    /// 插件投递的流事件。
    EmitStreamEvent(Box<tiangong_types::StreamEvent>),
    /// 插件内部模型调用产生的 token 用量。
    ReportUsage {
        usage: tiangong_types::TokenUsage,
        source: String,
        emit_event: bool,
    },
    /// 手动压缩上下文。
    CompressContext,
    /// 重置上下文。
    ResetContext,
    /// 关闭。
    Shutdown,
}
