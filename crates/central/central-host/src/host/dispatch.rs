//! 中枢派发执行器（R1 从 app.rs 拆出）：插件消息消费循环（CentralCommand
//! 通道）、派发计划（排队/复用/引导并入判定）、投递、终态回调挂载
//! （ExecutionRouter 按 execution_id 路由）、终态归属处理与回报构造——
//! 「中枢不直接执行工作」的宿主侧闭环。

use central_state::failure_looks_model_related;

use crate::host::events::{emit_central_work_updated, emit_central_work_updated_state};
use crate::state_of_central;

/// 中枢派发执行器通道（惰性启动消费循环）。
pub fn central_dispatch_sender<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> tokio::sync::mpsc::UnboundedSender<crate::CentralPluginMessage> {
    let state = crate::state_of_central(app);
    state.central_dispatch_tx
        .get_or_init(|| {
            let (tx, mut rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::CentralPluginMessage>();
            let app = app.clone();
            tokio::spawn(async move {
                while let Some(message) = rx.recv().await {
                    match message {
                        crate::CentralPluginMessage::Dispatch(request) => {
                            if let Err(error) =
                                handle_central_dispatch(&state_of_central(&app), &app, request.clone()).await
                            {
                                tracing::warn!(%error, work_id = %request.work_id, "中枢派发未执行");
                                // 派发未执行 ≠ 工作失败：不产生终态事件、不进
                                // 登记 registry（用户实测：授权拒绝被伪装成
                                // [后台工作失败] 会误导模型去「处理失败」——
                                // 换目录规避、连续重试——而不是等待授权）。
                                // 立即注入中枢对话（本轮派发的直接后续），
                                // 给出与错误类别匹配的行动指引。
                                let is_authorization = error.contains("未经用户授权");
                                let note = if is_authorization {
                                    format!(
                                        "[系统中枢] 派发未执行：{error}\n\
                                         授权卡片已发给用户（不必再调 request_workspace_approval，重复发起只会多出一张卡片）。\n\
                                         请向用户说明要在该目录做什么、为什么需要授权；等用户确认后重新派发同一任务。\n\
                                         不要更换目录规避授权，也不要在授权前重复派发。这不是工作失败，没有产生后台工作。"
                                    )
                                } else {
                                    format!(
                                        "[系统中枢] 派发未执行：{error}\n\
                                         没有产生后台工作（不是工作失败）。请修正问题后重派，或向用户说明无法派发的原因。"
                                    )
                                };
                                if let Err(deliver_error) =
                                    state_of_central(&app).deliver_central_message(
                                        scru128::new().to_string(),
                                        vec![tiangong_types::ContentBlock::text(note)],
                                        Some(crate::host::events::system_note_render(
                                            if is_authorization { "approval" } else { "dispatch-note" },
                                        )),
                                    )
                                {
                                    tracing::warn!(%deliver_error, "派发未执行说明注入中枢失败");
                                }
                            }
                        }
                        crate::CentralPluginMessage::Steer(request) => {
                            // 引导消息与派发同路径（登记/回调/回报时机一致），
                            // 仅语义不同：不新建会话、目标必须已存在（插件已校验）。
                            if let Err(error) =
                                handle_central_dispatch(&state_of_central(&app), &app, request.clone()).await
                            {
                                tracing::warn!(%error, work_id = %request.work_id, "中枢引导执行失败");
                                let notice = format!(
                                    "[后台工作失败] {}（引导消息未送达）\n目标会话：{}\n内容：{}\n失败原因：{error}",
                                    request.work_id,
                                    request.session_id,
                                    request.task
                                );
                                state_of_central(&app)
                                    .on_central_work_finished(&request, false, &notice);
                            }
                        }
                        crate::CentralPluginMessage::ReportsCollected => {
                            // collect_work_reports 已取走全部暂存回报：
                            // 清理状态登记并刷新前端。
                            let state = state_of_central(&app);
                            state.clear_queued_work_reports();
                            emit_central_work_updated(&app);
                        }
                        crate::CentralPluginMessage::Control(control) => {
                            handle_central_control(&state_of_central(&app), &app, control).await;
                        }
                        crate::CentralPluginMessage::Notification {
                            title,
                            message,
                            level,
                        } => {
                            use tauri::Emitter;
                            let _ = app.emit(
                                "central-notification",
                                serde_json::json!({
                                    "title": title,
                                    "message": message,
                                    "level": level,
                                }),
                            );
                        }
                        crate::CentralPluginMessage::RequestWorkspaceApproval {
                            path,
                            task_fingerprint,
                        } => {
                            // 模型只能「请求」授权（R3 票据化）：触发前端
                            // 授权卡片；票据由用户操作签发（approve 命令）。
                            crate::host::events::emit_approval_required(
                                &state_of_central(&app),
                                crate::host::events::ApprovalRequiredEvent {
                                    cwd: path.clone(),
                                    task: task_fingerprint.clone(),
                                    work_id: String::new(),
                                },
                            );
                            tracing::info!(
                                cwd = %path,
                                "Workspace 授权卡片已发起（模型请求）"
                            );
                        }
                    }
                }
            });
            tx
        })
        .clone()
}

/// 会话自身的工作目录（复用派发时沿用；会话不存在返回 None）。
fn self_session_cwd(state: &crate::CentralHostState, session_id: &str) -> Option<String> {
    state
        .core_manager
        .load_session(session_id)
        .ok()
        .map(|session| session.cwd)
        .filter(|cwd| !cwd.trim().is_empty())
}

/// 执行中枢派发：创建 Worker 会话（普通能力、全插件、指定模型）投递任务，
/// 并挂终态 watcher 把结果注入中枢对话。
/// Worker 派发指令：身份 + 决策协议（对话式：后台无 UI，关键分叉以标记
/// 上抛，中枢在对话文本中直接问用户——不依赖 request_user 弹层，用户
/// 裁决暂缓征询功能）+ 任务。
pub fn build_worker_instruction(work_id: &str, task: &str) -> String {
    format!(
        "[中枢派发的后台工作 {}]\n你是被天工中枢派发的后台工作会话。请独立完成以下任务并给出结论；完成后你的最终答复会被中枢转述给用户。\n\n【用户决策协议】你是后台会话，用户看不到你的界面——不要调用 request_user 等交互工具（无人应答，只会超时失败）。请自主决策完成任务；确因必须由用户决策（审批/确认/选择/补充关键信息）才能继续时：1) 以「【需要用户决策】」开头单独一行，完整写出问题（含背景与选项，一次问全）；2) 写完问题立即结束本轮回复；3) 中枢会在对话中直接询问用户，答复会以下一条消息回到本会话，届时继续任务。除关键分叉外应自主决策，最小化打断用户；无法完成时如实说明，不要虚构结果。\n\n任务：\n{}",
        work_id, task
    )
}

/// 中枢工作控制执行：cancelled/paused 中断运行时，resume 发送继续指令。
pub async fn handle_central_control<R: tauri::Runtime>(
    state: &crate::CentralHostState,
    app: &tauri::AppHandle<R>,
    control: crate::CentralControlRequest,
) {
    let was_running = state
        .central_running_works
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&control.work_id);
    match control.action.as_str() {
        "cancelled" | "paused" => {
            // 中断当前执行（在跑才发命令；空闲会话只改状态）
            if was_running {
                tiangong_core::shared_runtime::send_command(
                    &control.session_id,
                    tiangong_core::core::command::Command::Cancel,
                );
            }
            {
                let mut registry = state
                    .central_work_registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = registry
                    .iter_mut()
                    .rev()
                    .find(|e| e.session_id == control.session_id)
                {
                    entry.state = match control.action.as_str() {
                        "cancelled" => central_state::WorkState::Cancelled,
                        "paused" => central_state::WorkState::Paused,
                        _ => central_state::WorkState::Running,
                    };
                    if control.action == "cancelled" {
                        entry.pending_question = None;
                    }
                }
            }
            if control.action == "cancelled" {
                // 取消是终态：落盘 outcome（重启恢复后显示已取消）
                if let Err(error) = crate::mark_work_item_finished_with_session(
                    state.core_manager.storage_root(),
                    &control.work_id,
                    Some(&control.session_id),
                    false,
                ) {
                    tracing::warn!(%error, "中枢登记写失败（Z4：写失败显式可见）");
                }
                // outcome 目前只有 completed/failed——取消以显式改写 outcome 表达
                if let Err(error) = crate::set_work_item_outcome(
                    state.core_manager.storage_root(),
                    &control.session_id,
                    "cancelled",
                ) {
                    tracing::warn!(%error, "中枢登记写失败（Z4：写失败显式可见）");
                }
            }
            crate::host::central_events::record_by_session(
                state.core_manager.storage_root(),
                &control.session_id,
                if control.action == "cancelled" {
                    tiangong_central::event::WorkEventType::WorkItemCancelled
                } else {
                    // 事件清单无 paused 专用类型（§8.2）：以 blocked 承载，
                    // 原文动作保留在 note
                    tiangong_central::event::WorkEventType::WorkItemBlocked
                },
                if control.action == "cancelled" {
                    "用户取消：中断执行，工作终止"
                } else {
                    "用户暂停：中断执行，上下文保留"
                },
            );
            tracing::info!(
                work_id = %control.work_id,
                action = %control.action,
                "中枢工作控制已执行"
            );
        }
        "resume" => {
            // 恢复：向工作会话发送继续指令（复用 Steer 链路，挂回调走回报）
            crate::host::central_events::record_by_session(
                state.core_manager.storage_root(),
                &control.session_id,
                tiangong_central::event::WorkEventType::WorkItemStarted,
                "用户恢复：发送继续指令，回到执行中",
            );
            let request = crate::CentralDispatchRequest {
                work_id: control.work_id.clone(),
                // R0 契约：恢复是一次新执行（§5.4），生成新 execution_id
                execution_id: tiangong_central::new_execution_id(),
                session_id: control.session_id.clone(),
                task: "继续之前暂停的任务：请从中断处继续执行并完成既定目标，已有进展无需重做。"
                    .to_string(),
                model: None,
                cwd: None,
                after_work_id: None,
                parallel: false,
                reused_session: true,
            };
            tracing::info!(
                execution_id = %request.execution_id,
                work_id = %request.work_id,
                session_id = %request.session_id,
                "恢复暂停工作：发送继续指令"
            );
            if let Err(error) = handle_central_dispatch(state, app, request.clone()).await {
                tracing::warn!(%error, work_id = %control.work_id, "恢复指令发送失败");
            }
        }
        _ => {}
    }
    emit_central_work_updated_state(state);
}

