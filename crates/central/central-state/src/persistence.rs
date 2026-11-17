//! 中枢持久化对账（work-items / goals / reports / confirmed-workspaces）。
//!
//! 全部 IO 与状态重建逻辑集中于此（事务化原子写 + 失败记日志），宿主经
//! [`crate`] 的 re-export 调用——对账规则变化只改这一处。
//!
//! R2 事务加固（方案 §5.5）：随机后缀临时文件（不再共用固定 `.tmp`——
//! 并发写会互相覆盖半成品）、进程内全局写锁（同存储根串行化）、
//! fsync 后 rename（崩溃时不留下「半个文件」）；写失败显式记日志并
//! 清理临时残留，不静默声称成功。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::{WorkState, WorkStatus};

/// 同一进程内的持久化写入共用此锁（方案 §5.5：同一存储根的进程内写锁）。
static PERSISTENCE_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 临时文件序号（与纳秒时间戳组合，保证同进程内不重名）。
static TMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 事务化原子写：随机后缀 tmp + 写锁 + fsync + rename（方案 §5.5）。
///
/// - 临时文件名带随机后缀，多写者不会互相覆盖（旧实现共用固定
///   `.json.tmp`，并发下 rename 会错挂半成品）；
/// - 追加 fsync：崩溃后 tmp 是完整旧内容或不存在，目标文件不会是半个；
/// - rename 原子替换；失败清理 tmp 残留并返回错误。
pub fn atomic_write_bytes(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let _guard = PERSISTENCE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    atomic_write_bytes_locked(path, content)
}

/// 无锁写核心（调用方须已持有 PERSISTENCE_WRITE_LOCK——如
/// [`with_work_items`] 的 RMW 区间；直接调用会破坏互斥）。
fn atomic_write_bytes_locked(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = random_tmp_path(path);
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp_path, path)
    })();
    if result.is_err() {
        // 清理残留临时文件（失败路径不留垃圾，下次写入全新后缀）
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

/// 随机后缀临时路径：`{stem}.{nanos}-{seq}.{ext}.tmp`。
fn random_tmp_path(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seq = TMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "central.json".to_string());
    let tmp_name = format!("{file_name}.{nanos}-{seq}.tmp");
    path.with_file_name(tmp_name)
}

/// 快照 envelope（R5 附录 A.4 第 3 项）：schema 版本包裹记录数组，
/// 为后续格式演进与 recover 对账（事件 offset vs 快照版本）建立锚点。
#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotEnvelope<T> {
    schema_version: u32,
    records: Vec<T>,
}

/// 快照 schema 版本（与契约版本同步起点）。
const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// 兼容读：envelope 或裸数组（R5 前格式）——旧文件不迁移即读。
fn parse_snapshot<T: serde::de::DeserializeOwned>(
    content: &str,
) -> std::result::Result<Vec<T>, serde_json::Error> {
    match serde_json::from_str::<SnapshotEnvelope<T>>(content) {
        Ok(envelope) => {
            // 对账（R5）：快照版本超前于当前代码（未来格式被旧版本读取）
            // ——字段级 serde default 保证不崩，但版本差必须可观测。
            if envelope.schema_version > SNAPSHOT_SCHEMA_VERSION {
                tracing::warn!(
                    snapshot_version = envelope.schema_version,
                    supported = SNAPSHOT_SCHEMA_VERSION,
                    "快照 schema 版本超前（未知字段按默认值读取，建议升级应用）"
                );
            }
            Ok(envelope.records)
        }
        Err(_) => serde_json::from_str(content),
    }
}

/// 序列化为 envelope（新写入统一带版本）。
fn serialize_snapshot<T: serde::Serialize + Clone>(
    records: &[T],
) -> std::result::Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&SnapshotEnvelope {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        records: records.to_vec(),
    })
}

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
    /// 排队时刻（Some = 挂起待依赖终态；重启恢复重放投递）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<String>,
    /// 排队等待的依赖（work_id 或会话 ID；queued_at 有值时有效）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_work_id: Option<String>,
    /// 派发工作目录（排队重放还原——新建会话尚不存在，丢失 cwd 会在
    /// 默认工作区执行，第十三轮 P2-2）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 最近一次执行的归属（R0 契约，§5.4）：`exec-<scru128>`。会话维度
    /// upsert 时取最新派发的执行；恢复 / 重试 / 重放会换新 execution_id。
    /// None = 旧记录（字段上线前）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
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
    /// 事件归属的执行（R0 契约）：按该会话登记的执行回填（R5 起写
    /// 入事件日志；本字段为旧数据兼容读）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
}

