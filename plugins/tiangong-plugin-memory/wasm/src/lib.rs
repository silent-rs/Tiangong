//! Memory System 的 WASM 桥接组件。
//!
//! 本组件只做桥接：不承载 memory 纯逻辑（规划/提取/整理），不做内部编排。
//! 全部 memory 处理由 memory sidecar 完成（含 LLM/检索/存储的完整能力）。
//!
//! 工作方式：
//! - handle-tool(recall_memory) 时，经通用 sidecar host import
//!   把请求转发到 sidecar，结果原样返回。
//! - sidecar 不可用时，返回明确提示。
//!
//! 见 issue #321 / RFC docs/memory-system/11-memory-sidecar-wasm-bridge.md。

mod bindings;
mod sidecar_client;
mod turn_extract;

use bindings::exports::tiangong::plugin::plugin::{
    Guest, PluginDescriptor, PluginError, ToolCall, ToolExecutionRecord, ToolResult, ToolSpec,
};
use bindings::exports::tiangong::plugin::plugin_ui::{
    Contribution, Guest as UiGuest, ResourceResponse, ViewMessageRequest, ViewMessageResponse,
    ViewResponse,
};
use serde::Deserialize;
use tiangong_plugin_memory_protocol::recall::{
    Recall, RecallContext, RecallContextRequest, RecallQuery, RecallRequest,
};
use tiangong_plugin_memory_protocol::rumination::{
    RunEnhancedMicroRumination, RunEnhancedMicroRuminationRequest, RunMesoRumination,
    RunMesoRuminationRequest, RunMetaRumination, RunMetaRuminationRequest, TurnStatus,
};
use tiangong_plugin_memory_protocol::ui::{self, UiRequest};
use tiangong_plugin_memory_protocol::{Empty, MemoryOperation};

mod descriptor {
    pub const ID: &str = tiangong_plugin_memory_protocol::PLUGIN_ID;
    pub const NAME: &str = "Memory";
    pub const VERSION: &str = tiangong_plugin_memory_protocol::PLUGIN_VERSION;
}

#[derive(Debug, Default, Deserialize)]
struct RecallToolArguments {
    query: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    expected: Vec<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// 全局状态缓存（WASM 单线程，RefCell 安全）。
/// 存放 handle-tool 构建召回 context 所需的上一个 PluginSession 快照。
mod state {
    use std::cell::RefCell;

    struct PluginState {
        /// 上一次 on_turn_started / on_session_ready 收到的 PluginSession 快照。
        last_session: Option<tiangong_types::PluginSession>,
        /// 本轮是否已经召回过（每轮只召回一次）。
        recall_attempted: bool,
        /// per-session 轮次计数（每 10 轮触发 Meta 反刍）。
        turn_count: u32,
    }

    thread_local! {
        static STATE: RefCell<PluginState> = const { RefCell::new(PluginState {
            last_session: None,
            recall_attempted: false,
            turn_count: 0,
        }) };
    }

    pub fn set_last_session(session: tiangong_types::PluginSession) {
        STATE.with(|s| {
            s.borrow_mut().last_session = Some(session);
        });
    }

    pub fn last_session() -> Option<tiangong_types::PluginSession> {
        STATE.with(|s| s.borrow().last_session.clone())
    }

    /// 标记本轮已召回。
    pub fn mark_recall_attempted() {
        STATE.with(|s| s.borrow_mut().recall_attempted = true);
    }

    /// 重置本轮召回标志（在 on_turn_started 时调用）。
    pub fn reset_recall_attempted() {
        STATE.with(|s| s.borrow_mut().recall_attempted = false);
    }

    /// 本轮是否已经召回过。
    pub fn recall_attempted() -> bool {
        STATE.with(|s| s.borrow().recall_attempted)
    }

    /// 递增轮次计数并返回是否应该触发 Meta 反刍（每 10 轮）。
    pub fn increment_turn_and_check_meta() -> bool {
        const META_INTERVAL: u32 = 10;
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.turn_count += 1;
            st.turn_count.is_multiple_of(META_INTERVAL)
        })
    }
}

