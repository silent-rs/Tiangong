use crate::session::{Message, MessageRole, Session, UserSource};
use tiangong_llm::tool::ToolSpec;
use tiangong_llm::{ModelRequest, ReasoningEffort, SingleProviderClient, StopReason};
use tiangong_types::TokenUsage;

/// 判断消息是否为可压缩的有效消息。
pub(crate) fn is_compressible(message: &Message) -> bool {
    message.role != MessageRole::System
        && message.role != MessageRole::Notice
        // CompressedResume 是压缩阶段生成的恢复锚点，不能再次纳入摘要；
        // HostInjected 只是轮次记账与展示位语义，仍属于模型上下文，
        // 必须可被压缩覆盖。
        && message.user_source() != Some(UserSource::CompressedResume)
}

/// 使用 Session 和客户端快照生成上下文摘要，不负责任务调度、持久化或通知。
pub struct ContextCompressor {
    session: Session,
    client: SingleProviderClient,
    tools: Vec<ToolSpec>,
    reasoning_effort: ReasoningEffort,
}

#[derive(Debug, Clone)]
pub struct CompressionUpdate {
    pub summary: String,
    pub usage: TokenUsage,
    pub previous_summary_up_to: usize,
    pub summary_up_to: usize,
    pub boundary_message_id: String,
}

#[derive(Debug, Clone)]
pub struct CompressionError {
    pub message: String,
    pub usage: TokenUsage,
}

impl CompressionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            usage: TokenUsage::default(),
        }
    }

    fn with_usage(message: impl Into<String>, usage: TokenUsage) -> Self {
        Self {
            message: message.into(),
            usage,
        }
    }
}

impl std::fmt::Display for CompressionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CompressionError {}

impl ContextCompressor {
    pub fn new(
        session: Session,
        client: SingleProviderClient,
        tools: Vec<ToolSpec>,
        reasoning_effort: ReasoningEffort,
    ) -> Self {
        Self {
            session,
            client,
            tools,
            reasoning_effort,
        }
    }

    pub fn has_pending_messages(&self) -> bool {
        self.session.messages[self.session.summary_up_to..]
            .iter()
            .any(is_compressible)
    }

    /// 压缩当前摘要边界到 `split_point` 之间的有效消息。
    pub async fn compress(
        self,
        split_point: usize,
        max_output_tokens: u32,
    ) -> std::result::Result<CompressionUpdate, CompressionError> {
        let previous_summary_up_to = self.session.summary_up_to;
        let split_point = split_point.min(self.session.messages.len());
        if split_point <= previous_summary_up_to {
            return Err(CompressionError::new(format!(
                "无可压缩消息：summary_up_to({previous_summary_up_to}) 已到达末尾({split_point})"
            )));
        }
        if !self.session.messages[previous_summary_up_to..split_point]
            .iter()
            .any(is_compressible)
        {
            return Err(CompressionError::new(format!(
                "待压缩范围 [{previous_summary_up_to}..{split_point}] 内无有效消息"
            )));
        }

        let boundary_message_id = self.session.messages[split_point - 1].id.clone();
        let request = Self::summary_request(
            &self.session,
            split_point,
            max_output_tokens,
            self.reasoning_effort,
        );
        // 工具声明与 tool_choice 保持与主循环一致（未指定 → 有工具即 auto）：
        // 多数 provider 的缓存键/对话模板包含 tool_choice，改为 none 会使
        // 整段前缀缓存失效。是否调用工具由压缩指令约束，返回工具调用时
        // 下方拒绝提交摘要。
        let response = self
            .client
            .complete_async(&request.with_tools(self.tools, None))
            .await
            .map_err(|error| CompressionError::new(error.to_string()))?;
        let usage = response.usage.clone();
        if !response.tool_calls.is_empty() || !response.invalid_tool_calls.is_empty() {
            return Err(CompressionError::with_usage(
                "上下文压缩返回了工具调用，拒绝提交摘要",
                usage,
            ));
        }
        if response.stop_reason == Some(StopReason::MaxTokens) {
            return Err(CompressionError::with_usage(
                "上下文压缩输出达到最大 token 限制，拒绝提交截断摘要",
                usage,
            ));
        }
        let summary = Self::parse_output(&response.text)
            .map_err(|error| CompressionError::with_usage(error, usage.clone()))?;

        Ok(CompressionUpdate {
            summary,
            usage,
            previous_summary_up_to,
            summary_up_to: split_point,
            boundary_message_id,
        })
    }

