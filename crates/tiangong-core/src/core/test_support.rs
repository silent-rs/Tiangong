//! Core 集成测试共享基础设施：真实 `TiangongCore` 实例 + wiremock 假 LLM。
//!
//! 全部用例从 `deliver()` 发起，断言落在公开可见结果上：StreamEvent 事件流
//! 与磁盘 Session 终态。临时目录由 [`TestEnv`] 自动清理；事件等待保留全部
//! 已收到事件（不清空），模型请求以结构化方式解析（不做裸字符串包含判断）。

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tiangong_types::{StreamEvent, TurnStatus};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::agent_input::{AgentInput, AgentInputKind};
use crate::config::core::{CoreConfig, CoreConfigProvider};
use crate::core::TiangongCore;
use crate::permission::TrustMode;
use crate::session::Session;

pub const WAIT: Duration = Duration::from_secs(10);
pub const POLL: Duration = Duration::from_millis(10);

/// 按 session 隔离的一次性持久化失败槽。
///
/// 测试先登记 session ID，下一次该 session 的 `try_persist_to_disk` 消费登记并失败；
/// 其他 session 和后续补偿写入不受影响。
static NEXT_PERSISTENCE_FAILURES: std::sync::OnceLock<Mutex<HashSet<String>>> =
    std::sync::OnceLock::new();

fn next_persistence_failures() -> &'static Mutex<HashSet<String>> {
    NEXT_PERSISTENCE_FAILURES.get_or_init(|| Mutex::new(HashSet::new()))
}

static PERSISTENT_PERSISTENCE_FAILURES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::OnceLock::new();

