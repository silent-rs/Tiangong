//! 一次完整整理的执行流程：CLI 与 Memory sidecar（设置页）共用。
//!
//! 负责读取 Memory 配置中的模型、补写缺失向量、读取向量库、创建执行记录，
//! 再调用 [`run_consolidation`]。调用方只需提供已接入的 [`ManagedMemory`]
//! （CLI 自行竞争 Leader；sidecar 复用自身持有的连接）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use tiangong_llm::{EmbeddingProvider, LlmEndpointConfig, ProviderProtocol};
use tiangong_memory::election::{LeaderState, ManagedMemory};
use tiangong_memory::{MemoryConfig, MemoryLlmSource, MemoryOptions};

use crate::consolidation::{
    ConsolidationOptions, ConsolidationProgress, ConsolidationReport, ConsolidationStage,
    DEFAULT_BATCH_SIZE, DEFAULT_CONCURRENCY, DEFAULT_DISTILL_BATCH_SIZE,
    DEFAULT_DISTILL_WINDOW_MINUTES, DEFAULT_SIMILARITY_THRESHOLD, run_consolidation,
};
use crate::journal::Journal;
use crate::vector_repair::repair_missing_vectors;
use crate::vector_store::{self, StoredVectors};

/// 文本日志输出（CLI 打印到终端；sidecar 写入运行日志供页面展示）。
pub type RunLog<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// 一次整理的参数。
#[derive(Debug, Clone)]
pub struct RunPlan {
    /// 要执行的阶段（空表示全部）。
    pub stages: Vec<ConsolidationStage>,
    pub dry_run: bool,
    pub threshold: f64,
    pub batch_size: usize,
    pub max_llm_calls: usize,
    pub max_nodes: usize,
    pub offset: usize,
    pub max_pages: usize,
    pub keep_low_quality: bool,
    pub skip_vector_repair: bool,
    pub distill_batch_size: usize,
    pub distill_window_minutes: i64,
    pub concurrency: usize,
    /// Memory 数据目录（`<存储根>/data/memory`）：向量库在其下 `lancedb/`，
    /// 运行锁 `consolidation.lock` 也放在这里（CLI 与设置页互斥）。
    pub data_dir: PathBuf,
    /// 执行记录文件路径。
    pub journal_path: PathBuf,
    /// 写入执行记录 `run_start` 的调用来源（命令行参数或设置页参数）。
    pub invocation: serde_json::Value,
}

impl RunPlan {
    pub fn new(data_dir: PathBuf, journal_path: PathBuf) -> Self {
        Self {
            stages: Vec::new(),
            dry_run: false,
            threshold: DEFAULT_SIMILARITY_THRESHOLD,
            batch_size: DEFAULT_BATCH_SIZE,
            max_llm_calls: 0,
            max_nodes: 0,
            offset: 0,
            max_pages: 0,
            keep_low_quality: false,
            skip_vector_repair: false,
            distill_batch_size: DEFAULT_DISTILL_BATCH_SIZE,
            distill_window_minutes: DEFAULT_DISTILL_WINDOW_MINUTES,
            concurrency: DEFAULT_CONCURRENCY,
            data_dir,
            journal_path,
            invocation: serde_json::Value::Null,
        }
    }

    /// Memory 向量库目录。
    pub fn lancedb_dir(&self) -> PathBuf {
        self.data_dir.join("lancedb")
    }

    fn runs(&self, stage: ConsolidationStage) -> bool {
        self.stages.is_empty() || self.stages.contains(&stage)
    }

    /// 本次整理是否会执行缺失向量补写。
    pub fn repairs_vectors(&self) -> bool {
        self.runs(ConsolidationStage::Similar) && !self.skip_vector_repair
    }
}

/// 执行记录文件名：`<时间>-<阶段>[-dry-run]-<pid>.jsonl`。
pub fn journal_file_name(stages: &[ConsolidationStage], dry_run: bool) -> String {
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
    format!(
        "{stamp}-{stages}{}-{}.jsonl",
        if dry_run { "-dry-run" } else { "" },
        std::process::id()
    )
}

