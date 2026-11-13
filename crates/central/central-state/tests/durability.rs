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
                save_work_items(&base, &items);
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let text = std::fs::read_to_string(root.path().join("central").join("work-items.json"))
        .expect("最终文件必须存在");
    let parsed: Vec<WorkItemRecord> =
        serde_json::from_str(&text).expect("并发写后文件必须可完整解析（无半文件/交错）");
    assert_eq!(parsed.len(), 1, "最后一次写入的完整内容");
}

/// 正常写入不残留 .tmp（随机后缀 tmp 在 rename 后消失）。
#[test]
fn normal_write_leaves_no_tmp_residue() {
    let root = tempfile::tempdir().unwrap();
    upsert_work_item(root.path(), &record("s-clean", "任务"));
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
    upsert_work_item(root.path(), &record("s-rt", "往返任务"));
    let items = central_state::load_work_items(root.path());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].session_id, "s-rt");
    assert_eq!(items[0].task, "往返任务");
}
