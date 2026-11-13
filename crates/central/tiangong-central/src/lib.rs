//! 天工「中枢模式」域框架（POC 骨架）。
//!
//! 依据 `docs/central-mode-design-draft.md`（设计基线，跟踪 Issue #773），
//! 在 `poc/central-mode` 分支上以 worktree 隔离开发。
//!
//! 本 crate 承载中枢模式的执行层基础框架：
//! - 后台工作状态机（设计稿 §8.1）；
//! - 工作事件类型与结构（§8.2 / §8.3）；
//! - 数据模型：用户目标、后台工作、报告、建议、模型协商、中枢代决（§13）；
//! - 风险分级（§14.1）；
//! - 中枢助手工具白名单与模型切换票据（§5.3 / §15.10）；
//! - JSON 文件原子持久化（沿用 `tiangong-core::session::atomic_replace_file`
//!   与 scheduler `JobStore` 的既有范式）。
//!
//! 明确不在本 crate 范围内（由后续开发片承接）：
//! - 中枢模式 UI 入口与激活问候（阶段 1 第一片）；
//! - Worker 调度、事件总线订阅与代决执行；
//! - 模型接力的实际切换调用（复用现有 `TiangongCore::switch_model`）；
//! - 中枢对话消息链路（复用现有 Session 持久化，不新建消息体系）。

pub mod event;
pub mod ids;
pub mod model;
pub mod risk;
pub mod status;
pub mod store;
pub mod tools;

pub use event::{EventSeverity, WorkEvent, WorkEventType};
pub use ids::{new_id, now_text};
pub use model::{
    CentralConversation, CentralDecision, FailureClass, GoalPriority, GoalStatus, HandoverDecision,
    HandoverScope, ModelHandoverConsultation, Report, ReportType, Suggestion, SuggestionStatus,
    SuggestionType, UserGoal, WorkItem,
};
pub use risk::RiskLevel;
pub use status::WorkItemStatus;
pub use store::CentralStore;
pub use tools::{CENTRAL_TOOL_ALLOWLIST, ModelSwitchTicket, is_central_tool_allowed};
