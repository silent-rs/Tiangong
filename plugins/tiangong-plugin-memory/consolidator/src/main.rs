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
    ConsolidationOptions, DEFAULT_CANDIDATE_LIMIT, DEFAULT_GROUP_LIMIT,
    DEFAULT_SIMILARITY_THRESHOLD, run_consolidation,
};
use tiangong_memory_consolidator::vector_store::{self, StoredVectors};

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
    /// 单批最多处理多少条记忆，0 表示不限
    #[arg(long, default_value_t = 0)]
    max_nodes: usize,
    /// 跳过最新的 N 条记忆再开始整理（断点续跑 / 跳过已整理批次）
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// 最多整理多少批（0 表示直到全库完成；每批最多 5000 条）
    #[arg(long, default_value_t = 0)]
    max_pages: usize,
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
    let stored: Option<Arc<StoredVectors>> =
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
        llm: options.model.clone().map(prefer_non_reasoning_protocol),
        embedding,
        dry_run: cli.dry_run,
        forget_low_quality: !cli.keep_low_quality,
        max_nodes: cli.max_nodes,
        offset: cli.offset,
        max_pages: cli.max_pages,
        stored_vectors: stored,
        ..Default::default()
    };
    if consolidation.llm.is_none() {
        // 深度整理的核心是「由 LLM 判断哪些需要归一」，未配置 LLM 时直接报错，
        // 避免退化为规则整理悄悄改写记忆库。
        bail!(
            "未配置记忆 LLM：深度整理需要 LLM 判断归一与遗忘，请先在 Memory 设置中配置模型端点后重试"
        );
    }
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
    println!("{}", report.render());
    // managed Drop：停心跳、释放 Leader 租约；本进程是 Leader 时 actor 随之退出。
    drop(managed);
    if let Some(fatal) = &report.fatal_error {
        eprintln!("错误：记忆深度整理被中断：{fatal}");
        std::process::exit(1);
    }
    Ok(())
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
}
