//! 中枢模式状态机（纯逻辑，无宿主依赖）。
//!
//! 从 `tiangong-app` 的中枢实现中抽离的可测试核心：后台工作的状态集、
//! 终态转移规则（取消/暂停守卫、等待用户分流）、登记保留策略、回报
//! 时机判定与失败分类。宿主（app.rs 执行器）只做 IO 与投递，全部状态
//! 决策经本 crate——组合路径可在无 tauri 环境下全量测试。

use serde::{Deserialize, Serialize};

pub mod persistence;
pub use persistence::*;

/// 后台工作状态（设计稿 §8.1 的实现子集 + 控制态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    Running,
    ReportQueued,
    Completed,
    Failed,
    Interrupted,
    WaitingUser,
    Paused,
    Cancelled,
    Queued,
}

impl WorkState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::ReportQueued => "report_queued",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::WaitingUser => "waiting_user",
            Self::Paused => "paused",
            Self::Cancelled => "cancelled",
            Self::Queued => "queued",
        }
    }

    /// 是否终态（不再转移，除非重派覆盖）。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }

    /// 是否「在途」（占用执行资源：依赖判定视为未终态）。
    pub fn is_inflight(self) -> bool {
        matches!(self, Self::Running | Self::WaitingUser | Self::Paused)
    }

    /// 解析持久化字符串（未知值归 None，调用方决定回退）。
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "running" => Self::Running,
            "report_queued" => Self::ReportQueued,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "interrupted" => Self::Interrupted,
            "waiting_user" => Self::WaitingUser,
            "paused" => Self::Paused,
            "cancelled" => Self::Cancelled,
            "queued" => Self::Queued,
            _ => return None,
        })
    }
}

/// 状态登记条目（徽章与决策的数据源；宿主附加 IO 字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkStatus {
    pub work_id: String,
    pub session_id: String,
    pub task: String,
    pub model: Option<String>,
    pub dispatched_at: String,
    pub state: WorkState,
    pub reused_session: bool,
    #[serde(default)]
    pub last_activity: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// 工作回报结局（终态回调入参的分类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkOutcome {
    /// 正常终态（成功或失败由 `done` 区分）。
    Finished,
    /// 等待用户决策：回报含【需要用户决策】标记，工作未终态。
    WaitingUser,
}

/// 依据回报文本判定结局（Worker 用户决策协议）。
pub fn classify_outcome(notice: &str) -> WorkOutcome {
    if notice.contains("【需要用户决策】") {
        WorkOutcome::WaitingUser
    } else {
        WorkOutcome::Finished
    }
}

/// 终态转移决策：综合当前状态、回报结局与成败，给出登记应进入的状态。
///
/// 规则（行为锁定）：
/// - `cancelled`/`paused`：用户控制优先，流终态（Cancel→Done）不得覆盖。
/// - `waiting_user` 结局：进入 `waiting_user`（未终态，答复后恢复）。
/// - 否则按成败：成功 → `report_queued`，失败 → `failed`。
pub fn transition_on_finished(
    current: Option<WorkState>,
    outcome: WorkOutcome,
    done: bool,
) -> WorkState {
    match current {
        Some(WorkState::Cancelled) => WorkState::Cancelled,
        Some(WorkState::Paused) => WorkState::Paused,
        _ => match outcome {
            WorkOutcome::WaitingUser => WorkState::WaitingUser,
            WorkOutcome::Finished => {
                if done {
                    WorkState::ReportQueued
                } else {
                    WorkState::Failed
                }
            }
        },
    }
}

/// 回报注入时机：失败必达；成功仅在无其他在执行任务时触发汇总轮。
pub fn should_deliver_now(done: bool, has_running_work: bool) -> bool {
    !done || !has_running_work
}

/// 登记保留策略：completed 与 failed 各保留最近 `keep` 条（从最旧移除）。
/// 返回应移除的条目索引（升序；宿主按索引删除）。
pub fn retention_removals(entries: &[WorkStatus], keep: usize) -> Vec<usize> {
    let mut removals = Vec::new();
    for state in [WorkState::Completed, WorkState::Failed] {
        let indexes: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.state == state)
            .map(|(index, _)| index)
            .collect();
        if indexes.len() > keep {
            let overflow = indexes.len() - keep;
            removals.extend(indexes.into_iter().take(overflow));
        }
    }
    removals.sort_unstable();
    removals
}