/// 派发登记持久化路径：`{storage_root}/central/work-items.json`。
pub fn work_items_path(storage_root: &std::path::Path) -> std::path::PathBuf {
    storage_root.join("central").join("work-items.json")
}

/// 加载派发登记（损坏/缺失按空表处理；损坏时记日志便于排障）。
pub fn load_work_items(storage_root: &std::path::Path) -> Vec<WorkItemRecord> {
    let path = work_items_path(storage_root);
    match std::fs::read_to_string(&path) {
        Ok(content) => match parse_snapshot(&content) {
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

/// 全表落盘（原子写）。写失败显式返回 Err（Z4：不再静默吞错——
/// 调用方必须知道数据未更新；事实源见事件日志）。
pub fn save_work_items(
    storage_root: &std::path::Path,
    items: &[WorkItemRecord],
) -> Result<(), String> {
    let path = work_items_path(storage_root);
    let content =
        serialize_snapshot(items).map_err(|error| format!("派发登记序列化失败: {error}"))?;
    atomic_write_bytes(&path, content.as_bytes())
        .map_err(|error| format!("派发登记落盘失败（数据未更新）: {error}"))
}

/// 无锁 save（with_work_items 区间内使用）。
fn save_work_items_locked(
    storage_root: &std::path::Path,
    items: &[WorkItemRecord],
) -> Result<(), String> {
    let path = work_items_path(storage_root);
    let content =
        serialize_snapshot(items).map_err(|error| format!("派发登记序列化失败: {error}"))?;
    atomic_write_bytes_locked(&path, content.as_bytes())
        .map_err(|error| format!("派发登记落盘失败（数据未更新）: {error}"))
}

/// RMW 事务（Z4）：在进程内写锁内完成 load → mutate → save——
/// 消除锁外的「读-改-写」丢更新（两线程并发 upsert 不同会话，
/// 此前后写者的 load 看不到前写者的 save，覆盖丢失一行）。
///
/// `mutate` 返回值经 Ok 透传；落盘失败返回 Err（改动未生效）。
pub fn with_work_items<R>(
    storage_root: &std::path::Path,
    mutate: impl FnOnce(&mut Vec<WorkItemRecord>) -> R,
) -> Result<R, String> {
    let _guard = PERSISTENCE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut items = load_work_items(storage_root);
    let result = mutate(&mut items);
    save_work_items_locked(storage_root, &items)?;
    Ok(result)
}

/// 更新某条登记的终态时间并落盘（完成语义）。
pub fn mark_work_item_finished(
    storage_root: &std::path::Path,
    work_id: &str,
) -> Result<(), String> {
    mark_work_item_finished_with_session(storage_root, work_id, None, false)
}

/// 分组（用户实测：完成的任务重启后不显示——终态此前只存内存）。
pub fn mark_work_item_finished_with_session(
    storage_root: &std::path::Path,
    work_id: &str,
    session_id: Option<&str>,
    failed: bool,
) -> Result<(), String> {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let outcome = if failed { "failed" } else { "completed" };
    with_work_items(storage_root, |items| {
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
        }
    })
}

/// mark 写入，此函数只改 outcome 表达）。
pub fn set_work_item_outcome(
    storage_root: &std::path::Path,
    session_id: &str,
    outcome: &str,
) -> Result<(), String> {
    with_work_items(storage_root, |items| {
        if let Some(entry) = items
            .iter_mut()
            .rev()
            .find(|item| item.session_id == session_id)
        {
            entry.outcome = Some(outcome.to_string());
        }
    })
}

/// 标记会话登记条目为排队挂起（queued_at + 依赖），落盘。
pub fn mark_work_item_queued(
    storage_root: &std::path::Path,
    session_id: &str,
    after_work_id: &str,
    cwd: Option<&str>,
) -> Result<(), String> {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    with_work_items(storage_root, |items| {
        if let Some(entry) = items
            .iter_mut()
            .rev()
            .find(|item| item.session_id == session_id)
        {
            entry.queued_at = Some(now);
            entry.after_work_id = Some(after_work_id.to_string());
            entry.cwd = cwd.map(str::to_string);
        }
    })
}

/// 清除排队标记（依赖终态投递或工作终态时调用），落盘。
pub fn clear_work_item_queued(
    storage_root: &std::path::Path,
    session_id: &str,
) -> Result<(), String> {
    with_work_items(storage_root, |items| {
        if let Some(entry) = items
            .iter_mut()
            .rev()
            .find(|item| item.session_id == session_id)
        {
            entry.queued_at.take();
            entry.after_work_id.take();
        }
    })
}

/// 读取排队挂起的登记（重启重放投递的数据源）。
pub fn queued_work_items(storage_root: &std::path::Path) -> Vec<WorkItemRecord> {
    load_work_items(storage_root)
        .into_iter()
        .filter(|item| item.queued_at.is_some() && item.finished_at.is_none())
        .collect()
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
        } else if item.queued_at.is_some() {
            // 排队挂起（依赖未终态/写入锁）：恢复为 queued，由宿主重放投递
            // ——从未执行的工作不得显示为「已中断」（review P2）。
            WorkState::Queued
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
            execution_id: item.execution_id,
            pending_question: None,
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
pub fn upsert_work_item(
    storage_root: &std::path::Path,
    record: &WorkItemRecord,
) -> Result<(), String> {
    with_work_items(storage_root, |items| {
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
    })
}

/// 更新会话登记的当前执行归属（R0 契约，§5.4）。
///
/// 宿主在投递成功后调用：插件侧派发落盘已带 execution_id（幂等无害）；
/// 恢复 / 排队重放由宿主构造请求，经此把新 execution_id 写回登记，
/// 后续事件（R5：事件日志按登记执行归属）与终态日志
/// 才能归属到本次执行而非历史执行。
pub fn set_work_item_execution(
    storage_root: &std::path::Path,
    session_id: &str,
    execution_id: &str,
) -> Result<(), String> {
    with_work_items(storage_root, |items| {
        if let Some(entry) = items
            .iter_mut()
            .rev()
            .find(|item| item.session_id == session_id)
        {
            entry.execution_id = Some(execution_id.to_string());
        }
    })
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
pub fn add_confirmed_workspace(
    storage_root: &std::path::Path,
    path: &std::path::Path,
) -> Result<(), String> {
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
    let text = match serde_json::to_string_pretty(
        &list
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<String>>(),
    ) {
        Ok(text) => text,
        Err(error) => return Err(format!("已确认工作区序列化失败: {error}")),
    };
    // 事务化原子写（R2：随机后缀 + 写锁 + fsync；Z4：失败显式返回）
    atomic_write_bytes(&path, text.as_bytes())
        .map_err(|error| format!("已确认工作区落盘失败（数据未更新）: {error}"))
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
        Ok(content) => parse_snapshot(&content).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %path.display(), "目标记录损坏，按空表处理");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// 目标登记（active 上限 10：久远的 inactive 目标自动归档移除）。
pub fn save_goals(storage_root: &std::path::Path, goals: &[GoalRecord]) -> Result<(), String> {
    let path = goals_path(storage_root);
    let text = serialize_snapshot(goals).map_err(|error| format!("目标序列化失败: {error}"))?;
    atomic_write_bytes(&path, text.as_bytes())
        .map_err(|error| format!("目标落盘失败（数据未更新）: {error}"))
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
        Ok(content) => parse_snapshot(&content).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %path.display(), "报告存档损坏，按空表处理");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// 报告存档（保留最近 50 份）。
pub fn save_reports(
    storage_root: &std::path::Path,
    reports: &[ReportRecord],
) -> Result<(), String> {
    let path = reports_path(storage_root);
    let text = serialize_snapshot(reports).map_err(|error| format!("报告序列化失败: {error}"))?;
    atomic_write_bytes(&path, text.as_bytes())
        .map_err(|error| format!("报告落盘失败（数据未更新）: {error}"))
}

/// 路径规范化（canonicalize 失败退回原样，比较仍可进行）。
pub fn canonical_path_string(path: &str) -> String {
    std::path::Path::new(path.trim())
        .canonicalize()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.trim().to_string())
}

/// canonicalize，符号链接/尾斜杠不产生绕过）。
///
/// R28-8（第 28 轮 review S3，探针 PR-09）：空默认工作区不得成为空前缀
/// 放行任意绝对路径——默认子树放行仅在默认路径非空且真实存在时成立
/// （fail-closed：授权未知 ≠ 默认允许）。空 target 的放行保持：调用方
/// 在最终 cwd 非空且 is_dir 校验之后到达此函数（dispatch 规划段），
/// 「使用默认目录」的空目标由上游解析。
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
    let trimmed_default = default_workspace.trim();
    let default_usable =
        !trimmed_default.is_empty() && std::path::Path::new(trimmed_default).is_dir();
    if default_usable {
        let canonical_default = canonical_path_string(default_workspace);
        // 默认工作区子树：canonical 前缀 + '/' 边界（防 /a-b 误匹配 /a）
        if canonical_target == canonical_default
            || canonical_target
                .strip_prefix(&canonical_default)
                .is_some_and(|rest| rest.starts_with('/'))
        {
            return true;
        }
    }
    load_confirmed_workspaces(storage_root)
        .iter()
        .any(|confirmed| {
            let confirmed = confirmed.to_string_lossy();
            confirmed == canonical_target || confirmed == target
        })
}
