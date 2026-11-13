//! 中枢授权命令（R3 票据化的宿主签发通道）。
//!
//! 这些命令**只能由真实用户操作触发**（授权卡片 / 决定卡片 / 快捷操作），
//! 是 WorkspaceApprovalTicket 与 DecisionTicket 的唯一签发入口——
//! 模型侧没有对应工具，无法自证用户同意（方案 §5.6）。
//!
//! R4 前端授权卡片就绪前的调用方：设置界面/系统快捷授权（未来）与
//! 集成测试。派发侧消费见 `central_host::dispatch::plan_central_dispatch`
//! （双通道：目录长期授权 OR 一次性票据；R5 收敛为纯票据）。

use serde_json::json;
use tauri::{AppHandle, Manager};

use crate::app::TiangongApp;
use tiangong_central::tickets::RiskLevel;

/// 授权票据默认有效期（秒）：10 分钟——足够完成派发，过期不得默认视为同意。
const DEFAULT_TICKET_TTL_SECONDS: i64 = 600;

/// 用户授权 Workspace（R3）。
///
/// - `task_fingerprint`：用户看到的任务描述（与派发任务文本一致才放行，
///   防偷换）；空串视为仅授权目录浏览级操作，派发仍会被指纹校验拒绝。
/// - `persist`：true 时同时登记目录长期授权（「记住此工作区」语义，
///   兼容既有体验）；默认 false（仅一次性票据）。
#[tauri::command]
pub async fn central_approve_workspace(
    app: AppHandle,
    path: String,
    task_fingerprint: String,
    persist: Option<bool>,
    ttl_seconds: Option<i64>,
) -> Result<serde_json::Value, String> {
    let state = app.state::<TiangongApp>();
    let path = path.trim().to_string();
    if path.is_empty() {
        return Err("path 不能为空".to_string());
    }
    let target = std::path::Path::new(&path);
    if !target.is_dir() {
        return Err(format!("路径不存在或不是目录：{path}"));
    }
    if persist.unwrap_or(false) {
        if let Err(error) =
            crate::central::add_confirmed_workspace(state.core_manager.storage_root(), target)
        {
            tracing::warn!(%error, "授权目录长期登记失败（一次性票据仍然生效）");
        }
    }
    let ticket = state.ticket_registry.issue_workspace_approval(
        &path,
        task_fingerprint.trim(),
        RiskLevel::High,
        ttl_seconds.unwrap_or(DEFAULT_TICKET_TTL_SECONDS),
    );
    tracing::info!(
        approval_id = %ticket.approval_id,
        workspace = %path,
        persist = persist.unwrap_or(false),
        "Workspace 授权票据已签发（用户操作）"
    );
    // Z6 审计：授权事实入事件日志（work_id 空串标记目录级授权）
    super::central_events::record_central_event(
        state.core_manager.storage_root(),
        "workspace-approval",
        &ticket.approval_id,
        "",
        tiangong_central::event::WorkEventType::ApprovalRequired,
        &format!(
            "用户授权目录 {path}（任务指纹：{}；persist={}）",
            task_fingerprint.trim(),
            persist.unwrap_or(false)
        ),
    );
    // 授权闭环（用户实测修复）：授权完成后必须通知中枢，否则模型在
    // 等一个不会来的信号、不会自动重派。注入提示触发中枢 turn，
    // 模型立即重派同一任务（task 文本一致才放行）。
    let fingerprint_preview: String = task_fingerprint.trim().chars().take(60).collect();
    let note = format!(
        "[系统中枢] 用户已授权目录 {path}（任务：{fingerprint_preview}）。请立即重新派发该任务——dispatch_work 的 task 文本与上述任务保持完全一致（授权票据绑定任务指纹），不要改动任务描述。"
    );
    if let Err(error) = state.deliver_central_message(
        scru128::new().to_string(),
        vec![tiangong_types::ContentBlock::text(note)],
        Some(super::events::system_note_render("approval")),
    ) {
        tracing::warn!(%error, "授权完成提示注入中枢失败（用户下次发言时仍可重派）");
    }
    Ok(json!({
        "approval_id": ticket.approval_id,
        "workspace_id": ticket.workspace_id,
        "expires_at": ticket.expires_at,
        "persist": persist.unwrap_or(false),
    }))
}

/// 用户决定（R3 DecisionTicket 签发）：用户在决定卡片选定选项后触发。
///
/// 签发的票据绑定 `execution_id` 与问题版本；恢复等待用户的工作时由
/// 消费点校验（问题已更新则旧票据作废）。R4 决定卡片接线前，此通道
/// 供宿主集成与未来 UI 使用。
#[tauri::command]
pub async fn central_resolve_decision(
    app: AppHandle,
    execution_id: String,
    question_version: u32,
    option: String,
    ttl_seconds: Option<i64>,
) -> Result<serde_json::Value, String> {
    let state = app.state::<TiangongApp>();
    let execution_id = execution_id.trim().to_string();
    let option = option.trim().to_string();
    if execution_id.is_empty() || option.is_empty() {
        return Err("execution_id 与 option 不能为空".to_string());
    }
    let ticket = state.ticket_registry.issue_decision(
        &execution_id,
        question_version,
        &option,
        ttl_seconds.unwrap_or(DEFAULT_TICKET_TTL_SECONDS),
    );
    tracing::info!(
        ticket_id = %ticket.ticket_id,
        execution_id = %execution_id,
        question_version,
        "决策票据已签发（用户操作）"
    );
    Ok(json!({
        "ticket_id": ticket.ticket_id,
        "execution_id": ticket.execution_id,
        "chosen_option": ticket.chosen_option,
        "expires_at": ticket.expires_at,
    }))
}
