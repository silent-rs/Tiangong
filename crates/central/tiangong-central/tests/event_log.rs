//! event_log 领域测试（R7 测试集中：原 src/event_log.rs 内嵌测试迁出）。
use tiangong_central::event_log::{EventLog, LoggedEvent, project_work_state};
use tiangong_central::execution::CENTRAL_CONTRACT_SCHEMA_VERSION;

// 原 mod tests
use tiangong_central::event::{WorkEvent, WorkEventType};
use tiangong_central::ids::new_id;

fn temp_log() -> (tempfile::TempDir, EventLog) {
    let dir = tempfile::tempdir().unwrap();
    let log = EventLog::open(&dir.path().join("events.jsonl")).unwrap();
    (dir, log)
}

fn event(work: &str, event_type: WorkEventType) -> WorkEvent {
    WorkEvent::new(work, event_type)
}

#[test]
fn append_assigns_increasing_offsets() {
    let (_dir, log) = temp_log();
    let a = log
        .append(event("w1", WorkEventType::WorkItemCreated))
        .unwrap();
    let b = log
        .append(event("w1", WorkEventType::WorkItemStarted))
        .unwrap();
    assert_eq!((a, b), (1, 2));
    assert_eq!(log.last_offset().unwrap(), 2);
}

/// 方案 8.2「重复 event」：同 id 重复追加幂等，不产生第二条事实。
#[test]
fn duplicate_append_is_idempotent() {
    let (_dir, log) = temp_log();
    let e = event("w1", WorkEventType::WorkItemStarted);
    let first = log.append(e.clone()).unwrap();
    let second = log.append(e).unwrap();
    assert_eq!(first, second, "重复追加返回既有 offset");
    assert_eq!(log.load_all().unwrap().len(), 1, "日志中只有一条");
}

/// 方案 8.2「snapshot 写一半进程退出」的事件日志侧：尾部残片截断恢复。
#[test]
fn torn_tail_line_is_repaired_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let log = EventLog::open(&path).unwrap();
    log.append(event("w1", WorkEventType::WorkItemStarted))
        .unwrap();
    log.append(event("w1", WorkEventType::WorkItemCompleted))
        .unwrap();
    drop(log);
    // 同进程共享注册表记住了「已扫描」——重置以模拟新进程崩溃恢复
    EventLog::force_repair_on_next_open(&path);
    // 模拟崩溃：追加半行 JSON（无换行、不完整）
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"offset\":3,\"schema_ver").unwrap();
    }
    let reopened = EventLog::open(&path).unwrap();
    let entries = reopened.load_all().unwrap();
    assert_eq!(entries.len(), 2, "残片被截断，完整事件保留");
    assert_eq!(reopened.last_offset().unwrap(), 2);
    // 修复后可继续追加
    let next = reopened
        .append(event("w2", WorkEventType::WorkItemCreated))
        .unwrap();
    assert_eq!(next, 3);
}

