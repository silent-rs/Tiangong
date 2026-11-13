//! 中枢模式契约层（重构分支 `feature/central-refactor` 的 central-contract）。
//!
//! 依据 `docs/central-refactor-plan.md` §5.1：纯数据结构、契约与规则——
//! - 执行归属（`execution.rs`：Execution / execution_id / 契约版本）；
//! - 事件（`event.rs`：22 种工作事件）与追加式事件日志
//!   （`event_log.rs`：崩溃恢复 / 幂等 / 投影）；
//! - 授权票据（`tickets.rs`：Workspace / Decision 一次性 fail-closed）；
//! - 状态机（`status.rs` §8.1）与风险分级（`risk.rs` §14.1）。
//!
//! 持久化与对账在 `central-state`（登记快照），宿主编排/IO 在
//! `src-tauri/src/central_host/`——本 crate 不含 IO 与宿主依赖。
//! （R5 清理：POC 期的 store/model/tools 白名单死代码已删。）

pub mod event;
pub mod event_log;
pub mod execution;
pub mod ids;
pub mod risk;
pub mod status;
pub mod tickets;

pub use event::{EventSeverity, WorkEvent, WorkEventType};
pub use event_log::{EventLog, LoggedEvent, project_work_state};
pub use execution::{
    CENTRAL_CONTRACT_SCHEMA_VERSION, EXECUTION_ID_PREFIX, Execution, new_execution_id,
};
pub use ids::{new_id, now_text};
pub use risk::RiskLevel;
pub use status::WorkItemStatus;
pub use tickets::{DecisionTicket, TicketRegistry, TicketVerdict, WorkspaceApprovalTicket};
