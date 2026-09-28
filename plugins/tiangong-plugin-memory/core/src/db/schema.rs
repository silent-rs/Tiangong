//! SQLite 数据库 Schema 初始化

use anyhow::Result;
use rusqlite::Connection;

/// 初始化数据库 Schema（幂等，IF NOT EXISTS）
pub(crate) fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;
    super::migration::migrate_schema_columns(conn)?;
    Ok(())
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS memory_nodes (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    memory_type   TEXT NOT NULL DEFAULT 'factual',
    scope_type    TEXT NOT NULL,
    scope_id      TEXT,
    title         TEXT NOT NULL,
    summary       TEXT NOT NULL,
    keywords      TEXT NOT NULL DEFAULT '[]',
    importance    REAL NOT NULL DEFAULT 0.5,
    confidence    REAL NOT NULL DEFAULT 1.0,
    status        TEXT NOT NULL DEFAULT 'active',
    source        TEXT,
    usage_count   INTEGER NOT NULL DEFAULT 0,
    last_used_at  TEXT,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_nodes_scope ON memory_nodes(scope_type, scope_id);
CREATE INDEX IF NOT EXISTS idx_nodes_kind ON memory_nodes(kind);
CREATE INDEX IF NOT EXISTS idx_nodes_status ON memory_nodes(status);
CREATE INDEX IF NOT EXISTS idx_nodes_importance ON memory_nodes(importance DESC);

CREATE TABLE IF NOT EXISTS memory_relations (
    id             TEXT PRIMARY KEY,
    from_node_id   TEXT NOT NULL REFERENCES memory_nodes(id),
    to_node_id     TEXT NOT NULL REFERENCES memory_nodes(id),
    relation_kind  TEXT NOT NULL,
    weight         REAL NOT NULL DEFAULT 1.0,
    note           TEXT,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    UNIQUE(from_node_id, to_node_id, relation_kind)
);

CREATE INDEX IF NOT EXISTS idx_memory_relations_from ON memory_relations(from_node_id);
CREATE INDEX IF NOT EXISTS idx_memory_relations_to ON memory_relations(to_node_id);
CREATE INDEX IF NOT EXISTS idx_memory_relations_kind ON memory_relations(relation_kind);

CREATE TABLE IF NOT EXISTS episodes (
    id            TEXT PRIMARY KEY REFERENCES memory_nodes(id),
    session_id    TEXT NOT NULL,
    outcome       TEXT NOT NULL,
    tool_calls    TEXT NOT NULL DEFAULT '[]',
    full_content  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS entities (
    id               TEXT PRIMARY KEY REFERENCES memory_nodes(id),
    entity_type      TEXT NOT NULL,
    file_path        TEXT,
    related_episodes TEXT NOT NULL DEFAULT '[]',
    full_content     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS decisions (
    id           TEXT PRIMARY KEY REFERENCES memory_nodes(id),
    context      TEXT NOT NULL,
    alternatives TEXT NOT NULL DEFAULT '[]',
    chosen       TEXT NOT NULL,
    reasons      TEXT NOT NULL DEFAULT '[]',
    episode_ids  TEXT NOT NULL DEFAULT '[]',
    full_content TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS evidence (
    id            TEXT PRIMARY KEY REFERENCES memory_nodes(id),
    evidence_path TEXT NOT NULL,
    byte_size     INTEGER NOT NULL DEFAULT 0
);

-- ⚠️ 迁移过渡表，所有用户完成迁移后可删除
CREATE TABLE IF NOT EXISTS memory_vectors (
    node_id    TEXT PRIMARY KEY REFERENCES memory_nodes(id),
    title      TEXT NOT NULL,
    summary    TEXT NOT NULL,
    kind       TEXT NOT NULL,
    importance REAL NOT NULL DEFAULT 0.5,
    dimension  INTEGER NOT NULL,
    vector     TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_vectors_dimension ON memory_vectors(dimension);
"#;
