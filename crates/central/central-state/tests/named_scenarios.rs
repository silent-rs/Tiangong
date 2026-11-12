//! 宿主接线层行为的具名锁定（review 机检 1 的可离线部分）——场景均取自
//! 十一轮 review 累计清单，以纯数据序列模拟宿主状态流。

use central_state::*;

fn entry(work_id: &str, session: &str, state: WorkState) -> WorkStatus {
    WorkStatus {
        work_id: work_id.to_string(),
        session_id: session.to_string(),
        task: format!("任务 {work_id}"),
        model: None,
        dispatched_at: "2026-11-11 12:00:00".to_string(),
        state,
        reused_session: false,
        last_activity: None,
        cwd: None,
    }
}

/// 复用派发的登记：模型谎报防线（M5）——执行模型显式标注「已忽略请求的 X」。
#[test]
fn dispatch_reuse_model_ignored_is_explicit() {
    let entry = registry_entry_after_dispatch(
        "work-r1",
        "s-review",
        "继续 review",
        Some("k3"),
        true,
        Some("/ws/a"),
        "2026-11-11 15:00:00",
    );
    assert_eq!(
        entry.model.as_deref(),
        Some("会话既有模型（已忽略 k3）"),
        "复用登记不得谎报请求模型"
    );
    assert_eq!(entry.state, WorkState::Running);
    let fresh = registry_entry_after_dispatch(
        "work-n1",
        "s-new",
        "新任务",
        Some("k3"),
        false,
        None,
        "2026-11-11 15:00:00",
    );
    assert_eq!(fresh.model.as_deref(), Some("k3"));
    assert_eq!(fresh.cwd.as_deref(), None);
}

/// 复用工作完成 → 回报排队；同会话旧条目被 upsert 覆盖。
#[test]
fn dispatch_reuse_finish_queues_report() {
    let mut entries = vec![entry("work-old", "s-review", WorkState::Completed)];
    let dispatched = registry_entry_after_dispatch(
        "work-r2",
        "s-review",
        "增量 review",
        None,
        true,
        Some("/ws/a"),
        "2026-11-11 16:00:00",
    );
    upsert_registry_entry(&mut entries, dispatched);
    assert_eq!(entries.len(), 1, "会话维度单条");
    assert_eq!(entries[0].work_id, "work-r2");
    let next = transition_on_finished(Some(entries[0].state), WorkOutcome::Finished, true);
    assert_eq!(next, WorkState::ReportQueued);
    assert!(should_deliver_now(true, false));
}

/// 投递失败回滚：登记条目保留并转 failed（不因失败丢失条目）。
#[test]
fn delivery_failure_rollback_keeps_item() {
    let mut entries = vec![entry("work-f1", "s-x", WorkState::Running)];
    let next = transition_on_finished(Some(entries[0].state), WorkOutcome::Finished, false);
    entries[0].state = next;
    assert_eq!(entries[0].state, WorkState::Failed, "失败条目保留并分态");
    assert!(should_deliver_now(false, true));
}

/// 中断条目迁移与清理：同会话重派覆盖 interrupted（不再残留误报）。
#[test]
fn interrupted_items_migrate_and_clear() {
    let mut entries = vec![
        entry("work-i1", "s-review", WorkState::Interrupted),
        entry("work-other", "s-other", WorkState::Running),
    ];
    let re_dispatch = registry_entry_after_dispatch(
        "work-i2",
        "s-review",
        "重派中断的任务",
        None,
        true,
        Some("/ws/a"),
        "2026-11-11 17:00:00",
    );
    upsert_registry_entry(&mut entries, re_dispatch);
    assert!(
        entries.iter().all(|e| e.state != WorkState::Interrupted),
        "同会话重派后中断条目必须清除"
    );
    let review = entries.iter().find(|e| e.session_id == "s-review").unwrap();
    assert_eq!(review.state, WorkState::Running);
    assert_eq!(review.work_id, "work-i2");
}

/// app 入口派发目标守卫：中枢会话拒绝。
#[test]
fn dispatch_rejects_central_hub_at_app_entry() {
    assert!(validate_dispatch_target("central-hub").is_err());
    assert!(validate_dispatch_target("s-normal").is_ok());
    let error = validate_dispatch_target("central-hub").unwrap_err();
    assert!(
        error.contains("不能作为工作会话"),
        "拒绝信息可辨识：{error}"
    );
}
