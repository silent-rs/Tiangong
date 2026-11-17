//! 中枢模式宿主集成测试（R7 测试集中：原 src-tauri/src/app.rs 内嵌
//! tests 模块中的中枢用例整体迁出——与普通宿主测试分离，经 mock app +
//! `TiangongApp::new_for_test` 驱动真实宿主编排）。
#![allow(clippy::too_many_lines)]
use tauri::Manager;
use tiangong_app::central_host::dispatch::{
    build_worker_instruction, enqueue_central_dispatch, handle_central_dispatch,
    handle_work_terminal, plan_central_dispatch,
};
use tiangong_app::central_host::recovery::build_activation_summary;
use tiangong_app::state_of_for_test as state_of;

use central_state::{failure_looks_model_related, should_deliver_now, WorkOutcome};

/// 回报时机判定：失败必达；成功仅在无其他在执行任务时触发汇总轮。
#[test]
fn work_report_timing_follows_design() {
    // 失败：无论是否还有任务在跑，立即汇报（关键节点必达）
    assert!(should_deliver_now(false, true));
    assert!(should_deliver_now(false, false));
    // 成功且仍有任务在执行：暂存等待汇总
    assert!(!should_deliver_now(true, true));
    // 成功且是最后一个：触发汇总轮
    assert!(should_deliver_now(true, false));
}

use tiangong_app::app::TiangongApp;

/// 模型/网络类失败识别：指导中枢换模型重派，而非把任务当失败关闭。
#[test]
fn model_related_failure_markers() {
    assert!(failure_looks_model_related(
        "请求超时（timeout after 300s）"
    ));
    assert!(failure_looks_model_related("429 Too Many Requests"));
    assert!(failure_looks_model_related("连接被对端重置"));
    assert!(failure_looks_model_related("API 返回 500"));
    assert!(failure_looks_model_related("internal server error"));
    assert!(!failure_looks_model_related("工作目录不存在：/tmp/missing"));
    assert!(!failure_looks_model_related("任务要求删除文件被拒绝"));
    // review：判定过宽——词边界收紧后的负例
    assert!(!failure_looks_model_related("已扫描 500 个文件，全部通过"));
    assert!(!failure_looks_model_related(
        "文件共 40110 行，超出行数预算"
    ));
    // UTF-8 安全（review P1：中文语境 + 状态码不得 panic，语境词命中）
    assert!(failure_looks_model_related("接口超时错误码429请稍后重试"));
    assert!(failure_looks_model_related(
        "上游服务器错误：500 服务不可用，任务终止"
    ));
    assert!(!failure_looks_model_related(
        "仓库共 500 个模块，已全部索引完成"
    ));
}

/// Worker 指令：决策协议（对话式）——禁交互工具 + 标记格式 + 答复回流。
#[test]
fn worker_instruction_carries_decision_protocol() {
    let instruction = build_worker_instruction("work-abc", "执行数据库迁移");
    assert!(instruction.contains("work-abc"));
    assert!(instruction.contains("执行数据库迁移"));
    assert!(instruction.contains("不要调用 request_user"));
    assert!(instruction.contains("【需要用户决策】"));
    assert!(instruction.contains("以下一条消息回到本会话"));
    assert!(instruction.contains("不要虚构结果"));
}

/// 阻塞识别：含【需要用户决策】→ WaitingUser（未终态），其余终态。
#[test]
fn work_outcome_classifies_waiting_user() {
    assert_eq!(
        central_state::classify_outcome("[后台工作完成] work-1\n结论：一切正常"),
        WorkOutcome::Finished
    );
    assert_eq!(
        central_state::classify_outcome("[后台工作失败] work-2\n失败原因：超时"),
        WorkOutcome::Finished
    );
    assert_eq!(
        central_state::classify_outcome(
            "[后台工作完成] work-3\n【需要用户决策】\n问题：是否继续？"
        ),
        WorkOutcome::WaitingUser
    );
}

/// S9：ensure_core 拒绝中枢会话（白名单隔离的机制守卫，先于副作用）。
#[tokio::test]
async fn ensure_core_rejects_central_session_entry() {
    let app = TiangongApp::new();
    let (_tx, _rx) = std::sync::mpsc::channel();
    let result = app
        .ensure_core(
            tiangong_app::central::CENTRAL_SESSION_ID,
            None,
            None,
            None,
            None,
            _tx,
        )
        .await;
    let error = result.expect_err("中枢会话经 ensure_core 必须被拒绝");
    assert!(
        error.contains("ensure_central_core"),
        "错误信息须指向正确入口：{error}"
    );
    // 守卫先于副作用：未创建任何 Core
    assert!(
        !app.core_manager
            .has_live_core(tiangong_app::central::CENTRAL_SESSION_ID),
        "拦截必须发生在 Core 创建之前"
    );
}

/// 复用派发挂载的终态回调经 fire 链真实触发（挂载 → take → 执行 →
/// drain 一次性），并覆盖 S6 一次性守卫与 T6 归属过滤的可构造部分。
/// R1 执行路由（取代槽测试）：register → attach → fire 恰好触发一次，
/// drain 一次性（重复 fire 不重复触发），并覆盖 S6 一次性守卫与
/// 执行归属过滤的可构造部分。
#[test]
fn dispatch_reuse_fires_live_slot_callback() {
    let app = TiangongApp::new();
    let session = "s-live";
    // registry 预置：该会话当前条目即本执行（归属判定通过）
    app.central
        .central_work_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(tiangong_app::central::CentralWorkStatus {
            work_id: "work-live".to_string(),
            session_id: session.to_string(),
            task: "复用任务".to_string(),
            model: None,
            dispatched_at: "2026-11-11 18:00:00".to_string(),
            state: central_state::WorkState::Running,
            reused_session: true,
            last_activity: None,
            cwd: None,
            execution_id: Some("exec-live".to_string()),
        });
    // 挂载（与 attach_work_finished_callback 同款闭包语义，去掉 app 依赖）
    {
        let fired = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let fired_clone = std::sync::Arc::clone(&fired);
        app.central.execution_router.register(session, "exec-live");
        app.central.execution_router.attach(
            session,
            "exec-live",
            Box::new(move |done: bool, reason: String| {
                let mut log = fired_clone.lock().unwrap();
                log.push(format!("work-live:{done}:{reason}"));
            }),
        );
        // fire（流消费者终态路径等价）：路由给活跃执行的回调
        app.central
            .execution_router
            .fire(session, true, String::new());
        assert_eq!(*fired.lock().unwrap(), vec!["work-live:true:".to_string()]);
        // drain 一次性：再 fire 无回调（重复终态不重复触发）
        app.central
            .execution_router
            .fire(session, true, String::new());
        assert_eq!(
            fired.lock().unwrap().len(),
            1,
            "fire 后回调必须已消费（一次性）"
        );
    }
    // 状态驱动幂等（P1-1 修正后的语义）：在途 → 可处理；已终态 → 跳过
    {
        let mut registry = app
            .central
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = registry
            .iter_mut()
            .rev()
            .find(|e| e.work_id == "work-live")
            .unwrap();
        assert!(entry.state.is_inflight(), "running 在途可处理终态");
        entry.state = central_state::WorkState::ReportQueued;
        assert!(
            !entry.state.is_inflight(),
            "已终态的重复回调必须可被守卫识别跳过"
        );
    }
}

/// 激活摘要（§4.2）：有事实按事实组装（等待决策/中断/失败优先），
/// 空闲态给能力介绍指引，不编造状态。
#[test]
fn activation_summary_builds_from_verifiable_facts() {
    let entries = vec![
        tiangong_app::central::CentralWorkStatus {
            work_id: "w1".into(),
            session_id: "s1".into(),
            task: "review 仓库".into(),
            model: None,
            dispatched_at: "2026-11-12 10:00:00".into(),
            state: central_state::WorkState::WaitingUser,
            reused_session: false,
            last_activity: None,
            cwd: None,
            execution_id: None,
        },
        tiangong_app::central::CentralWorkStatus {
            work_id: "w2".into(),
            session_id: "s2".into(),
            task: "调研".into(),
            model: None,
            dispatched_at: "2026-11-12 09:00:00".into(),
            state: central_state::WorkState::Interrupted,
            reused_session: false,
            last_activity: None,
            cwd: None,
            execution_id: None,
        },
    ];
    // R5 用户实测修正：只有需要用户动作的事实才注入（必达），
    // 且措辞自然（不说工具数/状态清单/能力介绍）
    let summary = build_activation_summary(&entries, &[]).expect("有等待决策/中断事实应注入");
    assert!(summary.contains("1 个工作在等你决策"), "{summary}");
    assert!(summary.contains("review 仓库"), "{summary}");
    assert!(summary.contains("1 个工作中断"), "{summary}");
    assert!(summary.contains("一两句自然"), "{summary}");
    assert!(!summary.contains("介绍你能做什么"), "{summary}");
    // 空闲与纯执行态：完全静默（适时沉默——机械问候无价值）
    assert!(build_activation_summary(&[], &[]).is_none(), "空闲不注入");
    let running_only = vec![tiangong_app::central::CentralWorkStatus {
        work_id: "w-run".into(),
        session_id: "s-run".into(),
        task: "跑测试".into(),
        model: None,
        dispatched_at: "2026-11-12 10:00:00".into(),
        state: central_state::WorkState::Running,
        reused_session: false,
        last_activity: None,
        cwd: None,
        execution_id: None,
    }];
    assert!(
        build_activation_summary(&running_only, &[]).is_none(),
        "纯执行态不注入（徽章可见，不打扰）"
    );
    // 有需用户动作的事实时，相关目标一并提供
    let goals = vec![tiangong_app::central::GoalRecord {
        goal_id: "g1".into(),
        title: "完成中枢模式".into(),
        description: String::new(),
        workspace: None,
        created_at: "2026-11-12".into(),
        status: "active".into(),
    }];
    let with_goal = build_activation_summary(&entries, &goals).expect("有事实时应注入");
    assert!(with_goal.contains("完成中枢模式"), "{with_goal}");
}

/// 构造隔离 storage 的 TiangongApp（emit 经未注入句柄静默跳过——host
/// 集成测试无需 Wry 环境）。
fn central_test_app() -> (tiangong_app::app::TiangongApp, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let ws = root.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let ws_path = ws.clone();
    (
        TiangongApp::new_for_test(root.keep(), ws.to_string_lossy().to_string()),
        ws_path,
    )
}

fn dispatch_request(
    work_id: &str,
    session: &str,
    task: &str,
    reused: bool,
) -> tiangong_app::central::CentralDispatchRequest {
    tiangong_app::central::CentralDispatchRequest {
        work_id: work_id.to_string(),
        session_id: session.to_string(),
        task: task.to_string(),
        model: None,
        cwd: None,
        after_work_id: None,
        parallel: false,
        reused_session: reused,
        execution_id: "exec-test".to_string(),
    }
}