/// 派发排队决策：依赖（after_work_id）与同目录写入锁的统一判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueDecision {
    /// 立即派发。
    DispatchNow,
    /// 排队：等待 `work_id` 终态后投递。
    QueueAfter(String),
}

/// 判定新派发是否需要排队。
///
/// - 显式依赖 `after` 在途（running/waiting_user/paused/queued）→ 排队跟随；
/// - 写入锁：同 `cwd` 已有其他 running 工作（未声明 `parallel` 豁免、
///   非同会话重派）→ 排队跟随占用者；
/// - 其余 → 立即派发。
pub fn queue_decision(
    entries: &[WorkStatus],
    after: Option<&str>,
    session_id: &str,
    cwd: &str,
    parallel: bool,
) -> QueueDecision {
    if let Some(after) = after.map(str::trim).filter(|id| !id.is_empty()) {
        let dependency = entries
            .iter()
            .rev()
            .find(|entry| entry.work_id == after || entry.session_id == after);
        let dependency_inflight = dependency
            .map(|entry| entry.state.is_inflight() || entry.state == WorkState::Queued)
            .unwrap_or(false);
        if dependency_inflight {
            return QueueDecision::QueueAfter(after.to_string());
        }
    }
    if !parallel {
        if let Some(holder) = entries.iter().rev().find(|entry| {
            entry.state == WorkState::Running
                && entry.cwd.as_deref() == Some(cwd)
                && entry.session_id != session_id
        }) {
            return QueueDecision::QueueAfter(holder.work_id.clone());
        }
    }
    QueueDecision::DispatchNow
}

/// 等待用户恢复：目标会话存在 waiting_user 条目时，答复轮的终态应归属
/// 该主工作——返回其 (work_id, task)；否则 None（常规派发/引导）。
pub fn merge_waiting_user(
    entries: &mut [WorkStatus],
    session_id: &str,
) -> Option<(String, String)> {
    let position = entries.iter().rposition(|entry| {
        entry.session_id == session_id && entry.state == WorkState::WaitingUser
    })?;
    entries[position].state = WorkState::Running;
    Some((
        entries[position].work_id.clone(),
        entries[position].task.clone(),
    ))
}

/// 失败原因是否疑似模型/网络类（值得换模型重派）。
///
/// 状态码按「词边界 + HTTP 语境窗口」匹配（「500 个文件」不误判）。
pub fn failure_looks_model_related(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    const MARKERS: &[&str] = &[
        "timeout",
        "超时",
        "rate limit",
        "rate_limit",
        "限流",
        "quota",
        "配额",
        "unauthorized",
        "authentication",
        "认证",
        "connection",
        "连接",
        "eof",
        "dns",
        "tls",
        "ssl",
        "网络",
        "provider error",
        "model not",
        "context length",
        "上下文长度",
        "overloaded",
        "internal server error",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
        "too many requests",
        "请求过于频繁",
        "服务器错误",
    ];
    if MARKERS.iter().any(|marker| lower.contains(marker)) {
        return true;
    }
    const CODES: &[&str] = &["429", "401", "403", "500", "502", "503", "504"];
    const CONTEXT: &[&str] = &[
        "http",
        "api",
        "status",
        "code",
        "error",
        "错误",
        "状态码",
        "服务器",
        "请求",
    ];
    let bytes = lower.as_bytes();
    CODES.iter().any(|code| {
        lower.match_indices(code).any(|(idx, matched)| {
            let before_ok = idx == 0 || !lower[..idx].ends_with(|c: char| c.is_ascii_digit());
            let end = idx + matched.len();
            let after_ok =
                end >= lower.len() || !lower[end..].starts_with(|c: char| c.is_ascii_digit());
            if !(before_ok && after_ok) {
                return false;
            }
            let window_start = char_safe_start(&lower, idx.saturating_sub(16));
            let window_end = char_safe_end(&lower, (end + 16).min(lower.len()));
            let window = &lower[window_start..window_end];
            let _ = bytes;
            CONTEXT.iter().any(|context| window.contains(context))
        })
    })
}

/// 字节偏移回退到最近的 UTF-8 字符边界。
fn char_safe_start(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// 字节偏移前进到最近的 UTF-8 字符边界。
fn char_safe_end(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}
