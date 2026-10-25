//! 记忆深度整理独立工具（`tiangong-memory-consolidator`）。
//!
//! 用法：
//!
//! ```text
//! tiangong-memory-consolidator --root ~/.tiangong
//! ```
//!
//! 只需要指定天工根目录：工具自动识别 Memory 系统配置的模型
//! （LLM / Embedding / Rerank），接入（或竞争成为）Memory Leader 后执行
//! 一次全量整理。整理逻辑见 [`consolidation`]。
//!
//! 与 `tiangong-memory-sidecar --mcp/--daemon` 一样属于 Memory 通用模式：
//! 同一时刻只有一个写入者，天工正在运行时会以 Follower 身份经 IPC 转发，
//! 记忆数据始终一致。归一合并与低质量遗忘均为物理删除（经 Memory 的
//! `delete_node`），整理中的记忆一旦 LLM 连续失败即中断并以非零码退出。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use clap::Parser;
use tiangong_memory::election::{
    LeaderState, ManagedMemory, ProcessType, start_or_connect_with_options,
};
use tiangong_memory::{MemoryConfig, MemoryOptions};
use tiangong_memory_consolidator::consolidation::{
    ConsolidationStage, DEFAULT_BATCH_SIZE, DEFAULT_CONCURRENCY, DEFAULT_DISTILL_BATCH_SIZE,
    DEFAULT_DISTILL_WINDOW_MINUTES, DEFAULT_SIMILARITY_THRESHOLD, Progress,
};
use tiangong_memory_consolidator::runner::{RunPlan, execute, journal_file_name, repair_vectors};

/// 天工 Memory：记忆深度整理工具。
#[derive(Debug, Parser)]
#[command(name = "tiangong-memory-consolidator", version, about)]
struct Cli {
    /// 天工根目录（存储根）；缺省 ~/.tiangong，也可用环境变量 TIANGONG_STORAGE_ROOT
    #[arg(long)]
    root: Option<String>,
    /// 相似召回阈值（0~1），缺省 0.90：达到即送 LLM 判定（是否合并由 LLM 决定）
    #[arg(long, default_value_t = DEFAULT_SIMILARITY_THRESHOLD)]
    threshold: f64,
    /// 单次 LLM 判定最多包含的记忆数（含种子），缺省 20；种子的相似记忆超出时分多批判定
    #[arg(long, default_value_t = DEFAULT_BATCH_SIZE)]
    batch_size: usize,
    /// 最多调用多少次 LLM，0 表示不限（试运行时用于控制耗时与成本）
    #[arg(long, default_value_t = 0)]
    max_llm_calls: usize,
    /// 最多以多少条记忆为种子处理（相似记忆多的优先），0 表示不限
    #[arg(long, default_value_t = 0)]
    max_nodes: usize,
    /// 跳过最新的 N 条记忆再开始整理（断点续跑 / 跳过已整理批次）
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// 最多整理多少批（0 表示直到全库完成；每批最多 5000 条）
    #[arg(long, default_value_t = 0)]
    max_pages: usize,
    /// 试运行：只报告不落库（向量补写也只统计缺失数，不写入）
    #[arg(long)]
    dry_run: bool,
    /// 保留低质量记忆，不执行遗忘
    #[arg(long)]
    keep_low_quality: bool,
    /// 只补写缺失向量后退出，不执行整理（只新增向量，不改动记忆内容）
    #[arg(long, conflicts_with = "skip_vector_repair")]
    repair_vectors_only: bool,
    /// 整理前跳过缺失向量补写
    #[arg(long)]
    skip_vector_repair: bool,
    /// 只执行指定阶段（可重复或逗号分隔）：dedup=摘要完全一致归一（不调用
    /// LLM）；entity=同名实体由 LLM 合并；distill=工具日志由 LLM 按上下文提炼；
    /// similar=相似召回 + LLM 判定。均跨工作区整理。
    /// 缺省按 dedup → entity → distill → similar 全部执行
    #[arg(long, value_delimiter = ',', value_parser = parse_stage)]
    stage: Vec<ConsolidationStage>,
    /// distill：每次 LLM 调用最多包含的工具日志数
    #[arg(long, default_value_t = DEFAULT_DISTILL_BATCH_SIZE)]
    distill_batch_size: usize,
    /// distill：相邻工具日志间隔超过该分钟数即切分时间段
    #[arg(long, default_value_t = DEFAULT_DISTILL_WINDOW_MINUTES)]
    distill_window_minutes: i64,
    /// LLM 并发请求数：判定并发进行、落库仍串行（缺省 4；受模型服务限流约束）
    #[arg(long, default_value_t = DEFAULT_CONCURRENCY)]
    concurrency: usize,
    /// 执行记录文件（JSON Lines）；缺省写到
    /// <根目录>/data/memory-consolidation/<时间>-<阶段>[-dry-run]-<pid>.jsonl
    #[arg(long)]
    journal: Option<PathBuf>,
}

fn parse_stage(raw: &str) -> std::result::Result<ConsolidationStage, String> {
    ConsolidationStage::from_key(raw)
        .ok_or_else(|| format!("未知阶段 {raw}，可选：dedup、entity、distill、similar"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if !(0.5..=0.999).contains(&cli.threshold) {
        bail!(
            "--threshold 必须在 0.5 ~ 0.999 之间（当前 {}）",
            cli.threshold
        );
    }
    let root = resolve_storage_root(cli.root.as_deref())?;
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("warn,tiangong_memory_consolidator=info")
            }),
        )
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("构建 tokio 运行时失败")?;
    runtime.block_on(run(cli, root))
}