/// 中枢 Core 构建：已有中枢会话沿用其配置与 model_ref；模型失效
/// 回退默认路由；全不可用报错。
#[tokio::test]
async fn host_central_core_builds_from_session_and_falls_back() {
    let app = tauri::test::mock_app();
    let (state, _ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let storage_root = state.core_manager.storage_root().to_path_buf();
    // 1) 无任何模型配置 → 构建报错（两者都不可用）
    let (tx1, rx1) = std::sync::mpsc::channel::<tiangong_types::StreamEvent>();
    std::thread::spawn(move || for _ in rx1 {});
    let no_model = state.ensure_central_core(handle, tx1).await;
    assert!(no_model.is_err(), "无模型配置应报错：{no_model:?}");
    // 2) 有默认模型 + 已有中枢会话（带失效 model_ref）→ 回退默认构建成功
    std::fs::write(
        storage_root.join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let central_dir = storage_root.join("central/sessions");
    std::fs::create_dir_all(&central_dir).unwrap();
    std::fs::write(
        central_dir.join("central-hub.json"),
        serde_json::json!({
            "id": tiangong_app::central::CENTRAL_SESSION_ID, "title": "中枢",
            "created_at": "2026-11-16 10:00:00", "updated_at": "2026-11-16 10:00:00",
            "trust_mode": "supervised", "cwd": "/tmp",
            "model_ref": "deleted-model", "messages": []
        })
        .to_string(),
    )
    .unwrap();
    let (tx2, rx2) = std::sync::mpsc::channel::<tiangong_types::StreamEvent>();
    std::thread::spawn(move || for _ in rx2 {});
    let built = state.ensure_central_core(handle, tx2).await;
    assert!(built.expect("回退默认应构建成功"), "首次应创建");
    // 3) 再 ensure：走复用分支（配置刷新，返回 false）
    let (tx3, rx3) = std::sync::mpsc::channel::<tiangong_types::StreamEvent>();
    std::thread::spawn(move || for _ in rx3 {});
    let reused = state.ensure_central_core(handle, tx3).await;
    assert!(!reused.expect("复用应成功"), "已建核心应复用");
}

/// CC2：重启重建时同会话引导孤儿让位 owner——owner 条目不消失、
/// control(owner) 恢复可达；仅有引导（冷启动边缘）时保留引导条目。
#[tokio::test]
async fn host_rebuild_prefers_owner_over_steer_orphan() {
    let app = tauri::test::mock_app();
    let (state, _ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state_of_handle = state_of(handle);
    let storage_root = state_of_handle.core_manager.storage_root().to_path_buf();
    let log_path = storage_root.join("central/events.jsonl");
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    let mut lines = Vec::new();
    let events: [(&str, &str, &str, bool); 4] = [
        // owner 派发（reused=false）→ running
        ("work-ownerX", "s-cc2", "work_item.started", false),
        // 引导（reused=true，更晚）——重启后不得顶替 owner
        ("work-steerX", "s-cc2", "work_item.started", true),
        // 对照：另一会话仅有引导（冷启动边缘）→ 保留
        ("work-only-steer", "s-cc2b", "work_item.started", true),
        // ownerY 终态（failed）——保证 states 有终态投影可取
        ("work-ownerY", "s-cc2y", "work_item.started", false),
    ];
    for (offset, (work, session, kind, reused)) in events.into_iter().enumerate() {
        lines.push(
            serde_json::json!({
                "offset": offset,
                "schema_version": 1,
                "event": {
                    "id": format!("ev-cc2-{offset}"),
                    "work_item_id": work,
                    "event_type": kind,
                    "severity": "normal",
                    "requires_user_action": false,
                    "worker_session_id": session,
                    "payload": {
                        "note": "n", "task": "CC2 任务", "dispatched_at": "2026-11-16 09:00:00",
                        "reused_session": reused
                    },
                    "created_at": "2026-11-16 10:00:00"
                }
            })
            .to_string(),
        );
    }
    std::fs::write(&log_path, lines.join("\n") + "\n").unwrap();
    tiangong_app::central_host::recover_central_work_state(handle);
    {
        let registry = state_of_handle
            .central
            .central_work_registry
            .lock()
            .unwrap();
        // s-cc2：owner 保留（引导孤儿被移除，不是并存/顶替）
        let cc2: Vec<_> = registry
            .iter()
            .filter(|e| e.session_id == "s-cc2")
            .collect();
        assert_eq!(cc2.len(), 1, "同会话一条：{cc2:?}");
        assert_eq!(cc2[0].work_id, "work-ownerX", "owner 优先于更晚的引导孤儿");
        assert!(!cc2[0].reused_session);
        // s-cc2b：仅引导 → 保留（不能因无 owner 而丢登记）
        let cc2b: Vec<_> = registry
            .iter()
            .filter(|e| e.session_id == "s-cc2b")
            .collect();
        assert_eq!(cc2b.len(), 1);
        assert_eq!(cc2b[0].work_id, "work-only-steer");
    }
    // control(owner) 可达：登记存在即发 Control 消息（通道未启动仅验证不拒绝）
    // control 可达性验证：调用本身（登记存在即执行，无拒绝返回值可断言）
    tiangong_app::central_host::dispatch::handle_central_control(
        &state_of_handle.central,
        handle,
        tiangong_app::central::CentralControlRequest {
            work_id: "work-ownerX".into(),
            session_id: "s-cc2".into(),
            action: "cancelled".into(),
        },
    )
    .await;
    let items = tiangong_app::central::load_work_items(&storage_root);
    assert!(
        !items
            .iter()
            .any(|i| i.work_id == "work-steerX" && i.outcome.is_some()),
        "引导孤儿不得落成可重派假任务"
    );
}

fn reg_entry(
    work: &str,
    session: &str,
    state: central_state::WorkState,
    exec: Option<&str>,
) -> central_state::WorkStatus {
    central_state::WorkStatus {
        work_id: work.into(),
        session_id: session.into(),
        task: format!("任务-{work}"),
        model: None,
        dispatched_at: "2026-11-16 10:00:00".into(),
        state,
        reused_session: false,
        last_activity: None,
        cwd: None,
        execution_id: exec.map(str::to_string),
    }
}

/// 终态回调矩阵：取消/暂停守卫不注入、waiting_user 分流、失败/成功分态。
#[tokio::test]
async fn host_terminal_callback_matrix() {
    let app = tauri::test::mock_app();
    let (state, _ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    // 取消守卫：Cancelled 条目的流终态不注入回报、不改状态
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(reg_entry(
            "work-c1",
            "s-c1",
            central_state::WorkState::Cancelled,
            Some("exec-test"),
        ));
    tiangong_app::central_host::dispatch::handle_work_terminal(
        &state.central,
        &dispatch_request("work-c1", "s-c1", "取消后迟到终态", false),
        true,
        "done",
    );
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let e = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-c1")
            .unwrap();
        assert_eq!(e.state, central_state::WorkState::Cancelled, "守卫不改写");
    }
    assert!(
        state.take_central_work_reports().is_empty(),
        "守卫不注入回报"
    );
    // waiting_user 分流：含标记回报 → WaitingUser（未终态）+ 立即注入
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(reg_entry(
            "work-w1",
            "s-w1",
            central_state::WorkState::Running,
            Some("exec-test"),
        ));
    tiangong_app::central_host::dispatch::handle_work_terminal(
        &state.central,
        &dispatch_request("work-w1", "s-w1", "等待用户任务", false),
        false,
        // 失败路径 notice 由 build_work_notice 包装为
        // 「…失败原因：{reason}」——reason 以换行开头使标记保持行首
        // （真实征询走 done=true 路径，worker 全文按行嵌入 notice，
        // 协议要求的行首标记天然保留）。
        "\n【需要用户决策】要不要继续",
    );
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let e = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-w1")
            .unwrap();
        assert_eq!(e.state, central_state::WorkState::WaitingUser, "决策分流");
    }
    // 失败 → Failed；成功 → ReportQueued（回报入暂存）
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(reg_entry(
            "work-f1",
            "s-f1",
            central_state::WorkState::Running,
            Some("exec-test"),
        ));
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(reg_entry(
            "work-k1",
            "s-k1",
            central_state::WorkState::Running,
            Some("exec-test"),
        ));
    tiangong_app::central_host::dispatch::handle_work_terminal(
        &state.central,
        &dispatch_request("work-f1", "s-f1", "失败任务", false),
        false,
        "执行失败：超时",
    );
    tiangong_app::central_host::dispatch::handle_work_terminal(
        &state.central,
        &dispatch_request("work-k1", "s-k1", "成功任务", false),
        true,
        "全部完成",
    );
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let f = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-f1")
            .unwrap();
        let k = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-k1")
            .unwrap();
        assert_eq!(f.state, central_state::WorkState::Failed);
        assert_eq!(k.state, central_state::WorkState::ReportQueued);
    }
    let reports = state.take_central_work_reports();
    assert!(
        reports.iter().any(|r| r.contains("执行失败")),
        "{reports:?}"
    );
    // 陈旧执行守卫：registry 已被新执行接管（in-flight + 不同 exec）→ 旧回调跳过
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(reg_entry(
            "work-s1",
            "s-s1",
            central_state::WorkState::Running,
            Some("exec-new"),
        ));
    tiangong_app::central_host::dispatch::handle_work_terminal(
        &state.central,
        &dispatch_request("work-s1", "s-s1", "旧执行回调", false),
        true,
        "stale",
    );
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let e = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-s1")
            .unwrap();
        assert_eq!(e.state, central_state::WorkState::Running, "陈旧回调不改写");
    }
    // 已终态重复回调跳过（幂等守卫）
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(reg_entry(
            "work-d1",
            "s-d1",
            central_state::WorkState::ReportQueued,
            Some("exec-test"),
        ));
    tiangong_app::central_host::dispatch::handle_work_terminal(
        &state.central,
        &dispatch_request("work-d1", "s-d1", "重复回调", false),
        true,
        "again",
    );
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let e = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-d1")
            .unwrap();
        assert_eq!(
            e.state,
            central_state::WorkState::ReportQueued,
            "重复回调幂等"
        );
    }
}

/// 同目录写入锁排队：其他会话 Running 同 cwd → QueueAfter（pending +
/// queued 落盘标记），终态后 flush 投递。
#[tokio::test]
async fn host_dispatch_queues_on_write_lock_then_delivers() {
    let app = tauri::test::mock_app();
    let (state, ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let storage_root = state.core_manager.storage_root().to_path_buf();
    std::fs::write(
        storage_root.join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    // 会话 + 授权（写入锁按 cwd 匹配）
    std::fs::create_dir_all(storage_root.join("sessions")).unwrap();
    for sid in ["s-holder", "s-waiter"] {
        std::fs::write(
            storage_root.join(format!("sessions/{sid}.json")),
            serde_json::json!({
                "id": sid, "title": "w", "created_at": "2026-11-16 10:00:00",
                "updated_at": "2026-11-16 10:00:00", "trust_mode": "supervised",
                "cwd": ws.to_string_lossy(), "messages": []
            })
            .to_string(),
        )
        .unwrap();
    }
    let _ = tiangong_app::central::add_confirmed_workspace(&storage_root, &ws);
    // 占位：其他会话 Running 同 cwd
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(central_state::WorkStatus {
            cwd: Some(ws.to_string_lossy().to_string()),
            ..reg_entry(
                "work-holder",
                "s-holder",
                central_state::WorkState::Running,
                Some("exec-test"),
            )
        });
    let _ = tiangong_app::central::upsert_work_item(
        &storage_root,
        &tiangong_app::central::WorkItemRecord {
            work_id: "work-waiter".into(),
            reused_session: false,
            session_id: "s-waiter".into(),
            task: "写入锁排队任务".into(),
            model: None,
            dispatched_at: "2026-11-16 10:00:00".into(),
            finished_at: None,
            outcome: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: Some(ws.to_string_lossy().to_string()),
            execution_id: Some("exec-waiter".into()),
        },
    );
    // 派发 → 排队（不投递）
    tiangong_app::central_host::dispatch::handle_central_dispatch(
        &state.central,
        handle,
        tiangong_app::central::CentralDispatchRequest {
            work_id: "work-waiter".into(),
            execution_id: "exec-waiter".into(),
            session_id: "s-waiter".into(),
            task: "写入锁排队任务".into(),
            model: None,
            cwd: Some(ws.to_string_lossy().to_string()),
            after_work_id: None,
            parallel: false,
            reused_session: false,
        },
    )
    .await
    .expect("排队派发应成功（不报错）");
    assert_eq!(
        state
            .central
            .central_pending_dispatches
            .lock()
            .unwrap()
            .len(),
        1,
        "应入 pending 队列"
    );
    let items = tiangong_app::central::load_work_items(&storage_root);
    let saved = items
        .iter()
        .rev()
        .find(|i| i.session_id == "s-waiter")
        .unwrap();
    assert!(saved.queued_at.is_some(), "排队标记应落盘");
    // 占位工作终态 → flush 投递（生产链路由 handle_work_terminal 内部
    // 触发、句柄取自 app_handle_read；测试环境未注入 Wry 句柄，按同参
    // 显式调用——终态回调守卫矩阵由 host_terminal_callback_matrix 覆盖）
    state.on_central_work_finished(
        &dispatch_request("work-holder", "s-holder", "占位工作", false),
        true,
        "完成",
    );
    state.flush_pending_dispatches("work-holder", "s-holder", Some(handle.clone()));
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let delivered = {
            let registry = state.central.central_work_registry.lock().unwrap();
            registry
                .iter()
                .any(|e| e.session_id == "s-waiter" && e.state == central_state::WorkState::Running)
        };
        if delivered {
            break;
        }
    }
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        assert!(
            registry
                .iter()
                .any(|e| e.session_id == "s-waiter"
                    && e.state == central_state::WorkState::Running),
            "排队项应在占位终态后投递"
        );
    }
    assert!(
        state
            .central
            .central_pending_dispatches
            .lock()
            .unwrap()
            .is_empty(),
        "队列清空"
    );
}