/// 排队登记（依赖/写入锁共用）：挂起 pending + registry queued + 事件。
pub async fn enqueue_central_dispatch(
    state: &crate::CentralHostState,
    mut request: crate::CentralDispatchRequest,
    after: &str,
    cwd: &str,
) {
    // 阻塞源统一写入 after_work_id（批五测试发现：写入锁排队的请求
    // 原样入队时 after=None——依赖终态后 flush 的匹配条件
    // `after_work_id == Some(after)` 永不成立，排队项只能等重启重放，
    // 运行期永久挂起。flush 消费侧投递前已置 None，不影响执行语义）。
    request.after_work_id = Some(after.to_string());
    state
        .central_pending_dispatches
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(request.clone());
    {
        let mut registry = state
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // W4（第十四轮）：同会话已有在途（在跑/等待/暂停/已排队）条目时
        // 不覆盖——徽章保持显示当前工作，排队事实由 pending 队列与
        // work-items 落盘承载（此前 retain+push 会把在跑工作改成 Queued）。
        let session_idle = !registry.iter().any(|entry| {
            (entry.session_id == request.session_id && entry.state.is_inflight())
                || (entry.session_id == request.session_id
                    && entry.state == central_state::WorkState::Queued)
        });
        if session_idle {
            registry.push(crate::CentralWorkStatus {
                work_id: request.work_id.clone(),
                session_id: request.session_id.clone(),
                task: format!("[排队] 依赖 {after} 终态后投递：{}", request.task),
                model: request
                    .model
                    .clone()
                    .or_else(|| Some("路由默认".to_string())),
                dispatched_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                state: central_state::WorkState::Queued,
                reused_session: request.reused_session,
                last_activity: None,
                cwd: None,
                execution_id: Some(request.execution_id.clone()),
                pending_question: None,
            });
        }
    }
    // 落盘排队标记（重启恢复重放——否则挂起队列丢失后误显示中断）
    if let Err(error) = crate::mark_work_item_queued(
        state.core_manager.storage_root(),
        &request.session_id,
        after,
        Some(cwd),
    ) {
        tracing::warn!(%error, "中枢登记写失败（Z4：写失败显式可见）");
    }
    crate::host::central_events::record_by_session(
        state.core_manager.storage_root(),
        &request.session_id,
        tiangong_central::event::WorkEventType::WorkItemProgress,
        &format!("排队等待依赖 {after} 终态"),
    );
    emit_central_work_updated_state(state);
}

