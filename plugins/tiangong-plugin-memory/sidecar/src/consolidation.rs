//! 设置页触发的记忆深度整理：在 sidecar 进程内后台运行，页面轮询进度。
//!
//! 与独立工具 `tiangong-memory-consolidator` 共用同一套执行流程
//! （[`tiangong_memory_consolidator::runner`]）与运行锁：同一时刻只能有一个
//! 整理在跑。sidecar 自身就是（或连接到）Memory Leader，整理经同一个
//! [`ManagedMemory`] 读写，不会产生第二个写入者。
//!
//! 页面请求（经 `sidecar.<操作名>` 桥接）：
//! - `ui.memory.consolidation.start`：启动整理（立即返回当前状态）；
//! - `ui.memory.consolidation.status`：查询进度、日志与结果；
//! - `ui.memory.consolidation.stop`：请求停止（当前批次落库后结束）。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tiangong_memory::election::ManagedMemory;
use tiangong_memory_consolidator::consolidation::{ConsolidationStage, Progress};
use tiangong_memory_consolidator::runner::{RunPlan, execute, journal_file_name};

/// 页面展示的日志行数上限（完整内容见执行记录文件）。
const MAX_LOG_LINES: usize = 200;

/// 页面提交的整理参数（缺省值与命令行工具一致）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StartRequest {
    /// 要执行的阶段（空表示全部：dedup → entity → distill → similar）。
    pub stages: Vec<String>,
    pub dry_run: bool,
    /// 相似召回阈值（0.5~0.999），缺省 0.90。
    pub threshold: Option<f64>,
    /// LLM 并发数（1~32），缺省 4。
    pub concurrency: Option<usize>,
    /// 最多调用多少次 LLM（0 表示不限）。
    pub max_llm_calls: usize,
    pub keep_low_quality: bool,
    pub skip_vector_repair: bool,
}

/// 整理状态快照（页面轮询返回）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    /// idle | running | stopping | completed | stopped | failed
    pub state: &'static str,
    pub dry_run: bool,
    /// 本次执行的阶段（按执行顺序）。
    pub stages: Vec<&'static str>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// 最近一条进度。
    pub progress: Option<Progress>,
    /// 各阶段最后一条进度（便于页面展示已完成阶段的结果）。
    pub stage_progress: Vec<Progress>,
    /// 累计 LLM 调用次数。
    pub llm_calls: usize,
    /// 最近日志（最多 [`MAX_LOG_LINES`] 行）。
    pub logs: Vec<String>,
    /// 结束后的完整文本报告。
    pub report: Option<String>,
    /// 失败原因（准备失败或整理中断）。
    pub error: Option<String>,
    pub journal: Option<String>,
}

#[derive(Default)]
struct Job {
    status: Status,
    logs: VecDeque<String>,
    stop: Option<Arc<AtomicBool>>,
}

impl Job {
    fn snapshot(&self) -> Status {
        let mut status = self.status.clone();
        status.logs = self.logs.iter().cloned().collect();
        status
    }

    fn log(&mut self, line: &str) {
        for line in line.lines() {
            if self.logs.len() >= MAX_LOG_LINES {
                self.logs.pop_front();
            }
            self.logs.push_back(line.to_string());
        }
    }

    fn progress(&mut self, progress: &Progress) {
        self.status.llm_calls = self.status.llm_calls.max(progress.llm_calls);
        match self
            .status
            .stage_progress
            .iter_mut()
            .find(|item| item.stage == progress.stage)
        {
            Some(item) => *item = progress.clone(),
            None => self.status.stage_progress.push(progress.clone()),
        }
        self.status.progress = Some(progress.clone());
        // 单条记忆级别的进度只更新进度条，不刷日志；阶段级消息写入日志。
        if !progress.message.starts_with(char::is_whitespace) && !is_item_progress(progress) {
            self.log(&progress.message);
        }
    }
}

/// 逐条/逐批推进的高频进度（只更新进度条，不写入日志，避免刷掉阶段信息）。
fn is_item_progress(progress: &Progress) -> bool {
    progress.total > 0 && progress.done > 0
}

fn job() -> &'static Mutex<Job> {
    static JOB: OnceLock<Mutex<Job>> = OnceLock::new();
    JOB.get_or_init(|| {
        Mutex::new(Job {
            status: Status {
                state: "idle",
                ..Default::default()
            },
            ..Default::default()
        })
    })
}