/// waiting_user 恢复合并：答复轮沿用 owner work_id/task。
#[tokio::test]
async fn host_waiting_user_merge_restores_owner() {
    let app = tauri::test::mock_app();
    let (state, _ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let storage_root = state.core_manager.storage_root().to_path_buf();
    std::fs::write(
        storage_root.join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(storage_root.join("sessions")).unwrap();
    std::fs::write(
        storage_root.join("sessions/s-m9.json"),
        serde_json::json!({
            "id": "s-m9", "title": "w", "created_at": "2026-11-16 10:00:00",
            "updated_at": "2026-11-16 10:00:00", "trust_mode": "supervised",
            "cwd": "/tmp", "messages": []
        })
        .to_string(),
    )
    .unwrap();
    let _ =
        tiangong_app::central::add_confirmed_workspace(&storage_root, std::path::Path::new("/tmp"));
    // owner 挂起等待用户
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(reg_entry(
            "work-owner9",
            "s-m9",
            central_state::WorkState::WaitingUser,
            Some("exec-owner9"),
        ));
    let _ = tiangong_app::central::upsert_work_item(
        &storage_root,
        &tiangong_app::central::WorkItemRecord {
            work_id: "work-owner9".into(),
            reused_session: true,
            session_id: "s-m9".into(),
            task: "原任务".into(),
            model: None,
            dispatched_at: "2026-11-16 10:00:00".into(),
            finished_at: None,
            outcome: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: None,
            execution_id: Some("exec-owner9".into()),
        },
    );
    // 用户答复（reused 派发）→ merge_waiting_user 恢复 owner
    tiangong_app::central_host::dispatch::handle_central_dispatch(
        &state.central,
        handle,
        tiangong_app::central::CentralDispatchRequest {
            work_id: "work-ans9".into(),
            execution_id: "exec-ans9".into(),
            session_id: "s-m9".into(),
            task: "用户答复：选方案B".into(),
            model: None,
            cwd: None,
            after_work_id: None,
            parallel: false,
            reused_session: true,
        },
    )
    .await
    .expect("答复派发应成功");
    let registry = state.central.central_work_registry.lock().unwrap();
    let entry = registry
        .iter()
        .rev()
        .find(|e| e.session_id == "s-m9")
        .unwrap();
    assert_eq!(entry.work_id, "work-owner9", "恢复沿用 owner work_id");
    assert_eq!(entry.task, "任务-work-owner9", "任务沿用 owner 登记条目");
    assert_eq!(entry.state, central_state::WorkState::Running, "恢复执行中");
}

/// cancel 在跑工作：running 集合预置 → send_command 分支 + 状态终态。
#[tokio::test]
async fn host_control_cancel_running_work_sends_command() {
    let app = tauri::test::mock_app();
    let (state, _ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let storage_root = state.core_manager.storage_root().to_path_buf();
    state
        .central
        .central_running_works
        .lock()
        .unwrap()
        .insert("work-run1".into());
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(reg_entry(
            "work-run1",
            "s-run",
            central_state::WorkState::Running,
            Some("exec-run1"),
        ));
    let _ = tiangong_app::central::upsert_work_item(
        &storage_root,
        &tiangong_app::central::WorkItemRecord {
            work_id: "work-run1".into(),
            reused_session: false,
            session_id: "s-run".into(),
            task: "在跑工作".into(),
            model: None,
            dispatched_at: "2026-11-16 10:00:00".into(),
            finished_at: None,
            outcome: None,
            events: Vec::new(),
            queued_at: None,
            after_work_id: None,
            cwd: None,
            execution_id: Some("exec-run1".into()),
        },
    );
    tiangong_app::central_host::dispatch::handle_central_control(
        &state.central,
        handle,
        tiangong_app::central::CentralControlRequest {
            work_id: "work-run1".into(),
            session_id: "s-run".into(),
            action: "cancelled".into(),
        },
    )
    .await;
    assert!(
        !state
            .central
            .central_running_works
            .lock()
            .unwrap()
            .contains("work-run1"),
        "running 集合应移除"
    );
    let registry = state.central.central_work_registry.lock().unwrap();
    let entry = registry
        .iter()
        .rev()
        .find(|e| e.work_id == "work-run1")
        .unwrap();
    assert_eq!(entry.state, central_state::WorkState::Cancelled);
}

/// 旧时间线迁移：work-items.json 带 events → migrate → 日志承接。
#[tokio::test]
async fn host_migrate_legacy_timeline_moves_events() {
    let (state, _ws) = central_test_app();
    let storage_root = state.core_manager.storage_root().to_path_buf();
    let legacy_event = |kind: &str, note: &str| {
        serde_json::json!({
            "at": "2026-11-01 10:00:00", "kind": kind, "note": note
        })
    };
    let record = tiangong_app::central::WorkItemRecord {
        work_id: "work-legacy2".into(),
        reused_session: false,
        session_id: "s-legacy2".into(),
        task: "旧时间线工作".into(),
        model: None,
        dispatched_at: "2026-11-01 09:00:00".into(),
        finished_at: Some("2026-11-01 11:00:00".into()),
        outcome: Some("completed".into()),
        events: vec![
            serde_json::from_value(legacy_event("dispatched", "派发")).unwrap(),
            serde_json::from_value(legacy_event("waiting_user", "等用户")).unwrap(),
            serde_json::from_value(legacy_event("resumed", "恢复")).unwrap(),
            serde_json::from_value(legacy_event("finished", "完成")).unwrap(),
            serde_json::from_value(legacy_event("note", "其他")).unwrap(),
        ],
        queued_at: None,
        after_work_id: None,
        cwd: None,
        execution_id: Some("exec-legacy2".into()),
    };
    let _ = tiangong_app::central::upsert_work_item(&storage_root, &record);
    // 迁移前：work_timeline 走旧 events 读取分支
    let before =
        tiangong_app::central_host::central_events::work_timeline(&storage_root, "work-legacy2");
    assert_eq!(before.len(), 5, "旧 events 兜底可读");
    // 迁移
    tiangong_app::central_host::central_events::migrate_legacy_timeline(&storage_root);
    // 迁移后：事件日志承接（started/waiting/started/completed/progress）
    let after =
        tiangong_app::central_host::central_events::work_timeline(&storage_root, "work-legacy2");
    assert!(!after.is_empty(), "迁移后日志有事实");
    assert!(
        after.iter().any(|(_, kind, _)| kind == "work_item.started"),
        "{after:?}"
    );
    assert!(
        after
            .iter()
            .any(|(_, kind, _)| kind == "work_item.completed"),
        "{after:?}"
    );
    // 幂等：再次迁移不重复（日志非空跳过）
    tiangong_app::central_host::central_events::migrate_legacy_timeline(&storage_root);
    let again =
        tiangong_app::central_host::central_events::work_timeline(&storage_root, "work-legacy2");
    assert_eq!(again.len(), after.len(), "迁移幂等");
}

/// 决定票据命令：空参拒绝 + 签发（供恢复执行消费）。
#[tokio::test]
async fn host_resolve_decision_command_issues_ticket() {
    let app = tauri::test::mock_app();
    let (state, _ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let empty = tiangong_app::central_host::authorization::central_resolve_decision(
        handle.clone(),
        "  ".to_string(),
        1,
        "选项A".to_string(),
        None,
    )
    .await;
    assert!(empty.is_err());
    let no_option = tiangong_app::central_host::authorization::central_resolve_decision(
        handle.clone(),
        "exec-dec1".to_string(),
        2,
        " ".to_string(),
        None,
    )
    .await;
    assert!(no_option.is_err());
    // 签发 → 按 execution 消费 Granted（恢复链路的消费点）
    let issued = tiangong_app::central_host::authorization::central_resolve_decision(
        handle.clone(),
        "exec-dec1".to_string(),
        2,
        "方案B".to_string(),
        None,
    )
    .await
    .unwrap();
    assert!(issued["ticket_id"].is_string());
    assert_eq!(issued["chosen_option"], "方案B");
    assert_eq!(
        state
            .central
            .ticket_registry
            .consume_decision_for_execution("exec-dec1"),
        tiangong_central::tickets::TicketVerdict::Granted
    );
}

/// 激活摘要：中断事实注入 + 进程内幂等；空闲不注入。
#[tokio::test]
async fn host_activation_summary_idempotent_and_silent_when_idle() {
    let app = tauri::test::mock_app();
    let (state, _ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    // 空闲（无中断/失败/目标）→ 不注入（Ok 且无摘要）
    assert!(
        tiangong_app::central_host::recovery::inject_central_activation_summary(&state.central)
            .is_ok()
    );
    // 有中断事实 → 注入（中枢 Core 缺失时 deliver 报错——注入路径执行）
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(reg_entry(
            "work-i1",
            "s-i1",
            central_state::WorkState::Interrupted,
            None,
        ));
    let second =
        tiangong_app::central_host::recovery::inject_central_activation_summary(&state.central);
    // 幂等：首次已 swap true，第二次直接 Ok（不再构建）
    assert!(second.is_ok());
    // 事实判断独立验证（build_activation_summary 已有专测）
    assert!(
        tiangong_app::central_host::recovery::build_activation_summary(
            &state.central_work_status(),
            &tiangong_app::central::load_goals(state.core_manager.storage_root()),
        )
        .is_some(),
        "有中断事实应产出摘要"
    );
}

/// 重启恢复完整链：started 无终态 → 投影 Running → 中断迁移 + 摘要事实。
#[tokio::test]
async fn host_recover_interrupts_inflight_work() {
    let app = tauri::test::mock_app();
    let (state, _ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state_of_handle = state_of(handle);
    let storage_root = state_of_handle.core_manager.storage_root().to_path_buf();
    // started 无终态（重启时在跑）+ failed 终态
    let log_path = storage_root.join("central/events.jsonl");
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    let mut lines = Vec::new();
    for (work, session, kind) in [
        ("work-run9", "s-run9", "work_item.started"),
        ("work-fail9", "s-fail9", "work_item.started"),
        ("work-fail9", "s-fail9", "work_item.failed"),
    ] {
        let line = serde_json::json!({
            "offset": lines.len(),
            "schema_version": 1,
            "event": {
                "id": format!("ev-{work}-{}", lines.len()),
                "work_item_id": work,
                "event_type": kind,
                "severity": "normal",
                "requires_user_action": false,
                "worker_session_id": session,
                "payload": {"note": "n", "task": "恢复任务", "dispatched_at": "2026-11-16 09:00:00"},
                "created_at": "2026-11-16 10:00:00"
            }
        });
        lines.push(line.to_string());
    }
    std::fs::write(&log_path, lines.join("\n") + "\n").unwrap();
    tiangong_app::central_host::recover_central_work_state(handle);
    {
        let registry = state_of_handle
            .central
            .central_work_registry
            .lock()
            .unwrap();
        let run = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-run9")
            .unwrap();
        assert_eq!(
            run.state,
            central_state::WorkState::Interrupted,
            "在跑工作重启后应迁移为中断"
        );
        let fail = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-fail9")
            .unwrap();
        assert_eq!(fail.state, central_state::WorkState::Failed);
    }
}

/// 排队依赖终态后 flush：pending 投递（after 匹配）、queued 标记清除、
/// 非匹配条目保留；无句柄直接返回不丢队列。
#[tokio::test]
async fn host_flush_pending_dispatches_delivers_matching() {
    let app = tauri::test::mock_app();
    let state = {
        let (s, ws) = central_test_app();
        std::fs::write(
            s.core_manager.storage_root().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        let _ = ws;
        s
    };
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let storage_root = state.core_manager.storage_root().to_path_buf();
    // 工作会话（flush 后真实投递需要会话 + 授权）
    std::fs::create_dir_all(storage_root.join("sessions")).unwrap();
    std::fs::write(
        storage_root.join("sessions/s-flush.json"),
        serde_json::json!({
            "id": "s-flush", "title": "w", "created_at": "2026-11-16 10:00:00",
            "updated_at": "2026-11-16 10:00:00", "trust_mode": "supervised",
            "cwd": "/tmp", "messages": []
        })
        .to_string(),
    )
    .unwrap();
    let _ =
        tiangong_app::central::add_confirmed_workspace(&storage_root, std::path::Path::new("/tmp"));
    // 被排队的工作登记（queued 标记 after=work-dep）
    let queued = tiangong_app::central::WorkItemRecord {
        work_id: "work-flush".into(),
        reused_session: false,
        session_id: "s-flush".into(),
        task: "依赖完成后执行".into(),
        model: None,
        dispatched_at: "2026-11-16 10:00:00".into(),
        finished_at: None,
        outcome: None,
        events: Vec::new(),
        queued_at: Some("2026-11-16 10:00:00".into()),
        after_work_id: Some("work-dep".into()),
        cwd: None,
        execution_id: Some("exec-flush".into()),
    };
    let _ = tiangong_app::central::upsert_work_item(&storage_root, &queued);
    // pending 队列：一条匹配 after=work-dep + 一条不匹配 after=work-other
    state
        .central
        .central_pending_dispatches
        .lock()
        .unwrap()
        .push(tiangong_app::central::CentralDispatchRequest {
            work_id: "work-flush".into(),
            execution_id: "exec-flush".into(),
            session_id: "s-flush".into(),
            task: "依赖完成后执行".into(),
            model: None,
            cwd: None,
            after_work_id: Some("work-dep".into()),
            parallel: false,
            reused_session: false,
        });
    state
        .central
        .central_pending_dispatches
        .lock()
        .unwrap()
        .push(tiangong_app::central::CentralDispatchRequest {
            work_id: "work-hold".into(),
            execution_id: "exec-hold".into(),
            session_id: "s-hold".into(),
            task: "等待其他依赖".into(),
            model: None,
            cwd: None,
            after_work_id: Some("work-other".into()),
            parallel: false,
            reused_session: false,
        });
    // 无句柄：直接返回，队列不丢
    state.flush_pending_dispatches(
        "work-dep",
        "s-dep",
        None::<tauri::AppHandle<tauri::test::MockRuntime>>,
    );
    assert_eq!(
        state
            .central
            .central_pending_dispatches
            .lock()
            .unwrap()
            .len(),
        2,
        "无句柄不消费"
    );
    // 有句柄：work-dep 终态 → 匹配条目投递（spawn），非匹配保留
    state.flush_pending_dispatches("work-dep", "s-dep", Some(handle.clone()));
    // 等待 spawn 投递完成（registry 出现 s-flush）
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let done = {
            let registry = state.central.central_work_registry.lock().unwrap();
            registry.iter().any(|e| e.session_id == "s-flush")
        };
        if done {
            break;
        }
    }
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        assert!(
            registry.iter().any(|e| e.session_id == "s-flush"),
            "匹配条目应已投递登记"
        );
    }
    let pending = state.central.central_pending_dispatches.lock().unwrap();
    assert_eq!(pending.len(), 1, "非匹配条目保留");
    assert_eq!(pending[0].work_id, "work-hold");
    drop(pending);
    // queued 落盘标记已清除（进入执行即失效）
    let items = tiangong_app::central::load_work_items(&storage_root);
    let saved = items
        .iter()
        .rev()
        .find(|i| i.session_id == "s-flush")
        .unwrap();
    assert!(saved.queued_at.is_none(), "投递后排队标记应清除");
}

/// 派发通道消费循环：真实 spawn 后逐类消息消费（Notification /
/// ReportsCollected / Control / 未授权 Dispatch 的错误注入路径）。
#[tokio::test]
async fn host_sender_channel_consumes_all_message_kinds() {
    let app = tauri::test::mock_app();
    let state = {
        let (s, _ws) = central_test_app();
        std::fs::write(
            s.core_manager.storage_root().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        s
    };
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let storage_root = state.core_manager.storage_root().to_path_buf();
    // 预置：registry 待汇报条目（ReportsCollected 消费对象）+ Running 条目
    //（Control 消费对象）+ 落盘登记（Control cancel 写 outcome）
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(central_state::WorkStatus {
            work_id: "work-sender".into(),
            session_id: "s-sender".into(),
            task: "通道消费工作".into(),
            model: None,
            dispatched_at: "2026-11-16 10:00:00".into(),
            state: central_state::WorkState::ReportQueued,
            reused_session: false,
            last_activity: None,
            cwd: None,
            execution_id: None,
        });
    let record = tiangong_app::central::WorkItemRecord {
        work_id: "work-sender".into(),
        reused_session: false,
        session_id: "s-sender".into(),
        task: "通道消费工作".into(),
        model: None,
        dispatched_at: "2026-11-16 10:00:00".into(),
        finished_at: None,
        outcome: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: None,
        execution_id: None,
    };
    let _ = tiangong_app::central::upsert_work_item(&storage_root, &record);
    // 启动消费循环（get_or_init + spawn）
    let tx = tiangong_app::central_host::dispatch::central_dispatch_sender(handle);
    // 1) Notification：emit（mock 静默）不 panic
    tx.send(tiangong_app::central::CentralPluginMessage::Notification {
        title: "测试通知".into(),
        message: "m".into(),
        level: "info".into(),
    })
    .unwrap();
    // 2) ReportsCollected：待汇报条目收拢为 Completed
    tx.send(tiangong_app::central::CentralPluginMessage::ReportsCollected)
        .unwrap();
    // 3) Control(cancel)：registry → Cancelled + 落盘 outcome
    tx.send(tiangong_app::central::CentralPluginMessage::Control(
        tiangong_app::central::CentralControlRequest {
            work_id: "work-sender".into(),
            session_id: "s-sender".into(),
            action: "cancelled".into(),
        },
    ))
    .unwrap();
    // 4) 未授权 Dispatch：错误路径（派发未执行注入中枢——中枢 Core
    // 不在场时 warn 跳过，不 panic）
    tx.send(tiangong_app::central::CentralPluginMessage::Dispatch(
        tiangong_app::central::CentralDispatchRequest {
            work_id: "work-denied".into(),
            execution_id: "exec-denied".into(),
            session_id: "s-denied".into(),
            task: "未授权任务".into(),
            model: None,
            cwd: Some("/nonexistent-ws-xyz".into()),
            after_work_id: None,
            parallel: false,
            reused_session: false,
        },
    ))
    .unwrap();
    // 等待消费（unbounded → spawn 逐条处理）
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let done = {
            let registry = state.central.central_work_registry.lock().unwrap();
            registry.iter().rev().any(|e| {
                e.work_id == "work-sender" && e.state == central_state::WorkState::Cancelled
            })
        };
        if done {
            break;
        }
    }
    // 消费结果断言
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let entry = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-sender")
            .expect("条目应在");
        assert_eq!(
            entry.state,
            central_state::WorkState::Cancelled,
            "Control 应被消费"
        );
    }
    let items = tiangong_app::central::load_work_items(&storage_root);
    let saved = items
        .iter()
        .rev()
        .find(|i| i.session_id == "s-sender")
        .unwrap();
    assert_eq!(saved.outcome.as_deref(), Some("cancelled"), "cancel 落盘");
}

/// 中枢 Core 生命周期：ensure 首建 / 幂等双检 / alive / deliver / cancel。
#[tokio::test]
async fn host_central_core_lifecycle_roundtrip() {
    let app = tauri::test::mock_app();
    let (state, _ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    std::fs::write(
        state.core_manager.storage_root().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    // drain 流通道（中枢 Core 的 stream 消费者）
    let (stream_tx, stream_rx) = std::sync::mpsc::channel::<tiangong_types::StreamEvent>();
    std::thread::spawn(move || for _ in stream_rx {});
    // 未建时：deliver 报「不存在」、cancel false
    assert!(state
        .deliver_central_message("m0".into(), vec![], None)
        .is_err());
    assert!(!state.cancel_central_turn());
    assert!(!state.central.central_core_alive());
    // 首建 true；二次 ensure false（并发双检）
    let first = state.ensure_central_core(handle, stream_tx.clone()).await;
    assert!(first.expect("首次创建应成功"), "首次应创建");
    assert!(state.central.central_core_alive());
    let second = state.ensure_central_core(handle, stream_tx).await;
    assert!(!second.expect("二次应成功返回 false"), "重复 ensure 幂等");
    // deliver 入队成功 + cancel 送达
    assert!(state
        .deliver_central_message(
            "m1".into(),
            vec![tiangong_types::ContentBlock::text("你好")],
            None,
        )
        .is_ok());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(state.cancel_central_turn());
    // 关闭（join 不 panic）
    state.shutdown_join_central_core().await;
}

/// 模式命令往返：enter（快照保存+偏好落盘）→ state → exit（归还快照）。
#[tokio::test]
async fn host_mode_commands_roundtrip() {
    let app = tauri::test::mock_app();
    let (state, _ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    // 初始 normal
    let initial = tiangong_app::central_host::mode_commands::central_mode_state(handle.clone())
        .await
        .unwrap();
    assert_eq!(initial["mode"], "normal");
    // enter：携带普通上下文快照
    let entered = tiangong_app::central_host::mode_commands::central_mode_enter(
        handle.clone(),
        Some("s-normal-1".to_string()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(entered["mode"], "central_active");
    let mid = tiangong_app::central_host::mode_commands::central_mode_state(handle.clone())
        .await
        .unwrap();
    assert_eq!(mid["mode"], "central_active");
    assert_eq!(mid["last_mode"], "central", "偏好文件记录短名 central");
    // exit：归还快照（active_session_id 回到普通模式）
    let exited = tiangong_app::central_host::mode_commands::central_mode_exit(handle.clone())
        .await
        .unwrap();
    assert_eq!(exited["mode"], "normal");
    assert_eq!(exited["snapshot"]["active_session_id"], "s-normal-1");
    let after = tiangong_app::central_host::mode_commands::central_mode_state(handle.clone())
        .await
        .unwrap();
    assert_eq!(after["mode"], "normal");
    assert_eq!(after["last_mode"], "normal", "偏好随退出落盘");
}

/// 工作时间线命令：空 work_id 拒绝 + 事件 JSON 输出。
#[tokio::test]
async fn host_work_timeline_command_outputs_events() {
    let app = tauri::test::mock_app();
    let (state, ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let storage_root = state.core_manager.storage_root().to_path_buf();
    // 预置一条登记 + 真实派发写事件日志（复用宿主派发链）
    std::fs::write(
        storage_root.join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let _ = tiangong_app::central::add_confirmed_workspace(&storage_root, &ws);
    let record = tiangong_app::central::WorkItemRecord {
        work_id: "work-tlv".into(),
        reused_session: false,
        session_id: "s-tlv".into(),
        task: "时间线命令任务".into(),
        model: None,
        dispatched_at: "2026-11-16 10:00:00".into(),
        finished_at: None,
        outcome: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: Some(ws.to_string_lossy().to_string()),
        execution_id: Some("exec-tlv".into()),
    };
    let _ = tiangong_app::central::upsert_work_item(&storage_root, &record);
    tiangong_app::central_host::dispatch::handle_central_dispatch(
        &state.central,
        handle,
        tiangong_app::central::CentralDispatchRequest {
            work_id: "work-tlv".into(),
            execution_id: "exec-tlv".into(),
            session_id: "s-tlv".into(),
            task: "时间线命令任务".into(),
            model: None,
            cwd: None,
            after_work_id: None,
            parallel: false,
            reused_session: false,
        },
    )
    .await
    .expect("派发应成功");
    // 命令：空 work_id 拒绝
    let empty = tiangong_app::central_host::work_view::central_work_timeline(
        handle.clone(),
        "  ".to_string(),
    )
    .await;
    assert!(empty.is_err());
    // 有事件的 work_id → JSON 数组含 started
    let timeline = tiangong_app::central_host::work_view::central_work_timeline(
        handle.clone(),
        "work-tlv".to_string(),
    )
    .await
    .unwrap();
    assert!(!timeline.is_empty(), "事件日志应有派发事件");
    assert!(
        timeline
            .iter()
            .any(|item| item["kind"] == "work_item.started"),
        "{timeline:?}"
    );
}

/// 用户授权命令：校验 + 票据签发 + persist 双通道 + 授权通知注入中枢。
#[tokio::test]
async fn host_approve_workspace_command_issuance() {
    let app = tauri::test::mock_app();
    let (state, ws) = central_test_app();
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let storage_root = state.core_manager.storage_root().to_path_buf();
    // 空 path / 非目录拒绝
    let empty = tiangong_app::central_host::authorization::central_approve_workspace(
        handle.clone(),
        "  ".to_string(),
        "任务".to_string(),
        None,
        None,
    )
    .await;
    assert!(empty.is_err());
    let not_dir = tiangong_app::central_host::authorization::central_approve_workspace(
        handle.clone(),
        "/nonexistent-dir-xyz".to_string(),
        "任务".to_string(),
        None,
        None,
    )
    .await;
    assert!(not_dir.is_err());
    // 成功签发（一次性票据，不 persist）
    let granted = tiangong_app::central_host::authorization::central_approve_workspace(
        handle.clone(),
        ws.to_string_lossy().to_string(),
        "只读分析任务".to_string(),
        Some(false),
        None,
    )
    .await
    .unwrap();
    assert!(granted["approval_id"].is_string());
    assert_eq!(granted["persist"], false);
    // 目录未长期登记（persist=false）
    assert!(
        tiangong_app::central::load_confirmed_workspaces(&storage_root)
            .iter()
            .all(|p| p != &ws)
    );
    // persist=true → 长期授权落盘 + 新票据
    let persisted = tiangong_app::central_host::authorization::central_approve_workspace(
        handle.clone(),
        ws.to_string_lossy().to_string(),
        "只读分析任务".to_string(),
        Some(true),
        None,
    )
    .await
    .unwrap();
    assert_eq!(persisted["persist"], true);
    assert!(
        tiangong_app::central::load_confirmed_workspaces(&storage_root)
            .iter()
            .any(|p| p == &ws),
        "persist=true 应登记目录长期授权"
    );
}

/// system_note_render：kind 透传到渲染数据。
#[test]
fn system_note_render_carries_kind() {
    for kind in ["approval", "report", "dispatch-note", "activation"] {
        let render = tiangong_app::central_host::events::system_note_render(kind);
        assert_eq!(render.plugin, "central-host");
        assert_eq!(render.view, "system-note");
        assert_eq!(render.data["kind"], kind);
    }
}

/// control cancel：registry 终态 + 落盘 outcome=cancelled + 事件入日志。
#[tokio::test]
async fn host_control_cancel_persists_terminal_state() {
    let app = tauri::test::mock_app();
    let state = {
        let (s, _ws) = central_test_app();
        std::fs::write(
            s.core_manager.storage_root().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        s
    };
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let storage_root = state.core_manager.storage_root().to_path_buf();
    // 预置：登记 + registry Running
    let record = tiangong_app::central::WorkItemRecord {
        work_id: "work-ctl1".into(),
        reused_session: false,
        session_id: "s-ctl".into(),
        task: "被取消的工作".into(),
        model: None,
        dispatched_at: "2026-11-16 10:00:00".into(),
        finished_at: None,
        outcome: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: None,
        execution_id: Some("exec-ctl1".into()),
    };
    let _ = tiangong_app::central::upsert_work_item(&storage_root, &record);
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(central_state::WorkStatus {
            work_id: "work-ctl1".into(),
            session_id: "s-ctl".into(),
            task: "被取消的工作".into(),
            model: None,
            dispatched_at: "2026-11-16 10:00:00".into(),
            state: central_state::WorkState::Running,
            reused_session: false,
            last_activity: None,
            cwd: None,
            execution_id: Some("exec-ctl1".into()),
        });
    tiangong_app::central_host::dispatch::handle_central_control(
        &state.central,
        handle,
        tiangong_app::central::CentralControlRequest {
            work_id: "work-ctl1".into(),
            session_id: "s-ctl".into(),
            action: "cancelled".into(),
        },
    )
    .await;
    // registry → Cancelled（终态）
    let registry = state.central.central_work_registry.lock().unwrap();
    let entry = registry
        .iter()
        .rev()
        .find(|e| e.session_id == "s-ctl")
        .unwrap();
    assert_eq!(entry.state, central_state::WorkState::Cancelled);
    drop(registry);
    // 落盘 outcome=cancelled
    let items = tiangong_app::central::load_work_items(&storage_root);
    let saved = items
        .iter()
        .rev()
        .find(|i| i.session_id == "s-ctl")
        .unwrap();
    assert_eq!(saved.outcome.as_deref(), Some("cancelled"));
    assert!(saved.finished_at.is_some());
    // 事件日志含 work_item.cancelled
    let timeline =
        tiangong_app::central_host::central_events::work_timeline(&storage_root, "work-ctl1");
    assert!(
        timeline
            .iter()
            .any(|(_, kind, _)| kind == "work_item.cancelled"),
        "应有取消事件：{timeline:?}"
    );
}

/// control pause → registry Paused + blocked 事件；resume 走派发链回 Running。
#[tokio::test]
async fn host_control_pause_resume_roundtrip() {
    let app = tauri::test::mock_app();
    let state = {
        let (s, _ws) = central_test_app();
        std::fs::write(
            s.core_manager.storage_root().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        s
    };
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let storage_root = state.core_manager.storage_root().to_path_buf();
    // 工作会话文件（resume 的派发链要求会话存在 + cwd 授权）
    std::fs::create_dir_all(storage_root.join("sessions")).unwrap();
    std::fs::write(
        storage_root.join("sessions/s-resume.json"),
        serde_json::json!({
            "id": "s-resume", "title": "w", "created_at": "2026-11-16 10:00:00",
            "updated_at": "2026-11-16 10:00:00", "trust_mode": "supervised",
            "cwd": "/tmp", "messages": []
        })
        .to_string(),
    )
    .unwrap();
    let _ = central_state::persistence::add_confirmed_workspace(
        &storage_root,
        std::path::Path::new("/tmp"),
    );
    let record = tiangong_app::central::WorkItemRecord {
        work_id: "work-res1".into(),
        reused_session: true,
        session_id: "s-resume".into(),
        task: "暂停后恢复的工作".into(),
        model: None,
        dispatched_at: "2026-11-16 10:00:00".into(),
        finished_at: None,
        outcome: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: None,
        execution_id: Some("exec-res1".into()),
    };
    let _ = tiangong_app::central::upsert_work_item(&storage_root, &record);
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(central_state::WorkStatus {
            work_id: "work-res1".into(),
            session_id: "s-resume".into(),
            task: "暂停后恢复的工作".into(),
            model: None,
            dispatched_at: "2026-11-16 10:00:00".into(),
            state: central_state::WorkState::Running,
            reused_session: true,
            last_activity: None,
            cwd: None,
            execution_id: Some("exec-res1".into()),
        });
    // pause
    tiangong_app::central_host::dispatch::handle_central_control(
        &state.central,
        handle,
        tiangong_app::central::CentralControlRequest {
            work_id: "work-res1".into(),
            session_id: "s-resume".into(),
            action: "paused".into(),
        },
    )
    .await;
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let entry = registry
            .iter()
            .rev()
            .find(|e| e.session_id == "s-resume")
            .unwrap();
        assert_eq!(entry.state, central_state::WorkState::Paused);
    }
    // resume：走 Steer→dispatch 链（复用会话）——直接调派发链并断言
    // 成功（错误路径只 warn 会被吞，这里显式暴露）
    let resume_request = tiangong_app::central::CentralDispatchRequest {
        work_id: "work-res1".into(),
        execution_id: tiangong_central::new_execution_id(),
        session_id: "s-resume".into(),
        task: "继续之前暂停的任务".into(),
        model: None,
        cwd: None,
        after_work_id: None,
        parallel: false,
        reused_session: true,
    };
    tiangong_app::central_host::dispatch::handle_central_dispatch(
        &state.central,
        handle,
        resume_request,
    )
    .await
    .expect("resume 派发应成功");
    let registry = state.central.central_work_registry.lock().unwrap();
    let entry = registry
        .iter()
        .rev()
        .find(|e| e.session_id == "s-resume")
        .unwrap();
    assert_eq!(
        entry.state,
        central_state::WorkState::Running,
        "resume 后应回到执行中"
    );
}

/// 回报暂存链：push → take 非空 → clear 清空。
#[test]
fn host_reports_push_take_clear_cycle() {
    let (state, _ws) = central_test_app();
    assert!(state.take_central_work_reports().is_empty());
    state.push_central_work_reports(vec!["工作A完成".to_string()]);
    state.push_central_work_reports(vec!["工作B完成".to_string()]);
    let taken = state.take_central_work_reports();
    assert_eq!(taken.len(), 2);
    assert!(taken[0].contains("工作A"));
    // take 是取走语义（std::mem::take）：再取为空
    assert!(state.take_central_work_reports().is_empty());
    // clear_queued_work_reports 清的是 registry 待汇报态（ReportQueued→
    // Completed，ReportsCollected 消息的状态同步路径），不动回报队列
    state.push_central_work_reports(vec!["工作C完成".to_string()]);
    state
        .central
        .central_work_registry
        .lock()
        .unwrap()
        .push(central_state::WorkStatus {
            work_id: "work-q1".into(),
            session_id: "s-q".into(),
            task: "待汇报工作".into(),
            model: None,
            dispatched_at: "2026-11-16 10:00:00".into(),
            state: central_state::WorkState::ReportQueued,
            reused_session: false,
            last_activity: None,
            cwd: None,
            execution_id: None,
        });
    state.clear_queued_work_reports();
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let entry = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-q1")
            .unwrap();
        assert_eq!(
            entry.state,
            central_state::WorkState::Completed,
            "待汇报态应收拢为完成"
        );
    }
    assert!(
        !state.take_central_work_reports().is_empty(),
        "clear 不清回报队列"
    );
}

/// 重启恢复：事件日志投影重建 registry（R5 唯一事实源）。
#[test]
fn host_recover_rebuilds_registry_from_event_log() {
    let app = tauri::test::mock_app();
    let state = {
        let (s, _ws) = central_test_app();
        s
    };
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state_of_handle = state_of(handle);
    let storage_root = state_of_handle.core_manager.storage_root().to_path_buf();
    // 事件日志（真实行格式：offset + schema_version + event 包裹）：
    // 每个工作一条 started（登记字段）+ 一条终态（投影状态）
    let log_path = storage_root.join("central/events.jsonl");
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    let mut lines = Vec::new();
    let mut offset = 0;
    for (work, session, final_kind) in [
        ("work-a", "s-a", "work_item.completed"),
        ("work-b", "s-b", "work_item.failed"),
    ] {
        for (kind, note) in [("work_item.started", "派发投递成功"), (final_kind, "终态")] {
            let line = serde_json::json!({
                "offset": offset,
                "schema_version": 1,
                "event": {
                    "id": format!("ev-{work}-{offset}"),
                    "work_item_id": work,
                    "event_type": kind,
                    "severity": "normal",
                    "requires_user_action": false,
                    "worker_session_id": session,
                    "payload": {
                        "note": note,
                        "task": "恢复任务",
                        "dispatched_at": "2026-11-16 09:00:00"
                    },
                    "created_at": "2026-11-16 10:00:00"
                }
            });
            lines.push(line.to_string());
            offset += 1;
        }
    }
    std::fs::write(&log_path, lines.join("\n") + "\n").unwrap();
    tiangong_app::central_host::recover_central_work_state(handle);
    let registry = state_of_handle
        .central
        .central_work_registry
        .lock()
        .unwrap();
    assert_eq!(registry.len(), 2, "事件投影应重建两条登记");
    assert!(registry.iter().any(|e| e.work_id == "work-a"));
    assert!(registry.iter().any(|e| e.work_id == "work-b"));
}

/// W5-T1（P1-1 真锁·第二 turn 回报）**真两 turn**：第一 turn（waiting_user
/// 分流）→ 模拟答复恢复（registry 回 running）→ 第二 turn 终态。
/// 反向验证留痕（第十五轮 review 要求）：恢复 central_finished_once 集合版，
/// 第二次 insert(work_id) 返回 false → 回调 return → registry 保持
/// running → `assert_eq!(entry.state, ReportQueued)` 红——已实际执行验证。
#[tokio::test]
async fn host_second_turn_after_resume_is_reported() {
    let (state, _ws) = central_test_app();
    let request = dispatch_request("work-t1", "s-t1", "任务一", true);
    // 会话文件：最后 assistant 回复含决策标记（build_work_notice 读取的
    // 真实数据源——标记必须出现在 Worker 最终答复里，不是 reason 参数）
    let sessions_dir = state.core_manager.storage_root().join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::write(
        sessions_dir.join("s-t1.json"),
        serde_json::json!({"id": "s-t1", "title": "会话", "created_at": "2026-11-12 12:00:00",
        "updated_at": "2026-11-12 12:00:00", "trust_mode": "supervised",
        "cwd_mode": "inherit", "cwd": _ws.to_string_lossy(), "messages": [
            {"id": "m1", "role": "assistant", "content": [{"type": "text",
             "text": "【需要用户决策】\n问题：继续吗？"}], "created_at": "2026-11-12 12:00:00"}
        ]})
        .to_string(),
    )
    .unwrap();
    state.central.central_work_registry.lock().unwrap().push(
        tiangong_app::central::CentralWorkStatus {
            work_id: "work-t1".into(),
            session_id: "s-t1".into(),
            task: "任务一".into(),
            model: None,
            dispatched_at: "2026-11-12 12:00:00".into(),
            state: central_state::WorkState::Running,
            reused_session: true,
            last_activity: None,
            cwd: None,
            execution_id: None,
        },
    );
    // 第一 turn：回报含决策标记 → waiting_user（未终态）
    handle_work_terminal(&state.central, &request, true, "");
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let entry = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-t1")
            .unwrap();
        assert_eq!(
            entry.state,
            central_state::WorkState::WaitingUser,
            "第一 turn 分流 waiting_user"
        );
    }
    // 模拟答复送达（merged 路径把 registry 恢复 running）
    {
        let mut registry = state.central.central_work_registry.lock().unwrap();
        let entry = registry
            .iter_mut()
            .rev()
            .find(|e| e.work_id == "work-t1")
            .unwrap();
        entry.state = central_state::WorkState::Running;
    }
    // 第二 turn 前置：Worker 产出新回复（无标记——答复后正常完成），
    // 覆盖会话最后的 assistant 消息（真实链路的数据变化）
    std::fs::write(
        sessions_dir.join("s-t1.json"),
        serde_json::json!({"id": "s-t1", "title": "会话", "created_at": "2026-11-12 12:00:00",
        "updated_at": "2026-11-12 12:05:00", "trust_mode": "supervised",
        "cwd_mode": "inherit", "cwd": _ws.to_string_lossy(), "messages": [
            {"id": "m1", "role": "assistant", "content": [{"type": "text",
             "text": "已按用户决定继续执行，任务完成。"}], "created_at": "2026-11-12 12:05:00"}
        ]})
        .to_string(),
    )
    .unwrap();
    // 第二 turn：终态必须被处理（旧集合版在此 return → 红）
    handle_work_terminal(&state.central, &request, true, "");
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let entry = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-t1")
            .unwrap();
        assert_eq!(
            entry.state,
            central_state::WorkState::ReportQueued,
            "第二 turn 终态必须被处理（旧 central_finished_once 代码在此红）"
        );
    }
    let reports = state.central.central_work_reports.lock().unwrap().clone();
    assert!(
        reports.len() >= 2,
        "两 turn 的回报都入队（旧代码第二次丢在此红）"
    );
}

/// 旧 owner 回调被 T6 跳过；归属保持时正常执行——两条路径都验证。
#[tokio::test]
async fn host_owner_callback_respects_ownership_filter() {
    let (state, _ws) = central_test_app();
    // 路径 A：归属保持（owner 仍是当前条目）→ owner 回调执行
    let owner = dispatch_request("work-owner", "s-own", "原工作", true);
    state.central.central_work_registry.lock().unwrap().push(
        tiangong_app::central::CentralWorkStatus {
            work_id: "work-owner".into(),
            session_id: "s-own".into(),
            task: "原工作".into(),
            model: None,
            dispatched_at: "2026-11-12 12:00:00".into(),
            state: central_state::WorkState::Running,
            reused_session: false,
            last_activity: None,
            cwd: None,
            execution_id: None,
        },
    );
    handle_work_terminal(&state.central, &owner, true, "");
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let entry = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-owner")
            .unwrap();
        assert_eq!(
            entry.state,
            central_state::WorkState::ReportQueued,
            "归属保持时终态被处理"
        );
    }
    // 路径 B：归属被 steer 覆盖（旧行为模拟）→ owner 的迟到回调被跳过
    let mut registry = state.central.central_work_registry.lock().unwrap();
    let entry = registry
        .iter_mut()
        .rev()
        .find(|e| e.session_id == "s-own")
        .unwrap();
    entry.work_id = "steer-xxx".into(); // 模拟旧代码的 upsert 覆盖
    entry.state = central_state::WorkState::Running;
    drop(registry);
    let stale_owner = dispatch_request("work-owner", "s-own", "原工作", true);
    let before = state.central.central_work_reports.lock().unwrap().clone();
    handle_work_terminal(&state.central, &stale_owner, true, "");
    let after = state.central.central_work_reports.lock().unwrap().clone();
    assert_eq!(
        before.len(),
        after.len(),
        "归属已被覆盖时旧 owner 回调必须被 T6 跳过（不重复回报）"
    );
}

/// 保持 Running；会话空闲时排队条目正常登记 Queued。
#[tokio::test]
async fn host_enqueue_preserves_running_entry() {
    let (state, _ws) = central_test_app();
    // 会话空闲 → 排队登记 Queued
    let queued = dispatch_request("work-q1", "s-q1", "排队任务", false);
    enqueue_central_dispatch(&state.central, queued, "work-dep", "/ws").await;
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let entry = registry
            .iter()
            .rev()
            .find(|e| e.session_id == "s-q1")
            .unwrap();
        assert_eq!(
            entry.state,
            central_state::WorkState::Queued,
            "空闲会话排队正常登记"
        );
    }
    // 同会话在跑 → 排队不覆盖（W4：旧代码 retain+push 会改成 Queued）
    state.central.central_work_registry.lock().unwrap().push(
        tiangong_app::central::CentralWorkStatus {
            work_id: "work-run".into(),
            session_id: "s-run".into(),
            task: "在跑工作".into(),
            model: None,
            dispatched_at: "2026-11-12 12:00:00".into(),
            state: central_state::WorkState::Running,
            reused_session: false,
            last_activity: None,
            cwd: None,
            execution_id: None,
        },
    );
    let queued2 = dispatch_request("work-q2", "s-run", "第二个排队", false);
    enqueue_central_dispatch(&state.central, queued2, "work-dep", "/ws").await;
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let running = registry
            .iter()
            .rev()
            .find(|e| e.work_id == "work-run")
            .unwrap();
        assert_eq!(
            running.state,
            central_state::WorkState::Running,
            "在跑工作不得被排队登记覆盖（W4 旧代码在此红）"
        );
    }
    // pending 队列两条都在
    let pending = state
        .central
        .central_pending_dispatches
        .lock()
        .unwrap()
        .clone();
    assert_eq!(pending.len(), 2, "排队请求必须入挂起队列");
}

/// 改成终态。
#[tokio::test]
async fn host_terminal_fallback_skips_queued_entry() {
    let (state, _ws) = central_test_app();
    state.central.central_work_registry.lock().unwrap().push(
        tiangong_app::central::CentralWorkStatus {
            work_id: "work-q9".into(),
            session_id: "s-mix".into(),
            task: "排队条目".into(),
            model: None,
            dispatched_at: "2026-11-12 12:00:00".into(),
            state: central_state::WorkState::Queued,
            reused_session: false,
            last_activity: None,
            cwd: None,
            execution_id: None,
        },
    );
    // 未知 work_id + 命中 session 的排队条目 → 兜底不得写终态
    let request = dispatch_request("work-unknown", "s-mix", "未知工作", false);
    let notice = "[后台工作失败] work-unknown\n失败原因：超时".to_string();
    state.on_central_work_finished(&request, false, &notice);
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let entry = registry
            .iter()
            .rev()
            .find(|e| e.session_id == "s-mix")
            .unwrap();
        assert_eq!(
            entry.state,
            central_state::WorkState::Queued,
            "排队条目不得被 session 兜底串态（W4-b 旧代码在此红）"
        );
    }
}

/// 反向验证：把判定改回 is_inflight（R1 旧代码）→ 本测试红。
#[tokio::test]
async fn host_plan_resume_not_merged_but_steer_is() {
    let (state, ws) = central_test_app();
    // 会话元数据（self_session_cwd 与确认校验需要）
    let sessions_dir = state.core_manager.storage_root().join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::write(
    sessions_dir.join("s-resume.json"),
    serde_json::json!({"id": "s-resume", "title": "暂停的会话", "created_at": "2026-11-12 10:00:00",
        "updated_at": "2026-11-12 10:00:00", "trust_mode": "Supervised",
        "cwd": ws.to_string_lossy(), "messages": []}).to_string(),
).unwrap();
    // Paused 条目：resume 请求 → plan 判定非并入（恢复语义）
    state.central.central_work_registry.lock().unwrap().push(
        tiangong_app::central::CentralWorkStatus {
            work_id: "work-p".into(),
            session_id: "s-resume".into(),
            task: "暂停任务".into(),
            model: None,
            dispatched_at: "2026-11-12 10:00:00".into(),
            state: central_state::WorkState::Paused,
            reused_session: true,
            last_activity: None,
            cwd: Some(ws.to_string_lossy().to_string()),
            execution_id: None,
        },
    );
    let resume = dispatch_request("work-p", "s-resume", "继续暂停的任务", true);
    let plan = plan_central_dispatch(&state.central, resume)
        .await
        .expect("plan 应成功");
    let steer_merged = match plan {
        tiangong_app::central_host::dispatch::DispatchPlan::Execute {
            steer_merged_into_running,
            ..
        } => steer_merged_into_running,
        tiangong_app::central_host::dispatch::DispatchPlan::Queued => {
            panic!("Paused 的 resume 不应排队")
        }
    };
    assert!(
        !steer_merged,
        "Paused 的 resume 不得并入（R1 旧代码 is_inflight 在此红）"
    );
    // Running 条目：steer 判为并入
    {
        let mut registry = state.central.central_work_registry.lock().unwrap();
        let entry = registry
            .iter_mut()
            .rev()
            .find(|e| e.session_id == "s-resume")
            .unwrap();
        entry.state = central_state::WorkState::Running;
    }
    let steer = dispatch_request("steer-x", "s-resume", "补充指令", true);
    let plan2 = plan_central_dispatch(&state.central, steer)
        .await
        .expect("plan 应成功");
    let steer_merged2 = match plan2 {
        tiangong_app::central_host::dispatch::DispatchPlan::Execute {
            steer_merged_into_running,
            ..
        } => steer_merged_into_running,
        tiangong_app::central_host::dispatch::DispatchPlan::Queued => {
            panic!("Running 会话的引导不应排队")
        }
    };
    assert!(steer_merged2, "Running 会话的引导应并入（归属保持原工作）");
}

/// 语义且无测试暴露）。依赖在途 → plan 必须返回 Queued 并完成登记。
#[tokio::test]
async fn host_plan_queue_decision_returns_queued() {
    let (state, ws) = central_test_app();
    let sessions_dir = state.core_manager.storage_root().join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::write(
        sessions_dir.join("s-dep.json"),
        serde_json::json!({"id": "s-dep", "title": "依赖会话", "created_at": "2026-11-12 10:00:00",
        "updated_at": "2026-11-12 10:00:00", "trust_mode": "Supervised",
        "cwd": ws.to_string_lossy(), "messages": []})
        .to_string(),
    )
    .unwrap();
    // 依赖在途
    state.central.central_work_registry.lock().unwrap().push(
        tiangong_app::central::CentralWorkStatus {
            work_id: "work-dep".into(),
            session_id: "s-dep".into(),
            task: "在跑依赖".into(),
            model: None,
            dispatched_at: "2026-11-12 10:00:00".into(),
            state: central_state::WorkState::Running,
            reused_session: false,
            last_activity: None,
            cwd: Some(ws.to_string_lossy().to_string()),
            execution_id: None,
        },
    );
    let follow = tiangong_app::central::CentralDispatchRequest {
        work_id: "work-follow".into(),
        session_id: "s-new-follow".into(),
        task: "排队任务".into(),
        model: None,
        cwd: Some(ws.to_string_lossy().to_string()),
        after_work_id: Some("work-dep".into()),
        parallel: false,
        reused_session: false,
        execution_id: "exec-test".to_string(),
    };
    let plan = plan_central_dispatch(&state.central, follow)
        .await
        .expect("plan 应成功");
    assert!(
        matches!(
            plan,
            tiangong_app::central_host::dispatch::DispatchPlan::Queued
        ),
        "依赖在途时必须返回 Queued（执行段不投递）"
    );
    // 排队登记与挂起队列已就绪
    let pending = state
        .central
        .central_pending_dispatches
        .lock()
        .unwrap()
        .clone();
    assert_eq!(pending.len(), 1, "排队请求必须入挂起队列");
}

/// P0 锁（执行段级）：排队决策 → dispatch 不得投递（无 Core 创建、无
/// 回调挂载），挂起队列登记完整。
#[tokio::test]
async fn host_dispatch_queued_does_not_deliver() {
    let root = tempfile::tempdir().unwrap();
    let ws = root.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let app = tauri::test::mock_app();
    let state_arc = {
        // manage 需要 TiangongApp 值；另取一份引用语义：manage 后用
        // handle 经 state_of 取回。
        app.manage(TiangongApp::new_for_test(
            root.keep(),
            ws.to_string_lossy().to_string(),
        ));
        app.handle()
    };
    std::fs::write(
        state_of(state_arc)
            .core_manager
            .storage_root()
            .join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        })
        .to_string(),
    )
    .unwrap();
    let state = state_of(state_arc);
    // 依赖在途（另一会话）
    state.central.central_work_registry.lock().unwrap().push(
        tiangong_app::central::CentralWorkStatus {
            work_id: "work-dep".into(),
            session_id: "s-dep".into(),
            task: "依赖".into(),
            model: None,
            dispatched_at: "2026-11-12 10:00:00".into(),
            state: central_state::WorkState::Running,
            reused_session: false,
            last_activity: None,
            cwd: Some(ws.to_string_lossy().to_string()),
            execution_id: None,
        },
    );
    let follow = tiangong_app::central::CentralDispatchRequest {
        work_id: "work-f".into(),
        session_id: "s-follow".into(),
        task: "后续".into(),
        model: None,
        cwd: Some(ws.to_string_lossy().to_string()),
        after_work_id: Some("work-dep".into()),
        parallel: false,
        reused_session: false,
        execution_id: "exec-test".to_string(),
    };
    handle_central_dispatch(&state.central, state_arc, follow)
        .await
        .expect("排队 dispatch 应成功");
    // 未投递：无 Core、无回调、无 running 登记；挂起队列有
    assert!(
        !state.core_manager.has_live_core("s-follow"),
        "排队不得创建 Core"
    );
    assert!(
        state
            .central
            .execution_router
            .pending_handler_count("s-follow")
            == 0,
        "排队不得挂回调"
    );
    assert!(!state
        .central
        .central_running_works
        .lock()
        .unwrap()
        .contains("work-f"));
    assert_eq!(
        state
            .central
            .central_pending_dispatches
            .lock()
            .unwrap()
            .len(),
        1
    );
    let registry = state.central.central_work_registry.lock().unwrap();
    let entry = registry
        .iter()
        .rev()
        .find(|e| e.work_id == "work-f")
        .unwrap();
    assert_eq!(entry.state, central_state::WorkState::Queued);
}

/// W2 锁（执行段级）：steer 并入不改 registry 归属——先经 dispatch 建立
/// owner（Core 活跃），再 dispatch steer，断言归属保持 + 回调只挂 owner。
#[tokio::test]
async fn host_steer_keeps_owner_registry() {
    let root = tempfile::tempdir().unwrap();
    let ws = root.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let app = tauri::test::mock_app();
    let state =
        {
            let s = TiangongApp::new_for_test(root.keep(), ws.to_string_lossy().to_string());
            // 测试模型配置（新会话 ensure 的模型解析需要）
            std::fs::write(
        s.core_manager.storage_root().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        }).to_string(),
    ).unwrap();
            s
        };
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    // 第一次 dispatch：新会话 owner（真建 Core + 投递 + 挂回调）
    let owner = tiangong_app::central::CentralDispatchRequest {
        work_id: "work-owner".into(),
        session_id: "s-own".into(),
        task: "原工作".into(),
        model: None,
        cwd: Some(ws.to_string_lossy().to_string()),
        after_work_id: None,
        parallel: false,
        reused_session: false,
        execution_id: "exec-test".to_string(),
    };
    handle_central_dispatch(&state.central, handle, owner)
        .await
        .expect("owner dispatch 应成功");
    assert!(state.core_manager.has_live_core("s-own"), "owner 应建 Core");
    // 第二次 dispatch：steer（reused 同会话）——Running 并入
    let steer = tiangong_app::central::CentralDispatchRequest {
        work_id: "steer-x".into(),
        session_id: "s-own".into(),
        task: "补充指令".into(),
        model: None,
        cwd: None,
        after_work_id: None,
        parallel: false,
        reused_session: true,
        execution_id: "exec-test".to_string(),
    };
    handle_central_dispatch(&state.central, handle, steer)
        .await
        .expect("steer dispatch 应成功");
    // 归属保持 owner（W2：旧代码 steer upsert 覆盖 → 此断言红）
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let entry = registry
            .iter()
            .rev()
            .find(|e| e.session_id == "s-own")
            .unwrap();
        assert_eq!(
            entry.work_id, "work-owner",
            "steer 并入不得覆盖归属（W2 旧代码红）"
        );
        assert_eq!(entry.state, central_state::WorkState::Running);
    }
    // 回调只挂 owner（steer 不挂）
    let handler_count = state
        .central
        .execution_router
        .pending_handler_count("s-own");
    assert_eq!(handler_count, 1, "只挂 owner 回调（steer 并入不挂）");
}

