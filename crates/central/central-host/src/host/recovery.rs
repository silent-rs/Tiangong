//! 中枢重启恢复与激活摘要（R1 从 app.rs 拆出）：从 work-items.json 重建状态
//! 登记、排队重放投递、离开期间汇总回报、进入中枢的激活问候注入。

use crate::host::dispatch::handle_central_dispatch;
use crate::host::events::emit_central_work_updated_state;
use crate::state_of_central;

/// 重启恢复（§15.1 + 徽章「永不消失」）：从 work-items.json 全量重建
/// 状态登记（completed/failed 按 outcome、legacy 从中枢会话回报推断成败、
/// 未终态近期按 interrupted），并注入「离开期间」汇总回报——用户下一条
/// 消息时中枢能主动说明离开期间发生了什么（review P2：恢复后回报丢失
/// ——徽章显示已完成但中枢从未收到结果摘要）。
/// 幂等：registry 非空时跳过（防 setup 与 lazy 通道双跑重复注入）。
pub fn recover_central_work_state<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let state = crate::state_of_central(app);
    let state = &*state;
    // R5：登记由事件日志投影重建（唯一事实源重放）；日志为空（R5 前
    // 旧数据）回退 work-items.json 快照重建。
    let storage_root = state.core_manager.storage_root().to_path_buf();
    let entries = crate::host::central_events::rebuild_registry_from_log(&storage_root)
        .unwrap_or_else(|| crate::recover_registry_entries(&storage_root));
    if entries.is_empty() {
        return;
    }
    let mut registry = state
        .central_work_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !registry.is_empty() {
        return; // 已恢复过（双触发幂等）
    }
    // R2：旧工作项时间线一次性迁移至事件日志（幂等：日志非空跳过）。
    crate::host::central_events::migrate_legacy_timeline(
        state_of_central(app).core_manager.storage_root(),
    );
    // 汇总只在有「需要用户处理/知晓的新事实」时注入：interrupted（结果
    // 未产出，可重派）与 failed（可换模型重派）。completed 不注入——
    // 每次重启都注入全量历史完成数会重复刷屏且数字并非「离开期间」
    // （work-items.json 是滚动 20 条，无法区分本次离开新增）。
    // 先提取 owned 数据（避免与 extend 的所有权冲突）
    let failed_names: Vec<String> = entries
        .iter()
        .filter(|e| e.state == central_state::WorkState::Failed)
        .map(|e| {
            format!(
                "{}（{}）",
                e.work_id,
                e.task.chars().take(24).collect::<String>()
            )
        })
        .collect();
    let interrupted_count = entries
        .iter()
        .filter(|e| e.state == central_state::WorkState::Interrupted)
        .count();
    for entry in &entries {
        tracing::info!(
            work_id = %entry.work_id,
            state = entry.state.as_str(),
            "后台工作状态已从登记重建（重启恢复）"
        );
    }
    // 排队挂起重放：queued 条目重新进入派发链（依赖已终态 → 立即投递；
    // 仍在途 → 重新排队）。内存 pending 队列随重启丢失，落盘标记是唯一
    // 恢复源（review P2：队列不持久导致挂起任务丢失）。
    let queued_replay: Vec<crate::CentralDispatchRequest> =
        crate::queued_work_items(state.core_manager.storage_root())
            .into_iter()
            .map(|record| crate::CentralDispatchRequest {
                work_id: record.work_id,
                // R0 契约：排队重放是一次新执行（§5.4），旧 execution_id
                // 留在事件时间线中可追溯，投递用新归属。
                execution_id: tiangong_central::new_execution_id(),
                session_id: record.session_id,
                task: record.task,
                model: record.model,
                cwd: record.cwd.clone(),
                after_work_id: record.after_work_id,
                parallel: false,
                // 按记录还原复用语义——新建会话的排队重放保留标题写入
                reused_session: record.reused_session,
            })
            .collect();
    // 重启即中断（对齐快照恢复路径的 6 小时语义）：事件投影产出的
    // 在途条目（Running/WaitingUser/Paused）迁移为 Interrupted——
    // Core 已随进程消亡，保持 Running 是徽章幽灵且中断摘要漏报
    //（批五测试发现：投影路径此前漏迁移）。
    let entries: Vec<_> = entries
        .into_iter()
        .map(|mut entry| {
            if entry.state.is_inflight() {
                entry.state = central_state::WorkState::Interrupted;
            }
            entry
        })
        .collect();
    registry.extend(entries);
    drop(registry);
    for request in queued_replay {
        let app_handle = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(error) =
                handle_central_dispatch(&state_of_central(&app_handle), &app_handle, request).await
            {
                tracing::warn!(%error, "排队工作重启重放失败");
            }
        });
    }
    if !failed_names.is_empty() || interrupted_count > 0 {
        let failed_note = if failed_names.is_empty() {
            String::new()
        } else {
            format!("失败：{}。", failed_names.join("、"))
        };
        let summary = format!(
            "[系统中枢] 应用重启恢复：{interrupted_count} 个后台工作中断（结果未产出，可复用原会话重派）。{failed_note}请向用户转述并根据指示处理。"
        );
        state
            .central_work_reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(summary);
    }
    emit_central_work_updated_state(state);
}

