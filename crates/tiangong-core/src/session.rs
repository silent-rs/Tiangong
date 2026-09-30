use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::permission::TrustMode;
use tiangong_types::TokenUsage;

pub use tiangong_types::{
    ContentBlock, DeferredToolInjection, MediaAsset, MediaKind, Message, MessagePhase, MessageRole,
    MessageToolCall, StoredAsset, now_text,
};

/// 同一进程内的持久化写入共用此锁，避免 Core 与宿主同时替换会话文件。
static PERSISTENCE_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 在目标文件所在目录写入临时文件，并原子替换目标文件。
///
/// 所有调用共享同一把进程内锁。临时文件与目标文件位于同一文件系统，
/// `NamedTempFile::persist` 会在 Windows、macOS 与 Linux 上替换已有目标文件。
pub fn atomic_replace_file(path: &Path, content: &[u8]) -> io::Result<()> {
    let _write_guard = PERSISTENCE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let mut temp_file = tempfile::NamedTempFile::new_in(parent)?;
    temp_file.as_file_mut().write_all(content)?;
    temp_file.as_file().sync_all()?;
    temp_file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// 会话工作目录模式
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionCwdMode {
    /// 继承全局工作目录（桌面端默认）
    #[default]
    Inherit,
    /// 隔离模式：在 ~/.tiangong/workspaces/{session_id}/ 下创建独立目录
    Isolated,
    /// 用户手动指定
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub messages: Vec<Message>,
    /// 当前会话累计 token 用量。
    ///
    /// 会话级累计值
    /// 存在这里，供 GUI 切换会话时恢复原先统计。
    #[serde(default)]
    pub token_usage: TokenUsage,
    /// 最近一次主对话 LLM 请求的 prompt token 数，用于展示当前上下文大小。
    #[serde(default)]
    pub current_tokens: usize,
    /// 当前模型配置下触发上下文压缩的 token 阈值。
    #[serde(skip)]
    pub compression_threshold_tokens: usize,
    /// 当前模型配置下的上下文窗口上限。
    #[serde(skip)]
    pub context_limit_tokens: usize,
    /// 当前活跃 sub agent 的上下文 token 数
    #[serde(default)]
    pub active_agent_current_tokens: usize,
    /// 当前活跃 sub agent ID（None 表示主对话执行中）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_agent_id: Option<String>,
    /// agent_id → 最近一次上下文 token 数，用于 GUI 按 Agent Tab 切换展示。
    #[serde(default)]
    pub agent_current_tokens: HashMap<String, usize>,
    /// agent_id → 累计 token 用量，用于 GUI 按 Agent Tab 切换展示。
    #[serde(default)]
    pub agent_token_usage: HashMap<String, TokenUsage>,
    /// 会话级工作目录，工具执行时以此为根目录
    #[serde(default)]
    pub cwd: String,
    /// 工作目录模式
    #[serde(default)]
    pub cwd_mode: SessionCwdMode,
    /// 会话级信任模式；应用级默认值只在新建会话时复制到这里。
    #[serde(default)]
    pub trust_mode: TrustMode,
    /// 会话级思考强度；为空时使用应用级默认值。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "tiangong_llm::request::deserialize_reasoning_effort_option_flexible"
    )]
    pub reasoning_effort: Option<tiangong_llm::ReasoningEffort>,
    /// 早期对话的滚动摘要（用于无限上下文压缩）
    ///
    /// 当对话历史超过模型上下文阈值时，早期消息被 LLM 压缩为摘要存储在此。
    /// 构建 prompt 时注入为系统消息，原始 messages 保持完整供 UI 展示。
    /// 每次压缩会将旧摘要 + 新溢出消息折叠为新摘要，支持无限延伸。
    #[serde(default)]
    pub context_summary: Option<String>,
    /// 摘要覆盖到的消息索引（messages[0..summary_up_to] 已被摘要覆盖）
    #[serde(default)]
    pub summary_up_to: usize,
    /// 缓存的 system prompt 消息（role=System）。
    ///
    /// 在新对话、压缩对话、清空上下文时由外部调用 `rebuild_system_prompt()` 重建。
    /// `context()` 返回时会将其置于消息列表头部，由 `build_provider_messages()` 提取。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_message: Option<Message>,
    pub created_at: String,
    pub updated_at: String,
    /// 父会话 ID（Worker 子会话标注所属的父会话）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// 工具调用批次闭合前收到的外部工具输入；下一安全边界按顺序注入。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred_tool_injections: Vec<DeferredToolInjection>,
    /// 会话级对话模型（models 注册表 key）；None 表示跟随路由默认。
    ///
    /// 只由用户显式切换改写，系统行为一律不动它。存引用不存端点：
    /// ModelEndpoint 含 api_key，落盘即泄密。引用失效（key 或其 provider
    /// 已删）时由宿主在选择与投递入口给出明确报错，不静默回退默认。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<String>,
    /// 当前 Session 独立的持久化根，仅用于运行时，不写入会话 JSON。
    #[serde(skip)]
    storage_root: Option<PathBuf>,
}