/// 派发决策结果：Queued = 已入队（执行段不得投递）；Execute = 投递。
/// Execute 的请求体较大（任务全文），Box 收窄变体尺寸差（clippy
/// large_enum_variant）。
#[derive(Debug)]
pub enum DispatchPlan {
    Queued,
    Execute {
        merged_work: Box<crate::CentralDispatchRequest>,
        steer_merged_into_running: bool,
        /// G1（R28-1）：恢复场景的组合指令（原目标上下文 + 本次答复）；
        /// None = 常规派发（build_worker_instruction 生成）。
        instruction_override: Option<String>,
        /// G1：登记/事件投影显示的任务文本（保持 owner 原目标）；
        /// None = 常规派发（request.task）。
        registry_task: Option<String>,
        cwd: String,
        /// 本次执行消费掉的一次性授权票据 (cwd, task)：投递未成立时据此退还
        /// （目录长期授权放行时为 None）。
        approval_consumed: Option<(String, String)>,
    },
}

/// 授权查验（不消费）：目录长期授权，或存在匹配 (cwd, 任务指纹) 的未用、
/// 未过期一次性票据。
fn workspace_authorized(
    state: &crate::CentralHostState,
    default_workspace: &str,
    cwd: &str,
    task: &str,
) -> bool {
    crate::workspace_confirmed(state.core_manager.storage_root(), default_workspace, cwd)
        || state.ticket_registry.peek_workspace_approval_for(cwd, task)
}

/// 未授权拒绝：广播授权卡片（任务指纹 = 本次 task，用户点击即签发票据）、
/// 审计入事件日志，返回给模型的指引。卡片由系统自动发出——模型不必再调
/// request_workspace_approval（重复发起只会多出一张卡片让用户再点一次）。
fn reject_unauthorized(
    state: &crate::CentralHostState,
    request: &crate::CentralDispatchRequest,
    cwd: &str,
) -> String {
    // R5 授权卡片：拒绝事实广播前端（用户真实操作签发票据后模型重派即
    // 过；模型不能自证同意）
    crate::host::events::emit_approval_required(
        state,
        crate::host::events::ApprovalRequiredEvent {
            cwd: cwd.to_string(),
            task: request.task.clone(),
            work_id: request.work_id.clone(),
        },
    );
    // Z6 审计：拒绝事实入事件日志（唯一事实源——此前授权链路不留持久轨迹）
    crate::host::central_events::record_central_event(
        state.core_manager.storage_root(),
        &request.work_id,
        "",
        &request.session_id,
        tiangong_central::event::WorkEventType::ApprovalRequired,
        "派发被拒：目录未经用户授权（任务指纹已送授权卡片）",
    );
    // 十九轮 8.2：outcome 结构化（denied/granted）而非 note 文本区分
    if let Some(handle) = state.app_handle_read() {
        use tauri::Emitter;
        let _ = handle.emit(
            "central-approval-audit",
            serde_json::json!({ "outcome": "denied", "cwd": cwd, "work_id": request.work_id }),
        );
    }
    format!(
        "工作目录 {cwd} 尚未经用户授权（派发未执行）。授权卡片已自动发给用户，任务指纹就是本次 task——不必再调 request_workspace_approval（重复发起只会让用户多点一次）。请向用户说明要在该目录做什么、为什么需要授权；用户确认后重派同一任务（task 文本保持不变，否则票据不匹配）。授权只能由用户完成，不要更换目录规避，也不要在授权前重复派发。"
    )
}