    fn summary_request(
        session: &Session,
        split_point: usize,
        max_output_tokens: u32,
        reasoning_effort: ReasoningEffort,
    ) -> ModelRequest {
        let split_point = split_point.min(session.messages.len());
        let mut context = session.context();
        context.push(Message::new(
            MessageRole::User,
            Self::compress_instruction(session, split_point, max_output_tokens),
        ));

        ModelRequest {
            session_id: Some(session.id.clone()),
            user_input: String::new(),
            context,
            // 思考强度沿用主循环：thinking/effort 参与 provider 的缓存键与
            // 对话模板渲染，改变档位会使历史前缀缓存整体失效。思考长度改由
            // 压缩指令约束。
            reasoning_effort,
            // 仅为输出上限（不参与前缀缓存），按剩余上下文空间限制。
            max_output_tokens: Some(max_output_tokens),
            ..Default::default()
        }
    }

    fn compress_instruction(
        session: &Session,
        split_point: usize,
        max_output_tokens: u32,
    ) -> String {
        let existing_summary = session
            .context_summary
            .as_deref()
            .map(str::trim)
            .is_some_and(|summary| !summary.is_empty());
        let merge_hint = if existing_summary {
            "系统提示中已经包含此前对话摘要，请将它与以上新增对话合并；不要重复引用原摘要。\n"
        } else {
            ""
        };
        // 边界说明仅追加在请求末尾，不向原历史插入标记或重复整段工具输出。
        let boundary = session.messages[..split_point].iter().rev().find(|message| {
            message.role != MessageRole::System && message.role != MessageRole::Notice
        }).map(|message| {
            let tail: String = message.text_content().chars().rev().take(512).collect::<Vec<_>>().into_iter().rev().collect();
            serde_json::json!({"role":message.kind(),"tool_call_id":message.tool_call_id(),"tool_call_ids":message.tool_calls().iter().map(|call|&call.id).collect::<Vec<_>>(),"text_end":tail})
        }).unwrap_or(serde_json::Value::Null);
        format!(
            "请压缩以上对话历史。{merge_hint}\
             以上完整上下文供你理解任务。摘要截止于最后一条符合以下边界描述的历史消息（包含该消息）：{boundary}。\n\
             边界之后的消息将原样保留，只供参考，不要重复写入摘要。不要调用工具。\n\
             只整理已有信息，尽量缩短思考，仅做必要的简短核对；不要重新推导、扩展分析或解决任务，把输出预算优先用于完整摘要。\n\
             总输出不得超过 {max_output_tokens} tokens，请在达到预算前主动结束。\n\
             保留关键事实、决策、路径、错误和重要结果，删除重复内容、过程性描述和无效工具输出。\n\
             不要回答用户，严格按以下格式输出：\n\n\
             [[SUMMARY]]\n\
             <合并后的历史摘要>"
        )
    }

    fn parse_output(text: &str) -> std::result::Result<String, String> {
        const SUMMARY_MARKER: &str = "[[SUMMARY]]";
        let text = text.trim();
        if text.is_empty() {
            return Err("上下文压缩返回空内容，拒绝推进摘要边界".to_string());
        }
        let summary = text
            .split_once(SUMMARY_MARKER)
            .map_or(text, |(_, summary)| summary)
            .trim();
        if summary.is_empty() {
            return Err("上下文压缩返回空摘要，拒绝推进摘要边界".to_string());
        }
        Ok(summary.to_string())
    }
}

