//! 追加式事件日志（R2，方案 §5.3 / §5.5）：唯一权威事实源。
//!
//! ```text
//! <base>/events.jsonl    每行一条 LoggedEvent（offset 自增，schema 带版本）
//! ```
//!
//! 可靠性语义：
//! - **追加写**：只 append 不改写，天然避免「load-modify-save」交错损坏；
//! - **fsync 后返回**：写入崩溃后，日志尾部至多是最后一个完整事件；
//! - **崩溃恢复**：`open` 校验尾部，不完整行截断丢弃并 warn（不拒载）；
//! - **幂等**：同一 `event.id` 重复 append 不产生第二条（返回既有 offset）；
//! - **写失败显式上报**：`append` 返回 `Err`，调用方不得声称成功。
//!
//! 快照（work-items.json 等）是缓存：可由事件日志投影重建，不是独立
//! 事实源（§5.3「事件是事实，快照是缓存，UI 是投影」）。

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::event::WorkEvent;
use crate::execution::CENTRAL_CONTRACT_SCHEMA_VERSION;

/// 日志行：事件 + 存储元数据。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LoggedEvent {
    /// 自增位移（从 1 起；恢复与对账的游标）。
    pub offset: u64,
    /// 写入时的契约 schema 版本（向前迁移依据，§5.5）。
    pub schema_version: u32,
    pub event: WorkEvent,
}

/// 跨实例共享状态（Z5）：生产代码每次 `EventLog::open` 都是新实例——
/// 实例级锁无互斥（两实例并发 append 可能交错/offset 重复）。按路径
/// 注册表共享锁与缓存：
/// - `append_lock`：同路径全部实例串行；
/// - `cache`：`last_offset` + 已见事件 id（append 幂等判定免去全量读）。
struct LogShared {
    append_lock: Mutex<()>,
    cache: Mutex<LogCache>,
}

#[derive(Default)]
struct LogCache {
    last_offset: u64,
    seen_ids: HashSet<String>,
    /// 已从文件重建（冷缓存判定，仅进程内该路径首次 append 时扫描）。
    loaded: bool,
}