impl Session {
    pub fn has_user_messages(&self) -> bool {
        self.messages.iter().any(|m| m.role == MessageRole::User)
    }

    pub fn new(title: impl Into<String>) -> Self {
        let now = now_text();
        Self {
            id: new_id(),
            title: title.into(),
            messages: Vec::new(),
            token_usage: TokenUsage::default(),
            current_tokens: 0,
            compression_threshold_tokens: 0,
            context_limit_tokens: 0,
            active_agent_current_tokens: 0,
            active_agent_id: None,
            agent_current_tokens: HashMap::new(),
            agent_token_usage: HashMap::new(),
            cwd: String::new(),
            cwd_mode: SessionCwdMode::Inherit,
            trust_mode: TrustMode::default(),
            reasoning_effort: None,
            context_summary: None,
            summary_up_to: 0,
            system_prompt_message: None,
            created_at: now.clone(),
            updated_at: now,
            parent_session_id: None,
            deferred_tool_injections: Vec::new(),
            model_ref: None,
            storage_root: None,
        }
    }

    /// 创建隔离模式的会话（用于 Connector 接入）
    pub fn new_isolated(title: impl Into<String>, storage_root: &std::path::Path) -> Self {
        let id = new_id();
        let now = now_text();
        // 在 {storage_root}/workspaces/{session_id}/ 下创建独立目录
        let workspace_dir = storage_root.join("workspaces").join(&id);
        let _ = std::fs::create_dir_all(&workspace_dir);
        Self {
            id,
            title: title.into(),
            messages: Vec::new(),
            token_usage: TokenUsage::default(),
            current_tokens: 0,
            compression_threshold_tokens: 0,
            context_limit_tokens: 0,
            active_agent_current_tokens: 0,
            active_agent_id: None,
            agent_current_tokens: HashMap::new(),
            agent_token_usage: HashMap::new(),
            cwd: workspace_dir.to_string_lossy().to_string(),
            cwd_mode: SessionCwdMode::Isolated,
            trust_mode: TrustMode::default(),
            reasoning_effort: None,
            context_summary: None,
            summary_up_to: 0,
            system_prompt_message: None,
            created_at: now.clone(),
            updated_at: now,
            parent_session_id: None,
            deferred_tool_injections: Vec::new(),
            model_ref: None,
            storage_root: None,
        }
    }

    /// 将该 Session 的持久化固定到指定根目录。
    pub fn bind_storage_root(&mut self, storage_root: impl Into<PathBuf>) {
        self.storage_root = Some(storage_root.into());
    }

    /// 绑定独立持久化根并返回 Session。
    pub fn with_storage_root(mut self, storage_root: impl Into<PathBuf>) -> Self {
        self.bind_storage_root(storage_root);
        self
    }

    /// 当前绑定的独立持久化根。
    pub fn bound_storage_root(&self) -> Option<&Path> {
        self.storage_root.as_deref()
    }

    /// 存储根下的统一媒体目录（`<storage_root>/media`）。
    ///
    /// RFC 0017 安全边界：工具声明注入的图片必须位于该目录下才会被
    /// 采纳（沙箱内工具的写域不含此目录，媒体产物由宿主管理的免沙箱
    /// 插件写入），配合文件头校验与大小上限阻断任意文件外发。
    pub fn media_root(&self) -> Option<std::path::PathBuf> {
        self.storage_root.as_ref().map(|root| root.join("media"))
    }

