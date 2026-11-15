//! 中枢派发执行器（R1 从 app.rs 拆出）：插件消息消费循环（CentralCommand
//! 通道）、派发计划（排队/复用/引导并入判定）、投递、终态回调挂载
//! （ExecutionRouter 按 execution_id 路由）、终态归属处理与回报构造——
//! 「中枢不直接执行工作」的宿主侧闭环。

use central_state::failure_looks_model_related;

use crate::app::TiangongApp;

use super::events::{emit_central_work_updated, emit_central_work_updated_state};
use super::state_of;

/// 中枢派发执行器通道（惰性启动消费循环）。
pub(crate) fn central_dispatch_sender(
    app: &tauri::AppHandle,
) -> tokio::sync::mpsc::UnboundedSender<crate::central::CentralPluginMessage> {
    use tauri::Manager;
    let state = app.state::<TiangongApp>();
    state
        .inner()
        .central_dispatch_tx
        .get_or_init(|| {
            let (tx, mut rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::central::CentralPluginMessage>();
            let app = app.clone();
            tokio::spawn(async move {
                while let Some(message) = rx.recv().await {
                    match message {
                        crate::central::CentralPluginMessage::Dispatch(request) => {
                            if let Err(error) =
                                handle_central_dispatch(state_of(&app), &app, request.clone()).await
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
                                         授权卡片已发给用户（也可调 request_workspace_approval 主动发起）。\n\
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
                                    state_of(&app).deliver_central_message(
                                        scru128::new().to_string(),
                                        vec![tiangong_types::ContentBlock::text(note)],
                                        Some(super::events::system_note_render(
                                            if is_authorization { "approval" } else { "dispatch-note" },
                                        )),
                                    )
                                {
                                    tracing::warn!(%deliver_error, "派发未执行说明注入中枢失败");
                                }
                            }
                        }
                        crate::central::CentralPluginMessage::Steer(request) => {
                            // 引导消息与派发同路径（登记/回调/回报时机一致），
                            // 仅语义不同：不新建会话、目标必须已存在（插件已校验）。
                            if let Err(error) =
                                handle_central_dispatch(state_of(&app), &app, request.clone()).await
                            {
                                tracing::warn!(%error, work_id = %request.work_id, "中枢引导执行失败");
                                let notice = format!(
                                    "[后台工作失败] {}（引导消息未送达）\n目标会话：{}\n内容：{}\n失败原因：{error}",
                                    request.work_id,
                                    request.session_id,
                                    request.task
                                );
                                state_of(&app)
                                    .on_central_work_finished(&request, false, &notice);
                            }
                        }
                        crate::central::CentralPluginMessage::ReportsCollected => {
                            // collect_work_reports 已取走全部暂存回报：
                            // 清理状态登记并刷新前端。
                            let state = state_of(&app);
                            state.clear_queued_work_reports();
                            emit_central_work_updated(&app);
                        }
                        crate::central::CentralPluginMessage::Control(control) => {
                            handle_central_control(state_of(&app), &app, control).await;
                        }
                        crate::central::CentralPluginMessage::Notification {
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
                        crate::central::CentralPluginMessage::RequestWorkspaceApproval {
                            path,
                            task_fingerprint,
                        } => {
                            // 模型只能「请求」授权（R3 票据化）：触发前端
                            // 授权卡片；票据由用户操作签发（approve 命令）。
                            super::events::emit_approval_required(
                                state_of(&app),
                                super::events::ApprovalRequiredEvent {
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
fn self_session_cwd(state: &TiangongApp, session_id: &str) -> Option<String> {
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
pub(crate) fn build_worker_instruction(work_id: &str, task: &str) -> String {
    format!(
        "[中枢派发的后台工作 {}]\n你是被天工中枢派发的后台工作会话。请独立完成以下任务并给出结论；完成后你的最终答复会被中枢转述给用户。\n\n【用户决策协议】你是后台会话，用户看不到你的界面——不要调用 request_user 等交互工具（无人应答，只会超时失败）。请自主决策完成任务；确因必须由用户决策（审批/确认/选择/补充关键信息）才能继续时：1) 以「【需要用户决策】」开头单独一行，完整写出问题（含背景与选项，一次问全）；2) 写完问题立即结束本轮回复；3) 中枢会在对话中直接询问用户，答复会以下一条消息回到本会话，届时继续任务。除关键分叉外应自主决策，最小化打断用户；无法完成时如实说明，不要虚构结果。\n\n任务：\n{}",
        work_id, task
    )
}

/// 中枢工作控制执行：cancelled/paused 中断运行时，resume 发送继续指令。
async fn handle_central_control(
    state: &TiangongApp,
    app: &tauri::AppHandle,
    control: crate::central::CentralControlRequest,
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
                }
            }
            if control.action == "cancelled" {
                // 取消是终态：落盘 outcome（重启恢复后显示已取消）
                if let Err(error) = crate::central::mark_work_item_finished_with_session(
                    state.core_manager.storage_root(),
                    &control.work_id,
                    Some(&control.session_id),
                    false,
                ) {
                    tracing::warn!(%error, "中枢登记写失败（Z4：写失败显式可见）");
                }
                // outcome 目前只有 completed/failed——取消以显式改写 outcome 表达
                if let Err(error) = crate::central::set_work_item_outcome(
                    state.core_manager.storage_root(),
                    &control.session_id,
                    "cancelled",
                ) {
                    tracing::warn!(%error, "中枢登记写失败（Z4：写失败显式可见）");
                }
            }
            super::central_events::record_by_session(
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
            super::central_events::record_by_session(
                state.core_manager.storage_root(),
                &control.session_id,
                tiangong_central::event::WorkEventType::WorkItemStarted,
                "用户恢复：发送继续指令，回到执行中",
            );
            let request = crate::central::CentralDispatchRequest {
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
pub(crate) async fn enqueue_central_dispatch(
    state: &TiangongApp,
    request: crate::central::CentralDispatchRequest,
    after: &str,
    cwd: &str,
) {
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
            registry.push(crate::central::CentralWorkStatus {
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
            });
        }
    }
    // 落盘排队标记（重启恢复重放——否则挂起队列丢失后误显示中断）
    if let Err(error) = crate::central::mark_work_item_queued(
        state.core_manager.storage_root(),
        &request.session_id,
        after,
        Some(cwd),
    ) {
        tracing::warn!(%error, "中枢登记写失败（Z4：写失败显式可见）");
    }
    super::central_events::record_by_session(
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
pub(crate) enum DispatchPlan {
    Queued,
    Execute {
        merged_work: Box<crate::central::CentralDispatchRequest>,
        steer_merged_into_running: bool,
        cwd: String,
    },
}

pub(crate) async fn plan_central_dispatch(
    state: &TiangongApp,
    request: crate::central::CentralDispatchRequest,
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
    // 统一校验。放行条件（任一）：
    // 1. 目录长期授权（confirmed-workspaces，用户此前「记住」的目录）；
    // 2. 一次性票据（宿主用户操作签发，绑定任务指纹——防「授权 A、
    // 执行 B」偷换，消费即失效）。
    // 无票据通道前模型无法自证同意（confirm_workspace 已移除）。
    {
        let default_workspace = state
            .with_state_read(|core_state| Ok(core_state.workspace_dir.clone()))
            .await
            .unwrap_or_default();
        let directory_approved = crate::central::workspace_confirmed(
            state.core_manager.storage_root(),
            &default_workspace,
            &cwd,
        );
        let ticket_granted = !directory_approved
            && state
                .ticket_registry
                .consume_workspace_approval_for(&cwd, &request.task)
                .is_granted();
        if !directory_approved && !ticket_granted {
            // R5 授权卡片：拒绝事实广播前端（用户真实操作签发票据后
            // 模型重派即过；模型不能自证同意）
            super::events::emit_approval_required(
                state,
                super::events::ApprovalRequiredEvent {
                    cwd: cwd.clone(),
                    task: request.task.clone(),
                    work_id: request.work_id.clone(),
                },
            );
            // Z6 审计：拒绝事实入事件日志（唯一事实源——此前授权链路
            // 不留持久轨迹）
            super::central_events::record_central_event(
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
            return Err(format!(
                "工作目录 {cwd} 尚未经用户授权（派发未执行）。请调 request_workspace_approval(path=\"{cwd}\", task_fingerprint=<与本次 task 完全一致>) 向用户发起授权卡片，并向用户说明任务与影响；用户确认后重派同一任务。授权只能由用户完成，不要更换目录规避，也不要在授权前重复派发。"
            ));
        }
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

    let sid = request.session_id.clone();

    // 用户答复/引导送达：目标会话存在 waiting_user（等用户决策）条目时，
    // 本轮终态归属该主 work——恢复 running 并复用其 work_id/task 挂回调
    // 与登记（不另立条目），回复链完整闭合到原工作。
    let mut merged_work = request.clone();
    if request.reused_session {
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
            merged_work.work_id = owner_work_id;
            merged_work.task = owner_task;
            super::central_events::record_by_session(
                state.core_manager.storage_root(),
                &sid,
                tiangong_central::event::WorkEventType::WorkItemStarted,
                "收到用户答复，恢复执行",
            );
            tracing::info!(
                execution_id = %request.execution_id,
                work_id = %merged_work.work_id,
                session_id = %sid,
                "等待用户决策的工作已收到答复，恢复执行"
            );
        }
    }

    // 引导并入判定（第十三轮 P1-2）：reused 派发到「仍在跑」的会话 =
    // 对既有工作的引导补充——registry 归属保持原工作、不挂新回调
    // （turn 终态由原工作既有回调处理；否则 steer 覆盖归属后原工作
    // 终态被 T6 过滤跳过，永不终态）。空闲会话的 reused 派发 = 新工作
    // （覆盖登记 + 挂回调）。
    let mut steer_merged_into_running = false;
    if request.reused_session && merged_work.work_id == request.work_id {
        let registry = state
            .central_work_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 仅 Running 并入（turn 在跑，引导并入当前轮）——Paused 必须排除：
        // resume 走 Steer 到暂停会话，需走恢复语义（覆盖登记 + 挂回调），
        // 否则状态卡 paused 且恢复的 turn 无回报（深检 R1，第十三轮 P1-2
        // 曾误用 is_inflight 把 resume 一并吞掉）。WaitingUser 由上方
        // merged 分支处理，此处不会命中。
        let running_entry = registry.iter().rev().find(|entry| {
            entry.session_id == sid && entry.state == central_state::WorkState::Running
        });
        if let Some(entry) = running_entry {
            steer_merged_into_running = true;
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
        cwd,
    })
}

pub(crate) async fn handle_central_dispatch<R: tauri::Runtime>(
    state: &TiangongApp,
    app: &tauri::AppHandle<R>,
    request: crate::central::CentralDispatchRequest,
) -> Result<(), String> {
    let (merged_work, steer_merged_into_running, cwd) =
        match plan_central_dispatch(state, request.clone()).await? {
            // 排队已在决策段完成登记落盘——执行段到此为止（分离手术曾
            // 丢失此早退语义：返回值被当作投递继续，排队整体失效且无
            // 测试暴露——P0，第十四轮深评自检发现）
            DispatchPlan::Queued => return Ok(()),
            DispatchPlan::Execute {
                merged_work,
                steer_merged_into_running,
                cwd,
            } => (merged_work, steer_merged_into_running, cwd),
        };
    let sid = merged_work.session_id.clone();
    let effective_model = if merged_work.reused_session {
        None
    } else {
        merged_work.model.as_deref()
    };
    let instruction = build_worker_instruction(&merged_work.work_id, &merged_work.task);
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
        if let Err(error) = state
            .deliver_prepared_if_live(
                &sid,
                scru128::new().to_string(),
                prepared,
                effective_model,
                None,
            )
            .await
        {
            state.execution_router.cancel(&sid, &request.execution_id);
            return Err(error);
        }
        if !steer_merged_into_running {
            attach_work_finished_callback(state, app, (*merged_work).clone());
        }
    } else {
        // Core 不存在（新建会话/重启后复用）：ensure 创建并启动流消费者
        // （is_new 守卫：命中复用说明并发创建，流通道会被丢弃，显式报错）。
        let (stream_tx, stream_rx) = std::sync::mpsc::channel::<tiangong_types::StreamEvent>();
        let ensured = match state
            .ensure_core(
                &sid,
                Some(cwd.clone()),
                None,
                None,
                effective_model,
                stream_tx,
            )
            .await
        {
            Ok(ensured) => ensured,
            Err(error) => {
                state.execution_router.cancel(&sid, &request.execution_id);
                return Err(error);
            }
        };
        if !ensured.is_new {
            state.execution_router.cancel(&sid, &request.execution_id);
            return Err(format!(
                "会话 {} 的 Core 已被并发创建，本次流通道未绑定；请稍后重派",
                sid
            ));
        }
        crate::commands::start_stream_consumer(app.clone(), sid.clone(), stream_rx);
        if let Err(error) = state
            .deliver_prepared_if_live(
                &sid,
                scru128::new().to_string(),
                prepared,
                effective_model,
                None,
            )
            .await
        {
            state.execution_router.cancel(&sid, &request.execution_id);
            return Err(error);
        }
        attach_work_finished_callback(state, app, (*merged_work).clone());
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
            "task": request.task,
            "model": request.model,
            "cwd": cwd,
            "reused_session": request.reused_session,
            "parallel": request.parallel,
        });
        super::central_events::record_event(state.core_manager.storage_root(), dispatch_event);
    }
    // R0 契约：投递成功后同步执行归属到落盘登记——插件侧派发已带同值
    // （幂等），恢复 / 排队重放经此把新 execution_id 写回，事件时间线与
    // 终态日志才能归属到本次执行。
    if let Err(error) = crate::central::set_work_item_execution(
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
    // 登记在执行集合与状态登记（running 按 work_id），并通知前端刷新
    {
        let mut running = state
            .central_running_works
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !steer_merged_into_running {
            running.insert(merged_work.work_id.clone());
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
            central_state::upsert_registry_entry(
                &mut registry,
                central_state::registry_entry_after_dispatch(
                    &request.work_id,
                    &request.session_id,
                    &request.task,
                    request.model.as_deref(),
                    request.reused_session,
                    Some(cwd.as_str()),
                    &chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    &request.execution_id,
                ),
            );
        }
    }
    // 排队标记清除（第十三轮 P2-1）：进入执行即失效——重放的 DispatchNow
    // 路径不经 flush，若不在此清除，下次重启会再次重放导致重复执行。
    if let Err(error) = crate::central::clear_work_item_queued(
        state.core_manager.storage_root(),
        &request.session_id,
    ) {
        tracing::warn!(%error, "中枢登记写失败（Z4：写失败显式可见）");
    }
    // R5 单写：派发事实已由上方 record_for（WorkItemStarted）入事件日志；
    // 此处仅以 note 补充「恢复 / 新任务」语义。
    super::central_events::record_for(
        state,
        &merged_work.work_id,
        &request.execution_id,
        &sid,
        tiangong_central::event::WorkEventType::WorkItemProgress,
        &if merged_work.work_id != request.work_id {
            "派发执行（等待用户决策后恢复）".to_string()
        } else {
            "派发执行（新任务）".to_string()
        },
    );
    emit_central_work_updated_state(state);
    Ok(())
}

/// 向执行路由器挂载单个执行的终态回报回调（R1：按 execution_id 路由）。
fn attach_work_finished_callback<R: tauri::Runtime>(
    state: &TiangongApp,
    app: &tauri::AppHandle<R>,
    work: crate::central::CentralDispatchRequest,
) {
    let callback_app = app.clone();
    let work_for_handler = work.clone();
    state.execution_router.attach(
        &work.session_id,
        &work.execution_id,
        Box::new(move |done: bool, reason: String| {
            handle_work_terminal(state_of(&callback_app), &work_for_handler, done, &reason);
        }),
    );
}

/// 终态回调核心（host 集成测试的真实被测函数）：T6 归属过滤 + S6 状态
/// 幂等 + 回报入队 + 依赖 flush——attach 的闭包体纯提取（零行为变化），
/// 测试直接调用以锁定接线行为（旧代码会红）。
pub(crate) fn handle_work_terminal(
    state: &TiangongApp,
    work: &crate::central::CentralDispatchRequest,
    done: bool,
    reason: &str,
) {
    // 归属过滤（R1 升级）：路由器已保证只有活跃执行的回调被触发；此处
    // 为防御迟到路由再做执行归属校验——registry 当前条目若已是**另一
    // 个在途执行**（同会话新派发/恢复已接管），本回调属旧执行，跳过。
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
            Some(entry) => {
                let execution_differs = match entry.execution_id.as_deref() {
                    // 登记缺执行归属（R0 前旧数据）：回退 work_id 比对语义
                    None => entry.work_id != work.work_id,
                    Some(current_exec) => current_exec != work.execution_id.as_str(),
                };
                execution_differs && entry.state.is_inflight()
            }
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
    // 一次性守卫（review S6→第十三轮 P1-1 修正）：改为状态驱动幂等——
    // 该 work 在 registry 中仍在途（running/waiting_user）才处理终态；
    // 已终态（report_queued/failed/completed）说明已处理过，跳过。
    // 集合记忆版会误杀同 work_id 的第二个 turn（waiting_user 恢复、
    // pause→resume 都复用 work_id），导致回报丢失 + running 永久残留。
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
            Some(entry) if !entry.state.is_inflight() => {
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
    state: &TiangongApp,
    work: &crate::central::CentralDispatchRequest,
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
