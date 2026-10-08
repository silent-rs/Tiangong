//! 一轮对话的执行上下文。
//!
//! [`TurnContext`] 的生命周期严格限制为单个 turn：收到 Message 时由 typed builder
//! 构造,turn 结束后整体销毁。它持有 turn 执行所需的 client / 权限 / 工具 / 用量收集器。
//!
//! 与 `react/` 模块的关系:`TurnContext` 是被 react 层消费的能力集合,本身不属于
//! ReAct 执行流程。`react/execute.rs` 通过独立的 `execute_turn` 函数消费本结构。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use crate::config::agent::AgentConfig;
use crate::core::plugin::Plugin;
use crate::session::Session;
use crate::tools::extension::ToolOverrideHandler;
use tiangong_llm::SingleProviderClient;
use tiangong_llm::tool::ToolSpec;
use tiangong_types::StreamEvent;

use typed_builder::TypedBuilder;

/// 一轮对话的执行上下文（替代原 ReactEngine + RuntimeEngine）。
///
/// 生命周期严格限制为单个 turn：收到 Message 时构造,
/// turn 结束后整体销毁。不跨 turn 复用。
#[derive(TypedBuilder)]
#[builder(
    builder_method(vis = "pub(crate)"),
    builder_type(vis = "pub(crate)"),
    build_method(vis = "pub(crate)")
)]
pub struct TurnContext {
    /// 本轮发起时的用户消息 ID（起轮时即已知）：用量归属与插件本轮快照的
    /// 起点，运行中追加要求不改变。维护任务（手动压缩）没有用户消息，为 None。
    #[builder(default)]
    pub(crate) turn_id: Option<String>,
    /// 本轮当前要回应的用户消息 ID：起轮时同 `turn_id`，运行中注入引导消息后
    /// 改为该消息（ALR-107：最终 turn_status/elapsed_ms 写在它上面）。
    #[builder(default)]
    pub(crate) user_message_id: Option<String>,
    /// 模型请求客户端
    pub client: SingleProviderClient,
    /// 本轮会话（turn 期间独占,turn 结束时取回落盘）
    pub session: Session,
    /// 本轮内部事件发送端。
    pub stream_tx: Sender<StreamEvent>,
    /// 本轮使用的插件及生命周期钩子。
    pub plugins: Vec<Arc<dyn Plugin>>,
    /// 上下文 token 上限
    pub context_limit: usize,
    /// Agent 配置（reasoning_effort 等）
    pub agent_config: AgentConfig,
    /// 会话信任模式（供插件按自身策略使用）。
    pub trust_mode: crate::permission::TrustMode,
    /// 观测器（审计日志写入,持有 storage_root）
    pub observer: crate::observe::Observer,
    /// 构建前收集完成的工具覆盖处理器。
    pub(crate) tool_overrides: HashMap<String, Arc<dyn ToolOverrideHandler>>,
    // ===== turn 级配置 =====
    /// 当前执行单元可用的工具集
    pub tools: Vec<ToolSpec>,
}

impl TurnContext {
    /// 以已保存的用户消息发起本轮。
    pub(crate) fn begin_user_turn(&mut self, message_id: String) {
        self.user_message_id = Some(message_id.clone());
        self.turn_id = Some(message_id);
    }

    // ===== 能力 accessor =====

    pub fn client(&self) -> &SingleProviderClient {
        &self.client
    }

    pub fn agent_config(&self) -> &AgentConfig {
        &self.agent_config
    }
}
