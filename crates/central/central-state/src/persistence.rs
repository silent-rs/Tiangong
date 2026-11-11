//! 中枢持久化对账（work-items / goals / reports / confirmed-workspaces）。
//!
//! 全部 IO 与状态重建逻辑集中于此（原子写 + 失败记日志），宿主经
//! [`crate`] 的 re-export 调用——对账规则变化只改这一处。

use crate::{WorkState, WorkStatus};

/// 派发登记（中枢视角的最低事实：派发了什么、给谁、什么模型）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkItemRecord {
    pub work_id: String,
    /// 是否复用了已有会话（应用尽用；true 时 session_id 是既有会话）。
    #[serde(default)]
    pub reused_session: bool,
    pub session_id: String,
    pub task: String,
    #[serde(default)]
    pub model: Option<String>,
    pub dispatched_at: String,
    /// 终态时间（完成/失败到达时刻）；None = 未终态——应用重启后仍为
    /// None 的近期条目按「已中断」恢复到状态登记（§15.1 离开期间变化）。
    #[serde(default)]
    pub finished_at: Option<String>,
    /// 终态成败（"completed" / "failed" / "cancelled"）——重启后徽章按此
    /// 重建分组；None = 旧记录（字段上线前）或未终态。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// 事件时间线（派发/排队/阻塞/恢复/暂停/取消/终态等关键节点），
    /// 持久化供回查（阶段 2「事件持久化」）；每工作上限 30 条。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<WorkItemEvent>,
}

/// 后台工作事件（时间线条目，§13.4 轻量版）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkItemEvent {
    /// 本地时间 "%Y-%m-%d %H:%M:%S"
    pub at: String,
    /// dispatched / queued / waiting_user / resumed / paused / cancelled / finished
    pub kind: String,
    /// 人读说明
    pub note: String,
}

/// 保留最近 30 条）。找不到条目时静默跳过（时间线是附加信息，不阻塞主链）。
pub fn append_work_item_event(
    storage_root: &std::path::Path,
    session_id: &str,
    kind: &str,
    note: &str,
) {
    let mut items = load_work_items(storage_root);
    let Some(entry) = items
        .iter_mut()
        .rev()
        .find(|item| item.session_id == session_id)
    else {
        return;
    };
    entry.events.push(WorkItemEvent {
        at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        kind: kind.to_string(),
        note: note.to_string(),
    });
    let overflow = entry.events.len().saturating_sub(30);
    if overflow > 0 {
        entry.events.drain(..overflow);
    }
    save_work_items(storage_root, &items);
}

/// 派发登记持久化路径：`{storage_root}/central/work-items.json`。
pub fn work_items_path(storage_root: &std::path::Path) -> std::path::PathBuf {
    storage_root.join("central").join("work-items.json")
}

/// 加载派发登记（损坏/缺失按空表处理；损坏时记日志便于排障）。
pub fn load_work_items(storage_root: &std::path::Path) -> Vec<WorkItemRecord> {
    let path = work_items_path(storage_root);
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(items) => items,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "派发登记损坏，按空表处理");
                Vec::new()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "派发登记读取失败，按空表处理");
            Vec::new()
        }
    }
}

/// 全表落盘（原子写：tmp + rename；失败记日志，不静默吞错）。
pub fn save_work_items(storage_root: &std::path::Path, items: &[WorkItemRecord]) {
    let path = work_items_path(storage_root);
    let content = match serde_json::to_string_pretty(items) {
        Ok(content) => content,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "派发登记序列化失败，未落盘");
            return;
        }
    };
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            tracing::warn!(%error, path = %path.display(), "派发登记目录创建失败");
        }
    }
    let tmp_path = path.with_extension("json.tmp");
    if let Err(error) = std::fs::write(&tmp_path, &content) {
        tracing::warn!(%error, path = %tmp_path.display(), "派发登记临时文件写入失败");
        return;
    }
    if let Err(error) = std::fs::rename(&tmp_path, &path) {
        tracing::warn!(%error, from = %tmp_path.display(), to = %path.display(), "派发登记原子替换失败");
    }
}