/// 执行一次整理：准备模型与向量 → 记录 → 整理 → 写入结束记录。
///
/// 准备阶段失败（未配置 LLM、补写向量失败、无法创建执行记录）返回 `Err`；
/// 整理过程中的中断体现在报告的 `fatal_error` / `stopped` 中。
pub async fn execute(
    managed: &ManagedMemory,
    plan: &RunPlan,
    stop: Arc<AtomicBool>,
    log: RunLog<'_>,
    progress: Option<ConsolidationProgress<'_>>,
) -> Result<ConsolidationReport> {
    // 同一份记忆同时只允许一个整理（CLI 与设置页共用该锁）。
    let _lock = RunLock::acquire(&plan.data_dir)?;
    let config = MemoryConfig::load_or_default();
    let options: MemoryOptions = config.to_options();
    describe_models(&config, &options, log);
    let llm = options
        .model
        .clone()
        .map(|model| prefer_non_reasoning_protocol(model, log));
    let needs_llm = plan.runs(ConsolidationStage::Entity)
        || plan.runs(ConsolidationStage::Distill)
        || plan.runs(ConsolidationStage::Similar);
    if needs_llm && llm.is_none() {
        // 深度整理的核心是「由 LLM 判断哪些需要归一」，未配置 LLM 时直接报错，
        // 避免退化为规则整理悄悄改写记忆库（dedup 阶段不需要 LLM）。
        bail!(
            "未配置记忆 LLM：深度整理需要 LLM 判断归一与遗忘，请先在 Memory 设置中配置模型端点后重试"
        );
    }
    let embedding = match options.embedding.as_ref() {
        Some(endpoint) => match tiangong_llm::embedding_provider_from_config(endpoint) {
            Ok(provider) => Some(provider),
            Err(err) => {
                tracing::warn!("Embedding 端点不可用（{err:#}），改用召回候选 + 文本相似度");
                None
            }
        },
        None => None,
    };
    let needs_vectors = plan.runs(ConsolidationStage::Similar);
    // 先补齐缺失向量（工具侧计算并交给 Leader 写入），再读取向量库，保证
    // 所有活跃记忆都参与相似比较。向量只用于 similar 阶段。
    if plan.repairs_vectors() {
        repair_vectors(
            &plan.lancedb_dir(),
            &options,
            managed,
            plan.dry_run,
            log,
            progress,
        )
        .await
        .context("补写缺失向量失败；可跳过向量补写后再整理")?;
    }
    let stored: Option<Arc<StoredVectors>> = if needs_vectors {
        load_vectors(&plan.lancedb_dir(), &options, log).await
    } else {
        None
    };
    let journal = Arc::new(Journal::create(&plan.journal_path)?);
    log(&format!("执行记录：{}", journal.path().display()));
    let consolidation = ConsolidationOptions {
        similarity_threshold: plan.threshold,
        batch_size: plan.batch_size.clamp(2, 50),
        llm,
        embedding,
        dry_run: plan.dry_run,
        forget_low_quality: !plan.keep_low_quality,
        max_nodes: plan.max_nodes,
        max_llm_calls: plan.max_llm_calls,
        offset: plan.offset,
        max_pages: plan.max_pages,
        stored_vectors: stored,
        stages: plan.stages.clone(),
        distill_batch_size: plan.distill_batch_size.clamp(1, 100),
        distill_window_minutes: plan.distill_window_minutes.max(1),
        journal: Some(journal.clone()),
        stop,
        concurrency: plan.concurrency.clamp(1, 32),
        ..Default::default()
    };
    journal.record(
        "run_start",
        serde_json::json!({
            "args": plan.invocation,
            "dry_run": plan.dry_run,
            "stages": plan.stages.iter().map(|stage| stage.key()).collect::<Vec<_>>(),
            "threshold": plan.threshold,
            "llm": consolidation.llm.as_ref().map(|model| model.model.clone()),
        }),
    )?;
    log("开始记忆深度整理……");
    // 句柄提供者：Leader 退出后 follower 监控会自动接替并替换内部句柄，
    // 每次调用都取当前可用的最新句柄（见 consolidation::HandleProvider）。
    let provider = move || managed.handle();
    let report = run_consolidation(&provider, &consolidation, progress).await;
    // 结束记录写入完整报告（展示端只显示前若干条明细）。
    let mut full = report.render();
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
        log(&format!("警告：写入执行记录结尾失败：{err:#}"));
    }
    Ok(report)
}