fn with_job<T>(f: impl FnOnce(&mut Job) -> T) -> T {
    let mut guard = job()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

fn job_log(line: &str) {
    with_job(|job| job.log(line));
}

fn job_progress(progress: &Progress) {
    with_job(|job| job.progress(progress));
}

fn now() -> String {
    chrono::Local::now()
        .naive_local()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// 解析阶段名；未知阶段报错。
fn parse_stages(raw: &[String]) -> Result<Vec<ConsolidationStage>> {
    let mut stages = Vec::new();
    for key in raw {
        let stage = ConsolidationStage::from_key(key).with_context(|| {
            format!("未知整理阶段 {key}，可选：dedup、entity、distill、similar")
        })?;
        if !stages.contains(&stage) {
            stages.push(stage);
        }
    }
    // 按固定顺序执行，与命令行一致。
    stages.sort_by_key(|stage| {
        ConsolidationStage::ALL
            .iter()
            .position(|item| item == stage)
            .unwrap_or_default()
    });
    Ok(stages)
}

fn build_plan(request: &StartRequest, data_dir: PathBuf, journal_dir: PathBuf) -> Result<RunPlan> {
    let stages = parse_stages(&request.stages)?;
    let mut plan = RunPlan::new(
        data_dir,
        journal_dir.join(journal_file_name(&stages, request.dry_run)),
    );
    if let Some(threshold) = request.threshold {
        if !(0.5..=0.999).contains(&threshold) {
            bail!("相似阈值必须在 0.5 ~ 0.999 之间（当前 {threshold}）");
        }
        plan.threshold = threshold;
    }
    if let Some(concurrency) = request.concurrency {
        plan.concurrency = concurrency.clamp(1, 32);
    }
    plan.stages = stages;
    plan.dry_run = request.dry_run;
    plan.max_llm_calls = request.max_llm_calls;
    plan.keep_low_quality = request.keep_low_quality;
    plan.skip_vector_repair = request.skip_vector_repair;
    plan.invocation = serde_json::json!({
        "source": "settings",
        "stages": plan.stages.iter().map(|stage| stage.key()).collect::<Vec<_>>(),
        "dry_run": plan.dry_run,
        "threshold": plan.threshold,
        "concurrency": plan.concurrency,
        "max_llm_calls": plan.max_llm_calls,
        "keep_low_quality": plan.keep_low_quality,
        "skip_vector_repair": plan.skip_vector_repair,
    });
    Ok(plan)
}

/// 整理数据位置：Memory 数据目录与执行记录目录。
///
/// 天工内的 sidecar 运行在沙箱中，存储根下只有自有数据目录 `data/memory`
/// 可写，执行记录因此放在其下 `consolidation-journal/`（命令行工具缺省写到
/// `data/memory-consolidation/`）。
fn default_dirs() -> (PathBuf, PathBuf) {
    let data_dir = tiangong_plugin_sidecar::storage::storage_data_dir("memory");
    let journal_dir = data_dir.join("consolidation-journal");
    (data_dir, journal_dir)
}

/// 启动整理；已有整理在运行时报错。
pub fn start(managed: Arc<ManagedMemory>, request: StartRequest) -> Result<Status> {
    if tiangong_memory::is_memory_disabled() {
        bail!("Memory 已禁用，请先启用后再整理");
    }
    let (data_dir, journal_dir) = default_dirs();
    let plan = build_plan(&request, data_dir, journal_dir)?;
    let stop = Arc::new(AtomicBool::new(false));
    with_job(|job| {
        if matches!(job.status.state, "running" | "stopping") {
            bail!("记忆整理正在运行，请等待其结束或先停止");
        }
        *job = Job {
            status: Status {
                state: "running",
                dry_run: plan.dry_run,
                stages: ConsolidationStage::ALL
                    .into_iter()
                    .filter(|stage| plan.stages.is_empty() || plan.stages.contains(stage))
                    .map(ConsolidationStage::key)
                    .collect(),
                started_at: Some(now()),
                journal: Some(plan.journal_path.display().to_string()),
                ..Default::default()
            },
            logs: VecDeque::new(),
            stop: Some(stop.clone()),
        };
        Ok(())
    })?;
    tokio::spawn(async move {
        let outcome = execute(&managed, &plan, stop, &job_log, Some(&job_progress)).await;
        with_job(|job| {
            job.stop = None;
            job.status.finished_at = Some(now());
            match outcome {
                Ok(report) => {
                    job.status.llm_calls = job.status.llm_calls.max(report.llm_calls);
                    job.status.report = Some(report.render());
                    job.status.state = if let Some(fatal) = &report.fatal_error {
                        job.status.error = Some(fatal.clone());
                        "failed"
                    } else if report.stopped {
                        "stopped"
                    } else {
                        "completed"
                    };
                }
                Err(err) => {
                    let message = format!("{err:#}");
                    job.log(&format!("错误：{message}"));
                    job.status.error = Some(message);
                    job.status.state = "failed";
                    // 准备阶段失败时执行记录尚未创建，不展示不存在的路径。
                    if !plan.journal_path.exists() {
                        job.status.journal = None;
                    }
                }
            }
        });
    });
    Ok(status())
}

/// 当前状态。
pub fn status() -> Status {
    with_job(|job| job.snapshot())
}

/// 处理整理相关的页面操作；不是整理操作时返回 `None`（交给 Memory 分发）。
pub fn dispatch(
    managed: &Arc<ManagedMemory>,
    operation: &str,
    payload: serde_json::Value,
) -> Option<Result<serde_json::Value>> {
    use tiangong_plugin_memory_protocol::ui as plugin_ui;
    let status = match operation {
        plugin_ui::CONSOLIDATION_START_OPERATION => serde_json::from_value::<StartRequest>(payload)
            .context("解析整理参数失败")
            .and_then(|request| start(managed.clone(), request)),
        plugin_ui::CONSOLIDATION_STATUS_OPERATION => Ok(status()),
        plugin_ui::CONSOLIDATION_STOP_OPERATION => Ok(stop()),
        _ => return None,
    };
    Some(status.and_then(|status| serde_json::to_value(status).context("序列化整理状态失败")))
}

/// 请求停止：当前批次落库后结束（LLM 判定中最多等待一次调用）。
pub fn stop() -> Status {
    with_job(|job| {
        if let Some(stop) = job.stop.as_ref() {
            stop.store(true, Ordering::SeqCst);
            if job.status.state == "running" {
                job.status.state = "stopping";
                job.log("已请求停止：当前批次落库完成后结束");
            }
        }
        job.snapshot()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_are_parsed_in_execution_order() {
        let stages = parse_stages(&[
            "similar".to_string(),
            "DEDUP".to_string(),
            "similar".to_string(),
        ])
        .unwrap();
        assert_eq!(
            stages,
            vec![ConsolidationStage::Dedup, ConsolidationStage::Similar]
        );
        assert!(parse_stages(&["bogus".to_string()]).is_err());
        assert!(parse_stages(&[]).unwrap().is_empty());
    }

    #[test]
    fn plan_validates_threshold_and_clamps_concurrency() {
        let dirs = || (PathBuf::from("/d/memory"), PathBuf::from("/d/journal"));
        let (data, journal) = dirs();
        let plan = build_plan(
            &StartRequest {
                stages: vec!["dedup".to_string()],
                dry_run: true,
                concurrency: Some(99),
                ..Default::default()
            },
            data,
            journal,
        )
        .unwrap();
        assert_eq!(plan.concurrency, 32);
        assert!(plan.dry_run);
        assert!(plan.journal_path.starts_with("/d/journal"));
        assert!(
            plan.journal_path
                .to_string_lossy()
                .contains("dedup-dry-run")
        );
        assert_eq!(plan.invocation["source"], "settings");
        let (data, journal) = dirs();
        let err = build_plan(
            &StartRequest {
                threshold: Some(0.2),
                ..Default::default()
            },
            data,
            journal,
        )
        .unwrap_err();
        assert!(err.to_string().contains("相似阈值"), "{err:#}");
    }

    #[test]
    fn item_progress_updates_bar_without_flooding_logs() {
        let mut job = Job::default();
        job.progress(
            &Progress::new("similar", "第 1 批整理中 [3/10]（已删除 1 条）标题")
                .count(3, 10)
                .removed(1)
                .llm_calls(5),
        );
        job.progress(
            &Progress::new("similar", "  LLM 判定：种子 + 2 条相似记忆")
                .count(4, 9)
                .removed(1)
                .llm_calls(2),
        );
        job.progress(&Progress::new("distill", "distill：8 条工具日志，分为 4 批").count(0, 4));
        job.progress(&Progress::new("distill", "distill [1/4] ws：3 条日志").count(1, 4));
        assert_eq!(job.logs.len(), 1, "只有阶段级消息进入日志：{:?}", job.logs);
        assert!(job.logs[0].contains("分为 4 批"));
        assert_eq!(job.status.llm_calls, 5, "LLM 调用次数取最大值");
        assert_eq!(job.status.stage_progress.len(), 2);
        let similar = &job.status.stage_progress[0];
        assert_eq!((similar.done, similar.total, similar.removed), (4, 9, 1));
        assert_eq!(job.status.progress.as_ref().unwrap().stage, "distill");
    }

    #[test]
    fn logs_keep_only_recent_lines() {
        let mut job = Job::default();
        for index in 0..(MAX_LOG_LINES + 5) {
            job.log(&format!("line {index}"));
        }
        assert_eq!(job.logs.len(), MAX_LOG_LINES);
        assert_eq!(job.logs.front().unwrap(), "line 5");
    }
}
