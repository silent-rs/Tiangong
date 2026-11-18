//! 中枢事件日志接线（R2/R5）：宿主关键节点写入追加式事件日志
//! `central/events.jsonl`（`tiangong_central::EventLog`，唯一事实源）。
//! 旧内嵌时间线已停止写入（R5 双写消除），恢复时一次性迁移导入。

use tiangong_central::event::WorkEventType as E;
use tiangong_central::{EventLog, WorkEvent};

/// 事件日志路径：`{storage_root}/central/events.jsonl`。
pub fn event_log_path(storage_root: &std::path::Path) -> std::path::PathBuf {
    storage_root.join("central").join("events.jsonl")
}

/// 记录一个中枢事件（派发/终态/等待用户/恢复等）。
///
/// 打开失败或追加失败记 warn——事件日志是事实源，写失败必须显式可见；
/// 不阻塞主链路（调用方语义与旧时间线一致）。
pub fn record_central_event(
    storage_root: &std::path::Path,
    work_item_id: &str,
    execution_id: &str,
    worker_session_id: &str,
    event_type: E,
    note: &str,
) {
    let log = match EventLog::open(&event_log_path(storage_root)) {
        Ok(log) => log,
        Err(error) => {
            tracing::warn!(%error, "中枢事件日志打开失败（事实未入日志）");
            return;
        }
    };
    let mut event = WorkEvent::new(work_item_id, event_type)
        .with_execution(execution_id)
        .with_worker_session(worker_session_id);
    if !note.is_empty() {
        event.payload = serde_json::json!({ "note": note });
    }
    if let Err(error) = log.append(event) {
        tracing::warn!(%error, work_item_id, "中枢事件日志追加失败（事实未入日志）");
    }
}

/// 旧时间线一次性迁移（R2，方案「旧 JSON 到新 schema 的一次性迁移」）。
///
/// 事件日志为空且 work-items.json 各记录含 `events` 时间线时，把旧事件
/// 逐条转入事件日志（event_id 重生成 scru128；execution_id 取记录当前值；
/// 原始 kind/note 保真进 payload）。幂等：日志非空即跳过。
pub fn migrate_legacy_timeline(storage_root: &std::path::Path) {
    let log = match EventLog::open(&event_log_path(storage_root)) {
        Ok(log) => log,
        Err(error) => {
            tracing::warn!(%error, "中枢事件日志打开失败，跳过旧时间线迁移");
            return;
        }
    };
    match log.last_offset() {
        Ok(offset) if offset > 0 => return, // 已有事实，不重复迁移
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "中枢事件日志读取失败，跳过旧时间线迁移");
            return;
        }
    }
    let records = crate::load_work_items(storage_root);
    let mut migrated = 0usize;
    for record in &records {
        for legacy in &record.events {
            let event_type = match legacy.kind.as_str() {
                "dispatched" => E::WorkItemStarted,
                "waiting_user" => E::WorkItemWaitingUser,
                "resumed" => E::WorkItemStarted,
                "finished" => {
                    if legacy.note.contains("失败") {
                        E::WorkItemFailed
                    } else {
                        E::WorkItemCompleted
                    }
                }
                _ => E::WorkItemProgress,
            };
            let mut event =
                WorkEvent::new(&record.work_id, event_type).with_worker_session(&record.session_id);
            if let Some(execution_id) = record.execution_id.as_deref() {
                event = event.with_execution(execution_id);
            }
            // 登记字段随事件入 payload（review P1-2：迁移后投影重建的
            // 数据源——缺这些字段会导致任务名空/模型默认/cwd 丢失）
            event.payload = serde_json::json!({
                "legacy_kind": legacy.kind,
                "note": legacy.note,
                "at": legacy.at,
                "task": record.task,
                "model": record.model,
                "cwd": record.cwd,
                "reused_session": record.reused_session,
                "dispatched_at": record.dispatched_at,
            });
            if log.append(event).is_ok() {
                migrated += 1;
            }
        }
    }
    if migrated > 0 {
        tracing::info!(
            migrated,
            "旧工作项时间线已迁移至中枢事件日志（一次性，幂等）"
        );
    }
}

/// 便捷入口：从宿主状态取 storage_root 记录事件。
pub fn record_for(
    state: &crate::CentralHostState,
    work_item_id: &str,
    execution_id: &str,
    worker_session_id: &str,
    event_type: E,
    note: &str,
) {
    record_central_event(
        state.core_manager.storage_root(),
        work_item_id,
        execution_id,
        worker_session_id,
        event_type,
        note,
    );
}

