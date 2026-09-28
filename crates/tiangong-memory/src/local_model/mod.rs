//! 内置本地推理（`builtin` 来源的 Embedding / Rerank）。
//!
//! - [`catalog`]：各档位固定的模型清单（revision + sha256）；
//! - [`download`]：断点续传下载与校验；
//! - [`provider`]：基于 fastembed 的推理实现。
//!
//! 选择内置来源后，Memory Actor 在后台下载并加载模型，就绪前召回按
//! BM25（及其他已就绪组件）降级运行，就绪后自动启用。

pub(crate) mod catalog;
pub(crate) mod download;
pub(crate) mod provider;
pub(crate) mod runtime;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use serde::Serialize;
use tiangong_llm::{EmbeddingProvider, RerankProvider};

pub(crate) use download::CancelToken;
pub use download::MODEL_MIRROR_ENV;

use crate::config::MemoryLocalTier;
use catalog::{LocalModelKind, LocalModelSpec};

/// 本进程内的加载状态（下载进度另从磁盘读取，跨进程可见）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeState {
    Downloading,
    Loading,
    Ready,
    Failed(String),
}

fn runtime_states() -> &'static Mutex<HashMap<&'static str, RuntimeState>> {
    static STATES: OnceLock<Mutex<HashMap<&'static str, RuntimeState>>> = OnceLock::new();
    STATES.get_or_init(Default::default)
}

fn set_state(spec: &LocalModelSpec, state: RuntimeState) {
    if let Ok(mut states) = runtime_states().lock() {
        states.insert(spec.id, state);
    }
}

fn runtime_state(spec: &LocalModelSpec) -> Option<RuntimeState> {
    runtime_states()
        .lock()
        .ok()
        .and_then(|states| states.get(spec.id).cloned())
}

/// 下载（如需）并加载本地 Embedding 模型。
pub(crate) async fn load_embedding(
    tier: MemoryLocalTier,
    cancel: &download::CancelToken,
) -> Result<Arc<dyn EmbeddingProvider>> {
    let spec = catalog::embedding_model(tier);
    let dir = prepare(spec, cancel).await?;
    let loaded = load_blocking(spec, move || {
        provider::LocalEmbeddingProvider::load(&dir, spec)
    })
    .await?;
    Ok(Arc::new(loaded))
}

/// 下载（如需）并加载本地 Rerank 模型。
pub(crate) async fn load_rerank(
    tier: MemoryLocalTier,
    cancel: &download::CancelToken,
) -> Result<Arc<dyn RerankProvider>> {
    let spec = catalog::rerank_model(tier);
    let dir = prepare(spec, cancel).await?;
    let loaded = load_blocking(spec, move || {
        provider::LocalRerankProvider::load(&dir, spec)
    })
    .await?;
    Ok(Arc::new(loaded))
}

/// 集成测试入口：下载并加载内置 Embedding（真实模型冒烟用）。
#[doc(hidden)]
pub async fn load_embedding_for_test(tier: MemoryLocalTier) -> Result<Arc<dyn EmbeddingProvider>> {
    load_embedding(tier, &download::CancelToken::default()).await
}

/// 集成测试入口：下载并加载内置 Rerank。
#[doc(hidden)]
pub async fn load_rerank_for_test(tier: MemoryLocalTier) -> Result<Arc<dyn RerankProvider>> {
    load_rerank(tier, &download::CancelToken::default()).await
}

async fn prepare(
    spec: &'static LocalModelSpec,
    cancel: &download::CancelToken,
) -> Result<std::path::PathBuf> {
    if !runtime::platform_supported() {
        let message = format!(
            "当前平台（{}-{}）不支持内置本地模型，请改用在线端点",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        set_state(spec, RuntimeState::Failed(message.clone()));
        anyhow::bail!(message);
    }
    // Linux 需要先就绪 ONNX Runtime 动态库（其他平台为空操作）。
    if let Err(error) = runtime::ensure_loaded(cancel).await {
        if !is_transient(&error) {
            set_state(spec, RuntimeState::Failed(format!("{error:#}")));
        }
        return Err(error);
    }
    let root = download::models_dir();
    if !download::is_installed(&root, spec) {
        set_state(spec, RuntimeState::Downloading);
        tracing::info!(
            model = spec.id,
            size_mb = spec.total_size() / 1_000_000,
            "Memory 内置模型未下载，开始后台下载"
        );
    }
    match download::ensure_model(&root, spec, cancel).await {
        Ok(dir) => Ok(dir),
        // 锁竞争与取消都不是故障：不写 Failed 状态（页面继续显示下载进度），
        // 也不该让调用方按「加载失败」计入退避次数。
        Err(download::DownloadError::Busy) => Err(BusyOrCancelled::Busy.into_error()),
        Err(download::DownloadError::Cancelled) => Err(BusyOrCancelled::Cancelled.into_error()),
        Err(download::DownloadError::Failed(error)) => {
            set_state(spec, RuntimeState::Failed(format!("下载失败：{error:#}")));
            Err(error)
        }
    }
}

/// 非故障中断：调用方据此跳过失败计数。
enum BusyOrCancelled {
    Busy,
    Cancelled,
}

impl BusyOrCancelled {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Busy => anyhow::anyhow!(TRANSIENT_BUSY),
            Self::Cancelled => anyhow::anyhow!(TRANSIENT_CANCELLED),
        }
    }
}

/// 非故障中断标记（供调用方识别，不作为失败计数）。
pub(crate) const TRANSIENT_BUSY: &str = "另一个 Memory 进程正在下载内置模型，稍后重试";
pub(crate) const TRANSIENT_CANCELLED: &str = "内置模型下载已取消";