    /// 从指定存储根加载 Session，并保留该根作为后续持久化位置。
    pub fn load_from_storage(storage_root: &Path, session_id: &str) -> Result<Self, String> {
        let mut components = Path::new(session_id).components();
        let valid_id = matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none();
        if !valid_id {
            return Err("会话 ID 必须是单个安全路径片段".to_string());
        }

        let path = storage_root
            .join("sessions")
            .join(format!("{session_id}.json"));
        let content = std::fs::read_to_string(&path)
            .map_err(|error| format!("session 读取失败（{}）：{error}", path.display()))?;
        let mut session: Self = serde_json::from_str(&content)
            .map_err(|error| format!("session 反序列化失败（{}）：{error}", path.display()))?;
        if session.id != session_id {
            return Err(format!(
                "session 文件 ID 不匹配：期望 {session_id}，实际 {}",
                session.id
            ));
        }
        session.bind_storage_root(storage_root.to_path_buf());
        Ok(session)
    }

    /// 将 session 持久化到 `~/.tiangong/sessions/{id}.json`
    ///
    /// Core 在工具调用等关键节点调用此方法，确保中间数据不会因崩溃丢失。
    pub fn persist_to_disk(&self) {
        if let Err(err) = self.try_persist_to_disk() {
            tracing::warn!(error = %err, "session 持久化失败");
        }
    }

    /// 尝试将稳定会话状态持久化到磁盘，并把失败返回给调用方。
    /// 图片块中的瞬时 `data` 由类型合同保证永不序列化。
    pub fn try_persist_to_disk(&self) -> Result<(), String> {
        #[cfg(test)]
        if crate::core::test_support::is_persistence_persistently_failing(&self.id)
            || crate::core::test_support::take_persistence_failure_for_session(&self.id)
        {
            return Err(format!("测试注入的 session 持久化失败（{}）", self.id));
        }

        let storage_root = self.storage_root.as_ref().ok_or_else(|| {
            "session 未绑定 storage_root（创建或加载 Session 时必须绑定）".to_string()
        })?;
        let path = storage_root
            .join("sessions")
            .join(format!("{}.json", self.id));
        let content = serde_json::to_string_pretty(self)
            .map_err(|err| format!("session 序列化失败：{err}"))?;
        atomic_replace_file(&path, content.as_bytes())
            .map_err(|err| format!("session 持久化写入失败：{err}"))
    }

    pub fn append_message(&mut self, role: MessageRole, content: impl Into<String>) {
        self.append_message_with_reasoning(role, content, String::new());
    }

    pub fn append_message_with_reasoning(
        &mut self,
        role: MessageRole,
        content: impl Into<String>,
        reasoning_content: impl Into<String>,
    ) {
        self.messages.push(Message {
            id: new_id(),
            role,
            content: vec![ContentBlock::text(content.into())],
            reasoning_content: reasoning_content.into(),
            reasoning_signature: None,
            usage: None,
            worker_id: None,
            elapsed_ms: None,
            turn_status: None,
            reasoning_elapsed_ms: None,
            text_elapsed_ms: None,
            duration_ms: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_result_is_error: false,
            compact: false,
            phase: crate::session::MessagePhase::Normal,
            created_at: now_text(),
        });
    }

    /// 使用预生成的 ID 追加带结构化媒体的消息。
    pub fn append_message_with_id_and_media(
        &mut self,
        id: String,
        role: MessageRole,
        content: impl Into<String>,
        reasoning_content: impl Into<String>,
        media: Vec<tiangong_types::MediaAsset>,
    ) {
        let mut blocks = vec![ContentBlock::text(content.into())];
        for asset in &media {
            blocks.push(asset.to_content_block());
        }
        self.messages.push(Message {
            id,
            role,
            content: blocks,
            reasoning_content: reasoning_content.into(),
            reasoning_signature: None,
            usage: None,
            worker_id: None,
            elapsed_ms: None,
            turn_status: None,
            reasoning_elapsed_ms: None,
            text_elapsed_ms: None,
            duration_ms: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_result_is_error: false,
            compact: false,
            phase: crate::session::MessagePhase::Normal,
            created_at: now_text(),
        });
    }