/// 整理运行锁：持有期间其他整理（另一个 CLI 或设置页）无法启动；进程退出时
/// 系统自动释放，不会因崩溃残留。
pub struct RunLock {
    _file: std::fs::File,
}

impl RunLock {
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("创建 Memory 数据目录失败：{}", data_dir.display()))?;
        let path = data_dir.join("consolidation.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("打开整理运行锁失败：{}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("已有记忆整理正在运行（命令行工具或设置页），请等待其结束或先停止后再试")
            }
            Err(std::fs::TryLockError::Error(err)) => {
                Err(err).with_context(|| format!("获取整理运行锁失败：{}", path.display()))
            }
        }
    }
}

/// 读取向量库中已算好的向量（读取失败时退化为召回 + 文本相似度）。
async fn load_vectors(
    lancedb_dir: &Path,
    options: &MemoryOptions,
    log: RunLog<'_>,
) -> Option<Arc<StoredVectors>> {
    let model_hint = options
        .embedding
        .as_ref()
        .map(|embedding| embedding.model.clone())
        .or_else(|| {
            options
                .local_embedding
                .map(|tier| tiangong_memory::local_model::local_model_status(tier, true).model)
        });
    match vector_store::load_stored_vectors(lancedb_dir, model_hint.as_deref()).await {
        Ok(Some(stored)) => {
            log(&format!(
                "已读取向量库 {} 中的 {} 条向量（{} 维），相似度本地计算",
                stored.table,
                stored.vectors.len(),
                stored.dimension
            ));
            Some(Arc::new(stored))
        }
        Ok(None) => {
            log("未找到可用向量库，相似检索退化为「记忆召回 + 文本相似度」");
            None
        }
        Err(err) => {
            tracing::warn!("读取向量库失败（{err:#}），退化为「记忆召回 + 文本相似度」");
            None
        }
    }
}