fn persistent_persistence_failures() -> &'static std::sync::Mutex<std::collections::HashSet<String>>
{
    PERSISTENT_PERSISTENCE_FAILURES
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// 安排指定 session 的**持续性**持久化失败（补偿保存也失败）。
pub fn fail_all_persistence_for_session(sid: &str) {
    persistent_persistence_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(sid.to_string());
}

/// 清除持续性失败登记（测试收尾恢复磁盘可用）。
pub fn clear_persistent_persistence_failure(sid: &str) {
    persistent_persistence_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(sid);
}

pub(crate) fn is_persistence_persistently_failing(sid: &str) -> bool {
    persistent_persistence_failures()
        .lock()
        .map(|set| set.contains(sid))
        .unwrap_or(false)
}

/// 安排指定 session 的下一次持久化失败。
pub fn fail_next_persistence_for_session(sid: &str) {
    next_persistence_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(sid.to_string());
}

/// 由 `Session::try_persist_to_disk` 消费一次性失败登记。
pub(crate) fn take_persistence_failure_for_session(sid: &str) -> bool {
    next_persistence_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(sid)
}

/// 测试同步屏障：候选生成后与 turn 最终收尾前提供确定性同步
/// 提供确定性同步。
///
/// 协议：测试端 [`arm_seal`]/[`arm_turn_finish`] 按 session 预置；Core 到达屏障点
/// 后 ack 并阻塞等待 release；测试端收到 ack（窗口已冻结）后投递消息，
/// 再调用 [`SealHandle::release`] 让 Core 继续。按 session 多槽，并行测试互不干扰；
/// 未预置的 session 直接通过（不影响不用屏障的测试）。
/// 测试端持有的屏障句柄：`ack` 在 Core 到达冻结点时收到信号，
/// `release()` 解除冻结。
pub struct SealHandle {
    ack_rx: std::sync::mpsc::Receiver<()>,
    release_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl SealHandle {
    /// 等待 Core 到达冻结点（屏障窗口已冻结）。
    pub fn wait_frozen(&self) {
        self.ack_rx.recv_timeout(WAIT).expect("等待屏障冻结超时");
    }

    /// 解除冻结（重复调用安全）。
    pub fn release(&mut self) {
        if let Some(tx) = self.release_tx.take() {
            let _ = tx.send(());
        }
    }
}

struct SealGate {
    armed: std::sync::Mutex<std::collections::HashMap<String, GateChannel>>,
}

struct GateChannel {
    /// ack 发送到测试端；测试端 drop（panic/提前退出）时 send 失败，
    /// 屏障立即自动解除（不残留冻结的 driver）。
    ack_tx: std::sync::mpsc::Sender<()>,
    /// tokio oneshot：drop（测试结束）或 send（release）都解除等待。
    release_rx: tokio::sync::oneshot::Receiver<()>,
}

impl SealGate {
    fn new() -> Self {
        Self {
            armed: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn arm(&self, sid: &str) -> SealHandle {
        let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        self.armed
            .lock()
            .unwrap()
            .insert(sid.to_string(), GateChannel { ack_tx, release_rx });
        SealHandle {
            ack_rx,
            release_tx: Some(release_tx),
        }
    }
}

/// 进程级测试屏障注册表（按 session 多槽，并行测试互不干扰）。
static SEAL_GATES: std::sync::OnceLock<SealGates> = std::sync::OnceLock::new();

struct SealGates {
    seal: SealGate,
    turn_finish: SealGate,
}

fn seal_gates() -> &'static SealGates {
    SEAL_GATES.get_or_init(|| SealGates {
        seal: SealGate::new(),
        turn_finish: SealGate::new(),
    })
}

async fn wait_gate(gate: &SealGate, sid: &str) {
    let taken = gate.armed.lock().unwrap().remove(sid);
    let Some(gate) = taken else {
        return;
    };
    // 测试端已退出（panic 等）：解除冻结，Core 继续。
    if gate.ack_tx.send(()).is_err() {
        return;
    }
    let _ = gate.release_rx.await;
}

/// 候选已经生成、提交尚未开始。屏障只等待独立
/// 释放信号，不读取命令通道。
pub async fn seal_barrier(sid: &str) {
    wait_gate(&seal_gates().seal, sid).await;
}

/// Agent Loop 已提交结果、turn 尚未收尾。外部用户
/// 消息只能进入待执行单槽。屏障不消费 Cancel/Shutdown 等真实命令。
pub async fn turn_finish_barrier(sid: &str) {
    wait_gate(&seal_gates().turn_finish, sid).await;
}

/// 测试端：预置封口屏障（候选已生成、提交未开始的窗口）。
pub fn arm_seal(sid: &str) -> SealHandle {
    seal_gates().seal.arm(sid)
}

/// 测试端：预置 turn 收尾屏障。
pub fn arm_turn_finish(sid: &str) -> SealHandle {
    seal_gates().turn_finish.arm(sid)
}

/// 自动清理的测试环境：持有临时目录直到测试结束。
pub struct TestEnv {
    pub root: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl TestEnv {
    pub fn new(tag: &str) -> (Self, String) {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let env = Self {
            root: dir.path().to_path_buf(),
            _dir: dir,
        };
        (env, format!("it-{tag}-{}", scru128::new_string()))
    }

    pub fn load_session(&self, sid: &str) -> Session {
        Session::load_from_storage(&self.root, sid).expect("加载 session 失败")
    }
}

/// 事件日志：等待方法保留全部已收到事件，断言可反复检视完整历史。
pub struct EventLog {
    rx: Receiver<StreamEvent>,
    seen: Vec<StreamEvent>,
}

impl EventLog {
    pub fn new(rx: Receiver<StreamEvent>) -> Self {
        Self {
            rx,
            seen: Vec::new(),
        }
    }

    /// 非阻塞收取当前积压事件（保留进历史）。
    pub fn pump(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            self.seen.push(event);
        }
    }

    pub fn seen(&self) -> &[StreamEvent] {
        &self.seen
    }

    /// 带超时等待谓词命中；超时失败时附带已收到事件摘要。
    pub fn wait_until(&mut self, pred: impl Fn(&StreamEvent) -> bool, desc: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            self.pump();
            if self.seen.iter().any(&pred) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "等待事件超时：{desc}；已收到：{}",
                self.summarize()
            );
            std::thread::sleep(POLL);
        }
    }

    pub fn wait_done_count(&mut self, expected: usize) {
        let deadline = Instant::now() + WAIT;
        loop {
            self.pump();
            if self
                .seen
                .iter()
                .filter(|event| matches!(event, StreamEvent::Done { .. }))
                .count()
                >= expected
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "等待 {expected} 个成功终态超时；已收到：{}",
                self.summarize()
            );
            std::thread::sleep(POLL);
        }
    }

    pub fn wait_done(&mut self) {
        self.wait_done_count(1);
    }

    pub fn wait_cancelled(&mut self) {
        self.wait_until(
            |event| matches!(event, StreamEvent::Error { message } if message.contains("已取消")),
            "取消终态",
        );
    }

    /// 等待包含指定文本的错误事件（终态或提示）。
    pub fn wait_error_containing(&mut self, text: &str) {
        self.wait_until(
            |e| matches!(e, StreamEvent::Error { message } if message.contains(text)),
            &format!("含“{text}”的错误事件"),
        );
    }

    /// 等待指定动作的压缩完成事件，返回事件中的边界与剩余消息数
    ///（供与磁盘最终状态核对一致）。
    pub fn wait_context_compressed(
        &mut self,
        action: tiangong_types::stream::ContextCompressAction,
    ) -> (usize, usize) {
        let expected = action.clone();
        let label = format!("{action:?}");
        self.wait_until(
            move |e| {
                matches!(e, StreamEvent::ContextCompressed { action: a, .. } if *a == expected)
            },
            &format!("压缩完成事件（action={label}）"),
        );
        self.seen()
            .iter()
            .find_map(|e| {
                if let StreamEvent::ContextCompressed {
                    action: a,
                    summary_up_to,
                    remaining_messages,
                } = e
                {
                    (*a == action).then_some((*summary_up_to, *remaining_messages))
                } else {
                    None
                }
            })
            .expect("刚等待到的压缩事件必须存在")
    }

    fn count_errors(&mut self) -> usize {
        self.pump();
        self.seen()
            .iter()
            .filter(|e| matches!(e, StreamEvent::Error { .. }))
            .count()
    }

    fn count_done(&mut self) -> usize {
        self.pump();
        self.seen
            .iter()
            .filter(|e| matches!(e, StreamEvent::Done { .. }))
            .count()
    }

    fn count_cancelled(&mut self) -> usize {
        self.pump();
        self.seen
            .iter()
            .filter(|e| matches!(e, StreamEvent::Error { message } if message.contains("已取消")))
            .count()
    }

    /// 多轮成功场景：Done 总数恰为 n，且没有任何 Error。
    pub fn assert_done_count(&mut self, n: usize) {
        let done = self.count_done();
        assert_eq!(
            done,
            n,
            "Done 终态应恰为 {n} 个（每轮一次）；已收到：{}",
            self.summarize()
        );
        assert_eq!(
            self.count_errors(),
            0,
            "成功场景不得出现任何错误；已收到：{}",
            self.summarize()
        );
    }

    /// 终态唯一性（成功）：恰好一个 Done，且没有任何 Error。
    pub fn assert_single_success_terminal(&mut self) {
        let done = self.count_done();
        assert_eq!(
            done,
            1,
            "成功场景必须恰好一个 Done 终态；已收到：{}",
            self.summarize()
        );
        assert_eq!(
            self.count_errors(),
            0,
            "成功场景不得出现任何错误；已收到：{}",
            self.summarize()
        );
    }

    /// 终态唯一性（失败）：无 Done，全部 Error 恰好一个且包含目标文案。
    pub fn assert_single_failure_terminal(&mut self, text: &str) {
        let done = self.count_done();
        assert_eq!(
            done,
            0,
            "失败场景不得出现 Done；已收到：{}",
            self.summarize()
        );
        assert_eq!(
            self.count_errors(),
            1,
            "失败场景必须恰好一个错误终态；已收到：{}",
            self.summarize()
        );
        let matching = self
            .seen
            .iter()
            .filter(|e| matches!(e, StreamEvent::Error { message } if message.contains(text)))
            .count();
        assert_eq!(
            matching,
            1,
            "唯一失败终态必须包含目标文案；已收到：{}",
            self.summarize()
        );
    }

    /// 终态唯一性（取消）：恰好一个取消终态，无 Done，无其他错误。
    pub fn assert_single_cancelled_terminal(&mut self) {
        assert_eq!(
            self.count_cancelled(),
            1,
            "取消终态应恰好一次；已收到：{}",
            self.summarize()
        );
        assert_eq!(
            self.count_done(),
            0,
            "取消场景不得出现 Done；已收到：{}",
            self.summarize()
        );
        assert_eq!(
            self.count_errors(),
            1,
            "取消场景除取消终态外不得有其他错误；已收到：{}",
            self.summarize()
        );
    }

    fn summarize(&self) -> String {
        self.seen.iter().map(name_of).collect::<Vec<_>>().join(",")
    }
}