    /// 使用预生成 ID 原样追加宿主准备好的用户消息。
    pub fn append_prepared_user_message_with_id(&mut self, id: String, content: Vec<ContentBlock>) {
        self.messages.push(Message {
            id,
            role: MessageRole::User,
            content,
            reasoning_content: String::new(),
            reasoning_signature: None,
            usage: None,
            worker_id: None,
            elapsed_ms: None,
            turn_status: None,
            reasoning_elapsed_ms: None,
            text_elapsed_ms: None,
            duration_ms: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_result_is_error: false,
            compact: false,
            phase: crate::session::MessagePhase::Normal,
            created_at: now_text(),
        });
        self.updated_at = now_text();
    }

    /// 校验并事务性写入 Core 已接收的用户消息。
    ///
    /// 同 ID 的宿主镜像消息会先移除；只有完整 Session 成功落盘后才返回。失败时
    /// 恢复调用前的 Session。上一轮遗留工具调用由该轮取消收尾负责闭合。
    pub(crate) fn try_append_prepared_user_message_with_id(
        &mut self,
        id: String,
        content: Vec<ContentBlock>,
    ) -> Result<(), String> {
        tiangong_types::validate_ready_content_blocks(&content)?;

        if self
            .messages
            .iter()
            .any(|message| message.id == id && message.role != MessageRole::User)
        {
            return Err(format!("消息 ID {id} 已被非用户消息占用"));
        }

        let before = self.clone();
        self.messages
            .retain(|message| message.id != id || message.role != MessageRole::User);
        self.append_prepared_user_message_with_id(id, content);

        if let Err(error) = self.try_persist_to_disk() {
            *self = before;
            return Err(error);
        }

        Ok(())
    }