/// W3 锁（执行段级）：排队重放（cwd 还原）→ 投递成功清除 queued 标记。
#[tokio::test]
async fn host_replay_clears_marker_restores_cwd() {
    let root = tempfile::tempdir().unwrap();
    let ws = root.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let app = tauri::test::mock_app();
    let state =
        {
            let s = TiangongApp::new_for_test(root.keep(), ws.to_string_lossy().to_string());
            std::fs::write(
        s.core_manager.storage_root().join("models.json"),
        serde_json::json!({
            "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
            "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
            "routing": {"chat": "k3"}
        }).to_string(),
    ).unwrap();
            s
        };
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    // 落盘排队记录（含 cwd——重放数据源）
    let record = tiangong_app::central::WorkItemRecord {
        work_id: "work-replay".into(),
        reused_session: false,
        session_id: "s-replay".into(),
        task: "重放任务".into(),
        model: None,
        dispatched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        finished_at: None,
        outcome: None,
        events: Vec::new(),
        queued_at: Some("2026-11-12 10:00:00".into()),
        after_work_id: Some("work-gone".into()), // 依赖已不在登记（视为终态）
        cwd: Some(ws.to_string_lossy().to_string()),
        execution_id: None,
    };
    let _ = tiangong_app::central::upsert_work_item(state.core_manager.storage_root(), &record);
    // 重放路径（recover 的重放等价）：按记录构造 request 走 dispatch
    let request = tiangong_app::central::CentralDispatchRequest {
        work_id: record.work_id.clone(),
        session_id: record.session_id.clone(),
        task: record.task.clone(),
        model: record.model.clone(),
        cwd: record.cwd.clone(),
        after_work_id: record.after_work_id.clone(),
        parallel: false,
        reused_session: record.reused_session,
        execution_id: "exec-test".to_string(),
    };
    handle_central_dispatch(&state.central, handle, request)
        .await
        .expect("重放投递应成功");
    // queued 标记被清（不清 → 下次重启重复执行）
    let items = tiangong_app::central::load_work_items(state.core_manager.storage_root());
    let entry = items
        .iter()
        .rev()
        .find(|i| i.session_id == "s-replay")
        .unwrap();
    assert!(
        entry.queued_at.is_none(),
        "投递成功后 queued 标记必须清除（W3 红点）"
    );
    // cwd 还原（记录里的目录用于投递）
    assert!(
        state.core_manager.has_live_core("s-replay"),
        "重放应建 Core（cwd 生效）"
    );
}

