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

use anyhow::{Context, Result, bail};
use clap::Parser;
use tiangong_llm::embedding_provider_from_config;
use tiangong_llm::{LlmEndpointConfig, ProviderProtocol};
use tiangong_memory::election::{
    LeaderState, ManagedMemory, ProcessType, start_or_connect_with_options,
};
use tiangong_memory::{MemoryConfig, MemoryLlmSource, MemoryOptions};

use tiangong_memory_consolidator::consolidation::{
    ConsolidationOptions, ConsolidationStage, DEFAULT_BATCH_SIZE, DEFAULT_DISTILL_BATCH_SIZE,
    DEFAULT_DISTILL_WINDOW_MINUTES, DEFAULT_SIMILARITY_THRESHOLD, run_consolidation,
};
use tiangong_memory_consolidator::journal::Journal;
use tiangong_memory_consolidator::vector_repair::repair_missing_vectors;
use tiangong_memory_consolidator::vector_store::{self, StoredVectors};

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
    /// LLM）；distill=工具日志由 LLM 按上下文提炼；similar=相似召回 + LLM 判定。
    /// 缺省按 dedup → distill → similar 全部执行
    #[arg(long, value_delimiter = ',', value_parser = parse_stage)]
    stage: Vec<ConsolidationStage>,
    /// distill：每次 LLM 调用最多包含的工具日志数
    #[arg(long, default_value_t = DEFAULT_DISTILL_BATCH_SIZE)]
    distill_batch_size: usize,
    /// distill：相邻工具日志间隔超过该分钟数即切分时间段
    #[arg(long, default_value_t = DEFAULT_DISTILL_WINDOW_MINUTES)]
    distill_window_minutes: i64,
    /// 执行记录文件（JSON Lines）；缺省写到
    /// <根目录>/data/memory-consolidation/<时间>-<阶段>[-dry-run]-<pid>.jsonl
    #[arg(long)]
    journal: Option<PathBuf>,
}