fn name_of(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::Done { .. } => "Done",
        StreamEvent::Error { .. } => "Error",
        StreamEvent::UserMessage { .. } => "UserMessage",
        StreamEvent::ToolStart { .. } => "ToolStart",
        StreamEvent::ToolResult { .. } => "ToolResult",
        StreamEvent::ContextCompressing { .. } => "ContextCompressing",
        StreamEvent::ContextCompressed { .. } => "ContextCompressed",
        StreamEvent::TitleChanged { .. } => "TitleChanged",
        StreamEvent::Delta { .. } => "Delta",
        StreamEvent::ReactText { .. } => "ReactText",
        StreamEvent::SummaryText { .. } => "SummaryText",
        StreamEvent::TokenUsage { .. } => "TokenUsage",
        _ => "其他",
    }
}

/// 结构化的模型请求正文。
pub struct RequestBody {
    value: serde_json::Value,
}

impl RequestBody {
    pub fn messages(&self) -> Vec<&serde_json::Value> {
        self.value
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    }

    /// 指定角色的消息 content 文本是否包含 needle
    ///（content 为字符串或 blocks 数组时均做结构化遍历）。
    pub fn role_message_contains(&self, role: &str, needle: &str) -> bool {
        self.messages().iter().any(|message| {
            message.get("role").and_then(|r| r.as_str()) == Some(role)
                && content_text(message).is_some_and(|text| text.contains(needle))
        })
    }