pub fn mark_compact_boundary(messages: &mut [Message], split_point: usize) {
    for message in messages.iter_mut() {
        message.meta.compact = false;
    }
    if let Some(boundary) = split_point
        .checked_sub(1)
        .and_then(|index| messages.get_mut(index))
    {
        boundary.meta.compact = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message::new(MessageRole::User, text)
    }

    fn assistant(text: &str) -> Message {
        Message::new(MessageRole::Assistant, text)
    }

    #[test]
    fn summary_request_keeps_full_body_and_sets_budget() {
        let mut session = Session::new("test");
        session.system_prompt_message = Some(Message::new(MessageRole::System, "系统提示"));
        session.messages = vec![user("你好"), assistant("你好，有什么可以帮你？")];

        let request =
            ContextCompressor::summary_request(&session, 2, 10_000, ReasoningEffort::High);
        assert_eq!(request.session_id.as_deref(), Some(session.id.as_str()));
        assert_eq!(request.reasoning_effort, ReasoningEffort::High);

        assert_eq!(request.max_output_tokens, Some(10_000));
        assert_eq!(request.context.len(), 4);
        assert_eq!(request.context[0].role, MessageRole::System);
        assert_eq!(request.context[1].text_content(), "你好");
        assert_eq!(request.context[2].text_content(), "你好，有什么可以帮你？");
        let instruction = request.context[3].text_content();
        assert!(instruction.contains("不得超过 10000 tokens"));
        assert!(instruction.contains("[[SUMMARY]]"));
    }

    #[test]
    fn summary_keeps_effort_without_rewriting_thinking_history() {
        let mut session = Session::new("thinking-history");
        session.system_prompt_message = Some(Message::new(MessageRole::System, "固定系统提示"));
        let mut reply = assistant("已完成核对");
        if let crate::session::Role::Assistant {
            reasoning_content,
            reasoning_signature,
            ..
        } = &mut reply.role
        {
            *reasoning_content = "  保留原始思考\n及空白  ".into();
            *reasoning_signature = Some("original-signature".into());
        }
        session.messages = vec![user("核对记录"), reply, user("后续问题")];
        let original = serde_json::to_value(&session).unwrap();
        let history = serde_json::to_value(session.context()).unwrap();

        for effort in [
            ReasoningEffort::None,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ] {
            let request = ContextCompressor::summary_request(&session, 2, 10_000, effort);
            // 与主循环档位一致，避免 thinking/effort 变化导致缓存失效。
            assert_eq!(request.reasoning_effort, effort);
            let (instruction, prefix) = request.context.split_last().unwrap();
            assert_eq!(serde_json::to_value(prefix).unwrap(), history);
            assert_eq!(instruction.role, MessageRole::User);
            assert!(instruction.text_content().contains("尽量缩短思考"));
            assert_eq!(serde_json::to_value(&session).unwrap(), original);
        }
    }

    #[test]
    fn prompt_references_existing_summary_without_duplicating_it() {
        let mut session = Session::new("test");
        session.context_summary = Some("不应重复出现的旧摘要正文".to_string());

        let instruction = ContextCompressor::compress_instruction(&session, 0, 50_000);

        assert!(instruction.contains("系统提示中已经包含此前对话摘要"));
        assert!(!instruction.contains("不应重复出现的旧摘要正文"));
    }

    /// 旧版本会话可能仍含 CompressedResume 消息：压缩请求应继续把它们纳入
    /// 上下文（数据兼容，新架构不再产生）。
    #[test]
    fn summary_request_keeps_legacy_resume_messages() {
        let mut session = Session::new("test");
        session.system_prompt_message = Some(Message::new(MessageRole::System, "系统提示"));
        let resume = user("上一轮续接状态").with_source(UserSource::CompressedResume);
        session.messages = vec![resume, assistant("后续交互")];

        let request =
            ContextCompressor::summary_request(&session, 2, 10_000, ReasoningEffort::None);

        assert_eq!(request.context[1].role, MessageRole::User);
        assert_eq!(
            request.context[1].user_source(),
            Some(UserSource::CompressedResume)
        );
        assert_eq!(request.context[1].text_content(), "上一轮续接状态");
        assert_eq!(request.context[2].text_content(), "后续交互");
    }

    /// HostInjected 不因前端展示而豁免压缩，压缩请求必须保留它的
    /// 模型上下文内容，
    /// 使其与普通历史一样可以被摘要覆盖。
    #[test]
    fn summary_request_includes_host_injected_messages() {
        let mut session = Session::new("test-model-only");
        session.system_prompt_message = Some(Message::new(MessageRole::System, "系统提示"));
        let image = user("截图 provenance").with_source(UserSource::HostInjected);
        session.messages = vec![image, assistant("基于截图的后续回答")];

        let request =
            ContextCompressor::summary_request(&session, 2, 10_000, ReasoningEffort::None);
        assert_eq!(request.context.len(), 4);
        assert_eq!(
            request.context[1].user_source(),
            Some(UserSource::HostInjected)
        );
        assert_eq!(request.context[1].text_content(), "截图 provenance");
        assert_eq!(request.context[2].text_content(), "基于截图的后续回答");
    }

    #[test]
    fn parses_marked_summary() {
        let text = "[[SUMMARY]]\n历史摘要";
        assert_eq!(ContextCompressor::parse_output(text).unwrap(), "历史摘要");
    }

    #[test]
    fn rejects_empty_or_summary_missing_output() {
        assert!(ContextCompressor::parse_output("").is_err());
        assert!(ContextCompressor::parse_output("[[SUMMARY]]").is_err());
        assert!(ContextCompressor::parse_output("[[SUMMARY]]\n   ").is_err());
    }

    #[test]
    fn plain_non_empty_text_is_accepted_as_summary() {
        assert_eq!(
            ContextCompressor::parse_output("普通摘要").unwrap(),
            "普通摘要"
        );
    }
}
