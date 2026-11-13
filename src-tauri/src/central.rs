//! 中枢模块桥（薄层 re-export）：插件层在 `central-plugin`（22 工具、
//! 人设、派发通道消息），状态与持久化对账在 `central-state`；宿主编排
//! 在 `central_host/`（派发执行器/执行路由/模式协调/授权/事件）。
//! 本模块保持宿主内 `crate::central::xxx` 调用路径稳定。

pub use central_plugin::*;
pub use central_state::{
    add_confirmed_workspace, canonical_path_string, clear_work_item_queued, interrupted_work_items,
    load_confirmed_workspaces, load_goals, load_reports, load_work_items, mark_work_item_finished,
    mark_work_item_finished_with_session, mark_work_item_queued, queued_work_items,
    recover_registry_entries, save_goals, save_reports, save_work_items, set_work_item_execution,
    set_work_item_outcome, upsert_work_item, workspace_confirmed, GoalRecord, ReportRecord,
    WorkItemEvent, WorkItemRecord,
};
