//! 中枢事件日志接线（R2）：宿主关键节点写入追加式事件日志。
//!
//! 双写阶段：现有时间线（`WorkItemRecord.events`，R5 删除）照旧，
//! 本模块把同一事实另写入唯一权威事实源 `central/events.jsonl`
//! （`tiangong_central::EventLog`）。旧时间线在恢复时一次性迁移。

use tiangong_central::event::WorkEventType as E;
use tiangong_central::{EventLog, WorkEvent};

use crate::app::TiangongApp;

/// 事件日志路径：`{storage_root}/central/events.jsonl`。
pub(crate) fn event_log_path(storage_root: &std::path::Path) -> std::path::PathBuf {
    storage_root.join("central").join("events.jsonl")
}

/// 记录一个中枢事件（派发/终态/等待用户/恢复等）。
///
/// 打开失败或追加失败记 warn——事件日志是事实源，写失败必须显式可见；
/// 不阻塞主链路（调用方语义与旧时间线一致）。
pub(crate) fn record_central_event(
    storage_root: &std::path::Path,
    work_item_id: &str,
    execution_id: &str,
    worker_session_id: &str,
    event_type: E,
    note: &str,
) {
    let log = match EventLog::open(&event_log_path(storage_root)) {
        Ok(log) => log,
        Err(error) => {
            tracing::warn!(%error, "中枢事件日志打开失败（事实未入日志）");
            return;
        }
    };
    let mut event = WorkEvent::new(work_item_id, event_type)
        .with_execution(execution_id)
        .with_worker_session(worker_session_id);
    if !note.is_empty() {
        event.payload = serde_json::json!({ "note": note });
    }
    if let Err(error) = log.append(event) {
        tracing::warn!(%error, work_item_id, "中枢事件日志追加失败（事实未入日志）");
    }
}

/// 旧时间线一次性迁移（R2，方案「旧 JSON 到新 schema 的一次性迁移」）。
///
/// 事件日志为空且 work-items.json 各记录含 `events` 时间线时，把旧事件
/// 逐条转入事件日志（event_id 重生成 scru128；execution_id 取记录当前值；
/// 原始 kind/note 保真进 payload）。幂等：日志非空即跳过。
pub(crate) fn migrate_legacy_timeline(storage_root: &std::path::Path) {
    let log = match EventLog::open(&event_log_path(storage_root)) {
        Ok(log) => log,
        Err(error) => {
            tracing::warn!(%error, "中枢事件日志打开失败，跳过旧时间线迁移");
            return;
        }
    };
    match log.last_offset() {
        Ok(offset) if offset > 0 => return, // 已有事实，不重复迁移
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "中枢事件日志读取失败，跳过旧时间线迁移");
            return;
        }
    }
    let records = crate::central::load_work_items(storage_root);
    let mut migrated = 0usize;
    for record in &records {
        for legacy in &record.events {
            let event_type = match legacy.kind.as_str() {
                "dispatched" => E::WorkItemStarted,
                "waiting_user" => E::WorkItemWaitingUser,
                "resumed" => E::WorkItemStarted,
                "finished" => {
                    if legacy.note.contains("失败") {
                        E::WorkItemFailed
                    } else {
                        E::WorkItemCompleted
                    }
                }
                _ => E::WorkItemProgress,
            };
            let mut event =
                WorkEvent::new(&record.work_id, event_type).with_worker_session(&record.session_id);
            if let Some(execution_id) = record.execution_id.as_deref() {
                event = event.with_execution(execution_id);
            }
            event.payload = serde_json::json!({
                "legacy_kind": legacy.kind,
                "note": legacy.note,
                "at": legacy.at,
            });
            if log.append(event).is_ok() {
                migrated += 1;
            }
        }
    }
    if migrated > 0 {
        tracing::info!(
            migrated,
            "旧工作项时间线已迁移至中枢事件日志（一次性，幂等）"
        );
    }
}

/// 便捷入口：从宿主状态取 storage_root 记录事件。
pub(crate) fn record_for(
    state: &TiangongApp,
    work_item_id: &str,
    execution_id: &str,
    worker_session_id: &str,
    event_type: E,
    note: &str,
) {
    record_central_event(
        state.core_manager.storage_root(),
        work_item_id,
        execution_id,
        worker_session_id,
        event_type,
        note,
    );
}
