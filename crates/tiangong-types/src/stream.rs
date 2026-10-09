//! 统一的外部输出流事件

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// 外部输出流事件
///
/// CLI / GUI / Server / Connector 统一消费此类型。
/// 使用 serde tag 序列化，前端可直接用 event.type 判断类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Core 内部流屏障；仅用于统一终态提交，不会转发给外部消费者。
    #[serde(skip)]
    TurnBoundary { boundary_id: u64 },
    /// 文本增量（assistant 回复内容）
    Delta {
        /// 所属消息 ID（前端据此组装到正确的消息）
        message_id: String,
        content: String,
    },
    /// ReAct 工具执行阶段的过程性文本增量（前端紧凑展示，不提供复制按钮）
    ReactText { message_id: String, content: String },
    /// 总结阶段的最终回复文本增量（前端作为主消息展示，提供复制按钮）
    SummaryText { message_id: String, content: String },
    /// 单个 turn 的执行阶段切换通知
    PhaseChanged {
        /// 阶段名："tool_execution" / "summary"
        phase: String,
        /// 第几次外层循环（从 1 开始）
        iteration: u32,
    },
    /// 当前 turn 已运行的整秒数，仅用于实时展示，不进入 Session。
    TurnElapsed { seconds: u64 },
    /// 思考过程增量
    Reasoning {
        /// 所属消息 ID
        message_id: String,
        content: String,
    },
    /// 工具开始执行
    ToolStart { name: String, args_summary: String },
    /// 单个工具任务已执行完成（结果尚未提交）。
    ///
    /// 同批工具结果按模型声明顺序提交（`ToolResult`），先完成的后序工具需等待
    /// 前序工具。本事件在任务完成的瞬间发出，供界面停止该调用的运行计时并
    /// 展示真实耗时；完整结果仍以随后的 `ToolResult` 为准，不进入 Session。
    ToolFinished {
        tool_call_id: String,
        name: String,
        ok: bool,
        /// 工具执行耗时（毫秒），与随后 `ToolResult.duration_ms` 一致。
        duration_ms: u64,
    },
    /// 工具执行结果
    ToolResult {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        ok: bool,
        /// 给 UI/外部消费者展示的输出，可能被截断。
        output: String,
        /// Rust 内部落盘使用的完整输出，不序列化给前端或远端消费者。
        #[serde(default, skip)]
        full_output: Option<String>,
        /// 工具执行耗时（毫秒）。前端据此展示真实耗时，不再把后续模型等待算到工具上。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// LLM 决定调用工具
    ToolCalls {
        message_id: String,
        names: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<StreamToolCall>,
        /// 本次 LLM 调用的 token 用量
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<crate::TokenUsage>,
    },
    /// 单次 LLM 请求的 token 用量。
    ///
    /// 与 ToolCalls/Done 的聚合 usage 不同，此事件表示一次实际 LLM 请求，
    /// 供 GUI / Server 进行精确累计与上下文压缩进度展示。
    TokenUsage {
        usage: crate::TokenUsage,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_tokens: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        compression_threshold_tokens: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        context_limit_tokens: Option<usize>,
        source: String,
        /// 归属 agent ID，None 表示主对话
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
    /// 本轮完成
    Done {
        /// 本轮累计 token 用量
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<crate::TokenUsage>,
    },
    /// 执行出错
    Error { message: String },
    /// LLM 请求重试中
    Retry {
        message: String,
        attempt: u32,
        max_attempts: u32,
    },

    /// 用户消息（Core 收到用户输入后回传，供前端统一渲染）
    UserMessage {
        /// 该用户消息在 session 中的 ID
        message_id: String,
        content: String,
        /// 宿主准备完成并由 Core 原样保存的稳定内容块。
        /// `Image.data` 始终跳过序列化，不会进入事件负载。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content_blocks: Vec<crate::ContentBlock>,
        /// 旧版消费者兼容字段；仅包含稳定媒体引用，不携带运行时 base64。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        media: Vec<crate::MediaAsset>,
        /// 插件渲染声明（见 [`crate::MessageMeta::render`]）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        render: Option<crate::MessageRender>,
    },
    /// Core 会话中的稳定消息快照，供宿主按 ID 更新本地镜像。
    SessionMessageUpsert {
        message: crate::Message,
        /// 与消息快照同一原子状态变更中的延迟注入列表；None 表示不修改。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deferred_tool_injections: Option<Vec<crate::DeferredToolInjection>>,
    },
    /// 尚未到达安全注入边界的外部工具内容快照。
    DeferredToolInjectionsChanged {
        injections: Vec<crate::DeferredToolInjection>,
    },

    // ===== 记忆检索事件 =====
    /// 记忆检索开始
    MemoryRecallStart {
        /// 检索策略描述（如 "keyword" / "semantic" / "hybrid:0.6" / "skip"）
        strategy: String,
    },
    /// 记忆检索进度更新
    MemoryRecallProgress {
        /// 当前阶段描述
        phase: String,
    },
    /// 记忆检索完成
    MemoryRecallDone {
        /// 命中条数
        hit_count: usize,
        /// 命中摘要列表（每项为 "标题: 摘要"）
        hits: Vec<MemoryRecallHitSummary>,
    },

    // ===== 多智能体团队事件 =====
    /// Agent 创建
    AgentCreated {
        agent_id: String,
        role: String,
        label: String,
    },
    /// Agent 状态变更
    AgentStatusChanged {
        agent_id: String,
        label: String,
        status: String,
    },
    /// Agent 向用户直接推送的通知
    AgentNotification {
        agent_id: String,
        agent_label: String,
        content: String,
        level: String,
    },
    /// Agent 间消息
    AgentMessage {
        from_agent_id: String,
        from_agent_label: String,
        to_agent_id: String,
        to_agent_label: String,
        content: String,
    },
    /// 文件锁变更
    FileLockChanged {
        path: String,
        holder_agent_id: Option<String>,
        holder_agent_label: Option<String>,
        action: String,
    },
    /// 上下文压缩已开始
    ContextCompressing {
        summary_up_to: usize,
        total_messages: usize,
    },
    /// 上下文已压缩/清理
    ContextCompressed {
        action: ContextCompressAction,
        summary_up_to: usize,
        remaining_messages: usize,
    },
    /// 索引扫描状态
    IndexStatus {
        phase: String,
        #[serde(default)]
        count: usize,
    },
    /// 会话标题变更（标题生成完成 / 用户编辑）。消费线程据此 emit sessions_updated。
    TitleChanged { title: String },
    /// 模型切换已开始（切换前的上下文整理已完成）。
    ModelSwitchStarted {
        /// 目标模型名（供 UI 展示）。
        model_name: String,
    },
    /// 模型已切换：此后的请求使用新模型。
    ModelSwitched {
        /// 已生效的模型名（供 UI 展示）。
        model_name: String,
    },
}

/// 上下文压缩/清理操作类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompressAction {
    /// 手动压缩
    Compress,
    /// 无需压缩
    Noop,
    /// 清理上下文
    Clear,
    /// 自动压缩
    Auto,
    /// 压缩失败
    Failed,
    /// 压缩被取消
    Cancelled,
}

impl ContextCompressAction {
    /// 中文显示文案
    pub fn display_text(&self) -> &'static str {
        match self {
            Self::Compress => "压缩",
            Self::Noop => "无需压缩",
            Self::Clear => "清理",
            Self::Auto => "自动压缩",
            Self::Failed => "失败",
            Self::Cancelled => "取消",
        }
    }
}

/// 记忆检索命中项摘要（前端展示用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecallHitSummary {
    pub title: String,
    pub summary: String,
    pub score: f64,
}

/// 带会话标识的流事件
///
/// Core 输出的所有事件都携带 session_id，
/// 消费端（GUI / CLI / Server）可据此路由到正确的会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    /// 产生该事件的会话 ID
    pub session_id: String,
    /// 原始流事件
    pub event: StreamEvent,
}
