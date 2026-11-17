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
        execution_id: None,
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
    let owner = merge_waiting_user(&mut entries, "s1", Some("exec-resume"));
    assert_eq!(
        owner,
        Some(("work-a".to_string(), "任务 work-a".to_string()))
    );
    assert_eq!(entries[0].state, WorkState::Running);
    assert_eq!(merge_waiting_user(&mut entries, "s2", None), None);
    assert_eq!(merge_waiting_user(&mut entries, "s-none", None), None);
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
        queue_decision(&entries, Some("dep"), "s-new", "/ws/c", false, "普通任务"),
        QueueDecision::QueueAfter("dep".to_string())
    );
    entries[0].state = WorkState::Completed;
    assert_eq!(
        queue_decision(&entries, Some("dep"), "s-new", "/ws/c", false, "普通任务"),
        QueueDecision::DispatchNow
    );
    assert_eq!(
        queue_decision(&entries, None, "s-new", "/ws/b", false, "普通任务"),
        QueueDecision::QueueAfter("other".to_string())
    );
    assert_eq!(
        queue_decision(&entries, None, "s-new", "/ws/b", true, "普通任务"),
        QueueDecision::DispatchNow
    );
    // R3（方案 §3.4）：高风险任务（发布/删除/迁移/覆写）声明 parallel 也
    // 不得豁免写入锁——用户说「并行」不能绕过冲突保护。
    assert_eq!(
        queue_decision(
            &entries,
            None,
            "s-new",
            "/ws/b",
            true,
            "发布新版本到生产环境"
        ),
        QueueDecision::QueueAfter("other".to_string()),
        "高风险任务 parallel 豁免必须失效"
    );
    assert_eq!(
        queue_decision(
            &entries,
            None,
            "s-new",
            "/ws/b",
            true,
            "删除旧分支并迁移目录"
        ),
        QueueDecision::QueueAfter("other".to_string()),
        "删除/迁移类任务 parallel 豁免必须失效"
    );
    assert!(
        !crate::task_is_high_risk("重构解析器并补充测试"),
        "普通开发任务不得误判为高风险"
    );
    // 同会话互斥（用户实测 2026-11-16）：s-other 已在途，再派同会话 → 排队
    assert_eq!(
        queue_decision(&entries, None, "s-other", "/ws/b", false, "普通任务"),
        QueueDecision::QueueAfter("other".to_string())
    );
    for state in [WorkState::WaitingUser, WorkState::Paused, WorkState::Queued] {
        entries[0].state = state;
        assert_eq!(
            queue_decision(&entries, Some("dep"), "s-new", "/ws/c", false, "普通任务"),
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

// ============ 覆盖率补充批（2026-11-16）：枚举全分支 ============

/// WorkState::as_str ↔ parse 全枚举双向闭合；未知值归 None。
#[test]
fn work_state_str_parse_round_trip_all_variants() {
    let all = [
        WorkState::Running,
        WorkState::ReportQueued,
        WorkState::Completed,
        WorkState::Failed,
        WorkState::Interrupted,
        WorkState::WaitingUser,
        WorkState::Paused,
        WorkState::Cancelled,
        WorkState::Queued,
    ];
    for state in all {
        let text = state.as_str();
        assert_eq!(WorkState::parse(text), Some(state), "{text} 应双射");
    }
    assert_eq!(WorkState::parse("unknown-state"), None);
    assert_eq!(WorkState::parse(""), None);
}

/// WorkOutcome::as_str 短名；classify_outcome 按标记判定。
#[test]
fn work_outcome_names_and_classifier_boundaries() {
    assert_eq!(WorkOutcome::Finished.as_str(), "finished");
    assert_eq!(WorkOutcome::WaitingUser.as_str(), "waiting_user");
    assert_eq!(classify_outcome("正常完成"), WorkOutcome::Finished);
    assert_eq!(
        classify_outcome("问题：【需要用户决策】要不要继续"),
        WorkOutcome::WaitingUser
    );
    assert_eq!(classify_outcome(""), WorkOutcome::Finished);
}

/// 同会话互斥锁（用户实测 2026-11-16：日志同 session 反复派发，前一轮
/// 在途又派一轮 → 两个 turn 并发投递同一 Core，消息交错）。锁定期：
/// running / waiting_user / paused / queued 一律排队，终态后自动投递。
#[test]
fn same_session_inflight_queues_new_dispatch() {
    for state in [
        WorkState::Running,
        WorkState::WaitingUser,
        WorkState::Paused,
        WorkState::Queued,
    ] {
        let entries = vec![entry("work-h", "s-busy", state)];
        assert_eq!(
            queue_decision(&entries, None, "s-busy", "/ws/x", false, "普通任务"),
            QueueDecision::QueueAfter("work-h".to_string()),
            "{:?} 在途时同会话新派发必须排队",
            state
        );
    }
    // 终态/其他会话不受影响
    let done = vec![entry("work-d", "s-busy", WorkState::Completed)];
    assert_eq!(
        queue_decision(&done, None, "s-busy", "/ws/x", false, "普通任务"),
        QueueDecision::DispatchNow,
        "终态后同会话可再派"
    );
    let other = vec![entry("work-o", "s-other", WorkState::Running)];
    assert_eq!(
        queue_decision(&other, None, "s-busy", "/ws/x", false, "普通任务"),
        QueueDecision::DispatchNow,
        "不同会话不互斥（写入锁另有判定）"
    );
}