/// 方案 8.2「两线程并发 append」：无丢失、offset 唯一且有序。
/// Z5/Z8 生产形态：每次 `EventLog::open` 新实例（生产调用形态）——
/// 此前单 Arc 共享实例的「并发」测不到实例级锁无互斥的缺陷
/// （两实例交错 append / offset 重复），全局注册表锁后跨实例串行。
#[test]
fn concurrent_appends_do_not_lose_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let mut handles = Vec::new();
    for t in 0..4 {
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            let log = EventLog::open(&path).unwrap(); // 生产形态：新实例
            for i in 0..25 {
                let mut e = event(&format!("w{t}"), WorkEventType::WorkItemProgress);
                e.id = new_id();
                e.payload = serde_json::json!({ "i": i });
                log.append(e).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let verify = EventLog::open(&path).unwrap();
    let entries = verify.load_all().unwrap();
    assert_eq!(entries.len(), 100, "跨实例并发追加不丢事件");
    let offsets: Vec<u64> = entries.iter().map(|e| e.offset).collect();
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    assert_eq!(
        offsets, sorted,
        "offset 跨实例并发下仍须有序（十九轮 P2-2 恢复真断言）"
    );
    let unique: std::collections::HashSet<u64> = offsets.iter().copied().collect();
    assert_eq!(unique.len(), 100, "offset 全局唯一（实例级锁时代会重复）");
}

/// 十九轮 P2-1 具名锁：轮转写入-读回成对（口径 12）——归档 `.1`
/// 与当前文件拼接后历史完整、offset 递增连续。
#[test]
fn rotation_preserves_history_on_readback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let log = EventLog::open(&path).unwrap();
    for i in 0..5 {
        let mut e = event("w-rot", WorkEventType::WorkItemProgress);
        e.id = new_id();
        e.payload = serde_json::json!({ "i": i });
        log.append(e).unwrap();
    }
    // 模拟轮转：手工归档当前文件（绕过 100MB 阈值），再写新文件
    let archived = path.with_extension("jsonl.1");
    std::fs::rename(&path, &archived).unwrap();
    let reopened = EventLog::open(&path).unwrap();
    reopened
        .append(event("w-post-rot", WorkEventType::WorkItemStarted))
        .unwrap();
    // 读回：历史 5 条 + 新 1 条，offset 连续递增
    let entries = reopened.load_all().unwrap();
    assert_eq!(entries.len(), 6, "归档 + 当前拼接，历史不丢（P2-1）");
    let offsets: Vec<u64> = entries.iter().map(|e| e.offset).collect();
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    assert_eq!(offsets, sorted, "归档段 offset 先于当前段");
    assert!(entries.iter().any(|e| e.event.work_item_id == "w-post-rot"));
}

/// 十九轮 P2-3：重复 event id 行跳过（不截断其后内容）。
#[test]
fn duplicate_id_line_is_skipped_not_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let log = EventLog::open(&path).unwrap();
    let first = event("w-dup", WorkEventType::WorkItemStarted);
    let first_id = first.id.clone();
    log.append(first).unwrap();
    log.append(event("w-dup", WorkEventType::WorkItemCompleted))
        .unwrap();
    drop(log);
    // 追加一行重复 id 的完整 JSON（模拟轮转后历史重导入）
    {
        use std::io::Write as _;
        let dup = format!(
            "{{\"offset\":99,\"schema_version\":1,\"event\":{{\"id\":\"{first_id}\",\"work_item_id\":\"w-dup\",\"event_type\":\"work_item.started\",\"severity\":\"normal\",\"requires_user_action\":false,\"payload\":null,\"created_at\":\"2026-11-13 00:00:00\"}}}}\n"
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(dup.as_bytes()).unwrap();
    }
    EventLog::force_repair_on_next_open(&path);
    let reopened = EventLog::open(&path).unwrap();
    // 重复行被跳过，其后内容（此处无后续）不截断；完整事件保留
    let entries = reopened.load_all().unwrap();
    assert_eq!(
        entries.len(),
        3,
        "重复 id 行保留在日志中（跳过语义而非截断）"
    );
}

/// 投影：事件序列重建状态；终态吸收迟到事件。
#[test]
fn projection_rebuilds_states_and_terminal_absorbs_late_events() {
    let (_dir, log) = temp_log();
    log.append(event("w1", WorkEventType::WorkItemCreated))
        .unwrap();
    log.append(event("w1", WorkEventType::WorkItemStarted))
        .unwrap();
    log.append(event("w1", WorkEventType::WorkItemCompleted))
        .unwrap();
    // 迟到的 progress（终态后到达）
    log.append(event("w1", WorkEventType::WorkItemProgress))
        .unwrap();
    log.append(event("w2", WorkEventType::WorkItemStarted))
        .unwrap();

    let states = project_work_state(&log.load_all().unwrap());
    use tiangong_central::status::WorkItemStatus as S;
    assert_eq!(states.get("w1"), Some(&S::Completed), "终态吸收迟到事件");
    assert_eq!(states.get("w2"), Some(&S::Running));
}