    pub fn any_message_contains(&self, needle: &str) -> bool {
        self.messages()
            .iter()
            .any(|m| content_text(m).is_some_and(|t| t.contains(needle)))
    }

    pub fn latest_user_text(&self) -> Option<String> {
        self.messages().iter().rev().find_map(|message| {
            (message.get("role").and_then(|r| r.as_str()) == Some("user"))
                .then(|| content_text(message))
                .flatten()
        })
    }

    pub fn is_stream(&self) -> bool {
        self.value
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }

    pub fn is_compression(&self) -> bool {
        self.latest_user_text()
            .is_some_and(|text| text.contains("请压缩以上对话历史") && text.contains("[[SUMMARY]]"))
    }

    pub fn has_tool_result(&self, call_id: &str, needle: &str) -> bool {
        self.tool_results()
            .iter()
            .any(|(id, text)| id == call_id && text.contains(needle))
    }

    pub fn has_assistant_tool_call(&self, call_id: &str, name: &str) -> bool {
        self.assistant_tool_calls()
            .iter()
            .any(|(id, tool_name)| id == call_id && tool_name == name)
    }

    /// assistant 消息声明的工具调用（id, name）。
    pub fn assistant_tool_calls(&self) -> Vec<(String, String)> {
        let mut calls = Vec::new();
        for message in self.messages() {
            if message.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                continue;
            }
            if let Some(items) = message.get("tool_calls").and_then(|t| t.as_array()) {
                for item in items {
                    let id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let name = item
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    calls.push((id, name));
                }
            }
        }
        calls
    }

    /// 工具结果消息（tool_call_id, content 文本）。
    pub fn tool_results(&self) -> Vec<(String, String)> {
        self.messages()
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
            .map(|m| {
                (
                    m.get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    content_text(m).unwrap_or_default(),
                )
            })
            .collect()
    }

    /// 请求声明的工具定义名列表。
    pub fn defined_tools(&self) -> Vec<String> {
        self.value
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| {
                        t.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 请求是否允许模型调用工具（仅字符串 none 表示禁止）。
    pub fn allows_tool_calls(&self) -> bool {
        !self
            .value
            .get("tool_choice")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|choice| choice == "none")
    }
}