/// recall_memory 工具的 input_schema（JSON 文本）。
/// 与进程内版本 `tiangong-plugin-memory/src/handler.rs` 保持一致。
const RECALL_MEMORY_INPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "query": {
      "type": "string",
      "description": "要回忆的内容，结合用户当前请求改写成可检索查询"
    },
    "reason": {
      "type": "string",
      "description": "为什么需要回忆，简述当前任务依赖的历史语境"
    },
    "expected": {
      "type": "array",
      "items": { "type": "string" },
      "description": "期望找回的内容类型，如 media、file、tool_result、decision、code_context"
    },
    "limit": {
      "type": "integer",
      "description": "最多返回多少条记忆，默认 5，最大 10"
    }
  },
  "required": ["query"]
}"#;

const RECALL_MEMORY_DESCRIPTION: &str = "按需回忆历史上下文、跨会话结果、之前的工具输出或生成产物。用户提到刚刚、刚才、上次、之前、那个、继续、这张图、生成的图片等历史指代时，应先调用此工具。";

const MEMORY_PAGE_TEMPLATE: &str = include_str!("memory.html");
const MEMORY_PAGE_CSS: &str = include_str!("memory.css");
const MEMORY_PAGE_JS: &str = include_str!("memory.js");

fn memory_settings_html() -> String {
    MEMORY_PAGE_TEMPLATE
        .replace("/*__MEMORY_CSS__*/", MEMORY_PAGE_CSS)
        .replace("/*__MEMORY_JS__*/", MEMORY_PAGE_JS)
}

/// WASM 桥接组件（无状态）。
struct Component;

impl Guest for Component {
    fn describe() -> Result<PluginDescriptor, PluginError> {
        Ok(PluginDescriptor {
            id: descriptor::ID.to_string(),
            name: descriptor::NAME.to_string(),
            version: descriptor::VERSION.to_string(),
        })
    }

    fn tool_specs() -> Result<Vec<ToolSpec>, PluginError> {
        Ok(vec![ToolSpec {
            name: "recall_memory".to_string(),
            description: RECALL_MEMORY_DESCRIPTION.to_string(),
            input_schema: RECALL_MEMORY_INPUT_SCHEMA.to_string(),
        }])
    }

    fn prompt_sections() -> Result<Vec<String>, PluginError> {
        // 记忆只通过 recall_memory 的工具结果进入上下文，后台整理不得改写系统提示。
        Ok(Vec::new())
    }