/// 错误是否属于「暂时中断」而非真正失败。
pub(crate) fn is_transient(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains(TRANSIENT_BUSY) || text.contains(TRANSIENT_CANCELLED)
}

async fn load_blocking<T, F>(spec: &'static LocalModelSpec, load: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    set_state(spec, RuntimeState::Loading);
    let result = tokio::task::spawn_blocking(load)
        .await
        .map_err(|error| anyhow::anyhow!("加载本地模型任务异常退出: {error}"))
        .and_then(|result| result);
    match &result {
        Ok(_) => {
            set_state(spec, RuntimeState::Ready);
            tracing::info!(model = spec.id, "Memory 内置模型已加载");
        }
        Err(error) => set_state(spec, RuntimeState::Failed(format!("加载失败：{error:#}"))),
    }
    result
}

/// 页面展示用的本地模型状态。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LocalModelStatus {
    /// low | mid | high
    pub tier: String,
    /// embedding | rerank
    pub kind: String,
    pub model: String,
    /// 向量维度（仅 embedding）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimension: Option<usize>,
    /// 全部文件总字节数。
    pub size: u64,
    /// not_downloaded | downloading | loading | ready | installed | failed
    pub state: String,
    /// 已下载字节数（下载中时有意义）。
    pub downloaded: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 全部档位的本地模型状态。
pub fn local_model_statuses() -> Vec<LocalModelStatus> {
    let root = download::models_dir();
    let mut statuses = Vec::new();
    for tier in [
        MemoryLocalTier::Low,
        MemoryLocalTier::Mid,
        MemoryLocalTier::High,
    ] {
        for kind in [LocalModelKind::Embedding, LocalModelKind::Rerank] {
            statuses.push(status_of(&root, tier, catalog::model_for(kind, tier)));
        }
    }
    statuses
}

/// 指定档位与用途的模型状态。
pub fn local_model_status(tier: MemoryLocalTier, embedding: bool) -> LocalModelStatus {
    let kind = if embedding {
        LocalModelKind::Embedding
    } else {
        LocalModelKind::Rerank
    };
    status_of(
        &download::models_dir(),
        tier,
        catalog::model_for(kind, tier),
    )
}

fn status_of(root: &Path, tier: MemoryLocalTier, spec: &LocalModelSpec) -> LocalModelStatus {
    let installed = download::is_installed(root, spec);
    let downloaded = if installed {
        spec.total_size()
    } else {
        download::partial_bytes(root, spec)
    };
    let (state, error) = match runtime_state(spec) {
        _ if !runtime::platform_supported() => ("unsupported", None),
        Some(RuntimeState::Ready) => ("ready", None),
        Some(RuntimeState::Loading) => ("loading", None),
        Some(RuntimeState::Failed(message)) => ("failed", Some(message)),
        _ if installed => ("installed", None),
        Some(RuntimeState::Downloading) => ("downloading", None),
        // 本进程没在下载：只有确认存在活跃下载者才算「下载中」，否则残留的
        // 临时文件是上次中断留下的，标记为 interrupted 让页面提示可继续。
        None if downloaded > 0 => {
            if download::has_active_downloader(root) {
                ("downloading", None)
            } else {
                ("interrupted", None)
            }
        }
        None => ("not_downloaded", None),
    };
    LocalModelStatus {
        tier: tier.key().to_string(),
        kind: spec.kind.key().to_string(),
        model: spec.id.to_string(),
        dimension: (spec.dimension > 0).then_some(spec.dimension),
        size: spec.total_size(),
        state: state.to_string(),
        downloaded,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_cover_every_tier_and_kind() {
        let statuses = local_model_statuses();
        assert_eq!(statuses.len(), 6);
        let mid_embedding = statuses
            .iter()
            .find(|status| status.tier == "mid" && status.kind == "embedding")
            .expect("中档 embedding");
        assert_eq!(mid_embedding.model, "bge-base-zh-v1.5");
        assert_eq!(mid_embedding.dimension, Some(768));
        let high_rerank = local_model_status(MemoryLocalTier::High, false);
        assert_eq!(high_rerank.model, "bge-reranker-v2-m3");
        assert_eq!(high_rerank.dimension, None);
    }

    #[test]
    fn stale_partial_reports_interrupted_not_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let spec = &catalog::BGE_SMALL_ZH;
        let files = spec.files();
        let file = files.first().expect("模型至少一个文件");
        let partial = dir.path().join(format!(".partial-{}", spec.id));
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(partial.join(format!("{}.part", file.local_name())), b"abc").unwrap();

        // 无进程持有下载锁：残留的 .part 是上次中断留下的，不该显示「下载中」。
        let status = status_of(dir.path(), MemoryLocalTier::Low, spec);
        assert_eq!(status.state, "interrupted");
        assert_eq!(status.downloaded, 3);

        // 有下载者持锁时才算下载中。
        let root = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let lock = download::test_hooks::acquire_lock(&root).expect("取得下载锁");
            tx.send(()).unwrap();
            let _ = done_rx.recv();
            drop(lock);
        });
        rx.recv().unwrap();
        let status = status_of(dir.path(), MemoryLocalTier::Low, spec);
        assert_eq!(status.state, "downloading");
        done_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn failure_state_is_reported() {
        let spec = &catalog::BGE_RERANKER_V2_M3;
        set_state(spec, RuntimeState::Failed("下载失败：network".into()));
        let status = local_model_status(MemoryLocalTier::High, false);
        assert_eq!(status.state, "failed");
        assert_eq!(status.error.as_deref(), Some("下载失败：network"));
        runtime_states().lock().unwrap().remove(spec.id);
    }
}