/// 提取消息 content 的文本（字符串直取；数组拼接各块的 text 字段）。
fn content_text(message: &serde_json::Value) -> Option<String> {
    match message.get("content")? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let mut text = String::new();
            for block in blocks {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
            Some(text)
        }
        _ => None,
    }
}

/// 读取 mock 服务器收到的第 idx 个 /chat/completions 请求（结构化）。
pub async fn chat_request_at(server: &MockServer, idx: usize) -> RequestBody {
    let requests = server.received_requests().await.expect("读取请求失败");
    let request = requests
        .iter()
        .filter(|r| r.url.path() == "/chat/completions")
        .nth(idx)
        .unwrap_or_else(|| panic!("第 {idx} 个模型请求尚未发出"));
    RequestBody {
        value: serde_json::from_slice(&request.body).expect("请求正文应为 JSON"),
    }
}

/// 记录调用并返回固定结果的测试工具（经插件注册进 Core）。
pub struct RecordingTool {
    pub name: &'static str,
    pub invocations: Mutex<Vec<tiangong_llm::tool::ToolCall>>,
    pub ok: bool,
}

impl RecordingTool {
    pub fn succeed(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            invocations: Mutex::new(Vec::new()),
            ok: true,
        })
    }

    pub fn count(&self) -> usize {
        self.invocations.lock().unwrap().len()
    }

    pub fn call_ids(&self) -> Vec<String> {
        self.invocations
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.id.clone())
            .collect()
    }
}

impl crate::tools::extension::ToolOverrideHandler for RecordingTool {
    fn handle(
        &self,
        call: &tiangong_llm::tool::ToolCall,
        _session: &mut Session,
        _actor_id: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<crate::tools::result::ToolResult>> + Send>,
    > {
        self.invocations.lock().unwrap().push(call.clone());
        let ok = self.ok;
        let name = self.name;
        Box::pin(async move {
            Some(crate::tools::result::ToolResult {
                ok,
                summary: format!("{name} 已执行"),
                stdout: "done".to_string(),
                stderr: String::new(),
                exit_code: i32::from(!ok),
                execution: None,
            })
        })
    }
}

/// 提供单一工具的测试插件。
pub struct ToolPlugin {
    pub id: &'static str,
    pub tool: Arc<RecordingTool>,
}

impl crate::tools::extension::ToolSpecProvider for ToolPlugin {
    fn tool_specs(&self) -> Vec<tiangong_llm::tool::ToolSpec> {
        vec![tiangong_llm::tool::ToolSpec {
            name: self.tool.name.to_string(),
            description: "测试工具".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }]
    }
}

impl crate::tools::extension::ToolOverrideHandler for ToolPlugin {
    fn handle(
        &self,
        call: &tiangong_llm::tool::ToolCall,
        session: &mut Session,
        actor_id: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<crate::tools::result::ToolResult>> + Send>,
    > {
        self.tool.handle(call, session, actor_id)
    }
}

impl crate::tools::extension::PromptSectionProvider for ToolPlugin {}
impl crate::core::plugin::Plugin for ToolPlugin {
    fn id(&self) -> &str {
        self.id
    }
}

// ── mock 挂载 ──────────────────────────────────────────────────────────

pub type RequestPredicate = Arc<dyn Fn(&RequestBody) -> bool + Send + Sync>;

#[derive(Clone)]
pub enum MockReply {
    Sse {
        chunks: Vec<serde_json::Value>,
        delay: Option<Duration>,
    },
    Completion {
        content: String,
        delay: Option<Duration>,
    },
    Error {
        status: u16,
        message: String,
    },
}

impl MockReply {
    pub fn sse(chunks: Vec<serde_json::Value>) -> Self {
        Self::Sse {
            chunks,
            delay: None,
        }
    }