/// 按会话归属记录事件（R5 单写：事件日志是唯一事实源）。
///
/// 从落盘登记按 `session_id` 解析 work_id 与当前 execution_id，再写入
/// 事件日志——取代旧的 `append_work_item_event`（work-items.json 内嵌
/// 时间线的双写路径已删除）。
pub fn record_by_session(
    storage_root: &std::path::Path,
    session_id: &str,
    event_type: E,
    note: &str,
) {
    let record = crate::load_work_items(storage_root)
        .into_iter()
        .rev()
        .find(|item| item.session_id == session_id);
    let Some(record) = record else {
        tracing::warn!(
            session_id,
            event = event_type.as_str(),
            "事件归属会话无登记，事实未入日志"
        );
        return;
    };
    record_central_event(
        storage_root,
        &record.work_id,
        record.execution_id.as_deref().unwrap_or(""),
        session_id,
        event_type,
        note,
    );
}

/// 读一个工作项的事件时间线（R5：事件日志优先，旧时间线兼容读）。
///
/// 返回 (at, kind, note, execution_id) 列表；事件日志为空时回退读取
/// work-items.json 内嵌时间线（R5 前的旧数据，恢复时 migrate_legacy_timeline
/// 已导入，此处兜底极端时序）。
/// U1（第 28 轮 review）：第 4 元素携带 execution_id——前端步骤链按
/// 当前执行过滤，旧执行的 failed/cancelled 不污染新执行（legacy 事件
/// 无 execution 返回空串，前端按「全量」兼容）。
pub fn work_timeline(
    storage_root: &std::path::Path,
    work_item_id: &str,
) -> Vec<(String, String, String, String)> {
    let mut timeline = Vec::new();
    if let Ok(log) = EventLog::open(&event_log_path(storage_root)) {
        if let Ok(entries) = log.load_all() {
            for logged in entries {
                if logged.event.work_item_id == work_item_id {
                    let note = logged
                        .event
                        .payload
                        .get("note")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    timeline.push((
                        logged.event.created_at.clone(),
                        logged.event.event_type.as_str().to_string(),
                        note.to_string(),
                        logged.event.execution_id.clone().unwrap_or_default(),
                    ));
                }
            }
        }
    }
    if !timeline.is_empty() {
        return timeline;
    }
    crate::load_work_items(storage_root)
        .into_iter()
        .rev()
        .find(|item| item.work_id == work_item_id)
        .map(|item| {
            item.events
                .iter()
                .map(|e| {
                    (
                        e.at.clone(),
                        e.kind.clone(),
                        e.note.clone(),
                        // 旧内嵌时间线无执行归属：取记录当前 execution_id
                        //（R0 前旧数据可能为空——前端按全量兼容）
                        item.execution_id.clone().unwrap_or_default(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 事件 payload 的 reused_session 标记（迁移补登记字段后恒存在；缺省
/// 视为 owner 派发）。
fn payload_is_reused(logged: &tiangong_central::LoggedEvent) -> bool {
    logged
        .event
        .payload
        .get("reused_session")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 从事件日志投影重建登记（R5：registry 由事件重放，消除第二事实源）。
///
/// 每个 work_item：最后一次 `work_item.started` 的 payload 提供登记字段
/// （task/model/cwd/reused_session/execution_id），状态由
/// [`tiangong_central::project_work_state`] 投影。日志为空返回 None
/// （调用方回退旧路径 work-items.json——R5 前数据兼容）。
pub fn rebuild_registry_from_log(
    storage_root: &std::path::Path,
) -> Option<Vec<crate::CentralWorkStatus>> {
    let log = EventLog::open(&event_log_path(storage_root)).ok()?;
    // R5 对账：宽松读——中段损坏行跳过并 warn（丢失可观测），恢复以
    // 幸存事件为准；open 失败（含尾部修复失败）或读取锁失败回退快照。
    let (entries, _skipped) = log
        .load_all_lenient()
        .inspect_err(|error| tracing::warn!(%error, "读取中枢日志失败，回退工作快照"))
        .ok()?;
    if entries.is_empty() {
        return None;
    }
    let states = tiangong_central::project_work_state(&entries);
    let mut registry: Vec<crate::CentralWorkStatus> = Vec::new();
    // 倒序遍历：同会话首条命中即最后一次派发（P1-2 修复）；最终输出
    // 在函数末尾恢复 dispatched_at 升序（Z1 顺序不变量）
    for logged in entries.iter().rev() {
        if logged.event.event_type != tiangong_central::event::WorkEventType::WorkItemStarted {
            continue;
        }
        let work_id = &logged.event.work_item_id;
        // 会话维度保留最新 Started；只有显式 owner_work_id 或同 execution
        // 能证明异 id 引导关系时，才让较早 owner 接管。仅 reused=true
        // 不足以证明引导（可能是上一工作终态后复用会话做新工作）。
        let current_session = logged.event.worker_session_id.as_deref().unwrap_or("");
        let current_is_owner = !payload_is_reused(logged);
        if let Some(position) = registry
            .iter()
            .position(|entry| entry.session_id == current_session)
        {
            let existing = &registry[position];
            let existing_is_owner = !existing.reused_session;
            // R29-3：最新 Started 默认拥有会话。仅显式 owner 关联或共用
            // 同一 execution 才证明是引导延续；reused/在途都不能证明归属。
            // 同 work 的新执行保留最新字段，不能退回旧 owner execution。
            let linked_steer = entries
                .iter()
                .rev()
                .find(|entry| {
                    entry.event.event_type == E::WorkItemStarted
                        && entry.event.work_item_id == existing.work_id
                        && entry.event.worker_session_id.as_deref() == Some(current_session)
                })
                .is_some_and(|entry| {
                    entry
                        .event
                        .payload
                        .get("owner_work_id")
                        .and_then(|v| v.as_str())
                        == Some(work_id)
                        || (entry.event.execution_id.is_some()
                            && entry.event.execution_id == logged.event.execution_id)
                });
            if existing.work_id == *work_id
                || existing_is_owner
                || !current_is_owner
                || !linked_steer
            {
                continue;
            }
            registry.remove(position);
        }
        let Some(status) = states.get(work_id) else {
            continue;
        };
        let payload = &logged.event.payload;
        let session_id = logged.event.worker_session_id.clone().unwrap_or_default();
        registry.push(crate::CentralWorkStatus {
            work_id: work_id.clone(),
            session_id: session_id.clone(),
            task: payload
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            model: payload
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| Some("路由默认".to_string())),
            dispatched_at: payload
                .get("dispatched_at")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .unwrap_or(&logged.event.created_at)
                .to_string(),
            state: project_status_to_work_state(*status),
            reused_session: payload
                .get("reused_session")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            last_activity: None,
            cwd: payload
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            execution_id: logged.event.execution_id.clone(),
            // G3：投影重建时若终态为 waiting_user，从事件 payload 恢复
            // pending_question（标记行后的问题主体，重启不丢待决问题）
            pending_question: if *status == tiangong_central::status::WorkItemStatus::WaitingUser {
                entries
                    .iter()
                    .rev()
                    .find(|entry| {
                        entry.event.work_item_id == *work_id
                            && entry.event.execution_id == logged.event.execution_id
                            && entry.event.event_type == E::WorkItemWaitingUser
                    })
                    .and_then(|entry| entry.event.payload.get("pending_question"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            } else {
                None
            },
        });
    }
    if registry.is_empty() {
        None
    } else {
        // 顺序不变量（review Z1）：输出按 dispatched_at 升序——运行时
        // upsert 的登记也是时间正序（尾部最新），retention_removals 的
        // 「按位置取最旧」语义在两种来源下一致（倒序输出曾使保留策略
        // 淘汰最新记录）。
        registry.sort_by(|a, b| a.dispatched_at.cmp(&b.dispatched_at));
        Some(registry)
    }
}

/// 契约状态（WorkItemStatus）→ 登记状态（WorkState）。
///
/// 映射决策：completed 事件 → Completed（与旧重建路径的 outcome 分组
/// 一致，徽章重启前后不变）；waiting_resource → Running（在途）；
/// blocked → Paused（R5 起 paused 事件以 blocked 类型承载）。
fn project_status_to_work_state(
    status: tiangong_central::status::WorkItemStatus,
) -> central_state::WorkState {
    use central_state::WorkState;
    use tiangong_central::status::WorkItemStatus as S;
    match status {
        S::Draft | S::Planned | S::Queued => WorkState::Queued,
        S::Running | S::WaitingResource => WorkState::Running,
        S::WaitingUser => WorkState::WaitingUser,
        S::Blocked => WorkState::Paused,
        S::Failed => WorkState::Failed,
        S::Completed => WorkState::Completed,
        S::Cancelled => WorkState::Cancelled,
        S::Paused => WorkState::Paused,
    }
}

/// 写入一个已构造好的事件（payload 由调用方定制，R5 派发事件携带
/// 登记字段供投影重建）。
pub fn record_event(storage_root: &std::path::Path, event: tiangong_central::WorkEvent) {
    let log = match EventLog::open(&event_log_path(storage_root)) {
        Ok(log) => log,
        Err(error) => {
            tracing::warn!(%error, "中枢事件日志打开失败（事实未入日志）");
            return;
        }
    };
    if let Err(error) = log.append(event) {
        tracing::warn!(%error, "中枢事件日志追加失败（事实未入日志）");
    }
}
