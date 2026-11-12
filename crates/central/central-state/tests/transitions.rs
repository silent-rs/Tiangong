//! 状态机组合路径测试（行为锁定）——这些场景此前只能靠实机验证。

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

#[test]
fn cancel_guard_blocks_stream_terminal_override() {
    let next = transition_on_finished(Some(WorkState::Cancelled), WorkOutcome::Finished, true);
    assert_eq!(next, WorkState::Cancelled);
    let next_failed =
        transition_on_finished(Some(WorkState::Cancelled), WorkOutcome::Finished, false);
    assert_eq!(next_failed, WorkState::Cancelled);
}

#[test]
fn pause_guard_keeps_paused_on_stream_terminal() {
    let next = transition_on_finished(Some(WorkState::Paused), WorkOutcome::Finished, true);
    assert_eq!(next, WorkState::Paused);
}

#[test]
fn waiting_user_outcome_suspends_instead_of_finishing() {
    let next = transition_on_finished(Some(WorkState::Running), WorkOutcome::WaitingUser, true);
    assert_eq!(next, WorkState::WaitingUser);
    assert!(!next.is_terminal());
    assert!(next.is_inflight());
}

#[test]
fn normal_finish_splits_by_done() {
    assert_eq!(
        transition_on_finished(Some(WorkState::Running), WorkOutcome::Finished, true),
        WorkState::ReportQueued
    );
    assert_eq!(
        transition_on_finished(Some(WorkState::Running), WorkOutcome::Finished, false),
        WorkState::Failed
    );
    assert_eq!(
        transition_on_finished(None, WorkOutcome::Finished, true),
        WorkState::ReportQueued
    );
}

#[test]
fn merge_waiting_user_restores_and_returns_owner() {
    let mut entries = vec![
        entry("work-a", "s1", WorkState::WaitingUser),
        entry("work-b", "s2", WorkState::Running),
    ];
    let owner = merge_waiting_user(&mut entries, "s1");
    assert_eq!(
        owner,
        Some(("work-a".to_string(), "任务 work-a".to_string()))
    );
    assert_eq!(entries[0].state, WorkState::Running);
    assert_eq!(merge_waiting_user(&mut entries, "s2"), None);
    assert_eq!(merge_waiting_user(&mut entries, "s-none"), None);
}

#[test]
fn retention_keeps_latest_per_state() {
    let mut entries: Vec<WorkStatus> = (0..12)
        .map(|i| entry(&format!("c{i}"), &format!("s{i}"), WorkState::Completed))
        .collect();
    entries.push(entry("f0", "sf", WorkState::Failed));
    entries.push(entry("f1", "sf2", WorkState::Failed));
    let removals = retention_removals(&entries, 10);
    assert_eq!(removals, vec![0, 1]);
    let more: Vec<WorkStatus> = (0..12)
        .map(|i| entry(&format!("f{i}"), &format!("s{i}"), WorkState::Failed))
        .collect();
    assert_eq!(retention_removals(&more, 10), vec![0, 1]);
}

#[test]
fn queue_decision_combinations() {
    let mut entries = vec![
        entry("dep", "s-dep", WorkState::Running),
        entry("other", "s-other", WorkState::Running),
    ];
    entries[0].cwd = Some("/ws/a".to_string());
    entries[1].cwd = Some("/ws/b".to_string());

    assert_eq!(
        queue_decision(&entries, Some("dep"), "s-new", "/ws/c", false),
        QueueDecision::QueueAfter("dep".to_string())
    );
    entries[0].state = WorkState::Completed;
    assert_eq!(
        queue_decision(&entries, Some("dep"), "s-new", "/ws/c", false),
        QueueDecision::DispatchNow
    );
    assert_eq!(
        queue_decision(&entries, None, "s-new", "/ws/b", false),
        QueueDecision::QueueAfter("other".to_string())
    );
    assert_eq!(
        queue_decision(&entries, None, "s-new", "/ws/b", true),
        QueueDecision::DispatchNow
    );
    assert_eq!(
        queue_decision(&entries, None, "s-other", "/ws/b", false),
        QueueDecision::DispatchNow
    );
    for state in [WorkState::WaitingUser, WorkState::Paused, WorkState::Queued] {
        entries[0].state = state;
        assert_eq!(
            queue_decision(&entries, Some("dep"), "s-new", "/ws/c", false),
            QueueDecision::QueueAfter("dep".to_string()),
            "{state:?} 应视为在途依赖"
        );
    }
}

#[test]
fn model_failure_detection_boundaries() {
    assert!(failure_looks_model_related("请求超时（timeout）"));
    assert!(failure_looks_model_related("API 返回 500"));
    assert!(failure_looks_model_related("接口超时错误码429请稍后重试"));
    assert!(!failure_looks_model_related("已扫描 500 个文件，全部通过"));
    assert!(!failure_looks_model_related("文件共 40110 行"));
    assert!(!failure_looks_model_related("工作目录不存在"));
}

#[test]
fn flush_matches_accepts_work_id_and_session_id() {
    // 依赖以 work_id 或会话 ID 声明都必须命中（review P1：只按 work_id
    // 匹配会让 session_id 依赖永久 queued）
    assert!(flush_matches("work-a", "work-a", "s-1"));
    assert!(flush_matches("s-1", "work-a", "s-1"));
    assert!(!flush_matches("work-b", "work-a", "s-1"));
    assert!(!flush_matches("", "work-a", "s-1"));
}
