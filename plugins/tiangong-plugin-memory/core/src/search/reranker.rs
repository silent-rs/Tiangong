//! 召回结果融合与重排（Phase C）
//!
//! 将 Tantivy BM25 召回和 LanceDB 语义召回的结果融合、归一化、重排。

use crate::types::{MemoryKind, RecallHit, SearchStrategy};

/// 融合重排器
pub(crate) struct Reranker {
    /// BM25 召回权重（Keyword intent=0.7, Semantic=0.3, Hybrid=0.5）
    bm25_weight: f64,
    /// 语义召回权重（Keyword=0.3, Semantic=0.7, Hybrid=0.5）
    semantic_weight: f64,
}

impl Reranker {
    pub(crate) fn new(bm25_weight: f64, semantic_weight: f64) -> Self {
        Self {
            bm25_weight,
            semantic_weight,
        }
    }

    /// 根据 QueryIntent 创建 Reranker
    pub(crate) fn from_intent(intent: QueryIntent) -> Self {
        let s = intent.semantic_ratio();
        Self::from_semantic_ratio(s)
    }

    /// 根据外部传入的 SearchStrategy 创建 Reranker
    pub(crate) fn from_strategy(strategy: &SearchStrategy) -> Self {
        let s = match strategy {
            SearchStrategy::Skip | SearchStrategy::Keyword => 0.0,
            SearchStrategy::Semantic => 1.0,
            SearchStrategy::Hybrid { semantic_ratio } => semantic_ratio.clamp(0.0, 1.0),
        };
        Self::from_semantic_ratio(s)
    }

    /// 根据语义倾向比率创建 Reranker（内部共用逻辑）
    fn from_semantic_ratio(s: f64) -> Self {
        // BM25 权重从 0.7（Keyword）线性插值到 0.3（Semantic）
        let bm25_w = 0.7 - 0.4 * s;
        // 语义权重从 0.3（Keyword）线性插值到 0.7（Semantic）
        let sem_w = 0.3 + 0.4 * s;
        Self::new(bm25_w, sem_w)
    }

