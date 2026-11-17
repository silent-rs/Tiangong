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
use std::io::{BufRead, Seek, SeekFrom, Write};
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
    /// append 冷重建完成（offset 基准与已见 id 已从磁盘载入；L5：由
    /// append 实际重建后置位——open 不再提前置位导致重建块不可达）。
    loaded: bool,
    /// open 尾部修复已完成（每路径每进程一次；L4 语义）。
    tail_repaired: bool,
}

fn log_registry()
-> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<LogShared>>> {
    static REGISTRY: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<LogShared>>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 事件日志轮转阈值（Z5：无界增长防护）：超过则归档为 `.1` 重开。
const LOG_ROTATE_BYTES: u64 = 100 * 1024 * 1024;

/// 追加式事件日志。
pub struct EventLog {
    path: PathBuf,
    shared: std::sync::Arc<LogShared>,
    /// 轮转阈值（字节）。生产默认 `LOG_ROTATE_BYTES`；测试可注入小值
    ///（`open_with_rotate_threshold`）。
    rotate_bytes: u64,
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
            rotate_bytes: LOG_ROTATE_BYTES,
        };
        // 十九轮 P2-2 + L2/L4（第 28 轮 review）：尾部修复收进跨 rename
        // 稳定的锁文件（与 append 同锁——并发 append 写一半时，另一进程
        // 的 open 不得把残行判为崩溃残片并截断，丢失刚写入的事件）；
        // 每路径每进程仅首次 open 修复一次（性能优化保持）。
        {
            let lock_path = log.lock_file_path();
            let lock_file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)
                .with_context(|| format!("创建事件日志锁文件失败: {}", lock_path.display()))?;
            let _file_lock = FileLock::acquire(&lock_file)?;
            let need_repair = {
                let mut cache = log
                    .shared
                    .cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // replace 返回旧值：旧值 false = 尚未修复过 → 需要修复
                !std::mem::replace(&mut cache.tail_repaired, true)
            };
            if need_repair {
                log.repair_tail()?;
            }
        }
        Ok(log)
    }

    /// 测试专用：注入小轮转阈值（L2 轮转生命周期的可测构造；生产走
    /// `open` 的 `LOG_ROTATE_BYTES` 默认）。#[doc(hidden)]
    pub fn open_with_rotate_threshold(base: &Path, rotate_bytes: u64) -> Result<Self> {
        let log = Self::open(base)?;
        Ok(Self {
            path: log.path,
            shared: log.shared,
            rotate_bytes: rotate_bytes.max(1),
        })
    }

    /// 尾部修复：读到最后一个完整换行，截断其后残片（崩溃写一半）。
    /// P2-3（十九轮）：重复 event id 行**跳过而非截断**——只把「缺换行
    /// 且非空」的行当残片；重复 id 是幂等语义的合法形态（轮转后新进程
    /// 重新导入历史时可能出现），截断会静默删掉其后全部事件。
    /// L4（R28-6）：中段坏行不截断（详见 repair_tail 内注释）。
    #[doc(hidden)]
    pub fn force_repair_on_next_open(path: &Path) {
        // 测试辅助：同进程内共享注册表会记住「已修复」，无法模拟新
        // 进程的崩溃恢复——显式重置该路径的修复门槛与冷缓存。
        let registry = log_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(shared) = registry.get(path) {
            let mut cache = shared
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.loaded = false;
            cache.tail_repaired = false;
        }
    }
    fn repair_tail(&self) -> Result<()> {
        // L4（R28-6，探针 PR-07）：仅截断「末尾写一半」（无换行结尾的
        // 残片）——此前逐行扫描首遇中段坏行即停并 set_len，其后全部
        // 有效事件被删除（冷打开「好→坏→好→好」只剩第一条好行）。
        // 中段损坏不在此处理：原文件保留证据，由宽松读取（L3）跳过
        // 并计数。修复持 L2 生命周期锁（open 的调用方已持锁文件），
        // 避免另一进程正在追加时误判尾部。
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .with_context(|| format!("打开事件日志失败: {}", self.path.display()))?;
        let size = file.metadata()?.len();
        if size == 0 {
            return Ok(());
        }
        use std::io::Read;
        use std::io::Seek;
        let mut file = file;
        let mut last = [0u8; 1];
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
        if last[0] == b'\n' {
            // 尾部完整（中段损坏归 lenient 读取，原文件不动）
            return Ok(());
        }
        // 无换行结尾：回扫最后一个换行，截其后残片（逐步扩窗——残片
        // 可能是任意长度的半个长事件行）。
        let mut window: u64 = 4096;
        loop {
            let w = size.min(window);
            file.seek(SeekFrom::Start(size - w))?;
            let mut buf = vec![0u8; w as usize];
            file.read_exact(&mut buf)?;
            match buf.iter().rposition(|&b| b == b'\n') {
                Some(pos) => {
                    let complete_bytes = size - w + pos as u64 + 1;
                    tracing::warn!(
                        path = %self.path.display(),
                        partial_bytes = size - complete_bytes,
                        "事件日志尾部不完整（写入中断），已截断至最后完整事件"
                    );
                    file.set_len(complete_bytes)
                        .with_context(|| "截断事件日志尾部失败")?;
                    file.sync_all().ok();
                    return Ok(());
                }
                None if w >= size => {
                    // 整个文件无任何换行：首条写入即中断（append 的单次
                    // write_all 未完成）——清空重开。
                    tracing::warn!(
                        path = %self.path.display(),
                        bytes = size,
                        "事件日志无完整行（首写中断），已清空"
                    );
                    file.set_len(0).with_context(|| "清空事件日志失败")?;
                    file.sync_all().ok();
                    return Ok(());
                }
                None => window = window.saturating_mul(8),
            }
        }
    }

    /// 追加事件：fsync 后返回新 offset；同一 `event.id` 已存在时幂等返回
    /// 既有 offset（不产生第二条事实）。
    ///
    /// L2/L5（R28-4 + 跨进程幂等，第 28 轮 review）：「末行读取 + offset
    /// 分配 + 轮转判定 + 写入 + fsync」整段收进**跨 rename 稳定的锁文件**
    ///（同目录 `.jsonl.lock`，unix flock）——此前锁在当前日志 inode 上，
    /// 轮转 rename 后旧 fd 持锁、新文件无锁，跨进程编号回退；幂等在锁内
    /// 对磁盘做 id 查证（cache 仅进程内加速，不含其他进程写入的事实）。
    pub fn append(&self, event: WorkEvent) -> Result<u64> {
        let lock_path = self.lock_file_path();
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("打开事件日志锁文件失败: {}", lock_path.display()))?;
        let _file_lock = FileLock::acquire(&lock_file)?;
        // 确保当前文件存在（append+create 幂等）——后续 id 查证与冷重建
        // 的行级读取依赖文件已创建（新建日志的首次 append）。
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .read(true)
            .create(true)
            .open(&self.path)
            .with_context(|| format!("打开事件日志（追加）失败: {}", self.path.display()))?;
        // L5：跨进程幂等——锁内磁盘查证（事件 id 行级扫描）。低频写路径
        // 的顺序扫描成本可接受；cache 未命中 ≠ 磁盘不存在（另一进程可能
        // 已写同 id，探针 PR-03 两进程各写一条）。
        if let Some(existing_offset) = self.find_offset_by_event_id(&event.id)? {
            tracing::debug!(
                event_id = %event.id,
                offset = existing_offset,
                "事件 id 已存在于日志（跨进程幂等命中）"
            );
            return Ok(existing_offset);
        }
        // 冷缓存（进程内该路径首写）：从文件重建 offset 基准与已见 id。
        // L4：中段坏行存在时严格全量读会失败——冷重建用宽松语义（跳过
        // 坏行，基准取幸存事件最大 offset；坏行计数记 warn 可观测）。
        {
            let mut cache = self
                .shared
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !cache.loaded {
                let mut entries = Vec::new();
                let mut skipped = 0usize;
                let archived = self.path.with_extension("jsonl.1");
                if archived.exists() {
                    skipped += Self::parse_lenient_into(&archived, &mut entries);
                }
                if self.path.exists() {
                    skipped += Self::parse_lenient_into(&self.path, &mut entries);
                }
                if skipped > 0 {
                    tracing::warn!(
                        path = %self.path.display(),
                        skipped,
                        "冷重建跳过损坏行（基准取幸存事件最大 offset）"
                    );
                }
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
        // 末行实况 offset：tail 回读（当前文件优先；空/损坏时归档兜底
        // ——轮转瞬间当前为空，编号基准取归档末行保持跨代际连续）。
        let mut tail_offset = Self::tail_last_offset(&mut file)?;
        if tail_offset == 0 {
            let archived = self.path.with_extension("jsonl.1");
            if archived.exists() {
                let mut archived_file = std::fs::OpenOptions::new()
                    .read(true)
                    .open(&archived)
                    .with_context(|| format!("打开归档事件日志失败: {}", archived.display()))?;
                tail_offset = Self::tail_last_offset(&mut archived_file)?;
            }
        }
        let offset = {
            let mut cache = self
                .shared
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // 基准取实况与缓存的较大者：实况反映其他进程与归档的写入。
            if tail_offset > cache.last_offset {
                cache.last_offset = tail_offset;
            }
            cache.last_offset += 1;
            cache.seen_ids.insert(event.id.clone());
            cache.last_offset
        };
        // Z5 轮转（L2）：超过阈值——先 rename 归档，再重开当前文件写入
        // （此前持旧 inode fd 跨 rename 续写：事件进归档、当前路径消失、
        // 锁随 inode 分裂）。rename 失败显式返回（不 `let _` 后谎称成功）。
        if let Ok(meta) = file.metadata()
            && meta.len() > self.rotate_bytes
        {
            let rotated = self.path.with_extension("jsonl.1");
            std::fs::rename(&self.path, &rotated)
                .with_context(|| format!("事件日志轮转 rename 失败: {}", rotated.display()))?;
            file = std::fs::OpenOptions::new()
                .append(true)
                .read(true)
                .create(true)
                .open(&self.path)
                .with_context(|| format!("轮转后重开当前日志失败: {}", self.path.display()))?;
            tracing::info!(
                path = %self.path.display(),
                bytes = meta.len(),
                "事件日志超过轮转阈值，已归档重开（offset 连续性由基准保留）"
            );
        }
        let logged = LoggedEvent {
            offset,
            schema_version: CENTRAL_CONTRACT_SCHEMA_VERSION,
            event,
        };
        let mut line = serde_json::to_string(&logged)?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
            .with_context(|| format!("事件日志追加失败: {}", self.path.display()))?;
        Ok(offset)
    }

    /// 锁文件路径：与日志同目录的 `<name>.jsonl.lock`——跨 rename 稳定
    ///（L2：锁当前 inode 会在轮转后分裂，新文件/新进程失去互斥）。
    fn lock_file_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        name.push_str(".lock");
        self.path.with_file_name(name)
    }

    /// 按事件 id 查既有 offset（归档 + 当前，行级扫描；未找到 None）。
    fn find_offset_by_event_id(&self, event_id: &str) -> Result<Option<u64>> {
        let needle = format!("\"id\":\"{event_id}\"");
        let archived = self.path.with_extension("jsonl.1");
        if archived.exists()
            && let Some(offset) = Self::scan_offset_where(&archived, &needle)?
        {
            return Ok(Some(offset));
        }
        if self.path.exists() {
            return Self::scan_offset_where(&self.path, &needle);
        }
        Ok(None)
    }

    /// 逐行扫描：行含 `needle` 子串时解析该行 offset（行级顺序读，
    /// 不整体载入内存——大日志下成本可控且仅在低频 append 路径）。
    fn scan_offset_where(path: &Path, needle: &str) -> Result<Option<u64>> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("打开事件日志失败: {}", path.display()))?;
        let reader = std::io::BufReader::new(file);
        for line in reader.lines() {
            let line = line.with_context(|| "读取事件日志行失败")?;
            if !line.contains(needle) {
                continue;
            }
            let trimmed = line.trim();
            if let Some(offset) = serde_json::from_str::<serde_json::Value>(trimmed)
                .ok()
                .and_then(|value| value.get("offset").and_then(|v| v.as_u64()))
            {
                return Ok(Some(offset));
            }
        }
        Ok(None)
    }

    /// tail 回读末行 offset：逐步扩窗回扫，取最后一个完整行解析其
    /// `offset` 字段（不解析整行事件体——只取编号）。
    ///
    /// L1（R28-3，探针 PR-02）：事件行可达任意长度（Started payload
    /// 含任务全文），固定 4096 字节窗口在长末行时找不到行首 `{`，
    /// 解析失败退 0 → 重启后 offset 从 1 重来。改为 4K→32K→256K…
    /// 逐步扩窗（覆盖文件头即止），三步内覆盖绝大多数日志。
    fn tail_last_offset(file: &mut std::fs::File) -> Result<u64> {
        use std::io::Read;
        let size = file.seek(SeekFrom::End(0))?;
        if size == 0 {
            return Ok(0);
        }
        let mut window: u64 = 4096;
        loop {
            let w = size.min(window);
            file.seek(SeekFrom::Start(size - w))?;
            let mut buf = vec![0u8; w as usize];
            let mut read_total = 0usize;
            while read_total < buf.len() {
                let n = file.read(&mut buf[read_total..])?;
                if n == 0 {
                    break;
                }
                read_total += n;
            }
            buf.truncate(read_total);
            let text = String::from_utf8_lossy(&buf);
            // 窗口未覆盖全文件时首行可能被截一半——跳过；其余行从后
            // 往前找以 '{' 开头的完整 JSON 行并取 offset。
            let skip_first = w < size;
            let mut lines: Vec<&str> = text.split('\n').collect();
            if skip_first && !lines.is_empty() {
                lines.remove(0);
            }
            for line in lines.iter().rev() {
                let trimmed = line.trim();
                if trimmed.is_empty() || !trimmed.starts_with('{') {
                    continue;
                }
                if let Some(offset) = serde_json::from_str::<serde_json::Value>(trimmed)
                    .ok()
                    .and_then(|value| value.get("offset").and_then(|v| v.as_u64()))
                {
                    return Ok(offset);
                }
            }
            if w >= size {
                // 全文件（含归档整读）无有效行：空或全部损坏——归 0 由
                // 调用方决定后续（append 的归档兜底/显式失败语义）。
                return Ok(0);
            }
            window = window.saturating_mul(8);
        }
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
    ///
    /// L3（R28-5，探针 PR-06）：与严格读取共享文件范围（归档 `.1` 在前、
    /// 当前在后）。此前只读当前文件，轮转后恢复投影丢失全部归档事实
    /// （归档 3 条、当前空时返回 0 条）。
    pub fn load_all_lenient(&self) -> (Vec<LoggedEvent>, usize) {
        let _guard = self
            .shared
            .append_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut entries = Vec::new();
        let mut skipped = 0usize;
        let archived = self.path.with_extension("jsonl.1");
        if archived.exists() {
            skipped += Self::parse_lenient_into(&archived, &mut entries);
        }
        if self.path.exists() {
            skipped += Self::parse_lenient_into(&self.path, &mut entries);
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

    /// 宽松解析单文件：坏行跳过并计数（中段损坏保全语义，L4 配套）。
    fn parse_lenient_into(path: &Path, entries: &mut Vec<LoggedEvent>) -> usize {
        let Ok(file) = std::fs::File::open(path) else {
            return 0;
        };
        let reader = std::io::BufReader::new(file);
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
        skipped
    }

    fn load_all_inner(&self) -> Result<Vec<LoggedEvent>> {
        // 十九轮 P2-1：轮转读回——归档 `.1` 在前、当前文件在后拼接
        // （offset 天然递增；此前轮转只写不读，重启后历史只剩轮转后事件）
        let archived = self.path.with_extension("jsonl.1");
        let mut entries = Vec::new();
        if archived.exists() {
            self.parse_file_into(&archived, &mut entries)?;
        }
        self.parse_file_into(&self.path, &mut entries)?;
        Ok(entries)
    }

    fn parse_file_into(&self, path: &Path, entries: &mut Vec<LoggedEvent>) -> Result<()> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("打开事件日志失败: {}", path.display()))?;
        let reader = std::io::BufReader::new(file);
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
        Ok(())
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
/// S1（R28-7，探针 PR-04）：吸收仅在**同 execution** 内成立——同 work
/// 的新 execution（终态后复用会话做新工作的旁路/历史形态）不被旧终态
/// 吞掉；终态或事件缺 execution_id（legacy 数据）时保守保持旧行为。
pub fn project_work_state(
    events: &[LoggedEvent],
) -> std::collections::HashMap<String, crate::status::WorkItemStatus> {
    use crate::event::WorkEventType as E;
    use crate::status::WorkItemStatus as S;
    let mut states = std::collections::HashMap::new();
    let mut terminal_executions: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
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
        // 终态吸收：completed/cancelled 后的迟到事件不改写——同 execution
        // 内吸收；不同 execution 是该 work 的新执行（不吸收，覆盖推进）
        if matches!(current, Some(S::Completed | S::Cancelled)) {
            let same_execution = match (
                terminal_executions.get(&id),
                logged.event.execution_id.as_deref(),
            ) {
                (Some(terminal), Some(current_exec)) => terminal == current_exec,
                // 任一侧缺 execution（legacy/迁移数据）：保守吸收（旧行为）
                _ => true,
            };
            if same_execution {
                continue;
            }
        }
        if matches!(next, S::Completed | S::Cancelled)
            && let Some(execution) = &logged.event.execution_id
        {
            terminal_executions.insert(id.clone(), execution.clone());
        }
        states.insert(id, next);
    }
    states
}

/// 文件级互斥（append 区间）：unix `flock(LOCK_EX)`——跨进程串行化
/// 「末行读取 + offset 分配 + 写入」整段，防止行交错与 offset 撞号。
struct FileLock {
    #[cfg(unix)]
    fd: std::os::raw::c_int,
}
impl FileLock {
    fn acquire(file: &std::fs::File) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = file.as_raw_fd();
            // EINTR 重试
            loop {
                let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
                if rc == 0 {
                    break;
                }
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::Interrupted {
                    return Err(anyhow::anyhow!("事件日志文件锁失败：{err}"));
                }
            }
            // fd 生命周期由调用方 file 保证（Drop 先于 file 释放）
            Ok(Self { fd })
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            Ok(Self {})
        }
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.fd, libc::LOCK_UN);
        }
        #[cfg(not(unix))]
        {
            let _ = self;
        }
    }
}