    pub fn delayed_sse(chunks: Vec<serde_json::Value>, delay: Duration) -> Self {
        Self::Sse {
            chunks,
            delay: Some(delay),
        }
    }

    pub fn completion(content: impl Into<String>) -> Self {
        Self::Completion {
            content: content.into(),
            delay: None,
        }
    }

    pub fn delayed_completion(content: impl Into<String>, delay: Duration) -> Self {
        Self::Completion {
            content: content.into(),
            delay: Some(delay),
        }
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::Error {
            status,
            message: message.into(),
        }
    }

    fn response(&self) -> ResponseTemplate {
        match self {
            Self::Sse { chunks, delay } => {
                let mut response =
                    ResponseTemplate::new(200).set_body_raw(sse_body(chunks), "text/event-stream");
                if let Some(delay) = delay {
                    response = response.set_delay(*delay);
                }
                response
            }
            Self::Completion { content, delay } => {
                let body = serde_json::json!({
                    "id": "chatcmpl-it-compress",
                    "object": "chat.completion",
                    "model": "test-model",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": content},
                        "finish_reason": "stop",
                    }],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120},
                });
                let mut response = ResponseTemplate::new(200).set_body_json(body);
                if let Some(delay) = delay {
                    response = response.set_delay(*delay);
                }
                response
            }
            Self::Error { status, message } => {
                ResponseTemplate::new(*status).set_body_json(serde_json::json!({
                    "error": {"message": message, "type": "integration_test_error"}
                }))
            }
        }
    }
}

pub struct PromptRoute {
    name: &'static str,
    predicate: RequestPredicate,
    reply: MockReply,
    hits: Arc<AtomicUsize>,
}

impl PromptRoute {
    pub fn new(
        name: &'static str,
        predicate: impl Fn(&RequestBody) -> bool + Send + Sync + 'static,
        reply: MockReply,
    ) -> Self {
        Self {
            name,
            predicate: Arc::new(predicate),
            reply,
            hits: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[derive(Clone)]
pub struct PromptRouteHandle {
    name: &'static str,
    hits: Arc<AtomicUsize>,
}

impl PromptRouteHandle {
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub fn assert_hits(&self, expected: usize) {
        assert_eq!(
            self.hits(),
            expected,
            "prompt 路由 {} 命中次数不符",
            self.name
        );
    }
}

struct PromptRouter {
    routes: Vec<PromptRoute>,
}

impl Respond for PromptRouter {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let parsed = serde_json::from_slice::<serde_json::Value>(&request.body);
        let Ok(value) = parsed else {
            return MockReply::error(400, "mock 收到非 JSON 请求").response();
        };
        let body = RequestBody { value };
        let matched = self
            .routes
            .iter()
            .filter(|route| (route.predicate)(&body))
            .collect::<Vec<_>>();
        if let [route] = matched.as_slice() {
            route.hits.fetch_add(1, Ordering::SeqCst);
            return route.reply.response();
        }
        let latest_user = body.latest_user_text().unwrap_or_default();
        let route_names = matched
            .iter()
            .map(|route| route.name)
            .collect::<Vec<_>>()
            .join(",");
        MockReply::error(
            400,
            format!(
                "prompt 路由必须唯一匹配：matched=[{route_names}] stream={} compression={} latest_user={latest_user:?}",
                body.is_stream(),
                body.is_compression(),
            ),
        )
        .response()
    }
}

pub async fn mount_prompt_router(
    server: &MockServer,
    routes: Vec<PromptRoute>,
) -> HashMap<&'static str, PromptRouteHandle> {
    let handles = routes
        .iter()
        .map(|route| {
            (
                route.name,
                PromptRouteHandle {
                    name: route.name,
                    hits: route.hits.clone(),
                },
            )
        })
        .collect();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(PromptRouter { routes })
        .mount(server)
        .await;
    handles
}

pub fn latest_user_contains(text: &'static str) -> impl Fn(&RequestBody) -> bool {
    move |request| {
        request
            .latest_user_text()
            .is_some_and(|latest| latest.contains(text))
    }
}

pub fn stream_text_chunks(parts: &[&str]) -> Vec<serde_json::Value> {
    let mut chunks = parts
        .iter()
        .map(|part| text_delta_chunk(part))
        .collect::<Vec<_>>();
    chunks.push(finish_chunk("stop"));
    chunks.push(usage_delta_chunk(12, parts.len() as u64 + 1));
    chunks
}

pub fn stream_tool_call_chunks(
    call_id: &str,
    name: &str,
    argument_parts: &[&str],
) -> Vec<serde_json::Value> {
    let mut chunks = vec![tool_call_delta_chunk(call_id, name, "")];
    chunks.extend(
        argument_parts
            .iter()
            .map(|part| tool_call_delta_chunk("", "", part)),
    );
    chunks.push(finish_chunk("tool_calls"));
    chunks.push(usage_delta_chunk(15, 3));
    chunks
}

pub fn sse_body(chunks: &[serde_json::Value]) -> Vec<u8> {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {}\n\n", chunk));
    }
    body.push_str("data: [DONE]\n\n");
    body.into_bytes()
}