    /// 融合两路召回结果并返回 topK
    ///
    /// - 分数归一化：min-max
    /// - 双命中奖励：同时出现在两路结果中加 0.2
    /// - 重要度加权
    /// - 时间衰减（未来 Phase D 完善）
    pub(crate) fn fuse(
        &self,
        bm25: Vec<RecallHit>,
        semantic: Vec<RecallHit>,
        limit: usize,
    ) -> Vec<RecallHit> {
        use std::collections::HashMap;

        // 归一化 BM25 分数
        let bm25_norm = normalize_scores(bm25);
        // 归一化语义分数
        let semantic_norm = normalize_scores(semantic);

        // 合并：以 node_id 为 key，累加加权分数
        let mut fused: HashMap<String, FusedEntry> = HashMap::new();

        for hit in &bm25_norm {
            let entry = fused
                .entry(hit.node_id.clone())
                .or_insert_with(|| FusedEntry::from_hit(hit));
            entry.bm25_score = Some(hit.score);
        }

        for hit in &semantic_norm {
            let entry = fused
                .entry(hit.node_id.clone())
                .or_insert_with(|| FusedEntry::from_hit(hit));
            entry.semantic_score = Some(hit.score);
        }

        // 计算最终分数
        let bm25_w = self.bm25_weight;
        let semantic_w = self.semantic_weight;
        let mut results: Vec<RecallHit> = fused
            .into_values()
            .map(|entry| {
                let bm25_s = entry.bm25_score.unwrap_or(0.0);
                let sem_s = entry.semantic_score.unwrap_or(0.0);
                let dual_bonus = if entry.bm25_score.is_some() && entry.semantic_score.is_some() {
                    0.2
                } else {
                    0.0
                };
                let fused_score = bm25_s * bm25_w + sem_s * semantic_w + dual_bonus;
                // 重要度加权（±10%）
                let final_score = fused_score * (0.9 + entry.importance * 0.1);
                RecallHit {
                    node_id: entry.node_id,
                    title: entry.title,
                    summary: entry.summary,
                    score: final_score,
                    importance: entry.importance,
                    depth1_loaded: entry.depth1_loaded,
                    kind: entry.kind,
                }
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        results
    }
}

/// Query 意图分析
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QueryIntent {
    /// 偏关键词（含代码符号/文件路径/错误码）
    Keyword,
    /// 偏语义（自然语言、模糊引用）
    Semantic,
    /// 混合（附带连续倾向值 0..100，越大越偏语义）
    Hybrid(u8),
}

impl QueryIntent {
    /// 语义倾向比率，0.0=纯关键词, 1.0=纯语义
    fn semantic_ratio(&self) -> f64 {
        match self {
            QueryIntent::Keyword => 0.0,
            QueryIntent::Semantic => 1.0,
            QueryIntent::Hybrid(pct) => f64::from(*pct) / 100.0,
        }
    }
}

/// 分析查询意图（连续评分制）
///
/// 对查询文本提取多维特征并加权评分：
/// - 代码符号特征（`::`、`->`、`fn `、`impl `、`struct `、`class `、`def `、`import `、`#include`）
/// - 文件路径特征（`/`、`\`、常见文件扩展名）
/// - 错误码特征（`error[`、`E0`、`panic`、`traceback`、`exception`）
/// - 模糊引用特征（"之前"、"那个"、"上次"、"earlier" 等）
/// - 查询长度（短查询偏关键词，长查询偏语义）
/// - 特殊字符密度（高密度偏关键词）
///
/// 最终映射到 Keyword / Semantic / Hybrid(倾向值)。
pub(crate) fn analyze_query(text: &str) -> QueryIntent {
    let text_trimmed = text.trim();
    if text_trimmed.is_empty() {
        return QueryIntent::Hybrid(50);
    }

    // ---- 精确/关键词特征 → 负分（偏 keyword） ----
    let mut keyword_score: f64 = 0.0;
    let mut semantic_score: f64 = 0.0;

    // 代码符号（强精确信号）
    let code_patterns: &[(&str, f64)] = &[
        ("::", 3.0),
        ("->", 2.5),
        ("fn ", 2.5),
        ("impl ", 2.5),
        ("struct ", 2.0),
        ("enum ", 2.0),
        ("trait ", 2.0),
        ("class ", 2.0),
        ("def ", 2.0),
        ("import ", 1.5),
        ("#include", 2.0),
        ("pub ", 1.5),
        ("async ", 1.5),
        ("await ", 1.5),
        ("self.", 2.0),
        ("Self::", 2.5),
        ("crate::", 2.5),
        ("super::", 2.5),
    ];
    for &(pat, weight) in code_patterns {
        if text.contains(pat) {
            keyword_score += weight;
        }
    }

    // 文件路径（中等精确信号）
    let path_extensions: &[&str] = &[
        ".rs", ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".java", ".cpp", ".c", ".h", ".toml",
        ".yaml", ".yml", ".json", ".md",
    ];
    for ext in path_extensions {
        if text.contains(ext) {
            keyword_score += 2.0;
            break; // 只计一次
        }
    }
    // 显式路径分隔符
    if text.contains('/') || text.contains('\\') {
        keyword_score += 1.0;
    }

    // 错误码/堆栈（精确信号）
    let error_patterns: &[(&str, f64)] = &[
        ("error[", 3.0),
        ("E0", 2.0),
        ("panic", 2.0),
        ("traceback", 2.5),
        ("exception", 2.0),
        ("stack trace", 2.5),
        ("SIGSEGV", 3.0),
        ("segfault", 3.0),
    ];
    for &(pat, weight) in error_patterns {
        if text.contains(pat) {
            keyword_score += weight;
        }
    }

    // ---- 语义/模糊特征 → 正分（偏 semantic） ----

    // 模糊时间引用
    let vague_patterns: &[(&str, f64)] = &[
        ("之前", 2.0),
        ("那个", 1.5),
        ("上次", 2.5),
        ("刚才", 2.0),
        ("以前", 2.0),
        ("记得", 2.0),
        ("好像", 1.5),
        ("大概", 1.5),
        ("类似", 1.5),
        ("差不多", 1.5),
        ("last time", 2.5),
        ("earlier", 2.0),
        ("before", 1.5),
        ("remember", 2.0),
        ("similar", 1.5),
        ("something like", 2.0),
        ("previously", 2.0),
    ];
    for &(pat, weight) in vague_patterns {
        if text.contains(pat) {
            semantic_score += weight;
        }
    }

    // 疑问句式（偏语义）
    let question_patterns: &[(&str, f64)] = &[
        ("怎么", 1.5),
        ("为什么", 2.0),
        ("如何", 1.5),
        ("什么是", 2.0),
        ("能不能", 1.0),
        ("有没有", 1.0),
        ("how ", 1.5),
        ("why ", 2.0),
        ("what is", 2.0),
        ("what's", 2.0),
        ("can you", 1.0),
        ("explain", 2.0),
        ("help me", 1.5),
    ];
    for &(pat, weight) in question_patterns {
        if text.to_lowercase().contains(pat) {
            semantic_score += weight;
        }
    }

    // ---- 结构特征 ----

    // 查询长度：短查询偏关键词，长查询偏语义
    let char_count = text_trimmed.chars().count();
    if char_count <= 20 {
        keyword_score += 2.0;
    } else if char_count >= 60 {
        semantic_score += 2.0;
    } else if char_count >= 40 {
        semantic_score += 1.0;
    }

    // 特殊字符密度：高密度偏关键词（代码片段）
    let special_chars = text_trimmed
        .chars()
        .filter(|c| {
            matches!(
                c,
                '{' | '}'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '<'
                    | '>'
                    | ';'
                    | '='
                    | '&'
                    | '|'
                    | '!'
                    | '#'
                    | '@'
                    | '$'
                    | '%'
                    | '^'
                    | '~'
                    | '`'
            )
        })
        .count();
    let density = special_chars as f64 / char_count.max(1) as f64;
    if density > 0.1 {
        keyword_score += 3.0;
    } else if density > 0.05 {
        keyword_score += 1.5;
    }

    // ---- 最终判定 ----
    let total = keyword_score + semantic_score;
    if total < 0.5 {
        // 无明显特征 → 默认 Hybrid 中间值
        return QueryIntent::Hybrid(50);
    }

    let semantic_pct = semantic_score / total;
    if semantic_pct >= 0.85 {
        QueryIntent::Semantic
    } else if semantic_pct <= 0.15 {
        QueryIntent::Keyword
    } else {
        // 映射到 0..100 的连续倾向值
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let pct = (semantic_pct * 100.0).round() as u8;
        QueryIntent::Hybrid(pct)
    }
}

// ==================== 内部辅助 ====================

struct FusedEntry {
    node_id: String,
    title: String,
    summary: String,
    importance: f64,
    depth1_loaded: bool,
    kind: MemoryKind,
    bm25_score: Option<f64>,
    semantic_score: Option<f64>,
}

impl FusedEntry {
    fn from_hit(hit: &RecallHit) -> Self {
        Self {
            node_id: hit.node_id.clone(),
            title: hit.title.clone(),
            summary: hit.summary.clone(),
            importance: hit.importance,
            depth1_loaded: hit.depth1_loaded,
            kind: hit.kind.clone(),
            bm25_score: None,
            semantic_score: None,
        }
    }
}

/// Min-Max 归一化，将分数归一化到 [0, 1]
fn normalize_scores(hits: Vec<RecallHit>) -> Vec<RecallHit> {
    if hits.is_empty() {
        return hits;
    }
    let min = hits.iter().map(|h| h.score).fold(f64::MAX, f64::min);
    let max = hits.iter().map(|h| h.score).fold(f64::MIN, f64::max);
    let range = max - min;
    if range < 1e-10 {
        return hits;
    }
    hits.into_iter()
        .map(|mut h| {
            h.score = (h.score - min) / range;
            h
        })
        .collect()
}
