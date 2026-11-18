use std::fs::{OpenOptions, TryLockError};
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::mpsc;
use std::time::Duration;

use super::{EventLog, LoggedEvent};
use crate::event::{WorkEvent, WorkEventType};

const THRESHOLD: u64 = 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
const CHILD_LOG: &str = "CENTRAL_R29_8_CHILD_LOG";
const LOCK_BLOCKED: i32 = 73;

type ReadBarrier = (mpsc::Sender<()>, mpsc::Receiver<()>);
thread_local! {
    static READ_BARRIER: std::cell::RefCell<Option<ReadBarrier>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn after_archive() {
    if let Some((paused, resume)) = READ_BARRIER.take() {
        paused.send(()).unwrap();
        resume.recv_timeout(TIMEOUT).expect("reader 未收到放行信号");
    }
}

fn log_event(offset: u64, size: usize) -> WorkEvent {
    let mut event = WorkEvent::new("w-rotation", WorkEventType::WorkItemProgress);
    event.id = format!("ev-rotation-{offset}");
    event.payload = serde_json::json!({"pad": "x".repeat(size)});
    event
}

fn append_after_rotation(writer: &EventLog) {
    assert_eq!(writer.append(log_event(3, 0)).unwrap(), 3);
    assert_eq!(writer.append(log_event(4, 0)).unwrap(), 4);
}

fn assert_snapshot(entries: &[LoggedEvent], offsets: &[u64]) {
    let actual: Vec<_> = entries
        .iter()
        .map(|entry| (entry.offset, entry.event.id.clone()))
        .collect();
    let expected: Vec<_> = offsets
        .iter()
        .map(|offset| (*offset, format!("ev-rotation-{offset}")))
        .collect();
    assert_eq!(
        actual, expected,
        "归档与当前必须来自同一份快照（无漏代、无重复）"
    );
}

fn read(log: &EventLog, lenient: bool) -> Vec<LoggedEvent> {
    if lenient {
        let (entries, skipped) = log.load_all_lenient().unwrap();
        assert_eq!(skipped, 0);
        entries
    } else {
        log.load_all().unwrap()
    }
}

fn writer_can_lock(path: &Path) -> bool {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("jsonl.lock"))
        .unwrap();
    match file.try_lock() {
        Ok(()) => true, // fd 关闭后才能进入 append，以免测试自己重入加锁。
        Err(TryLockError::WouldBlock) => false,
        Err(error) => panic!("探测写锁失败: {error}"),
    }
}

fn child_writer(path: &Path) -> ExitStatus {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "event_log::tests::r29_8_rotation_child"])
        .env(CHILD_LOG, path)
        .output()
        .map(|output| {
            assert!(
                output.status.success() || output.status.code() == Some(LOCK_BLOCKED),
                "子进程失败: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            output.status
        })
        .unwrap()
}

/// 与集成测试的 multiprocess_child_append 相同：运行常规 suite 时为空操作。
#[test]
fn r29_8_rotation_child() {
    let Some(path) = std::env::var_os(CHILD_LOG) else {
        return;
    };
    let path = Path::new(&path);
    if !writer_can_lock(path) {
        std::process::exit(LOCK_BLOCKED);
    }
    let writer = EventLog::open_with_rotate_threshold(path, THRESHOLD).unwrap();
    append_after_rotation(&writer);
}

fn rotation_snapshot(lenient: bool, multiprocess: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let writer = EventLog::open_with_rotate_threshold(&path, THRESHOLD).unwrap();
    assert_eq!(writer.append(log_event(1, 2048)).unwrap(), 1);
    assert_eq!(writer.append(log_event(2, 2048)).unwrap(), 2);
    let reader = EventLog::open(&path).unwrap(); // 有独立实例/文件描述符。
    assert_snapshot(&read(&reader, lenient), &[1, 2]);

    // R29-8：按屏障固定「读完旧归档 1 → 写端尝试轮转 → 打开当前」的顺序。
    // 无共同锁时，写端先把 2 归档、留下当前 3/4，读者必得到 [1, 3, 4]。
    // 有锁时，非阻塞的独占探测必被拒绝；先放行 reader 完成，才让 writer
    // 提交轮转（否则测试自身死锁）。超时仅检测卡住，不决定交错顺序。
    let (paused, reached_archive) = mpsc::channel();
    let (resume, may_read_current) = mpsc::channel();
    let (entries, writer_blocked) = std::thread::scope(|scope| {
        let handle = scope.spawn(move || {
            READ_BARRIER.set(Some((paused, may_read_current)));
            read(&reader, lenient)
        });
        reached_archive.recv_timeout(TIMEOUT).unwrap();
        let writer_blocked = if multiprocess {
            child_writer(&path).code() == Some(LOCK_BLOCKED)
        } else if writer_can_lock(&path) {
            append_after_rotation(&writer);
            false
        } else {
            true
        };
        resume.send(()).unwrap();
        let entries = handle.join().unwrap();
        if writer_blocked {
            if multiprocess {
                assert!(child_writer(&path).success(), "释放读锁后应能轮转");
            } else {
                append_after_rotation(&writer);
            }
        }
        (entries, writer_blocked)
    });
    // 一级归档会按设计覆盖 1；轮转后应读到 2/3/4，每条恰好一次。
    assert_snapshot(&read(&writer, lenient), &[2, 3, 4]);
    assert_snapshot(&entries, &[1, 2]);
    assert!(
        writer_blocked,
        "读完整归档及当前之前，写端不得获得同一稳定锁"
    );
}

#[test]
fn r29_8_strict_snapshot_across_instances() {
    rotation_snapshot(false, false);
}

#[test]
fn r29_8_lenient_snapshot_across_instances() {
    rotation_snapshot(true, false);
}

#[test]
fn r29_8_strict_snapshot_across_processes() {
    rotation_snapshot(false, true);
}

#[test]
fn r29_8_lenient_snapshot_across_processes() {
    rotation_snapshot(true, true);
}
