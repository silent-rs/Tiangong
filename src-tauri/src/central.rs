//! 中枢宿主桥（薄层）：插件与状态机已迁移至独立 crate——
//! - `central_plugin`（crates/central/central-plugin）：22 工具插件层、
//!   人设与派发通道消息
//! - `central_state`（crates/central/central-state）：状态机与持久化对账
//!
//! 本模块仅 re-export，保持宿主内 `crate::central::xxx` 调用路径稳定。

pub use central_plugin::*;
pub use central_state::{
    add_confirmed_workspace, append_work_item_event, canonical_path_string, interrupted_work_items,
    load_confirmed_workspaces, load_goals, load_reports, load_work_items, mark_work_item_finished,
    mark_work_item_finished_with_session, recover_registry_entries, save_goals, save_reports,
    save_work_items, set_work_item_outcome, upsert_work_item, workspace_confirmed, GoalRecord,
    ReportRecord, WorkItemEvent, WorkItemRecord,
};