/// 投递未成立时退还刚消费的一次性授权票据：没有任何执行发生，用户的
/// 同意不应因投递失败作废（重签同 (cwd, task) 票据，有效期重新起算）。
pub fn refund_workspace_approval(
    state: &crate::CentralHostState,
    consumed: &Option<(String, String)>,
) {
    let Some((cwd, task)) = consumed else {
        return;
    };
    let ticket = state.ticket_registry.issue_workspace_approval(
        cwd,
        task,
        tiangong_central::tickets::RiskLevel::High,
        crate::host::authorization::DEFAULT_TICKET_TTL_SECONDS,
    );
    tracing::info!(
        approval_id = %ticket.approval_id,
        cwd,
        "投递未成立，一次性授权票据已退还"
    );
}

pub async fn plan_central_dispatch(
    state: &crate::CentralHostState,
    request: crate::CentralDispatchRequest,
) -> Result<DispatchPlan, String> {
    // app 入口目标守卫（机检 1：dispatch_rejects_central_hub_at_app_entry 的
    // 接线）——插件侧已拒，此处兜底拦截绕过插件直入执行器的路径。
    central_state::validate_dispatch_target(&request.session_id)?;
    let default_cwd = state
        .with_state_read(|core_state| Ok(core_state.workspace_dir.clone()))
        .await
        .unwrap_or_default();
    // 工作目录：请求指定 > 复用会话自身的 cwd > 全局默认
    let cwd = request
        .cwd
        .clone()
        .filter(|path| !path.trim().is_empty())
        .or_else(|| {
            // 复用已有会话：沿用该会话的工作目录（上下文连续的一部分）
            self_session_cwd(state, &request.session_id)
        })
        .unwrap_or(default_cwd);
    if !std::path::Path::new(&cwd).is_dir() {
        return Err(format!("工作目录不存在或不是目录：{cwd}"));
    }
    // Workspace 授权统一兜底（review P1-x2：Steer 绕过；R3 票据化双通道）
    // ——Dispatch/Steer 殊途同归至此，最终 cwd（请求指定或复用会话自身）
    // 统一校验，也是**唯一**的授权执行点（插件侧不做预检：一次性票据在
    // 宿主内存，插件只看得到落盘的长期授权，预检会让用户点「授权」后的
    // 重派在插件层被短路——2026-11-25 修复）。放行条件（任一）：
    // 1. 目录长期授权（confirmed-workspaces，用户此前「记住」的目录）；
    // 2. 一次性票据（宿主用户操作签发，绑定任务指纹——防「授权 A、
    //    执行 B」偷换，消费即失效）。
    // 这里只**查验**不消费：排队项要到依赖终态后投递时才真正执行，入队
    // 时就消费会让投递那一刻无票可用；消费点在下方执行段。
    let default_workspace = state
        .with_state_read(|core_state| Ok(core_state.workspace_dir.clone()))
        .await
        .unwrap_or_default();
    if !workspace_authorized(state, &default_workspace, &cwd, &request.task) {
        return Err(reject_unauthorized(state, &request, &cwd));
    }
    // 排队决策统一走 central-state（依赖 in-flight × 写入锁 × parallel
    // 豁免 × 同会话重派——行为锁定于 crate 测试，宿主不自持规则）。
    {
        let decision = {
            let registry = state
                .central_work_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            central_state::queue_decision(
                &registry,
                Some(&request.work_id),
                request.reused_session,
                request.after_work_id.as_deref(),
                &request.session_id,
                &cwd,
                request.parallel,
                &request.task,
            )
        };
        if let central_state::QueueDecision::QueueAfter(after) = decision {
            tracing::info!(
                work_id = %request.work_id,
                after = %after,
                "后台工作排队（依赖/写入锁），等待终态后自动投递"
            );
            enqueue_central_dispatch(state, request.clone(), &after, &cwd).await;
            return Ok(DispatchPlan::Queued);
        }
    }

    // 执行段：真正消费一次性票据（目录长期授权无需消费）。查验与消费之间
    // 票据若被并发消费或过期，按未授权处理（宁可让用户再点一次）。
    let approval_consumed = {
        let directory_approved =
            crate::workspace_confirmed(state.core_manager.storage_root(), &default_workspace, &cwd);
        if directory_approved {
            None
        } else if state
            .ticket_registry
            .consume_workspace_approval_for(&cwd, &request.task)
            .is_granted()
        {
            Some((cwd.clone(), request.task.clone()))
        } else {
            return Err(reject_unauthorized(state, &request, &cwd));
        }
    };

    let sid = request.session_id.clone();

    // G0 意图判定（第 28 轮 review，R28-2）：按 registry **转移前状态**
    // 显式分类——不再在 merge 置 Running 后用「work_id 是否相等」反向
    // 猜测（同 owner id 的答复转发曾被误判 steer，跳过 register/attach，
    // 新 execution 终态回调无挂载，探针 PR-08 callback=0）。
    let intent = {
        let registry = state
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let latest = registry
            .iter()
            .rev()
            .find(|entry| entry.session_id == sid)
            .map(|entry| (entry.state, entry.work_id.as_str()));
        central_state::classify_dispatch_intent(latest, &request.work_id, request.reused_session)
    };

    // 用户答复/引导送达（架构裁决 2026-11-17：Worker 的一切消息均由中枢
    // 下发，用户不直接与 Worker 交互）：目标会话存在 waiting_user（等
    // 用户决策）条目时，本轮终态归属该主 work——恢复 running 并复用其
    // work_id 挂回调与登记（不另立条目），回复链完整闭合到原工作。
    let mut merged_work = request.clone();
    // G1（R28-1）：恢复场景的组合指令（原目标上下文 + 本次答复）——
    // 不再用 owner_task 覆盖 request.task，Worker 真正收到用户/中枢决定。
    let mut instruction_override: Option<String> = None;
    // G1：登记/事件展示用任务文本（保持 owner 原目标，不被答复文本覆盖）。
    let mut registry_task: Option<String> = None;
    if request.reused_session && intent == central_state::DispatchIntent::ResumeWaiting {
        let mut registry = state
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 恢复归属统一走 central-state（waiting_user → running + 主工作回传）
        if let Some((owner_work_id, owner_task)) =
            central_state::merge_waiting_user(&mut registry, &sid, Some(&request.execution_id))
        {
            // Z6 决定票据消费点：该恢复执行若存在用户签发的未消费
            // DecisionTicket（决定卡片 → resolve_decision），在此一次性
            // 消费（fail-closed 渐进收紧：有票据则必须消费；自然语言答复
            // 无票据时暂放行——待决定协议结构化后转为硬校验，附录 A.2）
            if state
                .ticket_registry
                .consume_decision_for_execution(&request.execution_id)
                .is_granted()
            {
                tracing::info!(
                    execution_id = %request.execution_id,
                    "恢复经 DecisionTicket 授权（一次性消费）"
                );
            }
            merged_work.work_id = owner_work_id.clone();
            // G1（R28-1 核心）：登记与事件投影保留 owner 原目标；投递
            // 内容组合原目标上下文与本次答复（request.task 不被覆盖——
            // 此前 owner_task 直接顶替导致 Worker 收到旧任务重跑，用户
            // 决定丢失）。
            registry_task = Some(owner_task.clone());
            instruction_override = Some(format!(
                "[中枢继续既有工作 {owner_work_id}]\n该工作此前等待用户决策，现已收到答复，请继续执行并给出结论；完成后你的最终答复会被中枢转述给用户。\n\n【原目标】\n{owner_task}\n\n【本次用户/中枢答复】\n{}\n\n（原目标仅供上下文，以本次答复为准继续；后续仍遵循既定的用户决策协议：确需用户决策时以「【需要用户决策】」开头单独一行提出。）",
                request.task
            ));
            crate::host::central_events::record_by_session(
                state.core_manager.storage_root(),
                &sid,
                tiangong_central::event::WorkEventType::WorkItemStarted,
                "收到用户答复，恢复执行",
            );
            tracing::info!(
                execution_id = %request.execution_id,
                work_id = %merged_work.work_id,
                session_id = %sid,
                "等待用户决策的工作已收到答复，恢复执行（下发内容含真实答复）"
            );
        }
    }

    // G0：steer 并入判定收敛为意图枚举——Running × reused × id 一致
    // （引导并入当前 turn，归属保持原工作、不挂新回调；turn 终态由原
    // 工作既有回调处理）。Paused 恢复/新执行均走 register/attach。
    let steer_merged_into_running = intent == central_state::DispatchIntent::SteerRunning;
    if steer_merged_into_running {
        let registry = state
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = registry.iter().rev().find(|entry| {
            entry.session_id == sid && entry.state == central_state::WorkState::Running
        }) {
            tracing::info!(
                steer_work = %request.work_id,
                owner_work = %entry.work_id,
                "引导并入执行中的工作：归属保持原工作"
            );
        }
    }

    Ok(DispatchPlan::Execute {
        merged_work: Box::new(merged_work),
        steer_merged_into_running,
        instruction_override,
        registry_task,
        cwd,
        approval_consumed,
    })
}