/// 更新某条登记的终态时间并落盘（完成语义）。
pub fn mark_work_item_finished(storage_root: &std::path::Path, work_id: &str) {
    mark_work_item_finished_with_session(storage_root, work_id, None, false)
}

/// 分组（用户实测：完成的任务重启后不显示——终态此前只存内存）。
pub fn mark_work_item_finished_with_session(
    storage_root: &std::path::Path,
    work_id: &str,
    session_id: Option<&str>,
    failed: bool,
) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let outcome = if failed { "failed" } else { "completed" };
    let mut items = load_work_items(storage_root);
    let idx_by_work = items.iter().position(|item| item.work_id == work_id);
    let idx = idx_by_work.or_else(|| {
        session_id.and_then(|sid| {
            items
                .iter()
                .position(|item| item.session_id == sid && item.finished_at.is_none())
        })
    });
    if let Some(idx) = idx {
        items[idx].finished_at = Some(now);
        items[idx].outcome = Some(outcome.to_string());
    } else {
        return;
    }
    save_work_items(storage_root, &items);
}

/// mark 写入，此函数只改 outcome 表达）。
pub fn set_work_item_outcome(storage_root: &std::path::Path, session_id: &str, outcome: &str) {
    let mut items = load_work_items(storage_root);
    if let Some(entry) = items
        .iter_mut()
        .rev()
        .find(|item| item.session_id == session_id)
    {
        entry.outcome = Some(outcome.to_string());
        save_work_items(storage_root, &items);
    }
}

/// 从中枢会话回报中提取失败工作的 work_id 集合（legacy 终态成败推断）。
fn failed_work_ids_from_central_session(
    storage_root: &std::path::Path,
) -> std::collections::HashSet<String> {
    let session_path = storage_root
        .join("central")
        .join("sessions")
        .join("central-hub.json");
    let Ok(content) = std::fs::read_to_string(&session_path) else {
        return std::collections::HashSet::new();
    };
    let mut failed = std::collections::HashSet::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
        if let Some(messages) = value.get("messages").and_then(|m| m.as_array()) {
            for message in messages {
                let text = match message.get("content") {
                    Some(serde_json::Value::String(text)) => text.clone(),
                    Some(serde_json::Value::Array(blocks)) => blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => continue,
                };
                if let Some(rest) = text.strip_prefix("[后台工作失败] ") {
                    // 形如 "[后台工作失败] work-xxx（模型 …）…"——按 work_id
                    // 字符集（字母数字连字符）截断，不吃进中文括号注记
                    let id: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                        .collect();
                    if !id.is_empty() {
                        failed.insert(id);
                    }
                }
            }
        }
    }
    failed
}