    fn handle_tool(call: ToolCall) -> Result<ToolResult, PluginError> {
        if call.name != "recall_memory" {
            return Err(PluginError::Message(format!(
                "memory 组件不支持工具: {}",
                call.name
            )));
        }

        let started = bindings::tiangong::plugin::clock::now_millis();
        if state::recall_attempted() {
            emit_skip_events();
            return Ok(recall_tool_result(
                true,
                "本轮已完成回忆，跳过重复调用",
                "recall_memory 本轮已经执行过，回忆结果已经注入当前上下文。请直接基于已有回忆结果完成用户原始目标，不要再次调用 recall_memory。".to_string(),
                String::new(),
                0,
                vec!["duplicate-recall".to_string()],
                started,
            ));
        }
        state::mark_recall_attempted();

        let arguments =
            serde_json::from_str::<RecallToolArguments>(&call.arguments).map_err(|error| {
                PluginError::Message(format!("解析 recall_memory 参数失败: {error}"))
            })?;
        let fallback_query = state::last_session()
            .as_ref()
            .map(latest_user_query)
            .unwrap_or_default();
        let query = arguments.query.trim();
        let query = if query.is_empty() {
            fallback_query
        } else {
            query.to_string()
        };
        if query.is_empty() {
            emit_skip_events();
            return Ok(recall_tool_result(
                false,
                "缺少回忆查询",
                String::new(),
                "recall_memory.query is empty".to_string(),
                1,
                Vec::new(),
                started,
            ));
        }

        let context = state::last_session()
            .map(|session| build_recall_context(&session))
            .unwrap_or_default();
        let request = RecallContextRequest {
            request: RecallQuery {
                query: query.clone(),
                reason: arguments
                    .reason
                    .map(|reason| reason.trim().to_string())
                    .filter(|reason| !reason.is_empty()),
                expected: arguments
                    .expected
                    .into_iter()
                    .map(|item| item.trim().to_string())
                    .filter(|item| !item.is_empty())
                    .collect(),
                context,
                limit: arguments.limit.unwrap_or(5).clamp(1, 10),
            },
        };
        emit_stream_event(&tiangong_types::StreamEvent::MemoryRecallStart {
            strategy: "auto".to_string(),
        });

        match sidecar_client::invoke::<RecallContext>(&request) {
            Ok(result) => {
                let hits = result
                    .response
                    .hits
                    .iter()
                    .map(|hit| tiangong_types::MemoryRecallHitSummary {
                        title: hit.title.clone(),
                        summary: hit.summary.clone(),
                        score: hit.score,
                    })
                    .collect::<Vec<_>>();
                emit_stream_event(&tiangong_types::StreamEvent::MemoryRecallDone {
                    hit_count: hits.len(),
                    hits,
                });
                Ok(assemble_recall_result(result.response, query, started))
            }
            Err(sidecar_client::ClientError::NotConfigured)
            | Err(sidecar_client::ClientError::Unavailable(_)) => {
                emit_done_empty();
                Ok(recall_tool_result(
                    false,
                    "记忆系统未启用",
                    String::new(),
                    "memory disabled".to_string(),
                    1,
                    Vec::new(),
                    started,
                ))
            }
            Err(error) => {
                emit_done_empty();
                Ok(recall_tool_result(
                    false,
                    "记忆查询失败",
                    String::new(),
                    error.to_string(),
                    1,
                    vec![query],
                    started,
                ))
            }
        }
    }

    fn shutdown() -> Result<(), PluginError> {
        Ok(())
    }

    fn set_workspace(_workspace: Option<String>, _full_trust: bool) -> Result<(), PluginError> {
        Ok(())
    }

    fn on_config_updated(_config_json: String) -> Result<(), PluginError> {
        // Core 的聊天配置不属于 Memory 配置。后台在启动时读取自己的配置，
        // 设置页保存时 SetConfig 已直接完成重载；新会话和每轮发送无需唤醒它。
        Ok(())
    }

    // ── 生命周期钩子 ──
    //
    // session-json 为宿主 Session 的只读快照（可序列化部分）。
    // WASM 从中提取 memory 需要的数据，经 request 转发到 sidecar。
    // session 的所有修改权始终在 Core，WASM/ sidecar 绝不回写。

    fn on_session_ready(session_json: String) -> Result<(), PluginError> {
        // 会话就绪：缓存 PluginSession 供 handle-tool 构建回忆 context。
        if let Ok(session) = serde_json::from_str::<tiangong_types::PluginSession>(&session_json) {
            state::set_last_session(session);
        }
        Ok(())
    }

    fn on_turn_started(session_json: String, _turn_start_idx: u32) -> Result<(), PluginError> {
        // 轮次开始：更新 session 缓存 + 重置本轮召回标志。
        if let Ok(session) = serde_json::from_str::<tiangong_types::PluginSession>(&session_json) {
            state::set_last_session(session);
        }
        state::reset_recall_attempted();
        Ok(())
    }

    fn on_turn_finished(session_json: String, turn_start_idx: u32) -> Result<(), PluginError> {
        // 轮次结束：从 session 只读快照提取本轮信息，转发给 sidecar 做 micro 反刍。
        forward_turn_rumination(&session_json, turn_start_idx)?;

        // 每 10 轮触发 Meta 反刍（与原生版本一致）。
        if state::increment_turn_and_check_meta() {
            let _ =
                sidecar_client::invoke::<RunMetaRumination>(&RunMetaRuminationRequest::default());
        }
        Ok(())
    }

    fn on_session_ended(session_json: String) -> Result<(), PluginError> {
        // 会话结束：从 session 提取 id/cwd，转发给 sidecar 做 meso 反刍。
        let _ = forward_session_rumination(&session_json);
        Ok(())
    }
}

// ── UI 能力（plugin-ui 接口），独立于 Core 使用的 plugin 接口 ──