/// schema 版本随事件写入（向前迁移依据）。
#[test]
fn logged_events_carry_schema_version() {
    let (_dir, log) = temp_log();
    log.append(event("w1", WorkEventType::WorkItemStarted))
        .unwrap();
    let entries = log.load_all().unwrap();
    assert_eq!(entries[0].schema_version, CENTRAL_CONTRACT_SCHEMA_VERSION);
}

/// load_all_lenient：坏行跳过计数、空文件、文件不存在三态。
#[test]
fn lenient_read_skips_corrupt_lines_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    // 文件不存在 → 空且 0 跳过
    let missing = EventLog::open(&path).unwrap();
    let (empty_entries, empty_skipped) = missing.load_all_lenient();
    assert!(empty_entries.is_empty());
    assert_eq!(empty_skipped, 0);
    // 1 好 + 1 坏 + 空行 + 1 好 → 2 条 + 1 跳过
    let good = |work: &str, offset: u64| {
        serde_json::json!({
            "offset": offset, "schema_version": 1,
            "event": {
                "id": format!("ev-l{offset}"), "work_item_id": work,
                "event_type": "work_item.started", "severity": "normal",
                "requires_user_action": false, "payload": null,
                "created_at": "2026-11-16 10:00:00"
            }
        })
        .to_string()
    };
    std::fs::write(
        &path,
        format!(
            "{}\n{{broken json\n\n{}\n",
            good("w-lenient", 0),
            good("w-lenient", 1)
        ),
    )
    .unwrap();
    let log = EventLog::open(&path).unwrap();
    let (entries, skipped) = log.load_all_lenient();
    assert_eq!(entries.len(), 2, "幸存事件为准");
    assert_eq!(skipped, 1, "坏行计入 skipped");
    assert_eq!(entries[0].offset, 0);
    assert_eq!(entries[1].offset, 1);
}

/// project_work_state：各事件类型到工作状态的映射（§8.1 投影）。
#[test]
fn project_work_state_maps_event_kinds() {
    let make = |kind: WorkEventType| LoggedEvent {
        offset: 0,
        schema_version: 1,
        event: {
            let mut e = event("w-map", kind);
            e.id = new_id();
            e
        },
    };
    use tiangong_central::status::WorkItemStatus as S;
    let cases: Vec<(WorkEventType, Option<S>)> = vec![
        (WorkEventType::WorkItemStarted, Some(S::Running)),
        (WorkEventType::WorkItemProgress, Some(S::Running)),
        (WorkEventType::WorkItemWaitingUser, Some(S::WaitingUser)),
        (
            WorkEventType::WorkItemWaitingResource,
            Some(S::WaitingResource),
        ),
        (WorkEventType::WorkItemBlocked, Some(S::Blocked)),
        (WorkEventType::WorkItemFailed, Some(S::Failed)),
        (WorkEventType::WorkItemCompleted, Some(S::Completed)),
        (WorkEventType::WorkItemCancelled, Some(S::Cancelled)),
        (WorkEventType::ReportReady, None), // 非工作项事件不映射
    ];
    for (kind, expected) in cases {
        let states = project_work_state(&[make(kind)]);
        match expected {
            Some(want) => assert_eq!(
                states.get("w-map"),
                Some(&want),
                "{:?} 应投影为 {want:?}",
                kind.as_str()
            ),
            None => assert!(
                !states.contains_key("w-map"),
                "{:?} 不应产生状态",
                kind.as_str()
            ),
        }
    }
}

