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
        pending_question: None,
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
        queue_decision(
            &entries,
            None,
            false,
            Some("dep"),
            "s-new",
            "/ws/c",
            false,
            "普通任务"
        ),
        QueueDecision::QueueAfter("dep".to_string())
    );
    entries[0].state = WorkState::Completed;
    assert_eq!(
        queue_decision(
            &entries,
            None,
            false,
            Some("dep"),
            "s-new",
            "/ws/c",
            false,
            "普通任务"
        ),
        QueueDecision::DispatchNow
    );
    assert_eq!(
        queue_decision(
            &entries,
            None,
            false,
            None,
            "s-new",
            "/ws/b",
            false,
            "普通任务"
        ),
        QueueDecision::QueueAfter("other".to_string())
    );
    assert_eq!(
        queue_decision(
            &entries,
            None,
            false,
            None,
            "s-new",
            "/ws/b",
            true,
            "普通任务"
        ),
        QueueDecision::DispatchNow
    );
    // R3（方案 §3.4）：高风险任务（发布/删除/迁移/覆写）声明 parallel 也
    // 不得豁免写入锁——用户说「并行」不能绕过冲突保护。
    assert_eq!(
        queue_decision(
            &entries,
            None,
            false,
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
            false,
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
        queue_decision(
            &entries,
            None,
            false,
            None,
            "s-other",
            "/ws/b",
            false,
            "普通任务"
        ),
        QueueDecision::QueueAfter("other".to_string())
    );
    for state in [WorkState::WaitingUser, WorkState::Paused, WorkState::Queued] {
        entries[0].state = state;
        assert_eq!(
            queue_decision(
                &entries,
                None,
                false,
                Some("dep"),
                "s-new",
                "/ws/c",
                false,
                "普通任务"
            ),
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

/// WorkOutcome::as_str 短名；classify_outcome 按行首标记判定。
#[test]
fn work_outcome_names_and_classifier_boundaries() {
    assert_eq!(WorkOutcome::Finished.as_str(), "finished");
    assert_eq!(WorkOutcome::WaitingUser.as_str(), "waiting_user");
    assert_eq!(classify_outcome("正常完成"), WorkOutcome::Finished);
    // 标记独占行（协议形态，含缩进）→ 征询
    assert_eq!(
        classify_outcome("【需要用户决策】\n要不要继续"),
        WorkOutcome::WaitingUser
    );
    assert_eq!(
        classify_outcome("[后台工作完成] work-3\n【需要用户决策】\n问题：是否继续？"),
        WorkOutcome::WaitingUser
    );
    assert_eq!(
        classify_outcome("  【需要用户决策】  \n缩进+尾随空白独占行也算"),
        WorkOutcome::WaitingUser
    );
    // 句中/行中/行首叙述引用不是征询（2026-11-17 用户实测回归锁：
    // review 报告 §8.5 标题引用裁决记录「### 8.5 需要用户决策（换模型
    // A/B/C，第七次）【需要用户决策】」被全文子串匹配误判，第二十七
    // 轮终稿被永久登记为 waiting_user；P2-01/PR-10 又实证「标记+叙述
    // 文字」的行首形态仍误判——最终收敛到独占行协议边界）
    assert_eq!(
        classify_outcome("问题：【需要用户决策】要不要继续"),
        WorkOutcome::Finished
    );
    assert_eq!(
        classify_outcome("【需要用户决策】要不要继续"),
        WorkOutcome::Finished,
        "标记+问题同行的非协议形态按引用叙述处理（漏判可见全文，误判永久挂起）"
    );
    assert_eq!(
        classify_outcome(
            "# 第二十七轮 Review\n### 8.5 需要用户决策（换模型 A/B/C，第七次）【需要用户决策】\n背景：用户裁决 A（持久化）"
        ),
        WorkOutcome::Finished
    );
    // P2-01（第 28 轮 review）：真实多行样本保真——现场原文是标题行
    // 引用 + 叙述句引用形态（此前压缩为同一行，不能称现场原文锁定）。
    // 已知边界：代码块**内独占一行**的标记文本层无法区分（完整解法为
    // 结构化 outcome，S2 修法 1 的 render 层方向，属后续工作）。
    assert_eq!(
        classify_outcome(
            "审查完成，结论如下：\n## 8.5 需要用户决策（历史裁决引用）\n【需要用户决策】曾出现在第 20 轮。\n以上均为引用，非当前征询。"
        ),
        WorkOutcome::Finished,
        "标题行/正文句中/行首叙述句的引用都不是征询（独占行才算）"
    );
    // 对照：真实征询——独立行首标记 + 问题正文（协议形态）
    assert_eq!(
        classify_outcome("【需要用户决策】\n流星锤方案是否继续？背景：测试验证征询闭环。"),
        WorkOutcome::WaitingUser,
        "独立行首标记 + 问题正文是真实征询（WU-03 现场形态）"
    );
    assert_eq!(classify_outcome(""), WorkOutcome::Finished);
}

/// 同会话互斥锁（用户实测 2026-11-16：日志同 session 反复派发，前一轮
/// 在途又派一轮 → 两个 turn 并发投递同一 Core，消息交错）。锁定期：
/// running / paused / queued 一律排队，终态后自动投递。
/// WaitingUser 放行（2026-11-17 反转，第二十七轮 review P0）：征询时
/// worker turn 已结束、Core 空闲——计入互斥会把中枢的答复转发
/// QueueAfter 到该 waiting 条目自身（等它终态才投递，而它正等答复）
/// → 自锁；放行后由 merge_waiting_user 恢复（归属沿用主工作）。
#[test]
fn same_session_inflight_queues_new_dispatch() {
    for state in [WorkState::Running, WorkState::Paused, WorkState::Queued] {
        let entries = vec![entry("work-h", "s-busy", state)];
        assert_eq!(
            queue_decision(
                &entries,
                None,
                false,
                None,
                "s-busy",
                "/ws/x",
                false,
                "普通任务"
            ),
            QueueDecision::QueueAfter("work-h".to_string()),
            "{:?} 在途时同会话新派发必须排队",
            state
        );
    }
    // waiting_user 放行：答复转发不得自锁
    let waiting = vec![entry("work-w", "s-busy", WorkState::WaitingUser)];
    assert_eq!(
        queue_decision(
            &waiting,
            None,
            false,
            None,
            "s-busy",
            "/ws/x",
            false,
            "转发答复"
        ),
        QueueDecision::DispatchNow,
        "waiting_user 不是活跃在途——答复转发/新派发放行（恢复归 merge）"
    );
    // Running × reused × id 不一致：新工作撞在跑会话——排队（P2-02
    // 限缩：防并发 turn 与回调归属丢失）
    let running = vec![entry("work-r", "s-busy", WorkState::Running)];
    assert_eq!(
        queue_decision(
            &running,
            Some("work-other"),
            true,
            None,
            "s-busy",
            "/ws/x",
            false,
            "引导"
        ),
        QueueDecision::QueueAfter("work-r".to_string()),
        "reused 但 id 与在跑条目不一致必须排队"
    );
    // Running × 同 id × 非 reused：排队（新任务带旧 id 撞在跑）
    assert_eq!(
        queue_decision(
            &running,
            Some("work-r"),
            false,
            None,
            "s-busy",
            "/ws/x",
            false,
            "任务"
        ),
        QueueDecision::QueueAfter("work-r".to_string()),
        "非 reused 的同 id 派发不得放行（并发新 turn）"
    );
    // Running × reused × 同 id（steer 生产形态）：放行并入
    assert_eq!(
        queue_decision(
            &running,
            Some("work-r"),
            true,
            None,
            "s-busy",
            "/ws/x",
            false,
            "引导"
        ),
        QueueDecision::DispatchNow,
        "reused 且 id 一致的引导放行（并入当前 turn）"
    );
    // 自身条目跳过（flush 重入投递自锁回归锁）：排队项依赖终态后经
    // flush_pending_dispatches 重入派发链，registry 里它自己的 Queued
    // 条目仍在——不计入互斥（自身不代表 Core 占用），否则 QueueAfter
    // 自身永远投不出去。
    let self_queued = vec![entry("work-me", "s-busy", WorkState::Queued)];
    assert_eq!(
        queue_decision(
            &self_queued,
            Some("work-me"),
            false,
            None,
            "s-busy",
            "/ws/x",
            false,
            "任务"
        ),
        QueueDecision::DispatchNow,
        "flush 重入投递时自身的 Queued 条目不得拦截自己"
    );
    // 但同会话**其他**排队项仍保序（后到者排在其后）
    let two_queued = vec![
        entry("work-a", "s-busy", WorkState::Queued),
        entry("work-b", "s-busy", WorkState::Queued),
    ];
    assert_eq!(
        queue_decision(
            &two_queued,
            Some("work-b"),
            false,
            None,
            "s-busy",
            "/ws/x",
            false,
            "任务"
        ),
        QueueDecision::QueueAfter("work-a".to_string()),
        "同会话其他排队项仍在途——保序排队"
    );
    // 终态/其他会话不受影响
    let done = vec![entry("work-d", "s-busy", WorkState::Completed)];
    assert_eq!(
        queue_decision(
            &done,
            None,
            false,
            None,
            "s-busy",
            "/ws/x",
            false,
            "普通任务"
        ),
        QueueDecision::DispatchNow,
        "终态后同会话可再派"
    );
    let other = vec![entry("work-o", "s-other", WorkState::Running)];
    assert_eq!(
        queue_decision(
            &other,
            None,
            false,
            None,
            "s-busy",
            "/ws/x",
            false,
            "普通任务"
        ),
        QueueDecision::DispatchNow,
        "不同会话不互斥（写入锁另有判定）"
    );
}

/// G0 意图判定表驱动（第 28 轮 review R28-2）：按转移前状态显式分类，
/// 取代「merge 置 Running 后 work_id 反猜」（探针 PR-08 的误判形态）。
#[test]
fn dispatch_intent_classification_matrix() {
    use central_state::{classify_dispatch_intent, DispatchIntent};
    type Case = (
        Option<(WorkState, &'static str)>,
        &'static str,
        bool,
        DispatchIntent,
    );
    let cases: Vec<Case> = vec![
        // 无条目：新执行
        (None, "work-x", false, DispatchIntent::NewExecution),
        (None, "work-x", true, DispatchIntent::NewExecution),
        // 终态条目：新执行
        (
            Some((WorkState::Completed, "work-o")),
            "work-o",
            true,
            DispatchIntent::NewExecution,
        ),
        // waiting × reused：恢复（答复转发不要求 id 一致）
        (
            Some((WorkState::WaitingUser, "work-o")),
            "work-o",
            true,
            DispatchIntent::ResumeWaiting,
        ),
        (
            Some((WorkState::WaitingUser, "work-o")),
            "work-other",
            true,
            DispatchIntent::ResumeWaiting,
        ),
        // waiting × 非 reused：不合并（新工作撞等待会话——上游互斥放行
        // 后按新执行覆盖登记）
        (
            Some((WorkState::WaitingUser, "work-o")),
            "work-x",
            false,
            DispatchIntent::NewExecution,
        ),
        // paused × reused：恢复（覆盖登记 + 挂回调）
        (
            Some((WorkState::Paused, "work-p")),
            "work-p",
            true,
            DispatchIntent::ResumePaused,
        ),
        // running × reused × 同 id：steer 并入（R28-2 曾误判的形态：
        // merge 已把 waiting 改 Running，同 owner id 的答复转发被吞）
        (
            Some((WorkState::Running, "work-r")),
            "work-r",
            true,
            DispatchIntent::SteerRunning,
        ),
        // running × reused × 异 id / 非 reused：新执行语义（上游互斥已
        // 排队，到达此处的其余组合保守按新执行）
        (
            Some((WorkState::Running, "work-r")),
            "work-other",
            true,
            DispatchIntent::NewExecution,
        ),
        (
            Some((WorkState::Running, "work-r")),
            "work-r",
            false,
            DispatchIntent::NewExecution,
        ),
    ];
    for (latest, self_id, reused, expect) in cases {
        assert_eq!(
            classify_dispatch_intent(latest, self_id, reused),
            expect,
            "latest={latest:?} self={self_id} reused={reused}"
        );
    }
}