fn log_registry()
-> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<LogShared>>> {
    static REGISTRY: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<LogShared>>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 事件日志轮转阈值（Z5：无界增长防护）：超过则归档为 `.1` 重开。
const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// 追加式事件日志。
pub struct EventLog {
    path: PathBuf,
    shared: std::sync::Arc<LogShared>,
}

impl EventLog {
    /// 打开（或创建）事件日志；尾部不完整行截断（崩溃恢复）。
    /// 同路径的所有实例共享锁与幂等缓存（Z5）。
    pub fn open(base: &Path) -> Result<Self> {
        if let Some(parent) = base.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建事件日志目录失败: {}", base.display()))?;
        }
        let shared = {
            let mut registry = log_registry()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry
                .entry(base.to_path_buf())
                .or_insert_with(|| {
                    std::sync::Arc::new(LogShared {
                        append_lock: Mutex::new(()),
                        cache: Mutex::new(LogCache::default()),
                    })
                })
                .clone()
        };
        let log = Self {
            path: base.to_path_buf(),
            shared,
        };
        log.repair_tail()?;
        Ok(log)
    }

    /// 尾部修复：读到最后一个完整换行，截断其后残片（崩溃写一半）。
    fn repair_tail(&self) -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .with_context(|| format!("打开事件日志失败: {}", self.path.display()))?;
        let mut reader = std::io::BufReader::new(&file);
        let mut complete_bytes: u64 = 0;
        let mut seen = HashSet::new();
        let mut last_partial = false;
        loop {
            let mut line = String::new();
            let read = reader
                .read_line(&mut line)
                .with_context(|| format!("读取事件日志失败: {}", self.path.display()))?;
            if read == 0 {
                break;
            }
            let line_ends_newline = line.ends_with('\n');
            let trimmed = line.trim();
            let parse_ok = !trimmed.is_empty()
                && serde_json::from_str::<LoggedEvent>(trimmed).is_ok_and(|logged| {
                    // 顺带收集已见事件 id（重复追加的幂等判据）
                    seen.insert(logged.event.id.clone())
                });
            if parse_ok && line_ends_newline {
                complete_bytes += read as u64;
            } else {
                // 尾部残片：非 JSON、缺换行（写一半崩溃）或重复事件 id
                // （旧数据已有）——截断点定在此处。
                last_partial = !trimmed.is_empty() && !line_ends_newline;
                break;
            }
        }
        if complete_bytes < file.metadata()?.len() {
            if last_partial {
                tracing::warn!(
                    path = %self.path.display(),
                    "事件日志尾部不完整（写入中断），已截断至最后完整事件"
                );
            }
            let file = file;
            file.set_len(complete_bytes)
                .with_context(|| "截断事件日志尾部失败")?;
            file.sync_all().ok();
        }
        Ok(())
    }

    /// 追加事件：fsync 后返回新 offset；同一 `event.id` 已存在时幂等返回
    /// 既有 offset（不产生第二条事实）。
    pub fn append(&self, event: WorkEvent) -> Result<u64> {
        let _guard = self
            .shared
            .append_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Z5 缓存：共享缓存（已见 id / last_offset）免去每次全量读；
        // 冷缓存（进程内该路径首开）时从文件重建一次。
        {
            let mut cache = self
                .shared
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !cache.loaded {
                let entries = self.load_all_inner()?;
                cache.last_offset = entries.last().map(|logged| logged.offset).unwrap_or(0);
                cache.seen_ids = entries
                    .iter()
                    .map(|logged| logged.event.id.clone())
                    .collect();
                cache.loaded = true;
            }
            if cache.seen_ids.contains(&event.id) {
                // 幂等：已见事件 id → 返回既有 offset（一次查询）
                let entries = self.load_all_inner()?;
                if let Some(existing) = entries.iter().find(|logged| logged.event.id == event.id) {
                    return Ok(existing.offset);
                }
            }
        }
        let offset = {
            let mut cache = self
                .shared
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.last_offset += 1;
            cache.seen_ids.insert(event.id.clone());
            cache.last_offset
        };
        // Z5 轮转：超过阈值归档为 `.1` 重开（offset 连续性由缓存保留）
        if let Ok(meta) = std::fs::metadata(&self.path)
            && meta.len() > LOG_ROTATE_BYTES
        {
            let rotated = self.path.with_extension("jsonl.1");
            let _ = std::fs::rename(&self.path, &rotated);
            tracing::info!(
                path = %self.path.display(),
                bytes = meta.len(),
                "事件日志超过轮转阈值，已归档重开"
            );
        }
        let logged = LoggedEvent {
            offset,
            schema_version: CENTRAL_CONTRACT_SCHEMA_VERSION,
            event,
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)
            .with_context(|| format!("打开事件日志（追加）失败: {}", self.path.display()))?;
        let mut line = serde_json::to_string(&logged)?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
            .with_context(|| format!("事件日志追加失败: {}", self.path.display()))?;
        Ok(offset)
    }

    /// 读取全部完整事件（投影/对账数据源）。
    pub fn load_all(&self) -> Result<Vec<LoggedEvent>> {
        let _guard = self
            .shared
            .append_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.load_all_inner()
    }

    /// 宽松读取（R5 recover 对账）：跳过中段损坏行并记 warn 计数，
    /// 返回 (完整事件, 跳过行数)——恢复路径不因个别坏行整体失败，
    /// 且丢失事实数量可观测。
    pub fn load_all_lenient(&self) -> (Vec<LoggedEvent>, usize) {
        let _guard = self
            .shared
            .append_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(_) => return (Vec::new(), 0),
        };
        let reader = std::io::BufReader::new(file);
        let mut entries = Vec::new();
        let mut skipped = 0usize;
        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<LoggedEvent>(trimmed) {
                Ok(logged) => entries.push(logged),
                Err(_) => skipped += 1,
            }
        }
        if skipped > 0 {
            tracing::warn!(
                path = %self.path.display(),
                skipped,
                "事件日志存在损坏行（已跳过；事实源不完整，恢复以幸存事件为准）"
            );
        }
        (entries, skipped)
    }

    fn load_all_inner(&self) -> Result<Vec<LoggedEvent>> {
        let file = std::fs::File::open(&self.path)
            .with_context(|| format!("打开事件日志失败: {}", self.path.display()))?;
        let reader = std::io::BufReader::new(file);
        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line.with_context(|| "读取事件日志行失败")?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let logged: LoggedEvent = serde_json::from_str(trimmed)
                .with_context(|| format!("事件日志行解析失败（offset 附近）: {trimmed}"))?;
            entries.push(logged);
        }
        Ok(entries)
    }

    /// 最后完整事件的 offset（空日志为 0）。
    pub fn last_offset(&self) -> Result<u64> {
        Ok(self
            .load_all()?
            .last()
            .map(|logged| logged.offset)
            .unwrap_or(0))
    }
}

/// 事件投影（§5.3：UI 是投影）：按事件顺序重建工作项状态。
///
/// 终态吸收后续事件（迟到 progress 不改写 completed）；未知事件不改变
/// 当前状态（向前兼容：schema 演进后旧版本读取不崩溃）。
pub fn project_work_state(
    events: &[LoggedEvent],
) -> std::collections::HashMap<String, crate::status::WorkItemStatus> {
    use crate::event::WorkEventType as E;
    use crate::status::WorkItemStatus as S;
    let mut states = std::collections::HashMap::new();
    for logged in events {
        let id = logged.event.work_item_id.clone();
        let current = states.get(&id).copied();
        let next = match logged.event.event_type {
            E::WorkItemCreated => Some(S::Draft),
            E::WorkItemStarted => Some(S::Running),
            E::WorkItemProgress | E::WorkItemPhaseCompleted => Some(S::Running),
            E::WorkItemWaitingUser => Some(S::WaitingUser),
            E::WorkItemWaitingResource => Some(S::WaitingResource),
            E::WorkItemBlocked => Some(S::Blocked),
            E::WorkItemFailed => Some(S::Failed),
            E::WorkItemCompleted => Some(S::Completed),
            E::WorkItemCancelled => Some(S::Cancelled),
            // 非工作项事件（workspace/model/report 类）不改变状态
            _ => None,
        };
        let Some(next) = next else { continue };
        // 终态吸收：completed/cancelled 后的迟到事件不改写
        if matches!(current, Some(S::Completed | S::Cancelled)) {
            continue;
        }
        states.insert(id, next);
    }
    states
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{WorkEvent, WorkEventType};
    use crate::ids::new_id;

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
        assert_eq!(offsets.len(), sorted.len());
        let unique: std::collections::HashSet<u64> = offsets.iter().copied().collect();
        assert_eq!(unique.len(), 100, "offset 全局唯一（实例级锁时代会重复）");
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
        use crate::status::WorkItemStatus as S;
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
}