/// 跨实例 append 的 offset 连续性（二十一轮 review CC3 注释诚实化）：
/// 同进程内每路径共享 cache（Z5 注册表），第二实例命中共享 cache 不
/// 重扫——本测试锁定的是「无论实例边界，offset 不与既有行冲突」；
/// loaded=false 的真实重扫分支由每个**新路径**日志的首次 append
/// 覆盖（见 append_assigns_increasing_offsets / duplicate_* 系列，
/// 均在新 tempdir 路径上首 append）。跨进程场景不适用（单实例桌面
/// 应用，events.jsonl 无跨进程并发写）。
#[test]
fn second_instance_append_keeps_offsets_continuous() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let first = EventLog::open(&path).unwrap();
    first
        .append(event("w-x1", WorkEventType::WorkItemStarted))
        .unwrap();
    // 新实例（独立 cache）：首次 append 必须先读回文件补 offset，
    // 否则 offset 从 0 重写会与既有行冲突
    let second = EventLog::open(&path).unwrap();
    second
        .append(event("w-x2", WorkEventType::WorkItemStarted))
        .unwrap();
    let entries = second.load_all().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[1].offset,
        entries[0].offset + 1,
        "跨实例 offset 必须连续（不与既有行冲突）"
    );
    // 全量严格递增（防共享 cache 与文件状态分叉）
    for pair in entries.windows(2) {
        assert!(pair[1].offset > pair[0].offset, "offset 应严格递增");
    }
}

// ── 二十一轮 review CC3 跨进程实测（三进程并发 append 的 offset 唯一性）──

/// 子进程模式：env 携带日志路径时 append 一条并退出；正常测试运行时 no-op。
#[test]
fn multiprocess_child_append() {
    let Ok(log_path) = std::env::var("CENTRAL_MP_LOG") else {
        return;
    };
    let log = EventLog::open(std::path::Path::new(&log_path)).expect("子进程打开日志");
    let tag = std::env::var("CENTRAL_MP_TAG").unwrap_or_default();
    let mut event = event("w-mp", WorkEventType::WorkItemProgress);
    event.id = if std::env::var("CENTRAL_MP_DUP_ID").is_ok() {
        "ev-mp-dup".to_string()
    } else {
        format!("ev-mp-{tag}")
    };
    event.payload = serde_json::json!({ "tag": tag });
    log.append(event).expect("子进程 append");
    // 立即退出（不跑后续测试——父进程用 --exact 只选本测试）
    std::process::exit(0);
}

/// 三个独立进程并发 append 同一日志：offset 必须互异且严格递增，
/// 行不交错（review 实测曾三进程全为 offset=1——进程内 cache 假设被证伪，
/// 修复为 flock + 末行实况基准）。
#[test]
fn multiprocess_offsets_are_unique_and_ordered() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let exe = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for tag in ["a", "b", "c"] {
        children.push(
            std::process::Command::new(&exe)
                .arg("--exact")
                .arg("multiprocess_child_append")
                .arg("--nocapture")
                .env("CENTRAL_MP_LOG", &log_path)
                .env("CENTRAL_MP_TAG", tag)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn 子进程"),
        );
    }
    for mut child in children {
        let status = child.wait().expect("等待子进程");
        assert!(status.success(), "子进程应成功退出：{status:?}");
    }
    let log = EventLog::open(&log_path).unwrap();
    let entries = log.load_all().unwrap();
    assert_eq!(entries.len(), 3, "三进程各一条：{entries:?}");
    let offsets: Vec<u64> = entries.iter().map(|e| e.offset).collect();
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 3, "offset 必须互异：{offsets:?}");
    for pair in entries.windows(2) {
        assert!(
            pair[1].offset > pair[0].offset,
            "按行序严格递增：{offsets:?}"
        );
    }
    // 行完整（无交错损坏）：每行可独立解析（load_all 已验证）✓
}

// ── 第 28 轮 review 日志链 L1–L5 验收（R28-3~6 + 跨进程幂等）──