impl UiGuest for Component {
    fn contributions() -> Result<Vec<Contribution>, PluginError> {
        Ok(vec![Contribution {
            id: "memory".to_string(),
            title: "记忆".to_string(),
            description: "记忆系统配置（模型端点、向量检索等）".to_string(),
            icon: "brain".to_string(),
            group: "plugins".to_string(),
            has_view: true,
        }])
    }

    fn open_view(contribution_id: String) -> Result<ViewResponse, PluginError> {
        if contribution_id != "memory" {
            return Err(PluginError::Message(format!(
                "未知的 contribution: {contribution_id}"
            )));
        }
        Ok(ViewResponse {
            html: memory_settings_html(),
        })
    }

    fn get_view_resource(path: String) -> Result<ResourceResponse, PluginError> {
        match path.as_str() {
            "memory.css" => Ok(ResourceResponse {
                data: MEMORY_PAGE_CSS.as_bytes().to_vec(),
                mime: "text/css; charset=utf-8".to_string(),
            }),
            "memory.js" => Ok(ResourceResponse {
                data: MEMORY_PAGE_JS.as_bytes().to_vec(),
                mime: "text/javascript; charset=utf-8".to_string(),
            }),
            _ => Err(PluginError::Message(format!("无此资源: {path}"))),
        }
    }

    fn handle_view_message(
        request: ViewMessageRequest,
    ) -> Result<ViewMessageResponse, PluginError> {
        let payload = match request.method.as_str() {
            "bootstrap" => invoke_for_ui::<ui::GetConfig>(&Empty {}),
            "save_config" => {
                let selection = serde_json::from_str::<ui::MemorySelection>(&request.payload)
                    .map_err(|error| {
                        PluginError::Message(format!("解析 Memory 配置失败: {error}"))
                    })?;
                invoke_for_ui::<ui::SetConfig>(&selection)
            }
            "probe_config" => {
                let probe = serde_json::from_str::<ui::ProbeRequest>(&request.payload).map_err(
                    |error| PluginError::Message(format!("解析 Memory 探测请求失败: {error}")),
                )?;
                invoke_for_ui::<ui::ProbeConfig>(&probe)
            }
            "memory_request" => forward_memory_ui_request(&request.payload),
            other => Err(PluginError::Message(format!("未知消息: {other}"))),
        }?;
        Ok(ViewMessageResponse { payload })
    }
}

fn invoke_for_ui<O>(request: &O::Request) -> Result<String, PluginError>
where
    O: MemoryOperation,
    O::Response: serde::Serialize,
{
    let response = sidecar_client::invoke::<O>(request)
        .map_err(|error| PluginError::Message(error.to_string()))?;
    serde_json::to_string(&response)
        .map_err(|error| PluginError::Message(format!("序列化 {} 响应失败: {error}", O::NAME)))
}

fn forward_memory_ui_request(payload: &str) -> Result<String, PluginError> {
    let request = serde_json::from_str::<UiRequest>(payload)
        .map_err(|error| PluginError::Message(format!("解析 Memory 页面请求失败: {error}")))?;
    match request {
        UiRequest::ListNodes { query } => {
            invoke_for_ui::<ui::ListNodes>(&ui::ListNodesRequest { query })
        }
        UiRequest::CountNodes { query } => {
            invoke_for_ui::<ui::CountNodes>(&ui::CountNodesRequest { query })
        }
        UiRequest::ListRelations { node_id } => {
            invoke_for_ui::<ui::ListRelations>(&ui::ListRelationsRequest { node_id })
        }
        UiRequest::ListRelationsBatch { node_ids } => {
            invoke_for_ui::<ui::ListRelationsBatch>(&ui::ListRelationsBatchRequest { node_ids })
        }
        UiRequest::UpsertManualMemory { draft } => {
            invoke_for_ui::<ui::UpsertManualMemory>(&ui::UpsertManualMemoryRequest { draft })
        }
        UiRequest::SetNodeStatus { node_id, status } => {
            invoke_for_ui::<ui::SetNodeStatus>(&ui::SetNodeStatusRequest { node_id, status })
        }
        UiRequest::UpsertRelation { draft } => {
            invoke_for_ui::<ui::UpsertRelation>(&ui::UpsertRelationRequest { draft })
        }
        UiRequest::DeleteRelation { relation_id } => {
            invoke_for_ui::<ui::DeleteRelation>(&ui::DeleteRelationRequest { relation_id })
        }
        UiRequest::Recall { anchors, limit } => {
            invoke_for_ui::<Recall>(&RecallRequest { anchors, limit })
        }
    }
}