/// - completed/failed 各保留最近 10 条（与运行时清理上限一致）
pub fn recover_registry_entries(storage_root: &std::path::Path) -> Vec<crate::WorkStatus> {
    // legacy 终态记录（outcome 字段上线前）的成败从中枢会话回报推断：
    // 扫描 central-hub.json 消息中的「[后台工作失败] work-X」标记——
    // 命中即 failed，其余终态按 completed（review P2：一律近似 completed
    // 会把失败显示成成功固化进历史）。
    let failed_ids = failed_work_ids_from_central_session(storage_root);
    let cutoff = (chrono::Local::now() - chrono::Duration::hours(6))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let mut entries: Vec<WorkStatus> = Vec::new();
    for item in load_work_items(storage_root) {
        let state = if item.outcome.as_deref() == Some("failed") {
            WorkState::Failed
        } else if item.outcome.as_deref() == Some("cancelled") {
            WorkState::Cancelled
        } else if item.outcome.as_deref() == Some("completed") || item.finished_at.is_some() {
            // legacy 终态：中枢会话回报标记失败 → failed；否则完成
            if failed_ids.contains(&item.work_id) {
                WorkState::Failed
            } else {
                WorkState::Completed
            }
        } else if item.dispatched_at.as_str() > cutoff.as_str() {
            WorkState::Interrupted
        } else {
            continue;
        };
        entries.push(crate::WorkStatus {
            work_id: item.work_id,
            session_id: item.session_id,
            task: item.task,
            model: item.model,
            dispatched_at: item.dispatched_at,
            state,
            reused_session: item.reused_session,
            last_activity: None,
            cwd: None,
        });
    }
    // 截断：completed/failed 各保留最近 10 条（尾部为最新，从头部移除）
    for state in [crate::WorkState::Completed, crate::WorkState::Failed] {
        let count = entries.iter().filter(|e| e.state == state).count();
        if count > 10 {
            let mut to_remove = count - 10;
            entries.retain(|entry| {
                if entry.state == state && to_remove > 0 {
                    to_remove -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }
    entries
}

/// （终态机制上线前写入）不再每次启动都误报中断。
pub fn interrupted_work_items(storage_root: &std::path::Path) -> Vec<WorkItemRecord> {
    // dispatched_at 为「%Y-%m-%d %H:%M:%S」本地时间，字典序与时间序一致，
    // 字符串比较近似近 48 小时窗口。
    let cutoff = (chrono::Local::now() - chrono::Duration::hours(6))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    load_work_items(storage_root)
        .into_iter()
        .filter(|item| item.finished_at.is_none() && item.dispatched_at.as_str() > cutoff.as_str())
        .collect()
}

/// 会话级 upsert 派发登记并落盘（同会话更新、保留最近 20 条）。
pub fn upsert_work_item(storage_root: &std::path::Path, record: &WorkItemRecord) {
    let mut items = load_work_items(storage_root);
    // 会话维度记忆：同一会话的再次派发/引导更新原记录（反映该会话
    // 最近一次工作），不堆积多条；尾部为最新。
    if let Some(idx) = items
        .iter()
        .position(|item| item.session_id == record.session_id)
    {
        items.remove(idx);
    }
    items.push(record.clone());
    // 保留上限：登记表只服务「近期派发会话的记忆与复用」（按用户
    // 要求保留 20 个会话条目）；会话文件本身不受影响——那是复用与
    // 复核的真相源。
    const MAX_WORK_ITEMS: usize = 20;
    if items.len() > MAX_WORK_ITEMS {
        let overflow = items.len() - MAX_WORK_ITEMS;
        items.drain(..overflow);
    }
    if let Some(parent) = work_items_path(storage_root).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    save_work_items(storage_root, &items);
}

/// 已确认工作区持久化路径：`{storage_root}/central/confirmed-workspaces.json`。
pub fn confirmed_workspaces_path(storage_root: &std::path::Path) -> std::path::PathBuf {
    storage_root
        .join("central")
        .join("confirmed-workspaces.json")
}

/// 读取已确认工作区列表（canonicalize 后比较，容错符号链接/尾斜杠）。
pub fn load_confirmed_workspaces(storage_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_to_string(confirmed_workspaces_path(storage_root))
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .unwrap_or_default()
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect()
}

/// 追加已确认工作区（去重、canonicalize）并落盘。
pub fn add_confirmed_workspace(storage_root: &std::path::Path, path: &std::path::Path) {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let list = load_confirmed_workspaces(storage_root);
    let list: Vec<std::path::PathBuf> = std::iter::once(canonical)
        .chain(list)
        .filter(|item| item.is_absolute())
        .collect();
    let mut seen = std::collections::HashSet::new();
    let list: Vec<std::path::PathBuf> = list
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect();
    let path = confirmed_workspaces_path(storage_root);
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            tracing::warn!(%error, path = %path.display(), "已确认工作区目录创建失败");
        }
    }
    let text = match serde_json::to_string_pretty(
        &list
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<String>>(),
    ) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(%error, "已确认工作区序列化失败，未落盘");
            return;
        }
    };
    // 原子写：tmp + rename（与派发登记一致，失败记日志不吞错）
    let tmp_path = path.with_extension("json.tmp");
    if let Err(error) = std::fs::write(&tmp_path, text) {
        tracing::warn!(%error, path = %tmp_path.display(), "已确认工作区临时文件写入失败");
        return;
    }
    if let Err(error) = std::fs::rename(&tmp_path, &path) {
        tracing::warn!(%error, from = %tmp_path.display(), to = %path.display(), "已确认工作区原子替换失败");
    }
}

/// 用户目标（§13.2：中枢以用户目标为语义核心，目标跨工作持续存在）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalRecord {
    pub goal_id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// 关联工作区（可选）。
    #[serde(default)]
    pub workspace: Option<String>,
    pub created_at: String,
    /// active / achieved / abandoned（中枢按用户反馈更新）。
    #[serde(default)]
    pub status: String,
}