fn parse_stage(raw: &str) -> std::result::Result<ConsolidationStage, String> {
    ConsolidationStage::ALL
        .into_iter()
        .find(|stage| stage.key() == raw.trim().to_ascii_lowercase())
        .ok_or_else(|| format!("未知阶段 {raw}，可选：dedup、distill、similar"))
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
    // 向量库里已经算好向量：直接读出来本地算相似度，不重新向量化。
    let model_hint = options
        .embedding
        .as_ref()
        .map(|embedding| embedding.model.clone())
        .or_else(|| {
            options
                .local_embedding
                .map(|tier| tiangong_memory::local_model::local_model_status(tier, true).model)
        });
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
    // 先补齐缺失向量（工具侧计算并写入向量表），再读取向量库，保证所有
    // 活跃记忆都参与相似比较。
    // 向量只用于 similar 阶段的相似召回。
    let needs_vectors = cli.stage.is_empty() || cli.stage.contains(&ConsolidationStage::Similar);
    if cli.repair_vectors_only || (needs_vectors && !cli.skip_vector_repair) {
        let outcome = repair_vectors(&root, &options, &managed, cli.dry_run).await;
        if cli.repair_vectors_only {
            drop(managed);
            return outcome;
        }
        if let Err(err) = outcome {
            drop(managed);
            return Err(err.context("补写缺失向量失败；可加 --skip-vector-repair 跳过后整理"));
        }
    }
    let stored: Option<Arc<StoredVectors>> = if !needs_vectors {
        None
    } else {
        match vector_store::load_stored_vectors(&root, model_hint.as_deref()).await {
            Ok(Some(stored)) => {
                println!(
                    "已读取向量库 {} 中的 {} 条向量（{} 维），相似度本地计算",
                    stored.table,
                    stored.vectors.len(),
                    stored.dimension
                );
                Some(Arc::new(stored))
            }
            Ok(None) => {
                println!("未找到可用向量库，相似检索退化为「记忆召回 + 文本相似度」");
                None
            }
            Err(err) => {
                tracing::warn!("读取向量库失败（{err:#}），退化为「记忆召回 + 文本相似度」");
                None
            }
        }
    };
    let journal_path = cli
        .journal
        .clone()
        .unwrap_or_else(|| default_journal_path(&root, &cli.stage, cli.dry_run));
    let journal = Arc::new(Journal::create(&journal_path)?);
    println!("执行记录：{}", journal.path().display());
    let consolidation = ConsolidationOptions {
        similarity_threshold: cli.threshold,
        batch_size: cli.batch_size.clamp(2, 50),
        llm: options.model.clone().map(prefer_non_reasoning_protocol),
        embedding,
        dry_run: cli.dry_run,
        forget_low_quality: !cli.keep_low_quality,
        max_nodes: cli.max_nodes,
        max_llm_calls: cli.max_llm_calls,
        offset: cli.offset,
        max_pages: cli.max_pages,
        stored_vectors: stored,
        stages: cli.stage.clone(),
        distill_batch_size: cli.distill_batch_size.clamp(1, 100),
        distill_window_minutes: cli.distill_window_minutes.max(1),
        journal: Some(journal.clone()),
        ..Default::default()
    };
    let needs_llm = consolidation.runs(ConsolidationStage::Distill)
        || consolidation.runs(ConsolidationStage::Similar);
    if needs_llm && consolidation.llm.is_none() {
        // 深度整理的核心是「由 LLM 判断哪些需要归一」，未配置 LLM 时直接报错，
        // 避免退化为规则整理悄悄改写记忆库（dedup 阶段不需要 LLM）。
        bail!(
            "未配置记忆 LLM：深度整理需要 LLM 判断归一与遗忘，请先在 Memory 设置中配置模型端点后重试"
        );
    }
    journal.record(
        "run_start",
        serde_json::json!({
            "args": std::env::args().collect::<Vec<_>>(),
            "dry_run": cli.dry_run,
            "stages": cli.stage.iter().map(|stage| stage.key()).collect::<Vec<_>>(),
            "threshold": cli.threshold,
            "llm": consolidation.llm.as_ref().map(|model| model.model.clone()),
        }),
    )?;
    // Ctrl+C：第一次请求在当前批次落库后停止；第二次立即退出。
    let stop = consolidation.stop.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            eprintln!(
                "\n收到停止请求：当前批次落库完成后停止（LLM 判定中最多需等待一次调用）。再按一次 Ctrl+C 立即退出。"
            );
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("立即退出：当前批次可能未完成，下次运行会重新判定。");
                std::process::exit(130);
            }
        }
    });
    println!("开始记忆深度整理……");
    // 句柄提供者：Leader 退出后 follower 监控会自动接替并替换内部句柄，
    // 每次调用都取当前可用的最新句柄（见 consolidation::HandleProvider）。
    let managed_ref = &managed;
    let provider = move || managed_ref.handle();
    let report = run_consolidation(
        &provider,
        &consolidation,
        Some(&|progress: &str| println!("{progress}")),
    )
    .await;
    println!();
    let rendered = report.render();
    println!("{rendered}");
    // 结束记录写入完整报告（终端只显示前若干条明细）。
    let mut full = rendered.clone();
    if report.actions.len() > 200 {
        full.push_str("\n完整处理明细：\n");
        for action in &report.actions {
            full.push_str("  - ");
            full.push_str(action);
            full.push('\n');
        }
    }
    if let Err(err) = journal.record(
        "run_end",
        serde_json::json!({
            "fatal_error": report.fatal_error,
            "stopped": report.stopped,
            "llm_calls": report.llm_calls,
            "errors": report.errors,
            "report": full,
        }),
    ) {
        eprintln!("警告：写入执行记录结尾失败：{err:#}");
    }
    println!("执行记录已保存：{}", journal.path().display());
    // managed Drop：停心跳、释放 Leader 租约；本进程是 Leader 时 actor 随之退出。
    drop(managed);
    if let Some(fatal) = &report.fatal_error {
        eprintln!("错误：记忆深度整理被中断：{fatal}");
        std::process::exit(1);
    }
    Ok(())
}