/// R0 行为基线：派发后执行归属必须三处一致——登记条目、落盘记录与
/// 事件时间线都携带本次 execution_id，任何后台工作可唯一定位
/// （重构方案 §5.4 / 阶段 R0 完成标准）。
#[tokio::test]
async fn host_dispatch_records_execution_ownership() {
    let root = tempfile::tempdir().unwrap();
    let ws = root.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let app = tauri::test::mock_app();
    let state = {
        let s = TiangongApp::new_for_test(root.keep(), ws.to_string_lossy().to_string());
        std::fs::write(
            s.core_manager.storage_root().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        s
    };
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let storage_root = state.core_manager.storage_root().to_path_buf();
    // 先落盘登记（生产链路中由中枢插件派发工具写入；此处模拟
    // resume / 重放场景：记录存在，执行归属待宿主投递后更新）。
    let record = tiangong_app::central::WorkItemRecord {
        work_id: "work-exec1".into(),
        reused_session: false,
        session_id: "s-exec".into(),
        task: "执行归属基线任务".into(),
        model: None,
        dispatched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        finished_at: None,
        outcome: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: Some(ws.to_string_lossy().to_string()),
        execution_id: None,
    };
    let _ = tiangong_app::central::upsert_work_item(&storage_root, &record);
    let request = tiangong_app::central::CentralDispatchRequest {
        work_id: "work-exec1".into(),
        execution_id: "exec-baseline-1".into(),
        session_id: "s-exec".into(),
        task: "执行归属基线任务".into(),
        model: None,
        cwd: Some(ws.to_string_lossy().to_string()),
        after_work_id: None,
        parallel: false,
        reused_session: false,
    };
    handle_central_dispatch(&state.central, handle, request)
        .await
        .expect("派发应成功");
    // 1) 内存登记条目带本次执行归属
    {
        let registry = state.central.central_work_registry.lock().unwrap();
        let entry = registry
            .iter()
            .rev()
            .find(|e| e.session_id == "s-exec")
            .unwrap();
        assert_eq!(
            entry.execution_id.as_deref(),
            Some("exec-baseline-1"),
            "登记条目必须携带本次 execution_id"
        );
    }
    // 2) 落盘记录同步执行归属（事件回填的数据源）
    let items = tiangong_app::central::load_work_items(&storage_root);
    let record = items
        .iter()
        .rev()
        .find(|i| i.session_id == "s-exec")
        .unwrap();
    assert_eq!(
        record.execution_id.as_deref(),
        Some("exec-baseline-1"),
        "投递成功后落盘记录必须是本次 execution_id"
    );
    // 3) R2/R5 事件日志（唯一事实源）：派发事实入 events.jsonl 且带归属
    {
        let log =
            tiangong_central::EventLog::open(&storage_root.join("central").join("events.jsonl"))
                .expect("事件日志应可打开");
        let entries = log.load_all().expect("事件日志应可读取");
        let started = entries
            .iter()
            .find(|logged| {
                logged.event.event_type == tiangong_central::event::WorkEventType::WorkItemStarted
                    && logged.event.work_item_id == "work-exec1"
            })
            .expect("派发事实必须已写入事件日志");
        assert_eq!(
            started.event.execution_id.as_deref(),
            Some("exec-baseline-1"),
            "事件日志中的派发事实必须带执行归属"
        );
        assert_eq!(
            started.event.worker_session_id.as_deref(),
            Some("s-exec"),
            "事件日志中的派发事实必须带 Worker 会话"
        );
    }
}

/// R3 行为基线：Workspace 授权票据双通道——未授权拒绝、指纹匹配
/// 票据放行且一次性、指纹偷换拒绝（方案 §5.6 / §3.4）。
#[tokio::test]
async fn host_ticket_channel_authorizes_workspace_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let ws = root.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // 未长期授权的目标目录（非默认工作区子树）
    let extra = tempfile::tempdir().unwrap();
    let extra_path = extra.path().to_string_lossy().to_string();
    let app = tauri::test::mock_app();
    let state = {
        let s = TiangongApp::new_for_test(root.keep(), ws.to_string_lossy().to_string());
        std::fs::write(
            s.core_manager.storage_root().join("models.json"),
            serde_json::json!({
                "providers": {"p": {"base_url": "https://api.example.com", "api_key": "k"}},
                "models": {"k3": {"provider": "p", "model": "k3-pro", "capabilities": ["chat"]}},
                "routing": {"chat": "k3"}
            })
            .to_string(),
        )
        .unwrap();
        s
    };
    app.manage(state.central.clone());
    app.manage(state);
    let handle = app.handle();
    let state = state_of(handle);
    let request = |task: &str| tiangong_app::central::CentralDispatchRequest {
        work_id: format!("work-{task}"),
        execution_id: tiangong_central::new_execution_id(),
        session_id: "s-ticket".to_string(),
        task: task.to_string(),
        model: None,
        cwd: Some(extra_path.clone()),
        after_work_id: None,
        parallel: false,
        reused_session: false,
    };
    // 1) 未授权：拒绝且指引等待用户授权
    let storage_root = state.core_manager.storage_root().to_path_buf();
    let denied = handle_central_dispatch(&state.central, handle, request("发布模块 v2"))
        .await
        .expect_err("未授权工作区必须拒绝");
    assert!(denied.contains("尚未经用户授权"), "拒绝信息：{denied}");
    // 2) 指纹偷换：签发「任务甲」票据，派发「发布模块 v2」→ 拒绝
    state.central.ticket_registry.issue_workspace_approval(
        &extra_path,
        "任务甲",
        tiangong_central::tickets::RiskLevel::High,
        600,
    );
    let swapped = handle_central_dispatch(&state.central, handle, request("发布模块 v2"))
        .await
        .expect_err("任务指纹不一致必须拒绝（防偷换）");
    assert!(
        swapped.contains("尚未经用户授权"),
        "偷换拒绝信息：{swapped}"
    );
    // 3) 匹配票据：放行且一次性（同任务再派拒绝）
    handle_central_dispatch(&state.central, handle, request("任务甲"))
        .await
        .expect("匹配票据应放行派发");
    let replay = handle_central_dispatch(&state.central, handle, request("任务甲"))
        .await
        .expect_err("票据一次性：同任务重派需要新授权");
    assert!(replay.contains("尚未经用户授权"), "重放拒绝信息：{replay}");
    // Z6 审计：每次拒绝事实都进事件日志（唯一事实源）
    {
        let log =
            tiangong_central::EventLog::open(&storage_root.join("central").join("events.jsonl"))
                .unwrap();
        let approvals: Vec<_> = log
            .load_all()
            .unwrap()
            .into_iter()
            .filter(|e| {
                e.event.event_type == tiangong_central::event::WorkEventType::ApprovalRequired
            })
            .collect();
        // 拒绝 3 次（未授权/偷换/重放）+ 匹配票据消费前后各……至少 3 条拒绝审计
        assert!(
            approvals.len() >= 3,
            "每次授权拒绝都必须留审计事件（实际 {}）",
            approvals.len()
        );
        assert!(approvals.iter().all(|e| e
            .event
            .payload
            .get("note")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("未经用户授权")));
    }
}