// ============================================================================

fn goals_path(storage_root: &std::path::Path) -> std::path::PathBuf {
    storage_root.join("central").join("goals.json")
}

// 用户目标（§13.2）与结构化报告（§13.5）：完整实现的数据模型层。

pub fn load_goals(storage_root: &std::path::Path) -> Vec<GoalRecord> {
    let path = goals_path(storage_root);
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %path.display(), "目标记录损坏，按空表处理");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// 目标登记（active 上限 10：久远的 inactive 目标自动归档移除）。
pub fn save_goals(storage_root: &std::path::Path, goals: &[GoalRecord]) {
    let path = goals_path(storage_root);
    let Some(parent) = path.parent() else { return };
    if let Err(error) = std::fs::create_dir_all(parent) {
        tracing::warn!(%error, "目标目录创建失败");
        return;
    }
    match serde_json::to_string_pretty(goals) {
        Ok(text) => {
            let tmp = path.with_extension("json.tmp");
            if let Err(error) = std::fs::write(&tmp, text) {
                tracing::warn!(%error, "目标临时文件写入失败");
                return;
            }
            if let Err(error) = std::fs::rename(&tmp, &path) {
                tracing::warn!(%error, "目标原子替换失败");
            }
        }
        Err(error) => tracing::warn!(%error, "目标序列化失败"),
    }
}

/// 结构化报告（§13.5：工作终态的结论/产物/风险结构化存档，供回查）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReportRecord {
    pub report_id: String,
    pub work_id: String,
    pub summary: String,
    /// 产物路径列表（文件/报告）。
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// 风险与未尽事项。
    #[serde(default)]
    pub risks: Vec<String>,
    pub created_at: String,
}

// ============================================================================

fn reports_path(storage_root: &std::path::Path) -> std::path::PathBuf {
    storage_root.join("central").join("reports.json")
}

pub fn load_reports(storage_root: &std::path::Path) -> Vec<ReportRecord> {
    let path = reports_path(storage_root);
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %path.display(), "报告存档损坏，按空表处理");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// 报告存档（保留最近 50 份）。
pub fn save_reports(storage_root: &std::path::Path, reports: &[ReportRecord]) {
    let path = reports_path(storage_root);
    let Some(parent) = path.parent() else { return };
    if let Err(error) = std::fs::create_dir_all(parent) {
        tracing::warn!(%error, "报告目录创建失败");
        return;
    }
    match serde_json::to_string_pretty(reports) {
        Ok(text) => {
            let tmp = path.with_extension("json.tmp");
            if let Err(error) = std::fs::write(&tmp, text) {
                tracing::warn!(%error, "报告临时文件写入失败");
                return;
            }
            if let Err(error) = std::fs::rename(&tmp, &path) {
                tracing::warn!(%error, "报告原子替换失败");
            }
        }
        Err(error) => tracing::warn!(%error, "报告序列化失败"),
    }
}

/// 路径规范化（canonicalize 失败退回原样，比较仍可进行）。
pub fn canonical_path_string(path: &str) -> String {
    std::path::Path::new(path.trim())
        .canonicalize()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.trim().to_string())
}

/// canonicalize，符号链接/尾斜杠不产生绕过）。
pub fn workspace_confirmed(
    storage_root: &std::path::Path,
    default_workspace: &str,
    target: &str,
) -> bool {
    let target = target.trim();
    if target.is_empty() {
        return true;
    }
    let canonical_target = canonical_path_string(target);
    let canonical_default = canonical_path_string(default_workspace);
    // 默认工作区子树：canonical 前缀 + '/' 边界（防 /a-b 误匹配 /a）
    if canonical_target == canonical_default
        || canonical_target
            .strip_prefix(&canonical_default)
            .is_some_and(|rest| rest.starts_with('/'))
    {
        return true;
    }
    load_confirmed_workspaces(storage_root)
        .iter()
        .any(|confirmed| {
            let confirmed = confirmed.to_string_lossy();
            confirmed == canonical_target || confirmed == target
        })
}