/// 补写缺失向量：确定当前 embedding 模型与向量表，工具侧计算向量后交给
/// Memory Leader 写入。
///
/// 天工运行时 CLI 为 Follower，向量经 IPC 由天工的 Memory 写入并立即对召回
/// 生效；sidecar 内运行或天工未运行时本进程即 Leader。两种情况都只有 Leader
/// 写 LanceDB。
pub async fn repair_vectors(
    lancedb_dir: &Path,
    options: &MemoryOptions,
    managed: &ManagedMemory,
    dry_run: bool,
    log: RunLog<'_>,
    progress: Option<ConsolidationProgress<'_>>,
) -> Result<()> {
    let (model, dimension) = match (options.embedding.as_ref(), options.local_embedding) {
        (Some(endpoint), _) => (endpoint.model.clone(), endpoint.dimension),
        (None, Some(tier)) => {
            let status = tiangong_memory::local_model::local_model_status(tier, true);
            (status.model, status.dimension.unwrap_or_default())
        }
        (None, None) => {
            log("未配置 Embedding，跳过缺失向量补写");
            return Ok(());
        }
    };
    if dimension == 0 {
        bail!("无法确定 embedding 模型 {model} 的向量维度");
    }
    let table = tiangong_memory::vector_table_name(&model, dimension);
    let embedding = if dry_run {
        None
    } else {
        log(&format!(
            "加载 embedding 模型 {model}（{dimension} 维）用于补写向量……"
        ));
        Some(load_embedding(options).await?)
    };
    if !dry_run {
        // 先确认 Leader 能接收向量，再花时间计算 embedding。
        ensure_leader_accepts_vectors(managed, &model, dimension).await?;
    }
    let provider = move || managed.handle();
    let report =
        repair_missing_vectors(&provider, lancedb_dir, &table, embedding, progress).await?;
    log(&report.summary());
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
async fn load_embedding(options: &MemoryOptions) -> Result<Arc<dyn EmbeddingProvider>> {
    if let Some(endpoint) = options.embedding.as_ref() {
        return tiangong_llm::embedding_provider_from_config(endpoint)
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
pub fn prefer_non_reasoning_protocol(
    mut model: LlmEndpointConfig,
    log: RunLog<'_>,
) -> LlmEndpointConfig {
    if model.protocol == ProviderProtocol::OpenAi && is_volcengine_ark(&model.base_url) {
        log("记忆 LLM：火山方舟端点改用 Chat Completions 并关闭思考（Responses API 无法关闭推理）");
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

/// 输出自动识别到的模型配置（不回显密钥）。
pub fn describe_models(config: &MemoryConfig, options: &MemoryOptions, log: RunLog<'_>) {
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
    log(&format!("记忆 LLM：{source} → {resolved}"));
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
    log(&format!("记忆 Embedding：{embedding}"));
    log(&format!("记忆 Rerank：{rerank}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(base_url: &str, protocol: ProviderProtocol) -> LlmEndpointConfig {
        let mut model = LlmEndpointConfig::new("k", base_url, "ark-code-latest");
        model.protocol = protocol;
        model
    }

    fn quiet(_: &str) {}

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
        let switched = prefer_non_reasoning_protocol(
            endpoint(
                "https://ark.cn-beijing.volces.com/api/plan/v3",
                ProviderProtocol::OpenAi,
            ),
            &quiet,
        );
        assert_eq!(switched.protocol, ProviderProtocol::OpenAiChatCompletions);
        let openai = prefer_non_reasoning_protocol(
            endpoint("https://api.openai.com/v1", ProviderProtocol::OpenAi),
            &quiet,
        );
        assert_eq!(
            openai.protocol,
            ProviderProtocol::OpenAi,
            "其他厂商不改协议"
        );
        let anthropic = prefer_non_reasoning_protocol(
            endpoint(
                "https://ark.cn-beijing.volces.com/api/v3",
                ProviderProtocol::Anthropic,
            ),
            &quiet,
        );
        assert_eq!(anthropic.protocol, ProviderProtocol::Anthropic);
    }

    #[test]
    fn journal_file_name_includes_stages_and_dry_run() {
        let name = journal_file_name(
            &[ConsolidationStage::Dedup, ConsolidationStage::Similar],
            true,
        );
        assert!(name.contains("-dedup+similar-dry-run-"), "{name}");
        assert!(name.ends_with(".jsonl"));
        assert!(journal_file_name(&[], false).contains("-all-"));
    }

    #[test]
    fn run_lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let held = RunLock::acquire(dir.path()).expect("首次获取应成功");
        let err = RunLock::acquire(dir.path()).err().expect("持有期间应拒绝");
        assert!(err.to_string().contains("正在运行"), "{err:#}");
        drop(held);
        RunLock::acquire(dir.path()).expect("释放后应可再次获取");
    }

    #[test]
    fn vector_repair_follows_similar_stage() {
        let mut plan = RunPlan::new(PathBuf::new(), PathBuf::new());
        assert_eq!(plan.lancedb_dir(), PathBuf::from("lancedb"));
        assert!(plan.repairs_vectors(), "缺省全部阶段，需要补写");
        plan.stages = vec![ConsolidationStage::Dedup];
        assert!(!plan.repairs_vectors());
        plan.stages = vec![ConsolidationStage::Similar];
        plan.skip_vector_repair = true;
        assert!(!plan.repairs_vectors());
    }
}