    /// 补齐未完成工具调用的失败结果，并在有变更时立即落盘。
    ///
    /// 补齐结果落盘失败时，删除新增结果和对应的悬空调用后再次落盘，避免后续
    /// 请求读取到不完整的工具调用协议。二次落盘仍失败时保留清理后的内存状态，
    /// 供 turn 最终持久化继续重试。
    pub(crate) fn close_unfinished_tool_calls_with_reason(
        &mut self,
        reason: &str,
    ) -> Vec<(String, String, String)> {
        let Some((assistant_index, assistant)) = self
            .messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, message)| !message.tool_calls.is_empty())
        else {
            return Vec::new();
        };
        let completed = self.messages[assistant_index + 1..]
            .iter()
            .filter_map(|message| message.tool_call_id.as_deref())
            .collect::<std::collections::HashSet<_>>();
        let unfinished = assistant
            .tool_calls
            .iter()
            .filter(|call| !completed.contains(call.id.as_str()))
            .map(|call| (call.id.clone(), call.name.clone()))
            .collect::<Vec<_>>();
        if unfinished.is_empty() {
            return Vec::new();
        }

        let message_count_before_close = self.messages.len();
        let interrupted = unfinished
            .into_iter()
            .map(|(tool_call_id, tool_name)| {
                let output = reason.to_string();
                self.messages.push(
                    Message::tool_result(&tool_call_id, &tool_name, &output, true)
                        .with_phase(MessagePhase::React),
                );
                (tool_call_id, tool_name, output)
            })
            .collect::<Vec<_>>();
        self.updated_at = now_text();
        if let Err(close_error) = self.try_persist_to_disk() {
            self.messages.truncate(message_count_before_close);
            let interrupted_ids = interrupted
                .iter()
                .map(|(tool_call_id, _, _)| tool_call_id.as_str())
                .collect::<std::collections::HashSet<_>>();
            self.messages[assistant_index]
                .tool_calls
                .retain(|call| !interrupted_ids.contains(call.id.as_str()));
            self.updated_at = now_text();

            if let Err(remove_error) = self.try_persist_to_disk() {
                tracing::error!(
                    close_error = %close_error,
                    remove_error = %remove_error,
                    count = interrupted.len(),
                    "补齐未完成工具调用及删除悬空调用均持久化失败"
                );
                return Vec::new();
            }

            tracing::warn!(
                error = %close_error,
                count = interrupted.len(),
                "补齐未完成工具调用持久化失败，已删除悬空调用并重新落盘"
            );
            return Vec::new();
        }

        interrupted
    }

    pub(crate) fn has_unfinished_tool_calls(&self) -> bool {
        !self.unfinished_tool_calls().is_empty()
    }

    fn unfinished_tool_calls(&self) -> Vec<(String, String)> {
        let Some((assistant_index, assistant)) = self
            .messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, message)| !message.tool_calls.is_empty())
        else {
            return Vec::new();
        };
        let completed = self.messages[assistant_index + 1..]
            .iter()
            .filter_map(|message| message.tool_call_id.as_deref())
            .collect::<std::collections::HashSet<_>>();
        assistant
            .tool_calls
            .iter()
            .filter(|call| !completed.contains(call.id.as_str()))
            .map(|call| (call.id.clone(), call.name.clone()))
            .collect()
    }

    /// 用宿主准备好的内容原样替换已有用户消息。
    pub fn update_prepared_user_message(
        &mut self,
        message_id: &str,
        content: Vec<ContentBlock>,
    ) -> bool {
        let Some(message) = self
            .messages
            .iter_mut()
            .find(|message| message.id == message_id && message.role == MessageRole::User)
        else {
            return false;
        };
        message.content = content;
        self.updated_at = now_text();
        true
    }

    pub(crate) fn clear_transient_content(&mut self) {
        for message in &mut self.messages {
            message.clear_transient_data();
        }
    }

    /// 清除指定消息的瞬时图片数据；稳定图片路径不受影响。
    pub fn clear_transient_content_for_message(&mut self, message_id: &str) {
        if let Some(message) = self
            .messages
            .iter_mut()
            .find(|message| message.id == message_id)
        {
            message.clear_transient_data();
        }
    }

    pub(crate) fn defer_tool_injection(&mut self, tool_name: String, payload: serde_json::Value) {
        self.deferred_tool_injections
            .push(DeferredToolInjection { tool_name, payload });
    }

    pub fn append_worker_message(
        &mut self,
        role: MessageRole,
        content: impl Into<String>,
        worker_id: &str,
    ) {
        self.append_worker_message_with_reasoning(role, content, String::new(), worker_id);
    }

    pub fn append_worker_message_with_reasoning(
        &mut self,
        role: MessageRole,
        content: impl Into<String>,
        reasoning_content: impl Into<String>,
        worker_id: &str,
    ) {
        self.messages.push(Message {
            id: new_id(),
            role,
            content: vec![ContentBlock::text(content.into())],
            reasoning_content: reasoning_content.into(),
            reasoning_signature: None,
            usage: None,
            worker_id: Some(worker_id.to_string()),
            elapsed_ms: None,
            turn_status: None,
            reasoning_elapsed_ms: None,
            text_elapsed_ms: None,
            duration_ms: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_result_is_error: false,
            compact: false,
            phase: crate::session::MessagePhase::Normal,
            created_at: now_text(),
        });
    }

    /// 计算当前会话所有任务的累计 token 用量
    pub fn total_usage(&self) -> TokenUsage {
        self.token_usage.clone()
    }

    /// 重建 system prompt 消息
    ///
    /// 从 session 数据（cwd, context_summary）和外部配置构建完整的 system prompt，
    /// 存为 `Message { role: System }`，供 `context()` 返回。
    ///
    /// 应在以下时机调用：
    /// - 新对话首轮（system_prompt_message 为 None）
    /// - 压缩对话后
    /// - 清空上下文后
    pub fn rebuild_system_prompt(&mut self, config: &crate::prompt::SystemPromptConfig) {
        let msg = crate::prompt::sections::build_full_system_prompt(self, config);
        if self
            .system_prompt_message
            .as_ref()
            .is_some_and(|previous| previous.content == msg.content)
        {
            return;
        }
        self.system_prompt_message = Some(msg);
    }

    /// 构建 LLM 请求上下文
    ///
    /// 返回 system_prompt_message（如有）+ `summary_up_to` 之后的对话消息。
    /// System 消息由 `build_provider_messages` 提取到 system prompt。
    pub fn context(&self) -> Vec<Message> {
        let mut context = Vec::new();
        if let Some(ref msg) = self.system_prompt_message {
            context.push(msg.clone());
        }
        context.extend(
            self.messages[self.summary_up_to..]
                .iter()
                // 持久化 System 消息是 UI/恢复日志；唯一模型系统提示由
                // system_prompt_message 提供，避免日志覆盖完整规则。
                // Notice 是系统发给用户的通知，按角色整体排除出模型上下文。
                .filter(|message| {
                    message.role != MessageRole::System && message.role != MessageRole::Notice
                })
                .cloned(),
        );
        context
    }

    /// 截断指定消息之后的所有消息（保留该消息本身），返回移除数量
    pub fn truncate_after_message(&mut self, message_id: &str) -> usize {
        let Some(idx) = self.messages.iter().position(|m| m.id == message_id) else {
            return 0;
        };
        let remove_count = self.messages.len() - idx - 1;
        self.messages.truncate(idx + 1);
        remove_count
    }

    /// 获取最新用户消息的 index（轮次锚点）。
    ///
    /// 宿主注入的 role=User 消息（图片注入、压缩恢复锚点）不是用户
    /// 意图，不得作为锚点——否则轮次的 elapsed_ms/turn_status 会写到
    /// 前端不展示的消息上，执行总时长与轮次状态随之丢失。
    pub fn latest_user_message_index(&self) -> Option<usize> {
        self.messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, m)| m.role == MessageRole::User && m.phase.is_user_input())
            .map(|(idx, _)| idx)
    }
}