pub async fn handle_central_dispatch<R: tauri::Runtime>(
    state: &crate::CentralHostState,
    app: &tauri::AppHandle<R>,
    request: crate::CentralDispatchRequest,
) -> Result<(), String> {
    let (
        merged_work,
        steer_merged_into_running,
        instruction_override,
        registry_task,
        cwd,
        approval_consumed,
    ) = match plan_central_dispatch(state, request.clone()).await? {
        // 排队已在决策段完成登记落盘——执行段到此为止（分离手术曾
        // 丢失此早退语义：返回值被当作投递继续，排队整体失效且无
        // 测试暴露——P0，第十四轮深评自检发现）
        DispatchPlan::Queued => return Ok(()),
        DispatchPlan::Execute {
            merged_work,
            steer_merged_into_running,
            instruction_override,
            registry_task,
            cwd,
            approval_consumed,
        } => (
            merged_work,
            steer_merged_into_running,
            instruction_override,
            registry_task,
            cwd,
            approval_consumed,
        ),
    };
    let sid = merged_work.session_id.clone();
    // model 是统一的换模型通道（switch_worker_model 移除后的用户裁决）：
    // 复用会话同样生效——live Core 经 deliver 的 model_ref 走模型编排
    //（空闲：持久化选择 + 切换端点，本轮生效；执行中：引导并入当前
    // turn 沿用当前模型）；无 Core（重启后复用）经 ensure_core 以目标
    // 模型初始化 + 下方 set_core_model_ref 持久化，回收重建不回退。
    let effective_model = merged_work.model.as_deref();
    // G1（R28-1）：恢复场景使用 plan 组合的指令（原目标 + 真实答复）；
    // 常规派发生成标准工作指令。
    let resumed_with_answer = instruction_override.is_some();
    let instruction = instruction_override
        .unwrap_or_else(|| build_worker_instruction(&merged_work.work_id, &merged_work.task));
    let prepared = vec![tiangong_types::ContentBlock::text(instruction)];
    // 回调槽（每会话一个，可多挂载）：派发与引导共用该会话既有的流消费者，
    // 逐 work 挂各自的终态回调——不再 retire+重建（消除 sidecar 重启与
    // prompt cache 失效；引导语义也要求保留在途执行）。
    // R1 执行路由：投递前声明活跃执行——瞬间完成的 turn 在回调挂载前
    // fire 时，终态会暂存到该 execution_id，attach 后补发不丢归属。
    // 引导并入（steer_merged_into_running）不换活跃执行：turn 终态仍
    // 路由到原执行的回调（归属保持原工作）。
    if !steer_merged_into_running {
        state.execution_router.register(&sid, &request.execution_id);
    }
    if state.core_manager.has_live_core(&sid) {
        // Core 活跃（复用/引导/并发派发）：不重建，投递消息（忙→引导并入
        // 当前 turn；空闲→开新 turn，消费者已在线）。回调在投递成功后挂载
        // （投递失败经 cancel 清理，不残留）。
        if let Err(error) = (state.deliver_prepared)(
            sid.clone(),
            scru128::new().to_string(),
            prepared,
            effective_model.map(str::to_string),
        )
        .await
        {
            state.execution_router.cancel(&sid, &request.execution_id);
            refund_workspace_approval(state, &approval_consumed);
            return Err(error);
        }
        // 成功登记后统一 attach：同步重放不能先于 Started/Running。
    } else {
        // Core 不存在（新建会话/重启后复用）：ensure 创建并启动流消费者
        // （is_new 守卫：命中复用说明并发创建，流通道会被丢弃，显式报错）。
        let (stream_tx, stream_rx) = std::sync::mpsc::channel::<tiangong_types::StreamEvent>();
        let is_new = match (state.ensure_core_fn)(
            sid.clone(),
            Some(cwd.clone()),
            effective_model.map(str::to_string),
            stream_tx,
        )
        .await
        {
            Ok(is_new) => is_new,
            Err(error) => {
                state.execution_router.cancel(&sid, &request.execution_id);
                refund_workspace_approval(state, &approval_consumed);
                return Err(error);
            }
        };
        if !is_new {
            state.execution_router.cancel(&sid, &request.execution_id);
            refund_workspace_approval(state, &approval_consumed);
            return Err(format!(
                "会话 {} 的 Core 已被并发创建，本次流通道未绑定；请稍后重派",
                sid
            ));
        }
        (state.stream_consumer_starter)(sid.clone(), stream_rx);
        // ensure_core 的 initial_model_ref 只初始化新 Core 端点，不落
        // Session.model_ref——此处补写（Core 刚建必不忙），空闲回收后
        // 重建不再回退旧模型（二十一轮 review 修复的持久化缺口）。
        if let Some(model_key) = effective_model {
            if let Err(error) = state
                .core_manager
                .set_core_model_ref(&sid, Some(model_key.to_string()))
            {
                tracing::warn!(session_id = %sid, error, "复用派发的模型选择持久化失败");
            }
        }
        if let Err(error) = (state.deliver_prepared)(
            sid.clone(),
            scru128::new().to_string(),
            prepared,
            effective_model.map(str::to_string),
        )
        .await
        {
            state.execution_router.cancel(&sid, &request.execution_id);
            refund_workspace_approval(state, &approval_consumed);
            return Err(error);
        }
        // 回调统一在成功登记之后挂载（fire 已由 router 暂存）。
    }
    tracing::info!(
        execution_id = %request.execution_id,
        work_id = %request.work_id,
        session_id = %sid,
        cwd = %cwd,
        parallel = request.parallel,
        reused_session = request.reused_session,
        "中枢后台工作已派发"
    );
    // R29-4：引导并入当前 turn，不是新执行。只记录当前 execution 的
    // Progress；若写新 Started，严格投影将 owner 终态误当迟到事实丢弃。
    if steer_merged_into_running {
        if let Some(active_execution) = state.execution_router.active_execution(&sid) {
            if let Err(error) = crate::set_work_item_execution(
                state.core_manager.storage_root(),
                &sid,
                &active_execution,
            ) {
                tracing::warn!(%error, "引导并入的执行归属落盘失败");
            }
            crate::host::central_events::record_for(
                state,
                &merged_work.work_id,
                &active_execution,
                &sid,
                tiangong_central::event::WorkEventType::WorkItemProgress,
                "补充指令并入当前执行（归属保持 owner）",
            );
        }
        emit_central_work_updated_state(state);
        return Ok(());
    }
    // R5 事件日志（唯一事实源）：派发事实入日志，payload 携带登记字段
    // （registry 投影重建的数据源——重启后从事件重放，不再依赖快照）。
    {
        let mut dispatch_event = tiangong_central::WorkEvent::new(
            &request.work_id,
            tiangong_central::event::WorkEventType::WorkItemStarted,
        )
        .with_execution(&request.execution_id)
        .with_worker_session(&sid);
        dispatch_event.payload = serde_json::json!({
            "note": "派发投递成功",
            // G1：恢复场景投影显示 owner 原目标（registry_task），并另记
            // resume_answer 供追溯本次下发的真实答复（R28-1：原目标与
            // follow_up 都可查询，互不覆盖）。
            "task": registry_task.as_deref().unwrap_or(&request.task),
            "model": request.model,
            "cwd": cwd,
            "reused_session": request.reused_session,
            "parallel": request.parallel,
        });
        if resumed_with_answer {
            dispatch_event.payload["resume_answer"] = serde_json::json!(request.task);
        }
        crate::host::central_events::record_event(
            state.core_manager.storage_root(),
            dispatch_event,
        );
    }
    // R0 契约：投递成功后同步执行归属到落盘登记——插件侧派发已带同值
    // （幂等），恢复 / 排队重放经此把新 execution_id 写回，事件时间线与
    // 终态日志才能归属到本次执行。
    if let Err(error) = crate::set_work_item_execution(
        state.core_manager.storage_root(),
        &sid,
        &request.execution_id,
    ) {
        tracing::warn!(%error, "中枢登记写失败（Z4：写失败显式可见）");
    }
    // 给 Worker 会话写可识别标题（仅新建会话；复用/引导保留原标题）。
    if !request.reused_session {
        let title_preview: String = request.task.chars().take(24).collect();
        let title = format!("[后台工作] {title_preview}");
        if let Some(core) = state.core_manager.registry().get(&sid) {
            let _ = core.set_title(title, false);
        }
    }
    // 状态登记按**会话维度 upsert**（用户实测：同会话重派后旧 interrupted
    // 条目残留、多轮派发显示多条）——一个工作会话在徽章中始终是一条，
    // 重派/引导覆盖更新（work_id/task/时间取最新），中断旧条目自然消失。
    // waiting_user 恢复（merged）时条目已在上方更新为 running，不重建。
    if merged_work.work_id == request.work_id && !steer_merged_into_running {
        {
            let mut registry = state
                .central_work_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // R29-1/2：只保护本次 execution 已推进的状态，旧 Interrupted
            // 不阻止重派；WaitingUser/ReportQueued 也不可被 Running 回退。
            let already_advanced = registry.iter().rev().any(|entry| {
                entry.session_id == request.session_id
                    && entry.execution_id.as_deref() == Some(&request.execution_id)
                    && !matches!(
                        entry.state,
                        central_state::WorkState::Running | central_state::WorkState::Queued
                    )
            });
            if !already_advanced {
                // G1：恢复场景登记显示 owner 原目标（不被答复文本覆盖）
                let display_task = registry_task.as_deref().unwrap_or(&request.task);
                central_state::upsert_registry_entry(
                    &mut registry,
                    central_state::registry_entry_after_dispatch(
                        &request.work_id,
                        &request.session_id,
                        display_task,
                        request.model.as_deref(),
                        request.reused_session,
                        Some(cwd.as_str()),
                        &chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                        &request.execution_id,
                    ),
                );
            }
        }
    }
    // 排队标记清除（第十三轮 P2-1）：进入执行即失效——重放的 DispatchNow
    // 路径不经 flush，若不在此清除，下次重启会再次重放导致重复执行。
    if let Err(error) =
        crate::clear_work_item_queued(state.core_manager.storage_root(), &request.session_id)
    {
        tracing::warn!(%error, "中枢登记写失败（Z4：写失败显式可见）");
    }
    // R5 单写：派发事实已由上方 record_for（WorkItemStarted）入事件日志；
    // 此处仅以 note 补充「恢复 / 新任务」语义（G1：恢复判定不再依赖
    // work_id 是否改写——插件沿用 owner id 时两者相等，按恢复事实标注）。
    crate::host::central_events::record_for(
        state,
        &merged_work.work_id,
        &request.execution_id,
        &sid,
        tiangong_central::event::WorkEventType::WorkItemProgress,
        &if resumed_with_answer {
            "派发执行（等待用户决策后恢复，下发含真实答复）".to_string()
        } else {
            "派发执行（新任务）".to_string()
        },
    );
    // R29-2：登记与 Started 事实先于 attach。attach 可同步重放 fire，
    // 因此 running 必须先登记；重放之后不再有 Running upsert/集合 insert。
    // 与终态路径同锁序（running → registry），保护已推进/被接管的执行。
    if !steer_merged_into_running {
        let mut running = state
            .central_running_works
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let registry = state
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if registry
            .iter()
            .rev()
            .find(|entry| entry.session_id == sid)
            .is_some_and(|entry| {
                entry.execution_id.as_deref() == Some(&request.execution_id)
                    && entry.state == central_state::WorkState::Running
            })
        {
            running.insert(merged_work.work_id.clone());
        }
        drop(registry);
        drop(running);
        attach_work_finished_callback(state, app, (*merged_work).clone());
    }
    emit_central_work_updated_state(state);
    Ok(())
}

