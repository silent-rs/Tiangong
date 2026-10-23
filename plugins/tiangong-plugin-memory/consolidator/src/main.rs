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
//! 记忆数据始终一致；本工具不修改 Memory 插件本身。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tiangong_llm::embedding_provider_from_config;
use tiangong_memory::election::{
    LeaderState, ManagedMemory, ProcessType, start_or_connect_with_options,
};
use tiangong_memory::{MemoryConfig, MemoryLlmSource, MemoryOptions};

use tiangong_memory_consolidator::consolidation::{
    ConsolidationOptions, DEFAULT_CANDIDATE_LIMIT, DEFAULT_GROUP_LIMIT,
    DEFAULT_SIMILARITY_THRESHOLD, run_consolidation,
};

/// 天工 Memory：记忆深度整理工具。
#[derive(Debug, Parser)]
#[command(name = "tiangong-memory-consolidator", version, about)]
struct Cli {
    /// 天工根目录（存储根）；缺省 ~/.tiangong，也可用环境变量 TIANGONG_STORAGE_ROOT
    #[arg(long)]
    root: Option<String>,
    /// 相似度阈值（0~1），缺省 0.90，越高越保守
    #[arg(long, default_value_t = DEFAULT_SIMILARITY_THRESHOLD)]
    threshold: f64,
    /// 每组最多参与的相似记忆数，缺省 6
    #[arg(long, default_value_t = DEFAULT_GROUP_LIMIT)]
    group_limit: usize,
    /// 每条记忆的相似候选检索数，缺省 12
    #[arg(long, default_value_t = DEFAULT_CANDIDATE_LIMIT)]
    candidate_limit: usize,
    /// 最多处理多少条记忆，0 表示全部
    #[arg(long, default_value_t = 0)]
    max_nodes: usize,
    /// 试运行：只报告不落库
    #[arg(long)]
    dry_run: bool,
    /// 保留低质量记忆，不执行遗忘
    #[arg(long)]
    keep_low_quality: bool,
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
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
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
    // 配置（含模型端点）从指定根目录读取，自动识别 Memory 已配置的模型。
    let config = MemoryConfig::load_or_default();
    let options: MemoryOptions = config.to_options();
    print_detected(&root, &config, &options);
    let embedding = match options.embedding.as_ref() {
        Some(endpoint) => match embedding_provider_from_config(endpoint) {
            Ok(provider) => Some(provider),
            Err(err) => {
                tracing::warn!("Embedding 端点不可用（{err:#}），改用召回候选 + 文本相似度");
                None
            }
        },
        None => None,
    };
    let managed: ManagedMemory =
        start_or_connect_with_options(options.clone(), ProcessType::Sidecar)
            .await
            .context("接入 Memory Leader 失败")?;
    match managed.state() {
        LeaderState::Leader => println!("本进程已竞争为 Memory Leader，开始整理"),
        LeaderState::Follower { pid } => {
            println!("已有 Memory Leader 运行中（pid={pid}），本进程经 IPC 转发执行整理")
        }
    }
    let consolidation = ConsolidationOptions {
        similarity_threshold: cli.threshold,
        group_limit: cli.group_limit.clamp(1, 20),
        candidate_limit: cli.candidate_limit.clamp(1, 50),
        llm: options.model.clone(),
        embedding,
        dry_run: cli.dry_run,
        forget_low_quality: !cli.keep_low_quality,
        max_nodes: cli.max_nodes,
        ..Default::default()
    };
    if consolidation.llm.is_none() {
        println!("未配置 Memory LLM，将按规则整理（仍会合并一致记忆并重建关联链）");
    }
    if consolidation.embedding.is_none() {
        println!(
            "未配置可用的 Embedding 端点，相似检索退化为「记忆召回 + 文本相似度」；\
             在记忆配置页配置 Embedding 可获得更准确的向量检索"
        );
    }
    println!("开始记忆深度整理……");
    let report = run_consolidation(
        &managed.handle(),
        &consolidation,
        Some(&|progress: &str| println!("{progress}")),
    )
    .await;
    println!();
    println!("{}", report.render());
    // managed Drop：停心跳、释放 Leader 租约；本进程是 Leader 时 actor 随之退出。
    drop(managed);
    Ok(())
}

/// 打印自动识别到的模型配置（不回显密钥）。
fn print_detected(root: &std::path::Path, config: &MemoryConfig, options: &MemoryOptions) {
    println!("天工根目录：{}", root.display());
    let resolved = options
        .model
        .as_ref()
        .map(|model| format!("{}（{}）", model.model, model.base_url))
        .unwrap_or_else(|| "未配置".to_string());
    let source = match &config.model {
        Some(MemoryLlmSource::ModelsRef { key: Some(key) }) => format!("模型引用 {key}"),
        Some(MemoryLlmSource::ModelsRef { key: None }) => "默认路由（lite → chat）".to_string(),
        Some(MemoryLlmSource::Remote(_)) => "独立在线端点".to_string(),
        None => "未配置".to_string(),
    };
    println!("记忆 LLM：{source} → {resolved}");
    let embedding = match (
        options.embedding.as_ref(),
        options.local_embedding.is_some(),
    ) {
        (Some(embedding), _) => {
            format!("在线端点 {}（{} 维）", embedding.model, embedding.dimension)
        }
        (None, true) => "内置本地模型（本工具改用召回 + 文本相似度）".to_string(),
        (None, false) => "未配置".to_string(),
    };
    let rerank = match (options.rerank.as_ref(), options.local_rerank.is_some()) {
        (Some(rerank), _) => format!("在线端点 {}", rerank.model),
        (None, true) => "内置本地模型".to_string(),
        (None, false) => "未配置".to_string(),
    };
    println!("记忆 Embedding：{embedding}");
    println!("记忆 Rerank：{rerank}");
}
