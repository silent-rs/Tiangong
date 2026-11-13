//! 中枢工作详情命令（R4 前端工作视图的数据通道）。

use serde_json::json;
use tauri::{AppHandle, Manager};

use crate::app::TiangongApp;

/// 工作项事件时间线（R5：事件日志是唯一事实源，旧内嵌时间线兼容读）。
///
/// 供工作抽屉展示：每个关键节点（派发/排队/等待用户/恢复/控制/终态）
/// 的时间、类型与人读说明，最新在尾。
#[tauri::command]
pub async fn central_work_timeline(
    app: AppHandle,
    work_id: String,
) -> Result<Vec<serde_json::Value>, String> {
    let state = app.state::<TiangongApp>();
    let work_id = work_id.trim().to_string();
    if work_id.is_empty() {
        return Err("work_id 不能为空".to_string());
    }
    let timeline = crate::central_host::central_events::work_timeline(
        state.core_manager.storage_root(),
        &work_id,
    );
    Ok(timeline
        .into_iter()
        .map(|(at, kind, note)| json!({ "at": at, "kind": kind, "note": note }))
        .collect())
}