/// 从 session 快照提取本轮信息，转发给 sidecar 做 enhanced micro 反刍。
///
/// 从 PluginSession 提取本轮完整内容，组装成 EnhancedTurnResult 转发给 sidecar。
///
/// 排除规则：
/// - CompressedResume 消息不算轮次起点
/// - System/Notice 通知不进入任何提取
/// - recall_memory 等只读工具结果不生成候选（避免召回结果回写）
fn forward_turn_rumination(session_json: &str, turn_start_idx: u32) -> Result<(), PluginError> {
    let session: tiangong_types::PluginSession = serde_json::from_str(session_json)
        .map_err(|e| PluginError::Message(format!("解析 PluginSession 失败: {e}")))?;

    // 校验起点：必须是用户真实输入（User 且非宿主注入消息）。
    // HostInjected（插件反馈图片注入，RFC 0017）与 CompressedResume 都是
    // role=User 的模型上下文载体，不是用户意图，不得作为反刍轮次锚点。
    let Some(idx) = locate_turn_start(&session, turn_start_idx) else {
        return Ok(());
    };
    let all_messages = &session.messages;
    let start_msg = &all_messages[idx];

    let user_input = extract_message_text(start_msg);
    let messages = all_messages[idx..]
        .iter()
        .filter(|message| {
            !matches!(
                message.role,
                tiangong_types::MessageRole::System | tiangong_types::MessageRole::Notice
            )
        })
        .collect::<Vec<_>>();
    let turn_status = messages
        .iter()
        .rev()
        .find_map(|message| message.turn_status.as_ref())
        .map(|status| match status {
            tiangong_types::TurnStatus::Success => TurnStatus::Completed,
            tiangong_types::TurnStatus::Failed => TurnStatus::Failed,
            tiangong_types::TurnStatus::Cancelled => TurnStatus::Cancelled,
        })
        .unwrap_or_default();
    let turn_result =
        turn_extract::build_turn_memory_result(&session, &messages, &user_input, turn_status);
    let request = RunEnhancedMicroRuminationRequest { turn_result };
    sidecar_client::invoke::<RunEnhancedMicroRumination>(&request)
        .map(|_| ())
        .map_err(|error| PluginError::Message(format!("提交 Memory 增强反刍任务失败: {error}")))
}

/// 从 session 快照提取 id/cwd，转发给 sidecar 做 meso 反刍。
fn forward_session_rumination(session_json: &str) -> Result<(), PluginError> {
    let session: tiangong_types::PluginSession = serde_json::from_str(session_json)
        .map_err(|e| PluginError::Message(format!("解析 PluginSession 失败: {e}")))?;

    let request = RunMesoRuminationRequest {
        session_id: session.id,
        workspace_id: session.workspace_id,
    };
    let _ = sidecar_client::invoke::<RunMesoRumination>(&request);
    Ok(())
}

/// 定位本轮起始消息（on_turn_finished 反刍路径共用）。
///
/// 优先按本轮起始消息 ID 定位（不受快照消息增删影响）；旧宿主未提供时回退
/// idx。定位后必须校验：起点应是用户真实输入——`MessagePhase::is_user_input`
/// 排除宿主注入的 role=User 消息（`HostInjected` 图片注入、`CompressedResume`
/// 压缩恢复锚点），它们不是用户意图，作为锚点会产生空 user_input 的异常
/// 反刍数据。
fn locate_turn_start(
    session: &tiangong_types::PluginSession,
    turn_start_idx: u32,
) -> Option<usize> {
    let idx = match &session.turn_start_message_id {
        Some(id) => session
            .messages
            .iter()
            .position(|msg| &msg.id == id)
            .unwrap_or(turn_start_idx as usize),
        None => turn_start_idx as usize,
    };
    session
        .messages
        .get(idx)
        .is_some_and(|m| {
            matches!(m.role, tiangong_types::MessageRole::User) && m.phase.is_user_input()
        })
        .then_some(idx)
}