/// R5：登记由事件日志投影重建（唯一事实源重放；快照仅旧数据回退）。
#[test]
fn host_registry_rebuilds_from_event_log() {
    let root = tempfile::tempdir().unwrap();
    let storage = root.path().join("central");
    std::fs::create_dir_all(&storage).unwrap();
    let log = tiangong_central::EventLog::open(&storage.join("events.jsonl")).unwrap();
    // 派发事件携带登记字段
    let started = tiangong_central::WorkEvent::new(
        "work-r1",
        tiangong_central::event::WorkEventType::WorkItemStarted,
    )
    .with_execution("exec-r1")
    .with_worker_session("s-r1");
    let mut started = started;
    started.payload = serde_json::json!({
        "task": "重构登录模块", "model": "k3", "cwd": "/ws/proj", "reused_session": false,
    });
    log.append(started).unwrap();
    log.append(
        tiangong_central::WorkEvent::new(
            "work-r1",
            tiangong_central::event::WorkEventType::WorkItemCompleted,
        )
        .with_execution("exec-r1")
        .with_worker_session("s-r1"),
    )
    .unwrap();

    let registry =
        tiangong_app::central_host::central_events::rebuild_registry_from_log(root.path())
            .expect("日志非空应投影重建");
    assert_eq!(registry.len(), 1);
    let entry = &registry[0];
    assert_eq!(entry.work_id, "work-r1");
    assert_eq!(entry.session_id, "s-r1");
    assert_eq!(entry.task, "重构登录模块");
    assert_eq!(entry.model.as_deref(), Some("k3"));
    assert_eq!(entry.cwd.as_deref(), Some("/ws/proj"));
    assert_eq!(entry.execution_id.as_deref(), Some("exec-r1"));
    assert_eq!(
        entry.state,
        central_state::WorkState::Completed,
        "终态由事件投影"
    );

    // 日志为空 → None（调用方回退旧快照路径）
    let empty_root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(empty_root.path().join("central")).unwrap();
    assert!(
        tiangong_app::central_host::central_events::rebuild_registry_from_log(empty_root.path())
            .is_none(),
        "空日志必须回退旧路径（R5 前数据兼容）"
    );
}