/// 向执行路由器挂载单个执行的终态回报回调（R1：按 execution_id 路由）。
fn attach_work_finished_callback<R: tauri::Runtime>(
    state: &crate::CentralHostState,
    app: &tauri::AppHandle<R>,
    work: crate::CentralDispatchRequest,
) {
    let callback_app = app.clone();
    let work_for_handler = work.clone();
    state.execution_router.attach(
        &work.session_id,
        &work.execution_id,
        Box::new(move |done: bool, reason: String| {
            handle_work_terminal(
                &state_of_central(&callback_app),
                &work_for_handler,
                done,
                &reason,
            );
        }),
    );
}

/// 终态回调核心（host 集成测试的真实被测函数）：T6 归属过滤 + S6 状态
/// 幂等 + 回报入队 + 依赖 flush——attach 的闭包体纯提取（零行为变化），
/// 测试直接调用以锁定接线行为（旧代码会红）。
pub fn handle_work_terminal(
    state: &crate::CentralHostState,
    work: &crate::CentralDispatchRequest,
    done: bool,
    reason: &str,
) {
    // 归属过滤（R1 升级）：路由器已保证只有活跃执行的回调被触发；此处
    // 为防御迟到路由再做执行归属校验——registry 当前条目若已是**另一
    // 个执行**（同会话新派发/恢复已接管，即使新执行也已终态），旧回调
    // 必须跳过。成功派发先登记本 execution，再 attach，因此旧 Interrupted
    // 不会在此误挡新执行，旧迟到回调也不能覆盖新执行结果。
    // 按 execution_id 比对取代旧的 work_id 比对：同 work 重派是新执行，
    // 旧回调同样必须让位。
    {
        let registry = state
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let current = registry
            .iter()
            .rev()
            .find(|entry| entry.session_id == work.session_id);
        let displaced_by_newer_execution = match current {
            Some(entry) => match entry.execution_id.as_deref() {
                // 登记缺执行归属（R0 前旧数据）：回退 work_id 比对语义
                None => entry.work_id != work.work_id,
                Some(current_exec) => current_exec != work.execution_id.as_str(),
            },
            None => false,
        };
        if displaced_by_newer_execution {
            tracing::warn!(
                stale_execution = %work.execution_id,
                current_execution = ?current.and_then(|e| e.execution_id.clone()),
                "旧执行回调被跳过（终态归属当前执行）"
            );
            return;
        }
    }
    // 登记已由上一守卫确认本 execution；一次性状态保护同执行重复回调，
    // 不以旧 work_id 的终态误杀新 turn（恢复/重派都生成新 execution）。
    {
        let registry = state
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let self_entry = registry
            .iter()
            .rev()
            .find(|entry| entry.work_id == work.work_id);
        match self_entry {
            Some(entry)
                if !entry.state.is_inflight()
                    && entry
                        .execution_id
                        .as_deref()
                        .is_none_or(|exec| exec == work.execution_id) =>
            {
                tracing::warn!(
                    work_id = %work.work_id,
                    state = entry.state.as_str(),
                    "终态回调跳过：该工作已终态（重复 attach/陈旧回调）"
                );
                return;
            }
            _ => {}
        }
    }
    let notice = build_work_notice(state, work, done, reason);
    state.on_central_work_finished(work, done, &notice);
    // 依赖本工作的排队项：终态后自动投递
    state.flush_pending_dispatches(&work.work_id, &work.session_id, state.app_handle_read());
}

