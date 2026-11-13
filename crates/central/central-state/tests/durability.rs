//! R2 持久化事务加固的行为锁定（方案 §5.5 / 8.2）：
//! 随机后缀临时文件、进程内写锁、崩溃后无半文件、无固定 .tmp 残留。

use central_state::persistence::{save_work_items, upsert_work_item, WorkItemRecord};
use std::sync::Arc;

fn record(session: &str, task: &str) -> WorkItemRecord {
    WorkItemRecord {
        work_id: format!("work-{session}"),
        reused_session: false,
        session_id: session.to_string(),
        task: task.to_string(),
        model: None,
        dispatched_at: "2026-11-13 10:00:00".to_string(),
        finished_at: None,
        outcome: None,
        events: Vec::new(),
        queued_at: None,
        after_work_id: None,
        cwd: None,
        execution_id: None,
    }
}

/// 并发写：最终文件始终是某一次完整写入，可解析、无交错损坏。
#[test]
fn concurrent_writes_keep_file_consistent() {
    let root = tempfile::tempdir().unwrap();
    let base = Arc::new(root.path().to_path_buf());
    let mut handles = Vec::new();
    for thread in 0..8 {
        let base = Arc::clone(&base);
        handles.push(std::thread::spawn(move || {
            for i in 0..20 {
                let items = vec![record(
                    &format!("s-{thread}-{i}"),
                    &format!("任务 {thread}-{i}"),
                )];
                if let Err(error) = save_work_items(&base, &items) {
                    tracing::warn!(%error, "测试（Z4：写失败显式可见）");
                }
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    // envelope 格式经 load_work_items 读回（完整性 = 可完整解析且内容为最后写入）
    let parsed = central_state::load_work_items(root.path());
    assert_eq!(parsed.len(), 1, "最后一次写入的完整内容");
}

/// R5 envelope：新写入带 schema 版本包裹；旧裸数组文件不迁移即读。
#[test]
fn snapshot_envelope_round_trips_and_reads_legacy() {
    let root = tempfile::tempdir().unwrap();
    // 新写：envelope（含 schema_version）
    if let Err(error) = save_work_items(root.path(), &[record("s-env", "envelope 任务")]) {
        tracing::warn!(%error, "测试（Z4：写失败显式可见）");
    }
    let raw = std::fs::read_to_string(root.path().join("central").join("work-items.json")).unwrap();
    assert!(
        raw.contains("\"schema_version\""),
        "新写入必须是 envelope：{raw}"
    );
    let parsed = central_state::load_work_items(root.path());
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].task, "envelope 任务");

    // 旧格式（R5 前裸数组）直接可读
    let legacy = tempfile::tempdir().unwrap();
    let legacy_dir = legacy.path().join("central");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::write(
        legacy_dir.join("work-items.json"),
        serde_json::to_string(&vec![record("s-old", "旧格式任务")]).unwrap(),
    )
    .unwrap();
    let parsed = central_state::load_work_items(legacy.path());
    assert_eq!(parsed.len(), 1, "裸数组旧文件必须兼容读取");
    assert_eq!(parsed[0].session_id, "s-old");
    assert_eq!(parsed[0].task, "旧格式任务");
}

/// 正常写入不残留 .tmp（随机后缀 tmp 在 rename 后消失）。
#[test]
fn normal_write_leaves_no_tmp_residue() {
    let root = tempfile::tempdir().unwrap();
    if let Err(error) = upsert_work_item(root.path(), &record("s-clean", "任务")) {
        tracing::warn!(%error, "测试（Z4：写失败显式可见）");
    }
    let central_dir = root.path().join("central");
    let leftovers: Vec<_> = std::fs::read_dir(&central_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "正常路径不得残留临时文件");
}

/// 写后读一致：upsert 立即可读回（写锁内完成 rename）。
#[test]
fn write_then_read_round_trips() {
    let root = tempfile::tempdir().unwrap();
    if let Err(error) = upsert_work_item(root.path(), &record("s-rt", "往返任务")) {
        tracing::warn!(%error, "测试（Z4：写失败显式可见）");
    }
    let items = central_state::load_work_items(root.path());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].session_id, "s-rt");
    assert_eq!(items[0].task, "往返任务");
}

/// Z4 具名锁：生产形态 RMW 并发——两线程并发 upsert **不同会话**，
/// 两行都必须保留（此前 load-modify-save 在锁外，后写者的 load 看不到
/// 前写者的 save，整表覆盖丢一行；with_work_items 后串行化不丢）。
#[test]
fn concurrent_upserts_keep_both_sessions() {
    let root = tempfile::tempdir().unwrap();
    let base = Arc::new(root.path().to_path_buf());
    let mut handles = Vec::new();
    // 4 线程 × 4 次 = 16 会话（< 保留上限 20，避开 drain 裁剪干扰）
    for t in 0..4 {
        let base = Arc::clone(&base);
        handles.push(std::thread::spawn(move || {
            for i in 0..4 {
                let record_item = record(&format!("s-{t}-{i}"), &format!("任务 {t}-{i}"));
                let _ = upsert_work_item(&base, &record_item);
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let items = central_state::load_work_items(root.path());
    let sessions: std::collections::HashSet<_> =
        items.iter().map(|item| item.session_id.clone()).collect();
    assert_eq!(
        sessions.len(),
        items.len(),
        "每个会话恰好一条（会话维度 upsert）"
    );
    assert_eq!(
        sessions.len(),
        16,
        "并发 upsert 不丢会话（实际 {} 条；锁外 RMW 时代会互相覆盖丢行）",
        sessions.len()
    );
}

/// Z4 写失败诚实性：save/upsert 返回 Result（调用方可感知），
/// 正常路径 Ok。
#[test]
fn write_apis_report_result() {
    let root = tempfile::tempdir().unwrap();
    assert!(upsert_work_item(root.path(), &record("s-ok", "任务")).is_ok());
    assert!(
        central_state::persistence::save_work_items(root.path(), &[]).is_ok(),
        "空表落盘也应 Ok"
    );
}
