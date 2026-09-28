//! 基于 fastembed（ONNX Runtime）的本地 Embedding / Rerank Provider。
//!
//! fastembed 的推理需要 `&mut self` 且是 CPU 密集同步调用，因此模型放在
//! `Mutex` 中，每次推理经 `spawn_blocking` 执行，不阻塞 Memory Actor 的
//! 单线程 runtime。

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use fastembed::{
    InitOptionsUserDefined, OnnxSource, Pooling, RerankInitOptionsUserDefined, TextEmbedding,
    TextRerank, TokenizerFiles, UserDefinedEmbeddingModel, UserDefinedRerankingModel,
};
use tiangong_llm::{
    EmbeddingProvider, RerankProvider, RerankRequest, RerankResponse, RerankResult,
};

use super::catalog::LocalModelSpec;

/// 推理批大小（与远端 provider 一致）。
const BATCH_SIZE: usize = 32;
/// 最大输入 token 数：记忆节点都是短文本，512 足够且显著降低内存与延迟。
const MAX_LENGTH: usize = 512;

/// 推理线程数：最多用一半核心（至少 1、至多 4），给宿主与其他进程留余量。
fn intra_threads() -> usize {
    std::thread::available_parallelism()
        .map(|value| (value.get() / 2).clamp(1, 4))
        .unwrap_or(1)
}

fn read_tokenizer_files(dir: &Path, spec: &LocalModelSpec) -> Result<TokenizerFiles> {
    let read = |name: &str| {
        std::fs::read(dir.join(name)).with_context(|| format!("读取 {} 的 {name} 失败", spec.id))
    };
    Ok(TokenizerFiles {
        tokenizer_file: read(spec.tokenizer.local_name())?,
        config_file: read(spec.config.local_name())?,
        special_tokens_map_file: read(spec.special_tokens_map.local_name())?,
        tokenizer_config_file: read(spec.tokenizer_config.local_name())?,
    })
}

/// 本地 Embedding。
pub(crate) struct LocalEmbeddingProvider {
    model: Arc<Mutex<TextEmbedding>>,
    name: String,
    dimension: usize,
}

impl LocalEmbeddingProvider {
    /// 从已下载的模型目录加载（同步、耗时，调用方应放在阻塞线程）。
    pub(crate) fn load(dir: &Path, spec: &LocalModelSpec) -> Result<Self> {
        let onnx = std::fs::read(dir.join(spec.onnx.local_name()))
            .with_context(|| format!("读取 {} 的 ONNX 模型失败", spec.id))?;
        // BGE 系列统一使用 CLS 池化（fastembed 对应内置模型的默认值）。
        let model = UserDefinedEmbeddingModel::new(onnx, read_tokenizer_files(dir, spec)?)
            .with_pooling(Pooling::Cls);
        let options = InitOptionsUserDefined::new()
            .with_max_length(MAX_LENGTH)
            .with_intra_threads(intra_threads());
        let embedding = TextEmbedding::try_new_from_user_defined(model, options)
            .map_err(|error| anyhow!("加载本地 Embedding 模型 {} 失败: {error}", spec.id))?;
        Ok(Self {
            model: Arc::new(Mutex::new(embedding)),
            name: spec.id.to_string(),
            dimension: spec.dimension,
        })
    }
}

#[async_trait]
impl EmbeddingProvider for LocalEmbeddingProvider {
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let model = self.model.clone();
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || {
            let mut model = model
                .lock()
                .map_err(|_| anyhow!("本地 Embedding 模型锁已损坏"))?;
            model
                .embed(&texts, Some(BATCH_SIZE))
                .map_err(|error| anyhow!("本地 Embedding（{name}）推理失败: {error}"))
        })
        .await
        .context("本地 Embedding 推理任务异常退出")?
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn model(&self) -> &str {
        &self.name
    }
}

/// 本地 Rerank（交叉编码器）。
pub(crate) struct LocalRerankProvider {
    model: Arc<Mutex<TextRerank>>,
    name: String,
}

impl LocalRerankProvider {
    pub(crate) fn load(dir: &Path, spec: &LocalModelSpec) -> Result<Self> {
        let model = UserDefinedRerankingModel::new(
            OnnxSource::File(dir.join(spec.onnx.local_name())),
            read_tokenizer_files(dir, spec)?,
        );
        let mut options = RerankInitOptionsUserDefined::new().with_max_length(MAX_LENGTH);
        options = options.with_intra_threads(intra_threads());
        let rerank = TextRerank::try_new_from_user_defined(model, options)
            .map_err(|error| anyhow!("加载本地 Rerank 模型 {} 失败: {error}", spec.id))?;
        Ok(Self {
            model: Arc::new(Mutex::new(rerank)),
            name: spec.id.to_string(),
        })
    }
}

/// 交叉编码器输出的是 logit，映射到 0..1 便于与其他分数比较。
fn sigmoid(value: f32) -> f64 {
    1.0 / (1.0 + (-f64::from(value)).exp())
}

#[async_trait]
impl RerankProvider for LocalRerankProvider {
    async fn rerank(&self, request: RerankRequest) -> Result<RerankResponse> {
        if request.documents.is_empty() {
            return Ok(RerankResponse {
                results: Vec::new(),
            });
        }
        let model = self.model.clone();
        let name = self.name.clone();
        let top_n = request.top_n.max(1);
        tokio::task::spawn_blocking(move || {
            let mut model = model
                .lock()
                .map_err(|_| anyhow!("本地 Rerank 模型锁已损坏"))?;
            let documents = request
                .documents
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            let results = model
                .rerank(request.query.as_str(), documents, false, Some(BATCH_SIZE))
                .map_err(|error| anyhow!("本地 Rerank（{name}）推理失败: {error}"))?;
            Ok(RerankResponse {
                results: results
                    .into_iter()
                    .take(top_n)
                    .map(|result| RerankResult {
                        index: result.index,
                        relevance_score: sigmoid(result.score),
                    })
                    .collect(),
            })
        })
        .await
        .context("本地 Rerank 推理任务异常退出")?
    }

    fn model(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigmoid_maps_logits_monotonically_into_unit_range() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-9);
        assert!(sigmoid(8.0) > 0.99);
        assert!(sigmoid(-8.0) < 0.01);
        assert!(sigmoid(1.0) > sigmoid(0.5));
    }

    #[test]
    fn intra_threads_is_bounded() {
        let threads = intra_threads();
        assert!((1..=4).contains(&threads));
    }
}