/// 解析存储根：`--root` > 环境变量 > `~/.tiangong`，并写入环境变量供
/// Memory 各路径解析使用（必须在任何 Memory 调用之前）。
fn resolve_storage_root(root: Option<&str>) -> Result<PathBuf> {
    let path = match root.map(str::trim).filter(|value| !value.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => match std::env::var_os("TIANGONG_STORAGE_ROOT")
            .map(PathBuf::from)
            .filter(|value| !value.as_os_str().is_empty())
        {
            Some(path) => path,
            None => home_dir()
                .map(|home| home.join(".tiangong"))
                .context("无法确定用户主目录，请用 --root 显式指定天工根目录")?,
        },
    };
    if !path.is_dir() {
        bail!("天工根目录不存在或不是目录：{}", path.display());
    }
    let canonical = path.canonicalize().unwrap_or(path);
    // 与 tiangong-plugin-sidecar::env::STORAGE_ROOT_ENV 保持一致。
    // SAFETY: 启动阶段设置进程级环境变量，此时尚无其他线程读取。
    unsafe {
        std::env::set_var("TIANGONG_STORAGE_ROOT", &canonical);
    }
    Ok(canonical)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|value| !value.as_os_str().is_empty())
}

async fn run(cli: Cli, root: PathBuf) -> Result<()> {
    // 旧版数据位于存储根下 memory/，先迁入插件自有数据目录 data/memory/；
    // 失败只告警（通常是旧版进程仍在运行），不阻塞整理。
    if let Err(err) = tiangong_memory::migrate_legacy_storage() {
        tracing::warn!("迁移旧 Memory 数据未完成，可能整理不到旧目录记忆: {err:#}");
    }
    println!("天工根目录：{}", root.display());
    // 配置（含模型端点）从指定根目录读取，自动识别 Memory 已配置的模型。
    let options: MemoryOptions = MemoryConfig::load_or_default().to_options();
    let managed: ManagedMemory =
        start_or_connect_with_options(options.clone(), ProcessType::Sidecar)
            .await
            .context("接入 Memory Leader 失败")?;
    match managed.state() {
        LeaderState::Leader => println!("本进程已竞争为 Memory Leader"),
        LeaderState::Follower { pid } => {
            println!("已有 Memory Leader 运行中（pid={pid}），本进程经 IPC 转发执行")
        }
    }
    let data_dir = root.join("data").join("memory");
    let log = |line: &str| println!("{line}");
    let print_progress = |progress: &Progress| println!("{}", progress.message);
    if cli.repair_vectors_only {
        let outcome = repair_vectors(
            &data_dir.join("lancedb"),
            &options,
            &managed,
            cli.dry_run,
            &log,
            Some(&print_progress),
        )
        .await;
        drop(managed);
        return outcome;
    }
    let journal_path = cli.journal.clone().unwrap_or_else(|| {
        root.join("data")
            .join("memory-consolidation")
            .join(journal_file_name(&cli.stage, cli.dry_run))
    });
    let plan = RunPlan {
        stages: cli.stage.clone(),
        dry_run: cli.dry_run,
        threshold: cli.threshold,
        batch_size: cli.batch_size,
        max_llm_calls: cli.max_llm_calls,
        max_nodes: cli.max_nodes,
        offset: cli.offset,
        max_pages: cli.max_pages,
        keep_low_quality: cli.keep_low_quality,
        skip_vector_repair: cli.skip_vector_repair,
        distill_batch_size: cli.distill_batch_size,
        distill_window_minutes: cli.distill_window_minutes,
        concurrency: cli.concurrency,
        data_dir,
        journal_path,
        invocation: serde_json::json!(std::env::args().collect::<Vec<_>>()),
    };
    // Ctrl+C：第一次请求在当前批次落库后停止；第二次立即退出。
    let stop: Arc<AtomicBool> = Arc::default();
    let signal_stop = stop.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_stop.store(true, Ordering::SeqCst);
            eprintln!(
                "\n收到停止请求：当前批次落库完成后停止（LLM 判定中最多需等待一次调用）。再按一次 Ctrl+C 立即退出。"
            );
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("立即退出：当前批次可能未完成，下次运行会重新判定。");
                std::process::exit(130);
            }
        }
    });
    let outcome = execute(&managed, &plan, stop, &log, Some(&print_progress)).await;
    // managed Drop：停心跳、释放 Leader 租约；本进程是 Leader 时 actor 随之退出。
    drop(managed);
    let report = outcome?;
    println!();
    println!("{}", report.render());
    println!("执行记录已保存：{}", plan.journal_path.display());
    if let Some(fatal) = &report.fatal_error {
        eprintln!("错误：记忆深度整理被中断：{fatal}");
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_flag_accepts_repeated_and_comma_separated_values() {
        let cli = Cli::try_parse_from([
            "x",
            "--stage",
            "dedup,entity",
            "--stage",
            "distill",
            "--stage",
            "SIMILAR",
        ])
        .expect("应解析阶段参数");
        assert_eq!(cli.stage, ConsolidationStage::ALL.to_vec());
        assert!(Cli::try_parse_from(["x", "--stage", "bogus"]).is_err());
        assert!(Cli::try_parse_from(["x"]).unwrap().stage.is_empty());
    }
}