fn new_id() -> String {
    scru128::new().to_string()
}

#[cfg(test)]
mod persistence_tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    /// 轮次锚点必须落在用户真实输入上：宿主注入的 role=User 消息
    /// （图片注入、压缩恢复锚点）不是用户意图，不得劫持 latest 锚点。
    #[test]
    fn latest_user_message_index_skips_host_injected_user_messages() {
        let mut session = Session::new("anchor");
        session.append_message(MessageRole::User, "真实问题");
        session.append_message(MessageRole::Assistant, "回答");
        let mut injected = Message::new(MessageRole::User, "[injected-images]");
        injected.phase = MessagePhase::HostInjected;
        session.messages.push(injected);
        assert_eq!(
            session.latest_user_message_index(),
            Some(0),
            "图片注入消息不得成为轮次锚点"
        );
        let mut resume = Message::new(MessageRole::User, "上一轮续接状态");
        resume.phase = MessagePhase::CompressedResume;
        session.messages.push(resume);
        assert_eq!(
            session.latest_user_message_index(),
            Some(0),
            "压缩恢复锚点不得劫持轮次锚点"
        );
    }

    #[test]
    fn atomic_replace_file_serializes_complete_replacements() -> io::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let target = temp_dir.path().join("sessions").join("session.json");
        atomic_replace_file(&target, b"initial")?;

        let payloads = (0..4)
            .map(|writer| format!("writer-{writer}:{}", "x".repeat(16 * 1024)).into_bytes())
            .collect::<Vec<_>>();
        let barrier = Arc::new(Barrier::new(payloads.len()));
        std::thread::scope(|scope| -> io::Result<()> {
            let mut writers = Vec::new();
            for payload in &payloads {
                let target = target.clone();
                let barrier = Arc::clone(&barrier);
                writers.push(scope.spawn(move || {
                    barrier.wait();
                    atomic_replace_file(&target, payload)
                }));
            }
            for writer in writers {
                writer.join().expect("原子写入线程不应 panic")?;
            }
            Ok(())
        })?;

        let persisted = std::fs::read(&target)?;
        assert!(payloads.contains(&persisted));
        let entries = std::fs::read_dir(target.parent().expect("目标文件应有父目录"))?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(entries.len(), 1, "成功替换后不应遗留临时文件");
        assert_eq!(entries[0].path(), target);
        Ok(())
    }

    #[test]
    fn close_fallback_removes_only_unfinished_tool_calls() {
        let mut session = Session::new("tool-call-fallback");
        let mut assistant = Message::new(MessageRole::Assistant, "");
        assistant.tool_calls = vec![
            MessageToolCall {
                id: "completed-call".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({}),
            },
            MessageToolCall {
                id: "unfinished-call".to_string(),
                name: "write_file".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        session.messages.push(assistant);
        session.messages.push(Message::tool_result(
            "completed-call",
            "read_file",
            "done",
            false,
        ));

        let interrupted = session.close_unfinished_tool_calls_with_reason("interrupted");

        let remaining_calls = &session.messages[0].tool_calls;
        assert!(interrupted.is_empty());
        assert_eq!(remaining_calls.len(), 1);
        assert_eq!(remaining_calls[0].id, "completed-call");
        assert!(!session.has_unfinished_tool_calls());
        assert!(
            session
                .messages
                .iter()
                .all(|message| message.tool_call_id.as_deref() != Some("unfinished-call"))
        );
    }

    #[test]
    fn legacy_workspace_tabs_are_ignored_and_never_serialized_again() {
        let mut value = serde_json::to_value(Session::new("legacy-tabs")).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert(
            "tabs".to_string(),
            serde_json::json!([{
                "id": "terminal-1",
                "kind": "terminal",
                "title": "终端",
                "url": "",
                "created_at": "now"
            }]),
        );
        object.insert("active_tab_id".to_string(), serde_json::json!("terminal-1"));

        let restored: Session = serde_json::from_value(value).unwrap();
        let serialized = serde_json::to_value(restored).unwrap();
        assert!(serialized.get("tabs").is_none());
        assert!(serialized.get("active_tab_id").is_none());
    }

    #[test]
    fn derived_context_metrics_are_not_persisted_but_usage_is() {
        let mut session = Session::new("derived-context-metrics");
        session.compression_threshold_tokens = 190_000;
        session.context_limit_tokens = 200_000;
        session.current_tokens = 12_345;
        session.token_usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            prompt_cache_hit_tokens: Some(10),
            prompt_cache_miss_tokens: Some(90),
        };

        let mut value = serde_json::to_value(&session).unwrap();
        assert!(value.get("compression_threshold_tokens").is_none());
        assert!(value.get("context_limit_tokens").is_none());
        assert_eq!(value["current_tokens"], 12_345);
        assert_eq!(value["token_usage"]["total_tokens"], 120);

        value["compression_threshold_tokens"] = serde_json::json!(999_999);
        value["context_limit_tokens"] = serde_json::json!(1_000_000);
        let restored: Session = serde_json::from_value(value).unwrap();
        assert_eq!(restored.compression_threshold_tokens, 0);
        assert_eq!(restored.context_limit_tokens, 0);
        assert_eq!(restored.current_tokens, 12_345);
        assert_eq!(restored.token_usage.total_tokens, 120);
    }

    #[test]
    fn bound_storage_root_is_used_and_restored_on_load() {
        let root = tempfile::tempdir().unwrap();
        let session = Session::new("child").with_storage_root(root.path());
        session.try_persist_to_disk().unwrap();

        let path = root
            .path()
            .join("sessions")
            .join(format!("{}.json", session.id));
        assert!(path.is_file());
        assert!(
            serde_json::to_value(&session)
                .unwrap()
                .get("storage_root")
                .is_none()
        );

        let restored = Session::load_from_storage(root.path(), &session.id).unwrap();
        assert_eq!(restored.title, "child");
        assert_eq!(restored.bound_storage_root(), Some(root.path()));
    }

    #[test]
    fn load_from_storage_rejects_path_traversal() {
        let root = tempfile::tempdir().unwrap();
        let error = Session::load_from_storage(root.path(), "../outside").unwrap_err();
        assert!(error.contains("会话 ID"));
    }
}