pub fn text_delta_chunk(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-it",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": content},
            "finish_reason": null,
        }],
    })
}

pub fn usage_delta_chunk(prompt: u64, completion: u64) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-it",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        },
    })
}

pub fn finish_chunk(reason: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-it",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": reason,
        }],
    })
}

pub fn tool_call_delta_chunk(call_id: &str, name: &str, arguments: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-it",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }],
            },
            "finish_reason": null,
        }],
    })
}

pub fn tool_call_chunk(call_id: &str, name: &str, arguments: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-it",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }],
            },
            "finish_reason": "tool_calls",
        }],
    })
}

pub async fn mount_sse(
    server: &MockServer,
    chunks: Vec<serde_json::Value>,
    delay: Option<Duration>,
) {
    let mut response =
        ResponseTemplate::new(200).set_body_raw(sse_body(&chunks), "text/event-stream");
    if let Some(delay) = delay {
        response = response.set_delay(delay);
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(response)
        .up_to_n_times(1)
        .mount(server)
        .await;
}

pub async fn mount_completion(server: &MockServer, content: &str, delay: Option<Duration>) {
    let body = serde_json::json!({
        "id": "chatcmpl-it-compress",
        "object": "chat.completion",
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120},
    });
    let mut response = ResponseTemplate::new(200).set_body_json(body);
    if let Some(delay) = delay {
        response = response.set_delay(delay);
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(response)
        .up_to_n_times(1)
        .mount(server)
        .await;
}

pub async fn mount_permanent_error(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"message": "integration test force fail", "type": "invalid_request_error"}
        })))
        .mount(server)
        .await;
}

// ── Core 构建与投递 ────────────────────────────────────────────────────

/// 构建真实 TiangongCore（非默认标题跳过 lite 标题生成）并保留事件接收端。
pub fn core_with(
    env: &TestEnv,
    sid: &str,
    endpoint: &str,
    trust_mode: TrustMode,
    plugins: Vec<Arc<dyn crate::core::plugin::Plugin>>,
) -> (TiangongCore, EventLog) {
    let mut session = Session::new("集成测试会话".to_string());
    session.id = sid.to_string();
    session.bind_storage_root(&env.root);
    session.try_persist_to_disk().expect("预落盘 session 失败");

    let config = CoreConfig::builder().with_trust_mode(trust_mode).build();
    let (event_tx, event_rx) = std::sync::mpsc::channel::<StreamEvent>();
    let core = TiangongCore::builder()
        .session_id(sid.to_string())
        .config(CoreConfigProvider::new(config))
        .trust_mode(trust_mode)
        .storage_root(env.root.clone())
        .workspace_dir(env.root.to_string_lossy())
        .stream_tx(event_tx)
        .plugins(plugins)
        .model_endpoint(test_model(endpoint))
        .build();
    (core, EventLog::new(event_rx))
}