/// L1（R28-3，探针 PR-02）：任意长事件（≥8KB / 64KB / Unicode 多行
/// payload）后模拟新进程 append——offset 必须衔接末行严格递增（旧
/// 4096 字节固定窗口找不到行首 '{' 退 0，重启后 offset 从 1 重来）。
#[test]
fn long_event_tail_offset_survives_restart() {
    for (label, payload_size) in [("8kb", 8 * 1024), ("64kb", 64 * 1024)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = EventLog::open(&path).unwrap();
        let mut long_event = event(&format!("w-long-{label}"), WorkEventType::WorkItemStarted);
        long_event.id = format!("ev-long-{label}");
        // 单行 JSON：任务全文型 payload（含中文与引号转义，JSON 仍单行）
        long_event.payload = serde_json::json!({
            "task": "长任务".repeat(payload_size / 12),
            "note": "unicode 中文 ×2"
        });
        let first = log.append(long_event).unwrap();
        assert_eq!(first, 1);
        drop(log);
        // 模拟新进程：重置共享 cache + 全新实例
        EventLog::force_repair_on_next_open(&path);
        let reopened = EventLog::open(&path).unwrap();
        let next = reopened
            .append(event(
                &format!("w-next-{label}"),
                WorkEventType::WorkItemProgress,
            ))
            .unwrap();
        assert_eq!(
            next, 2,
            "{label} 长末行后新进程 append 必须衔接（offset==2）"
        );
        let entries = reopened.load_all().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[1].offset > entries[0].offset);
    }
}

/// L2（R28-4，探针 PR-05）：轮转全生命周期——触发轮转的事件必须位于
/// 约定当前文件（当前路径存在），结束后新进程写入 offset 连续，归档
/// + 当前 id 不重复、不漏触发事件。
#[test]
fn rotation_lifecycle_keeps_current_file_and_offset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    // 小阈值构造（生产 100MB；测试 1KB 足够触发）
    let log = EventLog::open_with_rotate_threshold(&path, 1024).unwrap();
    let mut first = event("w-rot", WorkEventType::WorkItemStarted);
    first.id = "ev-rot-1".into();
    first.payload = serde_json::json!({ "pad": "x".repeat(2048) });
    let off1 = log.append(first).unwrap();
    assert_eq!(off1, 1);
    // 触发轮转的第二条：必须进入新当前文件，当前路径必须存在
    let mut second = event("w-rot", WorkEventType::WorkItemCompleted);
    second.id = "ev-rot-2".into();
    second.payload = serde_json::json!({ "pad": "y".repeat(64) });
    let off2 = log.append(second).unwrap();
    assert_eq!(off2, 2, "轮转后 offset 必须连续（跨代际基准）");
    assert!(
        path.exists(),
        "轮转后当前路径必须存在（旧代码 rename 后向旧 fd 写）"
    );
    // 归档存在且含首条；当前含第二条
    let archived = path.with_extension("jsonl.1");
    assert!(archived.exists(), "归档应存在");
    let (entries, _) = {
        let check = EventLog::open(&path).unwrap();
        check.load_all_lenient()
    };
    let ids: Vec<&str> = entries.iter().map(|e| e.event.id.as_str()).collect();
    assert!(ids.contains(&"ev-rot-1"), "归档事实可读：{ids:?}");
    assert!(ids.contains(&"ev-rot-2"), "触发事件不丢：{ids:?}");
    // 模拟新进程：当前文件只有 1 条，offset 基准应从归档兜底衔接
    EventLog::force_repair_on_next_open(&path);
    let reopened = EventLog::open(&path).unwrap();
    let mut third = event("w-rot", WorkEventType::WorkItemProgress);
    third.id = "ev-rot-3".into();
    let off3 = reopened.append(third).unwrap();
    assert_eq!(
        off3, 3,
        "新进程在轮转后当前文件的 offset 必须从归档兜底衔接"
    );
}