#[cfg(test)]
mod ready_content_tests {
    use super::*;

    fn prepared_message(data: &str) -> Vec<ContentBlock> {
        vec![
            ContentBlock::text("分析图片"),
            ContentBlock::Image {
                asset: StoredAsset {
                    asset_id: "asset-1".to_string(),
                    local_path: "/tmp/asset-1.png".to_string(),
                    original_name: "asset-1.png".to_string(),
                    mime_type: "image/png".to_string(),
                    size: 4,
                    kind: MediaKind::Image,
                },
                data: Some(data.to_string()),
            },
        ]
    }

    #[test]
    fn transient_image_data_is_available_to_context_but_never_serialized() {
        let secret_data = "data:image/png;base64,THIS_MUST_NOT_PERSIST";
        let mut session = Session::new("ready-content");
        session.append_prepared_user_message_with_id(
            "message-1".to_string(),
            prepared_message(secret_data),
        );

        let json = serde_json::to_string(&session).unwrap();
        assert!(!json.contains("THIS_MUST_NOT_PERSIST"));
        assert!(json.contains("\"type\":\"image\""));
        assert!(json.contains("/tmp/asset-1.png"));

        let context = session.context();
        assert!(context[0].content.iter().any(|block| matches!(
            block,
            ContentBlock::Image { data: Some(data), .. } if data == secret_data
        )));
        assert!(
            !serde_json::to_string(&context)
                .unwrap()
                .contains("THIS_MUST_NOT_PERSIST")
        );
        session.clear_transient_content_for_message("message-1");
        assert!(matches!(
            &session.context()[0].content[1],
            ContentBlock::Image { data: None, .. }
        ));
    }
}