/// 测试用的运行时模型端点。
pub fn test_model(endpoint: &str) -> tiangong_llm::ModelEndpoint {
    tiangong_llm::ModelEndpoint {
        base_url: endpoint.to_string(),
        api_key: "test-key".to_string(),
        model: "test-model".to_string(),
        ..Default::default()
    }
}

pub fn core_for(env: &TestEnv, sid: &str, endpoint: &str) -> (TiangongCore, EventLog) {
    core_with(env, sid, endpoint, TrustMode::FullTrust, Vec::new())
}

/// 使用现成模型 client 构建真实 TiangongCore，供 provider fake 测试复用。
pub fn core_for_client(
    env: &TestEnv,
    sid: &str,
    client: tiangong_llm::SingleProviderClient,
) -> (TiangongCore, EventLog) {
    let mut session = Session::new("集成测试会话".to_string());
    session.id = sid.to_string();
    session.bind_storage_root(&env.root);
    session.try_persist_to_disk().expect("预落盘 session 失败");

    let config = CoreConfig::builder()
        .with_trust_mode(TrustMode::FullTrust)
        .build();
    let (event_tx, event_rx) = std::sync::mpsc::channel::<StreamEvent>();
    let core = TiangongCore::builder()
        .session_id(sid.to_string())
        .config(CoreConfigProvider::new(config))
        .trust_mode(TrustMode::FullTrust)
        .storage_root(env.root.clone())
        .workspace_dir(env.root.to_string_lossy())
        .stream_tx(event_tx)
        .plugins(Vec::new())
        .model_endpoint(test_model("http://scripted-provider.invalid"))
        .test_client(client)
        .build();
    (core, EventLog::new(event_rx))
}

pub fn send_message(core: &TiangongCore, message_id: &str, text: &str) {
    core.deliver(AgentInputKind::prepared_with_id(
        message_id,
        vec![tiangong_types::ContentBlock::text(text)],
    ))
    .expect("消息投递应被接受");
}

/// 等待指定用户消息获得 turn 终态（磁盘 Session 权威）。
pub async fn wait_turn_status(env: &TestEnv, sid: &str, message_id: &str) -> TurnStatus {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = Session::load_from_storage(&env.root, sid)
            .ok()
            .and_then(|session| {
                session
                    .messages
                    .iter()
                    .find(|m| m.id == message_id)
                    .and_then(|m| m.turn_status)
                    .filter(|status| status.is_terminal())
            })
        {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "等待消息 {message_id} 的 turn 终态超时"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// 等待 mock 服务器收到至少 `n` 个请求（超时 panic）。
pub async fn wait_requests(server: &MockServer, n: usize) {
    let deadline = Instant::now() + WAIT;
    while server.received_requests().await.map_or(0, |r| r.len()) < n {
        assert!(Instant::now() < deadline, "等待 {n} 个模型请求超时");
        tokio::time::sleep(POLL).await;
    }
}

/// 非断言版：在预算内等到返回 true，否则 false（调用方按落点分支断言）。
pub async fn try_wait_requests(server: &MockServer, n: usize, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while server.received_requests().await.map_or(0, |r| r.len()) < n {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL).await;
    }
    true
}

/// 等待 driver 回到空闲（超时即失败）。
pub async fn wait_idle(sid: &str) {
    let deadline = Instant::now() + WAIT;
    while crate::shared_runtime::is_running(sid) && Instant::now() < deadline {
        tokio::time::sleep(POLL).await;
    }
    assert!(
        !crate::shared_runtime::is_running(sid),
        "等待 driver 空闲超时"
    );
}
