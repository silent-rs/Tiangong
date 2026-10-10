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
use crate::session::{Message, MessageRole, Session};
use crate::tools::extension::ToolOverrideHandler;
use tiangong_llm::SingleProviderClient;
use tiangong_llm::tool::ToolSpec;
use tiangong_types::{StreamEvent, TurnStatus};

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
    /// 本轮的起轮用户消息 ID：用量归属、轮次状态与插件本轮快照的锚点，运行中
    /// 追加要求不改变。构建时从会话恢复（最近的起轮消息仍为 `Processing` 即
    /// 未收尾的轮次），否则为 None，由首条用户消息起轮时设置。
    #[builder(default)]
    pub(crate) turn_id: Option<String>,
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
    /// 校验并事务性写入 Core 已接收的用户消息，并维护本轮归属（`turn_id`）。
    ///
    /// 同 ID 的宿主镜像消息会先移除；只有完整 Session 成功落盘后才返回，失败时
    /// 恢复调用前的 Session 与 `turn_id`。轮次归属与消息同次落盘：
    /// - 本轮已确定（`turn_id` 已设置：执行中的引导，或构建时恢复的中断轮）：新
    ///   消息是引导消息、不带状态；本轮遗留的未完成工具调用先补齐失败结果（执行
    ///   中的引导已由中断收尾闭合，此处为空操作）；
    /// - 否则新消息起轮，状态为 `Processing`。
    ///
    /// 成功后向界面确认接收（用户消息事件）；界面按同一规则推导轮次状态。
    pub(crate) fn try_append_prepared_user_message(
        &mut self,
        mut message: Message,
    ) -> Result<(), String> {
        if message.kind() != MessageRole::User {
            return Err("只能追加用户消息".to_string());
        }
        tiangong_types::validate_ready_content_blocks(&message.content)?;
        if let Some(render) = &message.meta.render {
            render.validate()?;
        }
        let id = message.id.clone();
        let content = message.content.clone();
        let render = message.meta.render.clone();
        if self
            .session
            .messages
            .iter()
            .any(|message| message.id == id && message.role != MessageRole::User)
        {
            return Err(format!("消息 ID {id} 已被非用户消息占用"));
        }

        let before = (self.session.clone(), self.turn_id.clone());
        self.session
            .messages
            .retain(|message| message.id != id || message.role != MessageRole::User);
        let closed_tool_calls = if self.turn_id.is_some() {
            self.session
                .close_unfinished_tool_calls_with_reason("工具调用因执行意外中断，未完成。")
        } else {
            Vec::new()
        };
        if self.turn_id.is_none() {
            message.set_turn_status(TurnStatus::Processing);
            self.turn_id = Some(id.clone());
        }
        self.session.append_prepared_user_message(message);

        if let Err(error) = self.session.try_persist_to_disk() {
            (self.session, self.turn_id) = before;
            return Err(error);
        }

        for (tool_call_id, tool_name, output) in closed_tool_calls {
            let _ = self.stream_tx.send(StreamEvent::ToolResult {
                name: tool_name,
                tool_call_id: Some(tool_call_id),
                ok: false,
                output,
                full_output: None,
                duration_ms: None,
            });
        }
        let _ = self.stream_tx.send(StreamEvent::UserMessage {
            message_id: id,
            content: tiangong_types::content_blocks_text(&content),
            content_blocks: tiangong_types::stable_content_blocks(&content),
            media: Vec::new(),
            render,
        });
        Ok(())
    }

    // ===== 能力 accessor =====

    pub fn client(&self) -> &SingleProviderClient {
        &self.client
    }

    pub fn agent_config(&self) -> &AgentConfig {
        &self.agent_config
    }
}