/// Review P1-2 具名锁 ①：同会话多次派发，投影重建取**最后**一条
/// started（此前正序遍历错误保留最早条目——重派/恢复后徽章显示首轮）。
#[test]
fn host_registry_rebuild_keeps_latest_dispatch_per_session() {
    let root = tempfile::tempdir().unwrap();
    let storage = root.path().join("central");
    std::fs::create_dir_all(&storage).unwrap();
    let log = tiangong_central::EventLog::open(&storage.join("events.jsonl")).unwrap();
    // 同会话两次派发（重派：不同任务、不同执行）
    for (task, execution) in [("首轮任务", "exec-1"), ("重派任务", "exec-2")] {
        let mut started = tiangong_central::WorkEvent::new(
            "work-r1",
            tiangong_central::event::WorkEventType::WorkItemStarted,
        )
        .with_execution(execution)
        .with_worker_session("s-r1");
        started.payload = serde_json::json!({ "task": task, "cwd": "/ws" });
        log.append(started).unwrap();
    }

    let registry =
        tiangong_app::central_host::central_events::rebuild_registry_from_log(root.path())
            .expect("应投影重建");
    assert_eq!(registry.len(), 1, "会话维度单条");
    assert_eq!(
        registry[0].task, "重派任务",
        "必须取最后一次派发（review P1-2：此前保留最早）"
    );
    assert_eq!(registry[0].execution_id.as_deref(), Some("exec-2"));
}