/// 缺省执行记录路径：<根目录>/data/memory-consolidation/<时间>-<阶段>[-dry-run]-<pid>.jsonl。
///
/// 放在 Memory 插件数据目录（data/memory）之外：Memory 的迁移/恢复会整体替换
/// 该目录，记录不能随之丢失。
fn default_journal_path(
    root: &std::path::Path,
    stages: &[ConsolidationStage],
    dry_run: bool,
) -> PathBuf {
    let stages = if stages.is_empty() {
        "all".to_string()
    } else {
        stages
            .iter()
            .map(|stage| stage.key())
            .collect::<Vec<_>>()
            .join("+")
    };
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    root.join("data").join("memory-consolidation").join(format!(
        "{stamp}-{stages}{}-{}.jsonl",
        if dry_run { "-dry-run" } else { "" },
        std::process::id()
    ))
}

/// 补写缺失向量：确定当前 embedding 模型与向量表，工具侧计算向量后交给
/// Memory Leader 写入。
///
/// 天工运行时本进程为 Follower，向量经 IPC 由天工的 Memory 写入并立即对召回
/// 生效；天工未运行时本进程即 Leader。两种情况都只有 Leader 写 LanceDB。
async fn repair_vectors(
    root: &std::path::Path,
    options: &MemoryOptions,
    managed: &ManagedMemory,
    dry_run: bool,
) -> Result<()> {
    let (model, dimension) = match (options.embedding.as_ref(), options.local_embedding) {
        (Some(endpoint), _) => (endpoint.model.clone(), endpoint.dimension),
        (None, Some(tier)) => {
            let status = tiangong_memory::local_model::local_model_status(tier, true);
            (status.model, status.dimension.unwrap_or_default())
        }
        (None, None) => {
            println!("未配置 Embedding，跳过缺失向量补写");
            return Ok(());
        }
    };
    if dimension == 0 {
        bail!("无法确定 embedding 模型 {model} 的向量维度");
    }
    let table = tiangong_memory::vector_table_name(&model, dimension);
    let lancedb_dir = root.join("data").join("memory").join("lancedb");
    let embedding = if dry_run {
        None
    } else {
        println!("加载 embedding 模型 {model}（{dimension} 维）用于补写向量……");
        Some(load_embedding(options).await?)
    };
    if !dry_run {
        // 先确认 Leader 能接收向量，再花时间计算 embedding。
        ensure_leader_accepts_vectors(managed, &model, dimension).await?;
    }
    let managed_ref = managed;
    let provider = move || managed_ref.handle();
    let report = repair_missing_vectors(
        &provider,
        &lancedb_dir,
        &table,
        embedding,
        Some(&|progress: &str| println!("{progress}")),
    )
    .await?;
    println!("{}", report.summary());
    Ok(())
}

/// 以空批写入探测 Leader 能否接收外部向量（空批不改动数据）。
///
/// - 向量层未就绪（内置模型仍在加载，常见于天工或本进程刚启动）：每秒重试，
///   最多约 2 分钟；
/// - Leader 是不支持该接口的旧版 Memory：旧版收到未知请求会直接断开连接，
///   给出「重启天工」的明确提示；
/// - 模型/维度不一致等其他拒绝：原样报错。
async fn ensure_leader_accepts_vectors(
    managed: &ManagedMemory,
    model: &str,
    dimension: usize,
) -> Result<()> {
    let mut last_err = None;
    for _ in 0..120 {
        match managed
            .handle()
            .upsert_node_vectors(model.to_string(), dimension, Vec::new())
            .await
        {
            Ok(_) => return Ok(()),
            Err(err) if err.to_string().contains("未启用或尚未就绪") => {
                last_err = Some(err);
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            Err(err) if !managed.is_leader() && is_connection_closed(&err) => {
                let pid = match managed.state() {
                    LeaderState::Follower { pid } => pid.to_string(),
                    LeaderState::Leader => "?".to_string(),
                };
                bail!(
                    "当前 Memory Leader（pid={pid}）不支持写入外部向量，连接被断开。\
                     通常是天工仍在运行升级前启动的 Memory 进程：请完全退出并重启天工\
                     （或退出天工后由本工具自身作为 Leader 补写），再重试"
                );
            }
            Err(err) => return Err(err.context("Memory Leader 拒绝写入向量")),
        }
    }
    Err(last_err
        .map(|err| err.context("等待 Memory 向量层就绪超时"))
        .unwrap_or_else(|| anyhow::anyhow!("等待 Memory 向量层就绪超时")))
}

fn is_connection_closed(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let text = cause.to_string();
        text.contains("IPC 连接已关闭") || text.contains("读取 IPC 帧失败")
    })
}