/// 激活摘要组装（§4.2 基于可验证上下文生成问候的数据源；纯函数可测）。
pub fn build_activation_summary(
    entries: &[crate::CentralWorkStatus],
    goals: &[crate::GoalRecord],
) -> Option<String> {
    use central_state::WorkState;
    let count = |state: WorkState| entries.iter().filter(|e| e.state == state).count();
    let waiting = count(WorkState::WaitingUser);
    let interrupted = count(WorkState::Interrupted);
    let failed = count(WorkState::Failed);
    // 适时沉默（§9 / 用户实测反馈）：空闲与纯执行态（running/paused/
    // queued——徽章可见）一律不注入，不要求模型自我介绍或播报状态；
    // 只有「需要用户动作」的事实（等待决策 / 失败未处理 / 中断未收尾）
    // 才注入一条简洁提醒——这是关键节点必达，其余保持安静。
    if waiting == 0 && interrupted == 0 && failed == 0 {
        return None;
    }
    let mut facts: Vec<String> = Vec::new();
    if waiting > 0 {
        let names: Vec<String> = entries
            .iter()
            .filter(|e| e.state == WorkState::WaitingUser)
            .map(|e| {
                format!(
                    "{}（{}）",
                    e.work_id,
                    e.task.chars().take(20).collect::<String>()
                )
            })
            .collect();
        facts.push(format!("{waiting} 个工作在等你决策：{}", names.join("、")));
    }
    if failed > 0 {
        facts.push(format!("{failed} 个工作失败未处理（可换模型重派）"));
    }
    if interrupted > 0 {
        facts.push(format!(
            "{interrupted} 个工作中断未收尾（可复用原会话重派）"
        ));
    }
    let active_goals: Vec<&str> = goals
        .iter()
        .filter(|g| g.status == "active")
        .map(|g| g.title.as_str())
        .collect();
    let goal_note = if active_goals.is_empty() {
        String::new()
    } else {
        format!("\n相关目标：{}", active_goals.join("、"))
    };
    Some(format!(
        "[系统中枢][激活] 用户刚回到中枢。有需要用户知道的事：{}{goal_note}\n请用一两句自然的话告诉用户（说事实即可，不用列清单，不用介绍自己能做什么）；其余等用户开口。",
        facts.join("；")
    ))
}

/// 激活问候注入（§4.2）：进入中枢模式时把状态摘要投递给中枢 Core，
/// 驱动模型主动问候。幂等：每进程仅首次进入注入（避免频繁切换刷屏，
/// §10.4 频率控制）。中枢 Core 须已 ensure。
pub fn inject_central_activation_summary(state: &crate::CentralHostState) -> Result<(), String> {
    if state
        .central_activation_greeted
        .swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        return Ok(());
    }
    let entries = state.central_work_status();
    let goals = crate::load_goals(state.core_manager.storage_root());
    // 空闲与纯执行态不注入（适时沉默——用户实测：机械问候无价值）；
    // 仅存在需要用户动作的事实时注入一条自然提醒。
    let Some(summary) = build_activation_summary(&entries, &goals) else {
        return Ok(());
    };
    state.deliver_central_message(
        scru128::new().to_string(),
        vec![tiangong_types::ContentBlock::text(summary)],
        Some(crate::host::events::system_note_render("activation")),
    )
}
