//! 排队挂起的持久化往返（review P2：队列不持久导致重启误判中断）。

use central_state::{
    persistence::{
        mark_work_item_queued, queued_work_items, recover_registry_entries, upsert_work_item,
        WorkItemRecord,
    },
    WorkState,
};

fn record(session: &str) -> WorkItemRecord {
    WorkItemRecord {
        work_id: format!("work-{session}"),
        reused_session: false,
        session_id: session.to_string(),
        task: "排队任务".to_string(),
        model: None,
        dispatched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        finished_at: None,
        outcome: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
    }
}

#[test]
fn queued_state_survives_restart_as_queued_not_interrupted() {
    let root = tempfile::tempdir().unwrap();
    upsert_work_item(root.path(), &record("s-q"));
    mark_work_item_queued(root.path(), "s-q", "work-dep");
    // 落盘可读回（重放数据源）
    let queued = queued_work_items(root.path());
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].after_work_id.as_deref(), Some("work-dep"));
    // 恢复重建：queued 态（不是 interrupted——从未执行）
    let entries = recover_registry_entries(root.path());
    let entry = entries.iter().find(|e| e.session_id == "s-q").unwrap();
    assert_eq!(entry.state, WorkState::Queued);
}

#[test]
fn queued_records_require_queued_marker() {
    let root = tempfile::tempdir().unwrap();
    // 未标记排队的未终态近期条目 → interrupted（原有语义不变）
    upsert_work_item(root.path(), &record("s-run"));
    let entries = recover_registry_entries(root.path());
    let entry = entries.iter().find(|e| e.session_id == "s-run").unwrap();
    assert_eq!(entry.state, WorkState::Interrupted);
}