/// 加载与 Memory 相同的 embedding：在线端点优先，否则内置本地模型。
async fn load_embedding(
    options: &MemoryOptions,
) -> Result<Arc<dyn tiangong_llm::EmbeddingProvider>> {
    if let Some(endpoint) = options.embedding.as_ref() {
        return embedding_provider_from_config(endpoint)
            .map_err(|err| anyhow::anyhow!("创建 Embedding 端点失败：{err:#}"));
    }
    let tier = options.local_embedding.context("未配置 Embedding 模型")?;
    tiangong_memory::local_model::load_embedding_provider(tier)
        .await
        .context("加载内置 Embedding 模型失败")
}

/// 火山方舟（Ark）端点改走 Chat Completions 并关闭思考。
///
/// Ark 的 Responses API 没有关闭思考的语义：推理型模型（如 Agent Plan 的
/// `ark-code-latest`）会持续推理直到吃满输出上限、正文为空，整理永远拿不到
/// JSON。Chat Completions 在不需要推理时会携带 `thinking: {type: disabled}`，
/// Ark 据此直接输出正文（实测推理 token 为 0）。其他厂商保持原协议不变。
fn prefer_non_reasoning_protocol(mut model: LlmEndpointConfig) -> LlmEndpointConfig {
    if model.protocol == ProviderProtocol::OpenAi && is_volcengine_ark(&model.base_url) {
        println!(
            "记忆 LLM：火山方舟端点改用 Chat Completions 并关闭思考（Responses API 无法关闭推理）"
        );
        model.protocol = ProviderProtocol::OpenAiChatCompletions;
    }
    model
}

fn is_volcengine_ark(base_url: &str) -> bool {
    let host = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split(['/', ':'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    host == "volces.com" || host.ends_with(".volces.com")
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
        (None, true) => "内置本地模型（向量已入库，直接读取）".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(base_url: &str, protocol: ProviderProtocol) -> LlmEndpointConfig {
        let mut model = LlmEndpointConfig::new("k", base_url, "ark-code-latest");
        model.protocol = protocol;
        model
    }

    #[test]
    fn detects_volcengine_ark_host() {
        assert!(is_volcengine_ark(
            "https://ark.cn-beijing.volces.com/api/plan/v3"
        ));
        assert!(is_volcengine_ark("http://ark.volces.com:443/api/v3"));
        assert!(!is_volcengine_ark("https://api.openai.com/v1"));
        assert!(!is_volcengine_ark("https://evil.com/ark.volces.com"));
        assert!(!is_volcengine_ark("https://notvolces.com/v1"));
    }

    #[test]
    fn ark_responses_switches_to_chat_completions() {
        let switched = prefer_non_reasoning_protocol(endpoint(
            "https://ark.cn-beijing.volces.com/api/plan/v3",
            ProviderProtocol::OpenAi,
        ));
        assert_eq!(switched.protocol, ProviderProtocol::OpenAiChatCompletions);
        let openai = prefer_non_reasoning_protocol(endpoint(
            "https://api.openai.com/v1",
            ProviderProtocol::OpenAi,
        ));
        assert_eq!(
            openai.protocol,
            ProviderProtocol::OpenAi,
            "其他厂商不改协议"
        );
        let anthropic = prefer_non_reasoning_protocol(endpoint(
            "https://ark.cn-beijing.volces.com/api/v3",
            ProviderProtocol::Anthropic,
        ));
        assert_eq!(anthropic.protocol, ProviderProtocol::Anthropic);
    }

    #[test]
    fn stage_flag_accepts_repeated_and_comma_separated_values() {
        let cli = Cli::try_parse_from(["x", "--stage", "dedup,distill", "--stage", "SIMILAR"])
            .expect("应解析阶段参数");
        assert_eq!(
            cli.stage,
            vec![
                ConsolidationStage::Dedup,
                ConsolidationStage::Distill,
                ConsolidationStage::Similar
            ]
        );
        assert!(Cli::try_parse_from(["x", "--stage", "bogus"]).is_err());
        assert!(Cli::try_parse_from(["x"]).unwrap().stage.is_empty());
    }
}