/// L3（R28-5，探针 PR-06）：恢复宽松读取与严格读取覆盖相同历史——
/// 只有归档、当前空时归档事实不丢。
#[test]
fn lenient_read_includes_archived_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let log = EventLog::open(&path).unwrap();
    for i in 1..=3 {
        let mut e = event("w-arch", WorkEventType::WorkItemProgress);
        e.id = format!("ev-arch-{i}");
        log.append(e).unwrap();
    }
    drop(log);
    // 手工轮转：归档 3 条、当前空（模拟轮转瞬间的恢复场景）
    let archived = path.with_extension("jsonl.1");
    std::fs::rename(&path, &archived).unwrap();
    std::fs::write(&path, b"").unwrap();
    let reopened = EventLog::open(&path).unwrap();
    let (entries, skipped) = reopened.load_all_lenient();
    assert_eq!(skipped, 0);
    assert_eq!(
        entries.len(),
        3,
        "归档 3 条必须全部进入恢复投影（旧代码返回 0）"
    );
    // 严格读取一致
    let strict = reopened.load_all().unwrap();
    assert_eq!(strict.len(), 3, "严格/宽松读取覆盖相同文件范围");
}

/// L4（R28-6，探针 PR-07）：冷启动「好行→坏行→两条好行」——open 修复
/// 不得截掉中段坏行后的有效事件（旧代码只剩第一条好行）。
#[test]
fn cold_open_keeps_events_after_midfile_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    // 冷文件（不经 open 写入）：好→坏→好→好
    let good1 = serde_json::to_string(&LoggedEvent {
        offset: 1,
        schema_version: 1,
        event: {
            let mut e = event("w-cold", WorkEventType::WorkItemStarted);
            e.id = "ev-cold-1".into();
            e
        },
    })
    .unwrap();
    let bad = "{ this is not json at all !!!";
    let good2 = serde_json::to_string(&LoggedEvent {
        offset: 2,
        schema_version: 1,
        event: {
            let mut e = event("w-cold", WorkEventType::WorkItemCompleted);
            e.id = "ev-cold-2".into();
            e
        },
    })
    .unwrap();
    let good3 = serde_json::to_string(&LoggedEvent {
        offset: 3,
        schema_version: 1,
        event: {
            let mut e = event("w-cold2", WorkEventType::WorkItemStarted);
            e.id = "ev-cold-3".into();
            e
        },
    })
    .unwrap();
    std::fs::write(&path, format!("{good1}\n{bad}\n{good2}\n{good3}\n")).unwrap();
    // 冷启动 open：修复只允许截「末尾写一半」，中段坏行保留原文件
    let reopened = EventLog::open(&path).unwrap();
    let (entries, skipped) = reopened.load_all_lenient();
    assert_eq!(
        entries.len(),
        3,
        "坏行后的两条有效事件必须可见（旧代码被截掉）"
    );
    assert_eq!(skipped, 1, "中段坏行计数可观测");
    // 原文件保留损坏证据（不静默改写）
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("this is not json"), "损坏证据保留在原文件");
    // 继续追加 offset 衔接（以幸存最大有效 offset 为基准）
    let mut next = event("w-cold2", WorkEventType::WorkItemProgress);
    next.id = "ev-cold-4".into();
    let off = reopened.append(next).unwrap();
    assert_eq!(off, 4, "追加以幸存事件最大 offset 为基准");
}

/// L5（探针 PR-03）：两独立进程写相同 event id——最终只有一条事实，
/// 返回同一既有 offset（跨进程幂等；cache 不含其他进程写入）。
#[test]
fn multiprocess_duplicate_id_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("events.jsonl");
    let exe = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for tag in ["x", "y"] {
        children.push(
            std::process::Command::new(&exe)
                .arg("--exact")
                .arg("multiprocess_child_append")
                .arg("--nocapture")
                .env("CENTRAL_MP_LOG", &log_path)
                .env("CENTRAL_MP_TAG", tag)
                // 两进程写同一 id（旧代码：两条 offset 1、2）
                .env("CENTRAL_MP_DUP_ID", "1")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn 子进程"),
        );
    }
    for mut child in children {
        let status = child.wait().expect("等待子进程");
        assert!(status.success(), "子进程应成功退出：{status:?}");
    }
    let log = EventLog::open(&log_path).unwrap();
    let entries = log.load_all().unwrap();
    assert_eq!(entries.len(), 1, "同 id 跨进程只允许一条事实：{entries:?}");
}