/// Core Session → 插件只读快照转换。
///
/// 由 WASM Adapter 在生命周期钩子里调用，序列化为 JSON 传给 WASM。
/// 不暴露 Core 内部状态（token 计数、任务记录、信任模式等）。
impl From<&Session> for tiangong_types::PluginSession {
    fn from(session: &Session) -> Self {
        // 工作区标识：取 cwd 的末尾目录名（平台无关，由宿主生成）。
        let workspace_id = session
            .cwd
            .rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(&session.cwd)
            .to_string();
        Self {
            id: session.id.clone(),
            title: session.title.clone(),
            cwd: session.cwd.clone(),
            workspace_id,
            parent_session_id: session.parent_session_id.clone(),
            turn_start_message_id: None,
            reasoning_effort: session
                .reasoning_effort
                .map(|effort| effort.as_str().to_string()),
            // Notice 是宿主与用户之间的系统通知，不属于对话历史；
            // 插件快照始终剔除，旧版本插件的会话反序列化也不含该角色。
            messages: session
                .messages
                .iter()
                .filter(|message| message.role != MessageRole::Notice)
                .cloned()
                .collect(),
            context_summary: session.context_summary.clone(),
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
        }
    }
}

/// 换算 Core 消息位置到插件快照位置。
///
/// 插件生命周期钩子携带的 `turn_start_idx` 基于 Core 完整消息列表；
/// 快照剔除 Notice 后位置整体前移，必须同步换算才能仍指向同一条消息。
pub fn plugin_turn_start_idx(session: &Session, turn_start_idx: usize) -> usize {
    let removed_before = session
        .messages
        .get(..turn_start_idx.min(session.messages.len()))
        .map(|prefix| {
            prefix
                .iter()
                .filter(|message| message.role == MessageRole::Notice)
                .count()
        })
        .unwrap_or(0);
    turn_start_idx.saturating_sub(removed_before)
}

#[cfg(test)]
mod plugin_session_tests {
    use super::*;

    #[test]
    fn plugin_session_转换剔除_notice_消息() {
        let mut session = Session::new("含系统通知的会话");
        session.append_message(MessageRole::User, "用户输入");
        session.append_message(MessageRole::Notice, "系统通知");
        session.append_message(MessageRole::Assistant, "回复");

        let snapshot = tiangong_types::PluginSession::from(&session);
        let roles: Vec<&str> = snapshot
            .messages
            .iter()
            .map(|message| match message.role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
                MessageRole::Notice => "notice",
            })
            .collect();
        assert_eq!(roles, vec!["user", "assistant"]);
        // 原会话不受影响。
        assert_eq!(session.messages.len(), 3);
    }

    #[test]
    fn plugin_turn_start_idx_换算跳过位置之前的_notice() {
        let mut session = Session::new("位置换算");
        session.append_message(MessageRole::User, "第一轮");
        session.append_message(MessageRole::Notice, "失败通知");
        session.append_message(MessageRole::User, "第二轮");
        session.append_message(MessageRole::Notice, "另一条通知");
        session.append_message(MessageRole::User, "第三轮");

        // 前方有一条 Notice：位置前移 1，仍指向同一条用户消息。
        assert_eq!(plugin_turn_start_idx(&session, 2), 1);
        assert_eq!(
            tiangong_types::PluginSession::from(&session).messages[1].text_content(),
            session.messages[2].text_content()
        );

        // 前方有两条 Notice：位置前移 2。
        assert_eq!(plugin_turn_start_idx(&session, 4), 2);
        assert_eq!(
            tiangong_types::PluginSession::from(&session).messages[2].text_content(),
            session.messages[4].text_content()
        );

        // 位置 0 之前无 Notice：不变。
        assert_eq!(plugin_turn_start_idx(&session, 0), 0);
    }
}
