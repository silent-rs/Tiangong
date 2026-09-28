//! Tantivy 全文检索引擎
//!
//! 负责 Episode/Entity/Decision 的 BM25 全文索引和查询。

use anyhow::{Context, Result};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::*;
use tantivy::{Index, IndexWriter, ReloadPolicy, TantivyDocument};

use crate::types::RecallHit;
use crate::types::{MemoryKind, MemoryNode};

/// Tantivy 全文索引
pub(crate) struct TantivyIndex {
    index: Index,
    schema: Schema,
    writer: IndexWriter,
    /// 字段引用缓存
    fields: SchemaFields,
}

#[allow(dead_code)]
struct SchemaFields {
    id: Field,
    kind: Field,
    title: Field,
    body: Field,
    keywords: Field,
    tool_names: Field,
    code_symbols: Field,
    file_paths: Field,
    error_text: Field,
    source: Field,
    importance: Field,
}

/// 构建 Tantivy Schema
fn build_schema() -> (Schema, SchemaFields) {
    let mut builder = Schema::builder();

    let id = builder.add_text_field("id", STRING | STORED);
    let kind = builder.add_text_field("kind", STRING | STORED);
    let title = builder.add_text_field("title", TEXT | STORED);
    let body = builder.add_text_field("body", TEXT | STORED);
    let keywords = builder.add_text_field("keywords", TEXT | STORED);
    let tool_names = builder.add_text_field("tool_names", TEXT);
    let code_symbols = builder.add_text_field("code_symbols", TEXT);
    let file_paths = builder.add_text_field("file_paths", TEXT);
    let error_text = builder.add_text_field("error_text", TEXT);
    let source = builder.add_text_field("source", STRING);
    let importance = builder.add_f64_field("importance", INDEXED | STORED);

    let schema = builder.build();
    let fields = SchemaFields {
        id,
        kind,
        title,
        body,
        keywords,
        tool_names,
        code_symbols,
        file_paths,
        error_text,
        source,
        importance,
    };
    (schema, fields)
}

impl TantivyIndex {
    /// 打开或创建 Tantivy 索引
    pub(crate) fn open(base_dir: &std::path::Path) -> Result<Self> {
        let index_path = base_dir.join("tantivy_index");
        std::fs::create_dir_all(&index_path)
            .with_context(|| format!("创建 Tantivy 索引目录失败: {}", index_path.display()))?;

        let (schema, fields) = build_schema();

        let index = Index::open_or_create(
            tantivy::directory::MmapDirectory::open(&index_path)
                .with_context(|| "打开 Tantivy MmapDirectory 失败")?,
            schema.clone(),
        )
        .with_context(|| "打开或创建 Tantivy 索引失败")?;

        // 50MB 堆内存给 IndexWriter
        let writer = index
            .writer(50_000_000)
            .with_context(|| "创建 Tantivy IndexWriter 失败")?;

        Ok(Self {
            index,
            schema,
            writer,
            fields,
        })
    }

    /// 将 MemoryNode 写入 Tantivy 索引
    ///
    /// - `body_extra`：可附加额外全文内容（如 full_content 中的文本）
    pub(crate) fn index_node(&mut self, node: &MemoryNode, body_extra: &str) -> Result<()> {
        let mut doc = TantivyDocument::default();

        // 同一 node_id 重新索引时先删除旧文档，避免 Meso 重跑后 BM25 返回重复命中。
        let term = Term::from_field_text(self.fields.id, &node.id);
        self.writer.delete_term(term);

        doc.add_text(self.fields.id, &node.id);
        doc.add_text(self.fields.kind, kind_str(&node.kind));
        doc.add_text(self.fields.title, &node.title);
        doc.add_text(
            self.fields.body,
            format!("{} {}", node.summary, body_extra).trim(),
        );
        doc.add_text(self.fields.keywords, node.keywords.join(" "));
        doc.add_f64(self.fields.importance, node.importance as f64);

        if let Some(source) = &node.source {
            doc.add_text(self.fields.source, source);
        }

        self.writer.add_document(doc)?;

        Ok(())
    }

    /// BM25 全文搜索
    ///
    /// 返回 `(node_id, score)` 列表。
    pub(crate) fn search(&self, query_str: &str, limit: usize) -> Result<Vec<RecallHit>> {
        if query_str.trim().is_empty() {
            return Ok(Vec::new());
        }

        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .with_context(|| "创建 Tantivy reader 失败")?;

        let searcher = reader.searcher();

        // 在 title + body + keywords 上做全文查询
        let query_parser = QueryParser::for_index(
            &self.index,
            vec![self.fields.title, self.fields.body, self.fields.keywords],
        );

        let query = match query_parser.parse_query(query_str) {
            Ok(q) => q,
            Err(_) => {
                // 特殊字符导致解析失败，尝试简单转义后重试
                let escaped = escape_query(query_str);
                query_parser
                    .parse_query(&escaped)
                    .with_context(|| format!("Tantivy 查询解析失败: {query_str}"))?
            }
        };

        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(limit).order_by_score())
            .with_context(|| "Tantivy 搜索执行失败")?;

        let mut hits = Vec::new();
        for (score, doc_addr) in top_docs {
            let doc: TantivyDocument = searcher.doc(doc_addr)?;

            let id = get_text(&doc, self.fields.id, &self.schema).unwrap_or_default();
            let title = get_text(&doc, self.fields.title, &self.schema).unwrap_or_default();
            let body = get_text(&doc, self.fields.body, &self.schema).unwrap_or_default();
            let kind_raw = get_text(&doc, self.fields.kind, &self.schema).unwrap_or_default();

            hits.push(RecallHit {
                node_id: id,
                title,
                summary: body.chars().take(200).collect(),
                score: score as f64,
                kind: parse_kind(&kind_raw),
                importance: 0.5,
                depth1_loaded: false,
            });
        }

        Ok(hits)
    }

    /// 从索引中删除一条记录（通过 id 匹配）
    #[allow(dead_code)]
    pub(crate) fn delete_node(&mut self, node_id: &str) -> Result<()> {
        let term = Term::from_field_text(self.fields.id, node_id);
        self.writer.delete_term(term);
        Ok(())
    }

    /// 将缓冲区的写入操作持久化到磁盘
    pub(crate) fn commit(&mut self) -> Result<()> {
        self.writer.commit()?;
        Ok(())
    }
}

fn kind_str(kind: &MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Episode => "episode",
        MemoryKind::Entity => "entity",
        MemoryKind::Decision => "decision",
        MemoryKind::Evidence => "evidence",
    }
}

fn parse_kind(s: &str) -> MemoryKind {
    match s {
        "entity" => MemoryKind::Entity,
        "decision" => MemoryKind::Decision,
        "evidence" => MemoryKind::Evidence,
        _ => MemoryKind::Episode,
    }
}

fn get_text(doc: &TantivyDocument, field: Field, _schema: &Schema) -> Option<String> {
    doc.get_first(field)
        .and_then(|v| v.as_str().map(|s| s.to_string()))
}

/// 简单转义 Tantivy 查询中的特殊字符
fn escape_query(s: &str) -> String {
    let special = [
        '+', '-', '&', '|', '!', '(', ')', '{', '}', '[', ']', '^', '"', '~', '*', '?', ':', '\\',
        '/',
    ];
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        if special.contains(&c) {
            result.push('\\');
        }
        result.push(c);
    }
    result
}