/// 组装后台工作回报（读 Worker 会话最终答复，**不截断**）。
///
/// 不做长度截断的依据：完成汇报必须基于真实完整结果（设计稿 §2.3 /
/// §14.2「禁止推断完成」）——截断会制造残缺信息，转述反而误导；上下文
/// 安全由系统机制兜底（Core 自动压缩、模型上下文窗口、Worker 自身输出
/// 上限）。将来超长结果的正确形态是「结果落文件 + 注入摘要与引用」
/// （阶段 2 报告机制），而非拦腰截断。
fn build_work_notice(
    state: &crate::CentralHostState,
    work: &crate::CentralDispatchRequest,
    done: bool,
    failed_reason: &str,
) -> String {
    let model_note = work.model.as_deref().unwrap_or("（默认）");
    if done {
        let summary = state
            .core_manager
            .load_session(&work.session_id)
            .ok()
            .and_then(|session| {
                session
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == tiangong_types::MessageRole::Assistant)
                    .map(|message| {
                        message
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                tiangong_types::ContentBlock::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<String>()
                    })
            })
            .unwrap_or_else(|| "（未能读取到执行结果）".to_string());
        format!(
            "[后台工作完成] {}（模型 {}）\n任务：{}\n\n执行结果：\n{}\n\n（请向用户完整转述上述结果的关键结论与依据，不要过度省略；如需细节可指引查看工作会话 {}）",
            work.work_id, model_note, work.task, summary, work.session_id
        )
    } else {
        let guidance = if failure_looks_model_related(failed_reason) {
            "\n\n（失败疑似模型/网络问题。处置：list_models 查询可用模型后，用 dispatch_work 带 session_id 与新 model 在原会话重派——上下文保留，不要新建会话；确认无可用模型时再向用户说明。）"
        } else {
            ""
        };
        format!(
            "[后台工作失败] {}（模型 {}）\n任务：{}\n失败原因：{}{guidance}",
            work.work_id, model_note, work.task, failed_reason
        )
    }
}