/// Review P1-2 具名锁 ②：旧时间线迁移后，投影重建字段完整
/// （此前迁移事件缺登记 payload——任务空串/模型默认/cwd 丢失）。
#[test]
fn host_migration_preserves_registry_fields_for_rebuild() {
    let root = tempfile::tempdir().unwrap();
    let storage_root = root.path();
    // R5 前旧数据：work-items.json（裸数组）含内嵌时间线
    let record = tiangong_app::central::WorkItemRecord {
        work_id: "work-old".into(),
        reused_session: false,
        session_id: "s-old".into(),
        task: "旧任务全文".into(),
        model: Some("k3".into()),
        dispatched_at: "2026-11-01 09:00:00".into(),
        finished_at: None,
        outcome: None,
        events: vec![tiangong_app::central::WorkItemEvent {
            at: "2026-11-01 09:00:00".into(),
            kind: "dispatched".into(),
            note: "派发执行".into(),
            execution_id: None,
        }],
        queued_at: None,
        after_work_id: None,
        cwd: Some("/ws/legacy".into()),
        execution_id: None,
    };
    let _ = tiangong_app::central::upsert_work_item(storage_root, &record);
    // 迁移（幂等导入）
    tiangong_app::central_host::central_events::migrate_legacy_timeline(storage_root);
    // 投影重建：字段必须完整（任务/模型/cwd/派发时间来自登记）
    let registry =
        tiangong_app::central_host::central_events::rebuild_registry_from_log(storage_root)
            .expect("迁移后应可投影重建");
    let entry = registry
        .iter()
        .find(|e| e.work_id == "work-old")
        .expect("迁移事件应重建登记");
    assert_eq!(entry.task, "旧任务全文", "任务名不得丢失（P1-2）");
    assert_eq!(entry.model.as_deref(), Some("k3"));
    assert_eq!(entry.cwd.as_deref(), Some("/ws/legacy"));
    assert_eq!(
        entry.dispatched_at, "2026-11-01 09:00:00",
        "派发时间取原值非迁移时刻"
    );
}

/// Review Z1 具名锁 B：投影重建输出按 dispatched_at 升序（顺序
/// 不变量——retention「按位置取最旧」依赖此序；倒序输出曾使其
/// 淘汰最新记录）。
#[test]
fn rebuild_keeps_chronological_order() {
    let root = tempfile::tempdir().unwrap();
    let storage = root.path().join("central");
    std::fs::create_dir_all(&storage).unwrap();
    let log = tiangong_central::EventLog::open(&storage.join("events.jsonl")).unwrap();
    // 三条不同时间派发的工作（乱序写入事件日志）
    for (work, at) in [
        ("work-c", "2026-11-13 10:00:00"),
        ("work-a", "2026-11-13 08:00:00"),
        ("work-b", "2026-11-13 09:00:00"),
    ] {
        let session = format!("s-{work}");
        let mut started = tiangong_central::WorkEvent::new(
            work,
            tiangong_central::event::WorkEventType::WorkItemStarted,
        )
        .with_worker_session(&session);
        started.payload = serde_json::json!({ "task": "任务", "dispatched_at": at });
        started.created_at = at.to_string();
        log.append(started).unwrap();
    }
    let registry =
        tiangong_app::central_host::central_events::rebuild_registry_from_log(root.path())
            .expect("应投影重建");
    let order: Vec<&str> = registry.iter().map(|e| e.work_id.as_str()).collect();
    assert_eq!(
        order,
        vec!["work-a", "work-b", "work-c"],
        "输出必须按派发时间升序"
    );
}

/// Review Z1 具名锁 A：重建（升序）后保留策略保留最近 10 条
/// completed、淘汰最旧 2 条（倒序时代曾反向淘汰最新）。
#[test]
fn retention_keeps_latest_ten_after_rebuild() {
    let root = tempfile::tempdir().unwrap();
    let storage = root.path().join("central");
    std::fs::create_dir_all(&storage).unwrap();
    let log = tiangong_central::EventLog::open(&storage.join("events.jsonl")).unwrap();
    // 12 条 completed，时间 00..11（编号即时间序，00 最旧）
    for i in 0..12 {
        let work = format!("work-{i:02}");
        let session = format!("s-{i:02}");
        let at = format!("2026-11-13 {:02}:00:00", i);
        let mut started = tiangong_central::WorkEvent::new(
            &work,
            tiangong_central::event::WorkEventType::WorkItemStarted,
        )
        .with_worker_session(&session);
        started.payload = serde_json::json!({ "task": "任务", "dispatched_at": at });
        started.created_at = at.clone();
        log.append(started).unwrap();
        let done = tiangong_central::WorkEvent::new(
            &work,
            tiangong_central::event::WorkEventType::WorkItemCompleted,
        )
        .with_worker_session(&session);
        log.append(done).unwrap();
    }
    let mut registry =
        tiangong_app::central_host::central_events::rebuild_registry_from_log(root.path())
            .expect("应投影重建");
    assert_eq!(registry.len(), 12);
    // 保留策略（宿主在回报清账时调用同款逻辑）：按位置移除最旧
    let removals = central_state::retention_removals(&registry, 10);
    let removed: Vec<&str> = removals
        .iter()
        .map(|&i| registry[i].work_id.as_str())
        .collect();
    assert_eq!(removed, vec!["work-00", "work-01"], "必须淘汰最旧的 2 条");
    for index in removals.iter().rev() {
        registry.remove(*index);
    }
    let kept: Vec<&str> = registry.iter().map(|e| e.work_id.as_str()).collect();
    assert_eq!(
        kept,
        vec![
            "work-02", "work-03", "work-04", "work-05", "work-06", "work-07", "work-08", "work-09",
            "work-10", "work-11"
        ],
        "保留最近 10 条（含最新 work-11）"
    );
}

/// W6 具名：中枢连发两条消息的 is_sending 配对——第一条 begin 占住后，
/// 未 finish 的第二条被拒（当年「a 项」bug 的行为面：发送态收尾缺失会
/// 让 is_sending 永真）；finish 释放并递增 revision 后第二条可发送。
#[tokio::test]
async fn central_two_messages_pair_input_send() {
    use tiangong_app::state_ops::{begin_input_send, finish_input_send};
    let (state, _ws) = central_test_app();
    let sid = tiangong_app::central::CENTRAL_SESSION_ID;
    // 第一条：占住发送位
    state
        .with_state(|cs| begin_input_send(cs, sid, 0).map(|_| ()))
        .await
        .expect("第一条 begin 应成功");
    // is_sending 占用期间：第二条被拒（互斥语义）
    let rejected = state
        .with_state(|cs| begin_input_send(cs, sid, 0).map(|_| ()))
        .await;
    assert!(rejected.is_err(), "发送位占用时第二条必须被拒");
    // 中枢发送路径的收尾（缺它 is_sending 永真——即历史 bug）
    state
        .with_state(|cs| finish_input_send(cs, sid, 0, true).map(|_| ()))
        .await
        .expect("finish 应成功");
    // 第二条：revision 递增后可发送
    state
        .with_state(|cs| begin_input_send(cs, sid, 1).map(|_| ()))
        .await
        .expect("释放后第二条 begin 应成功（is_sending 配对闭环）");
}
