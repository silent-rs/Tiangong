//! 中枢工作详情命令（R4 前端工作视图的数据通道）。

use serde_json::json;
use tauri::AppHandle;

/// 工作项事件时间线（R5：事件日志是唯一事实源，旧内嵌时间线兼容读）。
///
/// 供工作抽屉展示：每个关键节点（派发/排队/等待用户/恢复/控制/终态）
/// 的时间、类型与人读说明，最新在尾。
#[tauri::command]
pub async fn central_work_timeline<R: tauri::Runtime>(
    app: AppHandle<R>,
    work_id: String,
) -> Result<Vec<serde_json::Value>, String> {
    let state = crate::state_of_central(&app);
    let work_id = work_id.trim().to_string();
    if work_id.is_empty() {
        return Err("work_id 不能为空".to_string());
    }
    let timeline =
        crate::host::central_events::work_timeline(state.core_manager.storage_root(), &work_id);
    Ok(timeline
        .into_iter()
        .map(|(at, kind, note)| json!({ "at": at, "kind": kind, "note": note }))
        .collect())
}

/// 等待用户决策的工作：卡片上直接答复（用户裁决 2026-11-16：保留
/// waiting_user 协议，闭环机制化——不再依赖中枢模型自觉转发，答复提交
/// 即恢复该工作，卡片随状态消失）。
#[tauri::command]
pub async fn central_answer_waiting<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    work_id: String,
    answer: String,
) -> Result<serde_json::Value, String> {
    let state = crate::state_of_central(&app);
    let answer = answer.trim().to_string();
    if answer.is_empty() {
        return Err("答复内容不能为空".to_string());
    }
    // 目标必须是 waiting_user 态的本工作（防误恢复终态/运行中工作）
    {
        let registry = state.central_work_registry.lock().unwrap();
        let entry = registry.iter().rev().find(|entry| entry.work_id == work_id);
        match entry {
            Some(entry) if entry.state == central_state::WorkState::WaitingUser => {}
            Some(entry) => {
                return Err(format!(
                    "工作 {work_id} 当前状态为 {}，已不在等待答复",
                    entry.state.as_str()
                ));
            }
            None => return Err(format!("未找到工作 {work_id}")),
        }
    }
    // 沿用 owner（waiting 条目即 owner）构造引导派发：merge_waiting_user
    // 会恢复 registry 为 running 并沿用该 work_id/任务（Z2 归属不变）
    let session_id = {
        let registry = state.central_work_registry.lock().unwrap();
        registry
            .iter()
            .rev()
            .find(|entry| entry.work_id == work_id)
            .map(|entry| entry.session_id.clone())
            .unwrap_or_default()
    };
    if session_id.is_empty() {
        return Err(format!("工作 {work_id} 缺少会话归属"));
    }
    let request = crate::CentralDispatchRequest {
        work_id: work_id.clone(),
        execution_id: tiangong_central::new_execution_id(),
        session_id: session_id.clone(),
        task: format!("[用户的答复] {answer}"),
        model: None,
        cwd: None,
        after_work_id: None,
        parallel: false,
        reused_session: true,
    };
    crate::host::dispatch::handle_central_dispatch(&state, &app, request).await?;
    Ok(serde_json::json!({ "work_id": work_id, "state": "running" }))
}