/// 提取最近一条用户真实输入文本，作为 recall_memory 缺省查询兜底。
///
/// 从后往前找 role=User 且 `phase.is_user_input()` 的消息，并跳过无文本的
/// 消息（如仅含插件反馈图片的 HostInjected 注入消息、纯图片用户输入），
/// 避免把空字符串当成兜底查询。
fn latest_user_query(session: &tiangong_types::PluginSession) -> String {
    session
        .messages
        .iter()
        .rev()
        .filter(|message| {
            matches!(message.role, tiangong_types::MessageRole::User)
                && message.phase.is_user_input()
        })
        .find_map(|message| {
            let text = extract_message_text(message);
            (!text.trim().is_empty()).then_some(text)
        })
        .unwrap_or_default()
}

/// 从 Message 的 ContentBlock 列表中提取文本内容。
/// Message.content 是 Vec<ContentBlock>，需要遍历 Text 块拼接。
fn extract_message_text(message: &tiangong_types::Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            tiangong_types::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn emit_stream_event(event: &tiangong_types::StreamEvent) {
    if let Ok(json) = serde_json::to_string(event) {
        bindings::tiangong::plugin::feedback::emit_stream_event(&json);
    }
}

fn emit_done_empty() {
    emit_stream_event(&tiangong_types::StreamEvent::MemoryRecallDone {
        hit_count: 0,
        hits: Vec::new(),
    });
}

fn emit_skip_events() {
    emit_stream_event(&tiangong_types::StreamEvent::MemoryRecallStart {
        strategy: "skip".to_string(),
    });
    emit_done_empty();
}

fn build_recall_context(session: &tiangong_types::PluginSession) -> Vec<String> {
    let mut items = session
        .messages
        .iter()
        .rev()
        .filter_map(|message| {
            let role = match message.role {
                tiangong_types::MessageRole::User => "user",
                tiangong_types::MessageRole::Assistant => "assistant",
                tiangong_types::MessageRole::System => return None,
                tiangong_types::MessageRole::Notice => return None,
                tiangong_types::MessageRole::Tool => message.tool_name.as_deref().unwrap_or("tool"),
            };
            let content = compact_memory_text(&message.text_content(), 900);
            (!content.is_empty()).then(|| format!("{role}: {content}"))
        })
        .take(30)
        .collect::<Vec<_>>();
    items.reverse();
    items
}

fn compact_memory_text(text: &str, max_chars: usize) -> String {
    let normalized = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    let mut clipped = normalized.chars().take(max_chars).collect::<String>();
    clipped.push_str("...");
    clipped
}

fn assemble_recall_result(
    response: tiangong_plugin_memory_protocol::recall::RecallContextResponse,
    query: String,
    started: u64,
) -> ToolResult {
    if response.hits.is_empty() {
        let stdout = if response.content.trim().is_empty() {
            format!("未找到与「{query}」相关的历史记忆。")
        } else {
            response.content
        };
        return recall_tool_result(
            true,
            "未找到相关记忆",
            format!(
                "recall_memory 已完成，但没有可用的增量历史记忆。请基于当前上下文继续完成用户原始目标；不要再次调用 recall_memory。\n\n{stdout}"
            ),
            String::new(),
            0,
            vec![query],
            started,
        );
    }

    let stdout = if response.content.trim().is_empty() {
        "没有发现当前上下文之外的增量记忆。".to_string()
    } else {
        response.content
    };
    let header = if stdout
        .trim()
        .starts_with("没有发现当前上下文之外的增量记忆")
    {
        "recall_memory 已完成，结果如下。请基于当前上下文继续完成用户原始目标；不要再次调用 recall_memory。"
    } else {
        "recall_memory 已完成。以下是可直接使用的回忆结果，请基于这些内容继续完成用户原始目标；不要再次调用 recall_memory，除非用户提出新的历史查询。"
    };
    recall_tool_result(
        true,
        format!("命中 {} 条相关记忆并完成整理", response.hits.len()),
        format!("{header}\n\n{stdout}"),
        String::new(),
        0,
        vec![query],
        started,
    )
}

fn recall_tool_result(
    ok: bool,
    summary: impl Into<String>,
    stdout: String,
    stderr: String,
    exit_code: i32,
    args: Vec<String>,
    started: u64,
) -> ToolResult {
    let summary = summary.into();
    ToolResult {
        ok,
        summary: summary.clone(),
        stdout,
        stderr,
        exit_code,
        execution: Some(ToolExecutionRecord {
            tool_name: "recall_memory".to_string(),
            args,
            duration_ms: bindings::tiangong::plugin::clock::now_millis().saturating_sub(started),
            ok,
            exit_code,
            summary,
        }),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn test_session(messages: Vec<tiangong_types::Message>) -> tiangong_types::PluginSession {
        tiangong_types::PluginSession {
            id: "s-1".to_string(),
            title: String::new(),
            cwd: "/tmp/ws".to_string(),
            workspace_id: "ws".to_string(),
            parent_session_id: None,
            turn_start_message_id: None,
            reasoning_effort: None,
            messages,
            context_summary: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// 构造 RFC 0017 的插件反馈图片注入消息：role=User、phase=HostInjected，
    /// 内容为 ModelInstruction（provenance）+ Image 块，`text_content()` 为空。
    fn host_injected_image_message() -> tiangong_types::Message {
        let mut message =
            tiangong_types::Message::new(tiangong_types::MessageRole::User, String::new());
        message.content = vec![
            tiangong_types::ContentBlock::model_instruction(
                "[injected-images provenance]\n- 工具 desktop_screenshot（call-1）产出图片 shot.png",
            ),
            tiangong_types::ContentBlock::image(
                tiangong_types::StoredAsset {
                    asset_id: "a-1".to_string(),
                    local_path: "/media/shot.png".to_string(),
                    original_name: "shot.png".to_string(),
                    mime_type: "image/png".to_string(),
                    size: 1024,
                    kind: tiangong_types::MediaKind::Image,
                },
                None,
            ),
        ];
        message.with_phase(tiangong_types::MessagePhase::HostInjected)
    }

    #[test]
    fn latest_user_query_skips_host_injected_image_message() {
        let session = test_session(vec![
            tiangong_types::Message::new(tiangong_types::MessageRole::User, "帮我看下这张截图"),
            host_injected_image_message(),
        ]);
        // 注入图片消息位于最后且文本为空：兜底查询必须回落到真实用户输入，
        // 而不是把空字符串当作查询导致 recall_memory 直接失败。
        assert_eq!(latest_user_query(&session), "帮我看下这张截图");
    }

    #[test]
    fn latest_user_query_returns_empty_when_only_injected_messages() {
        let session = test_session(vec![host_injected_image_message()]);
        assert_eq!(latest_user_query(&session), "");
    }

    #[test]
    fn locate_turn_start_rejects_host_injected_anchor() {
        let user = tiangong_types::Message::new(tiangong_types::MessageRole::User, "真实用户输入");
        let injected = host_injected_image_message();
        let mut session = test_session(vec![user.clone(), injected]);
        // 旧宿主回退 idx 路径指向注入消息：不得作为轮次锚点。
        assert_eq!(locate_turn_start(&session, 1), None);
        // 按消息 ID 定位到真实用户消息：正常放行。
        session.turn_start_message_id = Some(user.id.clone());
        assert_eq!(locate_turn_start(&session, 0), Some(0));
    }

    #[test]
    fn locate_turn_start_still_rejects_compressed_resume() {
        let resume =
            tiangong_types::Message::new(tiangong_types::MessageRole::User, "压缩恢复锚点")
                .with_phase(tiangong_types::MessagePhase::CompressedResume);
        let session = test_session(vec![resume]);
        assert_eq!(locate_turn_start(&session, 0), None);
    }
}

bindings::export!(Component with_types_in bindings);
