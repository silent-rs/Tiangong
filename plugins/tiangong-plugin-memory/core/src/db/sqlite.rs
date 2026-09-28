//! SQLite 明文连接、历史加密库恢复与 CRUD 操作

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::types::{
    Decision, Entity, EntityType, Episode, Evidence, ExpandedMemory, MemoryCognitiveType,
    MemoryKind, MemoryNode, MemoryRelation, MemoryRelationDraft, MemoryRelationKind,
    MemoryScopeType, MemoryStatus, RecallHit,
};

use super::schema;

const DB_KEY_DOMAIN_V2: &[u8] = b"tiangong-memory:metadata.db:key:v2\0";
const DB_KEY_BACKUP_PREFIX: &str = ".metadata.db.pre-plaintext-";
const LEGACY_BACKUP_PREFIX: &str = ".metadata.db.pre-key-v2-";

/// 供目录恢复识别迁移附属文件；沿用生成备份的格式，避免把用户文件误当成备份。
pub(crate) fn is_migration_backup_file(name: &str) -> bool {
    let base = ["-wal", "-shm", "-journal"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name);
    [DB_KEY_BACKUP_PREFIX, LEGACY_BACKUP_PREFIX]
        .iter()
        .any(|prefix| {
            base.strip_prefix(prefix)
                .and_then(|value| value.strip_suffix(".bak"))
                .is_some_and(|id| id.parse::<scru128::Id>().is_ok())
        })
}

/// Memory 元数据库（普通 SQLite）
pub(crate) struct MemoryDb {
    conn: Connection,
}

impl MemoryDb {
    /// 打开或创建普通数据库，必要时转换历史加密库，并初始化 Schema
    pub(crate) fn open() -> Result<Self> {
        Self::open_at_data_dir(&memory_base_path())
    }

    /// 在指定数据目录打开数据库，供数据恢复前核对节点数量。
    pub(crate) fn open_at_data_dir(data_dir: &Path) -> Result<Self> {
        let db_path = data_dir.join("metadata.db");

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建数据库目录失败: {}", parent.display()))?;
        }

        let conn = open_plaintext_conn(&db_path)?;
        schema::init_schema(&conn)?;

        Ok(Self { conn })
    }

    /// 插入 Episode 到 memory_nodes 和 episodes 表
    pub(crate) fn insert_episode(
        &self,
        episode: &Episode,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Local::now().naive_local().to_string();
        let keywords = serde_json::to_string(&episode.keywords)?;
        let tool_calls = serde_json::to_string(&episode.tool_calls)?;
        let outcome = serde_json::to_string(&episode.outcome)?;
        let full_content = serde_json::to_string(episode)?;

        // 写入 memory_nodes（scope_id 来自 workspace_id，不再硬编码 NULL）
        self.conn
            .execute(
                "INSERT OR REPLACE INTO memory_nodes
                 (id, kind, memory_type, scope_type, scope_id, title, summary, keywords, importance,
                  confidence, status, source, usage_count, created_at, updated_at)
                 VALUES (?1, 'episode', ?2, 'workspace', ?3, ?4, ?5, ?6, ?7, 1.0, 'active', ?8, 0, ?9, ?9)",
                rusqlite::params![
                    episode.id,
                    memory_cognitive_type_to_str(&episode.memory_type),
                    workspace_id,
                    episode.title,
                    episode.summary,
                    keywords,
                    episode.importance,
                    episode.session_id,
                    now,
                ],
            )
            .with_context(|| "写入 memory_nodes 失败")?;

        // 写入 episodes
        self.conn
            .execute(
                "INSERT OR REPLACE INTO episodes (id, session_id, outcome, tool_calls, full_content)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    episode.id,
                    episode.session_id,
                    outcome,
                    tool_calls,
                    full_content,
                ],
            )
            .with_context(|| "写入 episodes 失败")?;

        Ok(())
    }

    /// 更新 Episode 的摘要和关键词。
    pub(crate) fn update_episode_summary(
        &self,
        node_id: &str,
        summary: &str,
        keywords: &[String],
    ) -> Result<()> {
        let now = chrono::Local::now().naive_local().to_string();
        let keywords_json = serde_json::to_string(keywords)?;
        self.conn
            .execute(
                "UPDATE memory_nodes SET summary = ?1, keywords = ?2, updated_at = ?3 WHERE id = ?4",
                rusqlite::params![summary, keywords_json, now, node_id],
            )
            .with_context(|| "更新 Episode 摘要失败")?;
        Ok(())
    }

    /// 加载单个记忆节点。
    pub(crate) fn load_node(&self, node_id: &str) -> Result<Option<MemoryNode>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, kind, memory_type, scope_type, scope_id, title, summary, keywords, importance, confidence, status, source, usage_count, last_used_at, created_at, updated_at FROM memory_nodes WHERE id = ?1")?;
        let mut rows = stmt.query(rusqlite::params![node_id])?;
        match rows.next()? {
            Some(row) => {
                let kind_str: String = row.get(1)?;
                let kind = str_to_memory_kind(&kind_str);
                let mt_str: String = row.get(2)?;
                let memory_type = str_to_memory_cognitive_type(&mt_str);
                let scope_str: String = row.get(3)?;
                let scope_type = str_to_scope_type(&scope_str);
                let status_str: String = row.get(10)?;
                let status = str_to_memory_status(&status_str);
                let kw_str: String = row.get(7)?;
                let keywords: Vec<String> = serde_json::from_str(&kw_str).unwrap_or_default();
                Ok(Some(MemoryNode {
                    id: row.get(0)?,
                    kind,
                    memory_type,
                    scope_type,
                    scope_id: row.get(4)?,
                    title: row.get(5)?,
                    summary: row.get(6)?,
                    keywords,
                    importance: row.get(8)?,
                    confidence: row.get(9)?,
                    status,
                    source: row.get(11)?,
                    usage_count: row.get(12)?,
                    last_used_at: row.get(13)?,
                    created_at: row.get(14)?,
                    updated_at: row.get(15)?,
                }))
            }
            None => Ok(None),
        }
    }

    /// 插入或更新 Entity 到 memory_nodes 和 entities 表
    #[allow(dead_code)]
    pub(crate) fn upsert_entity(&self, entity: &Entity, workspace_id: Option<&str>) -> Result<()> {
        let keywords = serde_json::to_string(&entity.related_episodes)?;
        let related_episodes = serde_json::to_string(&entity.related_episodes)?;
        let full_content = serde_json::to_string(entity)?;

        self.upsert_memory_node(
            &entity.id,
            MemoryKind::Entity,
            MemoryScopeType::Workspace,
            workspace_id,
            &entity.name,
            &entity.description,
            &keywords,
            entity.importance,
            entity.file_path.as_deref(),
            MemoryCognitiveType::ProjectStructure,
            &entity.created_at,
            &entity.updated_at,
        )?;

        self.conn
            .execute(
                "INSERT OR REPLACE INTO entities
                 (id, entity_type, file_path, related_episodes, full_content)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    entity.id,
                    entity_type_to_str(&entity.entity_type),
                    entity.file_path,
                    related_episodes,
                    full_content,
                ],
            )
            .with_context(|| "写入 entities 失败")?;

        Ok(())
    }

    /// 列出工作区下的 Entity
    #[allow(dead_code)]
    pub(crate) fn list_entities(&self, workspace_id: Option<&str>) -> Result<Vec<Entity>> {
        let mut entities = Vec::new();
        if let Some(workspace_id) = workspace_id {
            let mut stmt = self.conn.prepare(
                "SELECT e.full_content
                 FROM entities e
                 JOIN memory_nodes n ON n.id = e.id
                 WHERE n.scope_type = 'workspace' AND n.scope_id = ?1
                 ORDER BY n.updated_at DESC",
            )?;
            let rows = stmt.query_map(rusqlite::params![workspace_id], |row| {
                row.get::<_, String>(0)
            })?;
            for row in rows {
                let full_content = row?;
                let entity = serde_json::from_str(&full_content)
                    .with_context(|| "解析 Entity 列表项失败")?;
                entities.push(entity);
            }
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT e.full_content
                 FROM entities e
                 JOIN memory_nodes n ON n.id = e.id
                 WHERE n.scope_type = 'workspace' AND n.scope_id IS NULL
                 ORDER BY n.updated_at DESC",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                let full_content = row?;
                let entity = serde_json::from_str(&full_content)
                    .with_context(|| "解析 Entity 列表项失败")?;
                entities.push(entity);
            }
        }
        Ok(entities)
    }

    /// 插入或更新 Decision 到 memory_nodes 和 decisions 表
    #[allow(dead_code)]
    pub(crate) fn upsert_decision(
        &self,
        decision: &Decision,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        let keywords = serde_json::to_string(&decision.reasons)?;
        let alternatives = serde_json::to_string(&decision.alternatives)?;
        let reasons = serde_json::to_string(&decision.reasons)?;
        let episode_ids = serde_json::to_string(&decision.episode_ids)?;
        let full_content = serde_json::to_string(decision)?;

        self.upsert_memory_node(
            &decision.id,
            MemoryKind::Decision,
            MemoryScopeType::Workspace,
            workspace_id,
            &decision.title,
            &decision.context,
            &keywords,
            0.7,
            Some(&decision.chosen),
            MemoryCognitiveType::ArchitectureDecision,
            &decision.created_at,
            &decision.created_at,
        )?;

        self.conn
            .execute(
                "INSERT OR REPLACE INTO decisions
                 (id, context, alternatives, chosen, reasons, episode_ids, full_content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    decision.id,
                    decision.context,
                    alternatives,
                    decision.chosen,
                    reasons,
                    episode_ids,
                    full_content,
                ],
            )
            .with_context(|| "写入 decisions 失败")?;

        Ok(())
    }

    /// 写入 Evidence（memory_node + evidence 扩展表）。
    pub(crate) fn insert_evidence(
        &self,
        id: &str,
        evidence: &Evidence,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Local::now().naive_local().to_string();
        let keywords = serde_json::to_string(&Vec::<String>::new())?;
        let evidence_path = evidence
            .file_path
            .as_ref()
            .or(evidence.url.as_ref())
            .cloned()
            .unwrap_or_default();

        self.conn.execute(
            "INSERT OR REPLACE INTO memory_nodes
             (id, kind, memory_type, scope_type, scope_id, title, summary, keywords, importance,
              confidence, status, source, usage_count, created_at, updated_at)
             VALUES (?1, 'evidence', 'factual', 'workspace', ?2, ?3, ?4, ?5, 0.5, 0.0, 'active', ?6, 0, ?7, ?7)",
            rusqlite::params![
                id,
                workspace_id,
                evidence.title,
                evidence.summary,
                keywords,
                evidence.source_tool,
                now,
            ],
        )?;

        self.conn.execute(
            "INSERT OR REPLACE INTO evidence (id, evidence_path, byte_size) VALUES (?1, ?2, 0)",
            rusqlite::params![id, evidence_path],
        )?;

        Ok(())
    }

    /// 列出工作区下的 Decision
    #[allow(dead_code)]
    pub(crate) fn list_decisions(&self, workspace_id: Option<&str>) -> Result<Vec<Decision>> {
        let mut decisions = Vec::new();
        if let Some(workspace_id) = workspace_id {
            let mut stmt = self.conn.prepare(
                "SELECT d.full_content
                 FROM decisions d
                 JOIN memory_nodes n ON n.id = d.id
                 WHERE n.scope_type = 'workspace' AND n.scope_id = ?1
                 ORDER BY n.updated_at DESC",
            )?;
            let rows = stmt.query_map(rusqlite::params![workspace_id], |row| {
                row.get::<_, String>(0)
            })?;
            for row in rows {
                let full_content = row?;
                let decision = serde_json::from_str(&full_content)
                    .with_context(|| "解析 Decision 列表项失败")?;
                decisions.push(decision);
            }
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT d.full_content
                 FROM decisions d
                 JOIN memory_nodes n ON n.id = d.id
                 WHERE n.scope_type = 'workspace' AND n.scope_id IS NULL
                 ORDER BY n.updated_at DESC",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                let full_content = row?;
                let decision = serde_json::from_str(&full_content)
                    .with_context(|| "解析 Decision 列表项失败")?;
                decisions.push(decision);
            }
        }
        Ok(decisions)
    }

    /// 查询最近的 Episode 摘要（用于 MesoRumination）
    #[allow(dead_code)]
    pub(crate) fn recent_episode_summaries(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, Vec<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.title, n.keywords
             FROM memory_nodes n
             WHERE n.kind = 'episode' AND n.status = 'active'
             ORDER BY n.created_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, String>(1).unwrap_or_default(),
                ))
            })?
            .filter_map(|r| r.ok())
            .map(|(title, kw_json)| {
                let kws: Vec<String> = serde_json::from_str(&kw_json).unwrap_or_default();
                (title, kws)
            })
            .collect();
        Ok(rows)
    }

    /// 查询最近的完整 Episode，供 MesoRumination 提炼 Entity / Decision。
    pub(crate) fn recent_episodes(
        &self,
        workspace_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        let (sql, params): (&str, Vec<Box<dyn rusqlite::ToSql>>) =
            if let Some(workspace_id) = workspace_id {
                (
                    "SELECT ep.full_content
                     FROM episodes ep
                     JOIN memory_nodes n ON n.id = ep.id
                     WHERE n.kind = 'episode'
                       AND n.status = 'active'
                       AND n.scope_type = 'workspace'
                       AND n.scope_id = ?1
                     ORDER BY n.created_at DESC
                     LIMIT ?2",
                    vec![Box::new(workspace_id.to_string()), Box::new(limit as i64)],
                )
            } else {
                (
                    "SELECT ep.full_content
                     FROM episodes ep
                     JOIN memory_nodes n ON n.id = ep.id
                     WHERE n.kind = 'episode'
                       AND n.status = 'active'
                       AND n.scope_type = 'workspace'
                       AND n.scope_id IS NULL
                     ORDER BY n.created_at DESC
                     LIMIT ?1",
                    vec![Box::new(limit as i64)],
                )
            };

        let mut stmt = self.conn.prepare(sql)?;
        let params = params.iter().map(|item| item.as_ref()).collect::<Vec<_>>();
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
            row.get::<_, String>(0)
        })?;
        let mut episodes = Vec::new();
        for row in rows {
            let full_content = row?;
            let episode = serde_json::from_str(&full_content)
                .with_context(|| "解析 Meso Episode 列表项失败")?;
            episodes.push(episode);
        }
        Ok(episodes)
    }

    /// 查询当前工作区内指定会话最近 Episode 的完整内容，供 Session Injection 使用。
    pub(crate) fn recent_episodes_for_session(
        &self,
        workspace_id: Option<&str>,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        let (sql, params): (&str, Vec<Box<dyn rusqlite::ToSql>>) =
            if let Some(workspace_id) = workspace_id {
                (
                    "SELECT ep.full_content
                     FROM episodes ep
                     JOIN memory_nodes n ON n.id = ep.id
                     WHERE n.kind = 'episode'
                       AND n.status = 'active'
                       AND n.scope_type = 'workspace'
                       AND n.scope_id = ?1
                       AND ep.session_id = ?2
                     ORDER BY n.created_at DESC
                     LIMIT ?3",
                    vec![
                        Box::new(workspace_id.to_string()),
                        Box::new(session_id.to_string()),
                        Box::new(limit as i64),
                    ],
                )
            } else {
                (
                    "SELECT ep.full_content
                     FROM episodes ep
                     JOIN memory_nodes n ON n.id = ep.id
                     WHERE n.kind = 'episode'
                       AND n.status = 'active'
                       AND n.scope_type = 'workspace'
                       AND n.scope_id IS NULL
                       AND ep.session_id = ?1
                     ORDER BY n.created_at DESC
                     LIMIT ?2",
                    vec![Box::new(session_id.to_string()), Box::new(limit as i64)],
                )
            };

        let mut stmt = self.conn.prepare(sql)?;
        let params = params.iter().map(|item| item.as_ref()).collect::<Vec<_>>();
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
            row.get::<_, String>(0)
        })?;
        let mut episodes = Vec::new();
        for row in rows {
            let full_content = row?;
            let episode = serde_json::from_str(&full_content)
                .with_context(|| "解析 Session Episode 列表项失败")?;
            episodes.push(episode);
        }
        Ok(episodes)
    }

    /// 列出超过指定天数未使用且重要度低于阈值的节点（用于归档）
    pub(crate) fn list_stale_nodes(
        &self,
        days_threshold: i64,
        importance_threshold: f64,
    ) -> Result<Vec<(String, f64)>> {
        let cutoff = chrono::Local::now().naive_local() - chrono::TimeDelta::days(days_threshold);
        let cutoff_str = cutoff.format("%Y-%m-%d %H:%M:%S%.f").to_string();

        let mut stmt = self.conn.prepare(
            "SELECT id, importance FROM memory_nodes
             WHERE status = 'active'
               AND (last_used_at IS NULL OR last_used_at < ?1)
               AND importance < ?2
             ORDER BY importance ASC",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![cutoff_str, importance_threshold], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// 更新节点状态（active → archived）
    pub(crate) fn update_node_status(&self, node_id: &str, status: &MemoryStatus) -> Result<()> {
        let status_str = match status {
            MemoryStatus::Active => "active",
            MemoryStatus::Archived => "archived",
        };
        let now = chrono::Local::now().naive_local().to_string();
        self.conn.execute(
            "UPDATE memory_nodes SET status = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![status_str, now, node_id],
        )?;
        Ok(())
    }

    /// 列出记忆节点，供 GUI 手动管理使用。
    pub(crate) fn list_memory_nodes(
        &self,
        workspace_id: Option<&str>,
        query: Option<&str>,
        status: Option<&MemoryStatus>,
        created_after: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<MemoryNode>> {
        let status_str = status.map(memory_status_to_str);
        let query_like = query
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                format!(
                    "%{}%",
                    value
                        .replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                )
            });
        let limit = if limit == 0 { 100 } else { limit.min(500) };

        let mut sql = String::from(
            "SELECT id, kind, memory_type, scope_type, scope_id, title, summary, keywords, importance,
                    confidence, status, source, usage_count, last_used_at, created_at, updated_at
             FROM memory_nodes WHERE 1 = 1",
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(workspace_id) = workspace_id {
            sql.push_str(" AND scope_type = 'workspace' AND scope_id = ?");
            params.push(Box::new(workspace_id.to_string()));
        }
        if let Some(status_str) = status_str {
            sql.push_str(" AND status = ?");
            params.push(Box::new(status_str.to_string()));
        }
        if let Some(query_like) = query_like {
            sql.push_str(
                " AND (title LIKE ? ESCAPE '\\' OR summary LIKE ? ESCAPE '\\' OR keywords LIKE ? ESCAPE '\\')",
            );
            params.push(Box::new(query_like.clone()));
            params.push(Box::new(query_like.clone()));
            params.push(Box::new(query_like));
        }
        if let Some(created_after) = created_after
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            sql.push_str(" AND created_at >= ?");
            params.push(Box::new(created_after.to_string()));
        }
        sql.push_str(" ORDER BY updated_at DESC LIMIT ? OFFSET ?");
        params.push(Box::new(limit as i64));
        params.push(Box::new(offset as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let params = params.iter().map(|item| item.as_ref()).collect::<Vec<_>>();
        let rows = stmt.query_map(rusqlite::params_from_iter(params), row_to_memory_node)?;

        let mut nodes = Vec::new();
        for row in rows {
            nodes.push(row.with_context(|| "读取 memory_nodes 行失败")?);
        }
        Ok(nodes)
    }

    /// 按 ID 升序分页列出需要向量化的活跃节点（向量索引回填使用）。
    ///
    /// 覆盖所有 kind：手动记忆、语义/程序性记忆等在写入时同样进向量索引
    /// （见 `MemoryStore::upsert_node` 调用点），只回填 episode 会让它们在
    /// 换模型后永久缺失语义索引。
    ///
    /// `after` 为上一批最后一个节点 ID（不含），None 表示从头开始。
    pub(crate) fn list_vector_backfill_nodes(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryNode>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, memory_type, scope_type, scope_id, title, summary, keywords, importance,
                    confidence, status, source, usage_count, last_used_at, created_at, updated_at
             FROM memory_nodes
             WHERE status = 'active' AND id > ?1
             ORDER BY id ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![after.unwrap_or(""), limit.max(1) as i64],
            row_to_memory_node,
        )?;
        let mut nodes = Vec::new();
        for row in rows {
            nodes.push(row.with_context(|| "读取回填节点失败")?);
        }
        Ok(nodes)
    }

    /// 统计记忆节点数量，供 GUI 展示真实总数。
    pub(crate) fn count_memory_nodes(
        &self,
        workspace_id: Option<&str>,
        query: Option<&str>,
        status: Option<&MemoryStatus>,
        created_after: Option<&str>,
    ) -> Result<usize> {
        let status_str = status.map(memory_status_to_str);
        let query_like = query
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                format!(
                    "%{}%",
                    value
                        .replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                )
            });

        let mut sql = String::from("SELECT COUNT(*) FROM memory_nodes WHERE 1 = 1");
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(workspace_id) = workspace_id {
            sql.push_str(" AND scope_type = 'workspace' AND scope_id = ?");
            params.push(Box::new(workspace_id.to_string()));
        }
        if let Some(status_str) = status_str {
            sql.push_str(" AND status = ?");
            params.push(Box::new(status_str.to_string()));
        }
        if let Some(query_like) = query_like {
            sql.push_str(
                " AND (title LIKE ? ESCAPE '\\' OR summary LIKE ? ESCAPE '\\' OR keywords LIKE ? ESCAPE '\\')",
            );
            params.push(Box::new(query_like.clone()));
            params.push(Box::new(query_like.clone()));
            params.push(Box::new(query_like));
        }
        if let Some(created_after) = created_after
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            sql.push_str(" AND created_at >= ?");
            params.push(Box::new(created_after.to_string()));
        }

        let params = params.iter().map(|item| item.as_ref()).collect::<Vec<_>>();
        let count: i64 = self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(params), |row| row.get(0))
            .with_context(|| "统计 memory_nodes 数量失败")?;
        Ok(count.max(0) as usize)
    }

    /// 更新记忆节点元信息，供手动调整使用。
    pub(crate) fn update_memory_node_details(
        &self,
        node_id: &str,
        title: &str,
        summary: &str,
        keywords: &[String],
        importance: f32,
        memory_type: &MemoryCognitiveType,
    ) -> Result<MemoryNode> {
        let now = chrono::Local::now().naive_local().to_string();
        let keywords_json = serde_json::to_string(keywords)?;
        self.conn
            .execute(
                "UPDATE memory_nodes
                 SET title = ?1, summary = ?2, keywords = ?3, importance = ?4,
                     memory_type = ?5, status = 'active', updated_at = ?6
                 WHERE id = ?7",
                rusqlite::params![
                    title,
                    summary,
                    keywords_json,
                    importance,
                    memory_cognitive_type_to_str(memory_type),
                    now,
                    node_id,
                ],
            )
            .with_context(|| format!("更新记忆节点失败: {node_id}"))?;
        self.get_memory_node(node_id)?
            .with_context(|| format!("记忆节点不存在: {node_id}"))
    }

    /// 按 ID 获取记忆节点。
    pub(crate) fn get_memory_node(&self, node_id: &str) -> Result<Option<MemoryNode>> {
        self.conn
            .query_row(
                "SELECT id, kind, memory_type, scope_type, scope_id, title, summary, keywords, importance,
                        confidence, status, source, usage_count, last_used_at, created_at, updated_at
                 FROM memory_nodes WHERE id = ?1",
                rusqlite::params![node_id],
                row_to_memory_node,
            )
            .optional()
            .with_context(|| format!("查询记忆节点失败: {node_id}"))
    }

    /// 新增或更新记忆图关系。
    pub(crate) fn upsert_memory_relation(
        &self,
        draft: MemoryRelationDraft,
    ) -> Result<MemoryRelation> {
        let id = draft.id.unwrap_or_else(|| scru128::new().to_string());
        let from_node_id = draft.from_node_id;
        let to_node_id = draft.to_node_id;
        let relation_kind = draft.relation_kind;
        let note = draft.note;
        let now = chrono::Local::now().naive_local().to_string();
        let weight = if draft.weight > 0.0 {
            draft.weight.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.conn
            .execute(
                "INSERT INTO memory_relations
                 (id, from_node_id, to_node_id, relation_kind, weight, note, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                 ON CONFLICT(from_node_id, to_node_id, relation_kind)
                 DO UPDATE SET weight = excluded.weight, note = excluded.note, updated_at = excluded.updated_at",
                rusqlite::params![
                    &id,
                    &from_node_id,
                    &to_node_id,
                    memory_relation_kind_to_str(&relation_kind),
                    weight,
                    &note,
                    &now,
                ],
            )
            .with_context(|| "写入 memory_relations 失败")?;

        self.conn
            .query_row(
                "SELECT id, from_node_id, to_node_id, relation_kind, weight, note, created_at, updated_at
                 FROM memory_relations
                WHERE from_node_id = ?1 AND to_node_id = ?2 AND relation_kind = ?3",
                rusqlite::params![
                    &from_node_id,
                    &to_node_id,
                    memory_relation_kind_to_str(&relation_kind),
                ],
                row_to_memory_relation,
            )
            .with_context(|| "查询写入后的记忆关系失败")
    }

    /// 列出指定节点的图关系。包含出边和入边，便于 GUI 展示和深度召回扩展。
    pub(crate) fn list_memory_relations(&self, node_id: &str) -> Result<Vec<MemoryRelation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, from_node_id, to_node_id, relation_kind, weight, note, created_at, updated_at
             FROM memory_relations
             WHERE from_node_id = ?1 OR to_node_id = ?1
             ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![node_id], row_to_memory_relation)?;
        let mut relations = Vec::new();
        for row in rows {
            relations.push(row.with_context(|| "读取 memory_relations 行失败")?);
        }
        Ok(relations)
    }

    /// 批量列出多个节点的关联关系（去重）。
    pub(crate) fn list_memory_relations_batch(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<MemoryRelation>> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        // 构建 IN 子句的占位符
        let placeholders: Vec<String> = node_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect();
        let sql = format!(
            "SELECT DISTINCT id, from_node_id, to_node_id, relation_kind, weight, note, created_at, updated_at
             FROM memory_relations
             WHERE from_node_id IN ({}) OR to_node_id IN ({})
             ORDER BY updated_at DESC",
            placeholders.join(", "),
            placeholders.join(", ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = node_ids
            .iter()
            .map(|id| id as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt.query_map(params.as_slice(), row_to_memory_relation)?;
        let mut relations = Vec::new();
        for row in rows {
            relations.push(row.with_context(|| "批量读取 memory_relations 行失败")?);
        }
        Ok(relations)
    }

    /// 删除指定记忆关系。
    pub(crate) fn delete_memory_relation(&self, relation_id: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM memory_relations WHERE id = ?1",
                rusqlite::params![relation_id],
            )
            .with_context(|| format!("删除记忆关系失败: {relation_id}"))?;
        Ok(())
    }

    /// 读取图关系邻接节点，供 deep recall 继续展开使用。
    pub(crate) fn list_related_node_ids(&self, node_ids: &[String]) -> Result<Vec<String>> {
        let mut related = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT CASE WHEN from_node_id = ?1 THEN to_node_id ELSE from_node_id END
             FROM memory_relations
             WHERE from_node_id = ?1 OR to_node_id = ?1
             ORDER BY weight DESC, updated_at DESC",
        )?;
        for node_id in node_ids {
            let rows = stmt.query_map(rusqlite::params![node_id], |row| row.get::<_, String>(0))?;
            for row in rows {
                related.push(row?);
            }
        }
        Ok(dedupe_strings(related))
    }

    /// 暴露底层连接，仅供 `db::migration` 模块访问。
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    /// 按节点 ID 加载完整内容，供 LoadDepth2 定向展开使用。
    ///
    /// 返回顺序与传入 ID 顺序一致；不存在或已归档的节点会被跳过。
    pub(crate) fn load_expanded_memories(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<ExpandedMemory>> {
        let mut items = Vec::new();
        for node_id in node_ids {
            let item = self.load_expanded_memory(node_id)?;
            if let Some(item) = item {
                self.mark_node_used(node_id)?;
                items.push(item);
            }
        }
        Ok(items)
    }

    /// 按节点 ID 加载 RecallHit 元数据，供 deep recall 关系追溯把源节点补回候选集。
    ///
    /// 返回顺序与传入 ID 顺序一致；不存在或已归档的节点会被跳过。
    pub(crate) fn load_recall_hits_by_ids(&self, node_ids: &[String]) -> Result<Vec<RecallHit>> {
        let mut hits = Vec::new();
        for node_id in node_ids {
            let hit = self
                .conn
                .query_row(
                    "SELECT id, title, summary, kind, importance
                     FROM memory_nodes
                     WHERE id = ?1 AND status = 'active'",
                    rusqlite::params![node_id],
                    |row| {
                        let kind: String = row.get(3)?;
                        Ok(RecallHit {
                            node_id: row.get(0)?,
                            title: row.get(1)?,
                            summary: row.get(2)?,
                            score: 0.65,
                            kind: str_to_memory_kind(&kind),
                            importance: row.get::<_, f64>(4)?,
                            depth1_loaded: true,
                        })
                    },
                )
                .optional()
                .with_context(|| format!("查询 RecallHit 节点失败: {node_id}"))?;
            if let Some(hit) = hit {
                self.mark_node_used(node_id)?;
                hits.push(hit);
            }
        }
        Ok(hits)
    }

    /// 批量查询节点的 created_at 时间戳
    pub(crate) fn batch_load_created_at(
        &self,
        node_ids: &[&str],
    ) -> Result<std::collections::HashMap<String, String>> {
        if node_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders: Vec<String> = node_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect();
        let sql = format!(
            "SELECT id, created_at FROM memory_nodes WHERE id IN ({}) AND status = 'active'",
            placeholders.join(",")
        );
        let params: Vec<&dyn rusqlite::ToSql> = node_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let mut map = std::collections::HashMap::new();
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params.as_slice())?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let created_at: String = row.get(1)?;
            map.insert(id, created_at);
        }
        Ok(map)
    }

    /// 加载最近的活跃记忆节点，按创建时间降序排列。
    ///
    /// 用于元查询（"上次做了什么"等）场景，跳过 BM25 直接返回最近记忆。
    pub(crate) fn recent_active_nodes(&self, limit: usize) -> Result<Vec<MemoryNode>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, memory_type, scope_type, scope_id, title, summary,
                    keywords, importance, confidence, source, status, created_at
             FROM memory_nodes
             WHERE status = 'active'
             ORDER BY created_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], |row| {
            let id: String = row.get(0)?;
            let kind: String = row.get(1)?;
            let memory_type: String = row.get(2)?;
            let scope_type: String = row.get(3)?;
            let scope_id: Option<String> = row.get(4)?;
            let title: String = row.get(5)?;
            let summary: String = row.get(6)?;
            let keywords_json: String = row.get(7)?;
            let importance: f32 = row.get(8)?;
            let confidence: f32 = row.get(9)?;
            let source: Option<String> = row.get(10)?;
            let status: String = row.get(11)?;
            let created_at: String = row.get(12)?;
            Ok((
                id,
                kind,
                memory_type,
                scope_type,
                scope_id,
                title,
                summary,
                keywords_json,
                importance,
                confidence,
                source,
                status,
                created_at,
            ))
        })?;
        let mut nodes = Vec::new();
        for row in rows {
            let (
                id,
                kind,
                memory_type,
                scope_type,
                scope_id,
                title,
                summary,
                keywords_json,
                importance,
                confidence,
                source,
                status,
                created_at,
            ) = row?;
            let keywords: Vec<String> = serde_json::from_str(&keywords_json).unwrap_or_default();
            let updated_at = created_at.clone();
            nodes.push(MemoryNode {
                id,
                kind: str_to_memory_kind(&kind),
                memory_type: str_to_memory_cognitive_type(&memory_type),
                scope_type: str_to_scope_type(&scope_type),
                scope_id,
                title,
                summary,
                keywords,
                importance,
                confidence,
                source,
                status: str_to_memory_status(&status),
                usage_count: 0,
                last_used_at: None,
                created_at,
                updated_at,
            });
        }
        Ok(nodes)
    }

    fn load_expanded_memory(&self, node_id: &str) -> Result<Option<ExpandedMemory>> {
        let row = self
            .conn
            .query_row(
                "SELECT n.kind, n.title, n.summary,
                        ep.full_content,
                        en.full_content,
                        de.full_content,
                        ev.evidence_path,
                        ev.byte_size
                 FROM memory_nodes n
                 LEFT JOIN episodes ep ON ep.id = n.id
                 LEFT JOIN entities en ON en.id = n.id
                 LEFT JOIN decisions de ON de.id = n.id
                 LEFT JOIN evidence ev ON ev.id = n.id
                 WHERE n.id = ?1 AND n.status = 'active'",
                rusqlite::params![node_id],
                |row| {
                    Ok(ExpandedRow {
                        kind: row.get(0)?,
                        title: row.get(1)?,
                        summary: row.get(2)?,
                        episode_content: row.get(3)?,
                        entity_content: row.get(4)?,
                        decision_content: row.get(5)?,
                        evidence_path: row.get(6)?,
                        evidence_byte_size: row.get(7)?,
                    })
                },
            )
            .optional()
            .with_context(|| format!("查询展开节点失败: {node_id}"))?;

        let Some(row) = row else {
            return Ok(None);
        };
        let full_content = match row.kind.as_str() {
            "episode" => row.episode_content,
            "entity" => row.entity_content,
            "decision" => row.decision_content,
            "evidence" => row.evidence_path.map(|path| {
                serde_json::json!({
                    "kind": "evidence",
                    "title": row.title,
                    "summary": row.summary,
                    "evidence_path": path,
                    "byte_size": row.evidence_byte_size.unwrap_or_default(),
                })
                .to_string()
            }),
            _ => None,
        };

        Ok(full_content.map(|full_content| ExpandedMemory {
            node_id: node_id.to_string(),
            full_content,
        }))
    }

    fn mark_node_used(&self, node_id: &str) -> Result<()> {
        let now = chrono::Local::now().naive_local().to_string();
        self.conn
            .execute(
                "UPDATE memory_nodes
                 SET usage_count = usage_count + 1, last_used_at = ?1, updated_at = ?1
                 WHERE id = ?2",
                rusqlite::params![now, node_id],
            )
            .with_context(|| format!("更新节点使用状态失败: {node_id}"))?;
        Ok(())
    }

    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    fn upsert_memory_node(
        &self,
        id: &str,
        kind: MemoryKind,
        scope_type: MemoryScopeType,
        scope_id: Option<&str>,
        title: &str,
        summary: &str,
        keywords: &str,
        importance: f32,
        source: Option<&str>,
        memory_type: MemoryCognitiveType,
        created_at: &str,
        updated_at: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO memory_nodes
                 (id, kind, memory_type, scope_type, scope_id, title, summary, keywords, importance,
                  confidence, status, source, usage_count, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1.0, 'active', ?10, 0, ?11, ?12)",
                rusqlite::params![
                    id,
                    memory_kind_to_str(kind),
                    memory_cognitive_type_to_str(&memory_type),
                    scope_type_to_str(scope_type),
                    scope_id,
                    title,
                    summary,
                    keywords,
                    importance,
                    source,
                    created_at,
                    updated_at,
                ],
            )
            .with_context(|| "写入 memory_nodes 失败")?;
        Ok(())
    }
}

fn row_to_memory_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryNode> {
    let kind_raw: String = row.get(1)?;
    let memory_type_raw: String = row.get(2)?;
    let scope_raw: String = row.get(3)?;
    let keywords_raw: String = row.get(7)?;
    let status_raw: String = row.get(10)?;
    Ok(MemoryNode {
        id: row.get(0)?,
        kind: str_to_memory_kind(&kind_raw),
        memory_type: str_to_memory_cognitive_type(&memory_type_raw),
        scope_type: str_to_scope_type(&scope_raw),
        scope_id: row.get(4)?,
        title: row.get(5)?,
        summary: row.get(6)?,
        keywords: serde_json::from_str(&keywords_raw).unwrap_or_default(),
        importance: row.get::<_, f64>(8)? as f32,
        confidence: row.get::<_, f64>(9)? as f32,
        status: str_to_memory_status(&status_raw),
        source: row.get(11)?,
        usage_count: row.get(12)?,
        last_used_at: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
    })
}

fn row_to_memory_relation(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRelation> {
    let kind_raw: String = row.get(3)?;
    Ok(MemoryRelation {
        id: row.get(0)?,
        from_node_id: row.get(1)?,
        to_node_id: row.get(2)?,
        relation_kind: str_to_memory_relation_kind(&kind_raw),
        weight: row.get::<_, f64>(4)? as f32,
        note: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn memory_status_to_str(status: &MemoryStatus) -> &'static str {
    match status {
        MemoryStatus::Active => "active",
        MemoryStatus::Archived => "archived",
    }
}

fn str_to_scope_type(value: &str) -> MemoryScopeType {
    match value {
        "global" => MemoryScopeType::Global,
        "session" => MemoryScopeType::Session,
        _ => MemoryScopeType::Workspace,
    }
}

fn str_to_memory_status(value: &str) -> MemoryStatus {
    match value {
        "archived" => MemoryStatus::Archived,
        _ => MemoryStatus::Active,
    }
}

fn memory_cognitive_type_to_str(memory_type: &MemoryCognitiveType) -> &'static str {
    match memory_type {
        MemoryCognitiveType::Factual => "factual",
        MemoryCognitiveType::UserPreference => "user_preference",
        MemoryCognitiveType::UserHabit => "user_habit",
        MemoryCognitiveType::Skill => "skill",
        MemoryCognitiveType::ProjectStructure => "project_structure",
        MemoryCognitiveType::ArchitectureDecision => "architecture_decision",
        MemoryCognitiveType::ProblemIncident => "problem_incident",
        MemoryCognitiveType::DomainKnowledge => "domain_knowledge",
    }
}

fn str_to_memory_cognitive_type(value: &str) -> MemoryCognitiveType {
    match value {
        "user_preference" => MemoryCognitiveType::UserPreference,
        "user_habit" => MemoryCognitiveType::UserHabit,
        "skill" => MemoryCognitiveType::Skill,
        "project_structure" => MemoryCognitiveType::ProjectStructure,
        "architecture_decision" => MemoryCognitiveType::ArchitectureDecision,
        "problem_incident" => MemoryCognitiveType::ProblemIncident,
        "domain_knowledge" => MemoryCognitiveType::DomainKnowledge,
        _ => MemoryCognitiveType::Factual,
    }
}

fn memory_relation_kind_to_str(kind: &MemoryRelationKind) -> &'static str {
    match kind {
        MemoryRelationKind::RelatedTo => "related_to",
        MemoryRelationKind::DependsOn => "depends_on",
        MemoryRelationKind::Supports => "supports",
        MemoryRelationKind::Contradicts => "contradicts",
        MemoryRelationKind::Supersedes => "supersedes",
        MemoryRelationKind::CausedBy => "caused_by",
        MemoryRelationKind::BelongsTo => "belongs_to",
        MemoryRelationKind::LearnedFrom => "learned_from",
        MemoryRelationKind::ValidatedBy => "validated_by",
    }
}

fn str_to_memory_relation_kind(value: &str) -> MemoryRelationKind {
    match value {
        "depends_on" => MemoryRelationKind::DependsOn,
        "supports" => MemoryRelationKind::Supports,
        "contradicts" => MemoryRelationKind::Contradicts,
        "supersedes" => MemoryRelationKind::Supersedes,
        "caused_by" => MemoryRelationKind::CausedBy,
        "belongs_to" => MemoryRelationKind::BelongsTo,
        "learned_from" => MemoryRelationKind::LearnedFrom,
        "validated_by" => MemoryRelationKind::ValidatedBy,
        _ => MemoryRelationKind::RelatedTo,
    }
}

fn dedupe_strings(items: Vec<String>) -> Vec<String> {
    let mut deduped = Vec::new();
    for item in items {
        if !deduped.iter().any(|value| value == &item) {
            deduped.push(item);
        }
    }
    deduped
}

struct ExpandedRow {
    kind: String,
    title: String,
    summary: String,
    episode_content: Option<String>,
    entity_content: Option<String>,
    decision_content: Option<String>,
    evidence_path: Option<String>,
    evidence_byte_size: Option<i64>,
}

fn memory_base_path() -> PathBuf {
    crate::paths::memory_data_dir()
}

fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(profile));
    }
    None
}

/// 仅用于读取历史 v2 加密库：密钥基于宿主明确注入的存储根做词法归一化，不访问文件系统。
/// 这样同一存储根在宿主与 AppContainer 中得到完全相同的结果。
fn derive_db_password() -> String {
    derive_current_db_password(&crate::paths::storage_root())
}

fn derive_current_db_password(storage_root: &Path) -> String {
    let identity = stable_storage_root_identity(storage_root);
    let mut hasher = Sha256::new();
    hasher.update(DB_KEY_DOMAIN_V2);
    hasher.update(identity.as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)
}

#[cfg(windows)]
fn stable_storage_root_identity(storage_root: &Path) -> String {
    let value = storage_root.to_string_lossy().replace('/', "\\");
    let value = if let Some(rest) = value.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{rest}")
    } else if let Some(rest) = value.strip_prefix("\\\\?\\") {
        rest.to_string()
    } else {
        value
    };
    value.trim_end_matches('\\').to_lowercase()
}

#[cfg(not(windows))]
fn stable_storage_root_identity(storage_root: &Path) -> String {
    storage_root
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string()
}

/// 未包含 v2 兼容处理的历史版本使用 home 目录规范化路径的 SHA-256。
/// 不能仅依据插件版本判断密钥格式，必须先验证数据库再决定是否迁移。
fn derive_legacy_db_password(home: &Path) -> String {
    let digest = Sha256::digest(home.to_string_lossy().as_bytes());
    hex::encode(digest)
}

fn legacy_db_passwords() -> Vec<String> {
    let mut passwords = Vec::new();
    let storage_root = crate::paths::storage_root();
    if storage_root
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(".tiangong"))
        && let Some(parent) = storage_root.parent()
    {
        push_legacy_password_candidates(parent, &mut passwords);
    }
    if let Some(home) = home_dir() {
        push_legacy_password_candidates(&home, &mut passwords);
    }
    passwords
}

fn push_legacy_password(passwords: &mut Vec<String>, path: &Path) {
    let password = derive_legacy_db_password(path);
    if !passwords.contains(&password) {
        passwords.push(password);
    }
}

fn push_legacy_password_candidates(home: &Path, passwords: &mut Vec<String>) {
    #[cfg(windows)]
    {
        let raw = home.to_string_lossy().replace('/', "\\");
        let extended = if raw.starts_with("\\\\?\\") {
            Some(raw.clone())
        } else if let Some(rest) = raw.strip_prefix("\\\\") {
            Some(format!("\\\\?\\UNC\\{rest}"))
        } else if raw.as_bytes().get(1) == Some(&b':') {
            Some(format!("\\\\?\\{raw}"))
        } else {
            None
        };
        if let Some(extended) = extended {
            push_legacy_password(passwords, Path::new(&extended));
        }
    }
    if let Ok(canonical) = home.canonicalize() {
        push_legacy_password(passwords, &canonical);
    }
    push_legacy_password(passwords, home);
}

#[allow(dead_code)]
fn memory_kind_to_str(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Episode => "episode",
        MemoryKind::Entity => "entity",
        MemoryKind::Decision => "decision",
        MemoryKind::Evidence => "evidence",
    }
}

fn str_to_memory_kind(kind: &str) -> MemoryKind {
    match kind {
        "entity" => MemoryKind::Entity,
        "decision" => MemoryKind::Decision,
        "evidence" => MemoryKind::Evidence,
        _ => MemoryKind::Episode,
    }
}

#[allow(dead_code)]
fn scope_type_to_str(scope_type: MemoryScopeType) -> &'static str {
    match scope_type {
        MemoryScopeType::Global => "global",
        MemoryScopeType::Workspace => "workspace",
        MemoryScopeType::Session => "session",
    }
}

#[allow(dead_code)]
fn entity_type_to_str(entity_type: &EntityType) -> &'static str {
    match entity_type {
        EntityType::Project => "project",
        EntityType::Repository => "repository",
        EntityType::Server => "server",
        EntityType::Skill => "skill",
        EntityType::Provider => "provider",
        EntityType::Document => "document",
        EntityType::Module => "module",
    }
}

/// 优先打开明文库，仅为历史加密库派生兼容密钥
fn open_plaintext_conn(db_path: &Path) -> Result<Connection> {
    let current_password = derive_db_password();
    let legacy_passwords = legacy_db_passwords();
    open_plaintext_conn_with_passwords(db_path, &current_password, &legacy_passwords)
}

fn open_plaintext_conn_with_passwords(
    db_path: &Path,
    current_password: &str,
    legacy_passwords: &[String],
) -> Result<Connection> {
    recover_interrupted_migration(db_path, current_password, legacy_passwords)?;
    match open_connection_with_key(db_path, "") {
        Ok(conn) => configure_connection(conn),
        Err(plain_error) => {
            for password in
                std::iter::once(current_password).chain(legacy_passwords.iter().map(String::as_str))
            {
                let Ok(conn) = open_connection_with_key(db_path, password) else {
                    continue;
                };
                return migrate_legacy_database(db_path, conn);
            }
            Err(plain_error).with_context(|| {
                format!(
                    "无法读取 Memory 数据库（明文及历史密钥均失败）: {}",
                    db_path.display()
                )
            })
        }
    }
}

fn open_connection_with_key(db_path: &Path, password: &str) -> Result<Connection> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("打开数据库失败: {}", db_path.display()))?;
    if !password.is_empty() {
        conn.pragma_update(None, "key", password)
            .with_context(|| "设置历史数据库密钥失败")?;
    }
    validate_database_key(&conn)?;
    Ok(conn)
}

fn validate_database_key(conn: &Connection) -> Result<()> {
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
        row.get::<_, i64>(0)
    })
    .map(|_| ())
    .with_context(|| "验证数据库加密密钥失败")
}

fn configure_connection(conn: Connection) -> Result<Connection> {
    let mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        .with_context(|| "设置 WAL 模式失败")?;
    if !mode.eq_ignore_ascii_case("wal") {
        bail!("设置 WAL 模式失败: 返回模式为 {mode}");
    }
    Ok(conn)
}

fn migrate_legacy_database(db_path: &Path, legacy_conn: Connection) -> Result<Connection> {
    verify_database_integrity(&legacy_conn).with_context(|| "旧 Memory 数据库完整性检查失败")?;
    checkpoint_wal(&legacy_conn)?;
    let staged = StagedDatabase::new(db_path)?;
    export_plaintext_database(&legacy_conn, &staged.path)?;
    drop(legacy_conn);
    // 最后一个连接关闭后 SQLite 会移除日志；仍存在日志时拒绝替换，避免并发连接继续写旧库。
    if database_files(db_path)
        .iter()
        .skip(1)
        .any(|path| path.exists())
    {
        bail!("Memory 日志仍被占用，暂不替换数据库");
    }
    install_migrated_database(db_path, &staged)
}

fn checkpoint_wal(conn: &Connection) -> Result<()> {
    let (busy, log_frames, checkpointed_frames): (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .with_context(|| "执行旧 Memory 数据库 WAL 检查点失败")?;
    if busy != 0 || (log_frames >= 0 && checkpointed_frames < log_frames) {
        bail!(
            "旧 Memory 数据库 WAL 检查点未完成: busy={busy}, log={log_frames}, checkpointed={checkpointed_frames}"
        );
    }
    Ok(())
}

fn export_plaintext_database(conn: &Connection, staged_path: &Path) -> Result<()> {
    conn.execute(
        "ATTACH DATABASE ?1 AS plaintext KEY ''",
        [staged_path.to_string_lossy().as_ref()],
    )?;
    conn.query_row("SELECT sqlcipher_export('plaintext')", [], |_| Ok(()))
        .with_context(|| "导出普通 SQLite 数据库失败")?;
    for pragma in ["user_version", "application_id"] {
        let value: i64 = conn.pragma_query_value(None, pragma, |row| row.get(0))?;
        conn.pragma_update(Some("plaintext"), pragma, value)?;
    }
    // 除完整性外，逐表核对数量及双向内容差集，包括所有关联表。
    let tables = conn
        .prepare("SELECT name FROM main.sqlite_schema WHERE type='table'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for table in tables {
        let quoted = format!("\"{}\"", table.replace('"', "\"\""));
        let sql = format!(
            "SELECT (SELECT count(*) FROM main.{quoted}) = (SELECT count(*) FROM plaintext.{quoted})
             AND NOT EXISTS (SELECT * FROM main.{quoted} EXCEPT SELECT * FROM plaintext.{quoted})
             AND NOT EXISTS (SELECT * FROM plaintext.{quoted} EXCEPT SELECT * FROM main.{quoted})"
        );
        if !conn.query_row(&sql, [], |row| row.get::<_, bool>(0))? {
            bail!("Memory 明文副本内容不一致: {table}");
        }
    }
    conn.execute_batch("DETACH DATABASE plaintext")?;
    let reopened = open_connection_with_key(staged_path, "")?;
    verify_database_integrity(&reopened)?;
    drop(reopened);
    std::fs::OpenOptions::new()
        .write(true)
        .open(staged_path)?
        .sync_all()?;
    Ok(())
}

fn verify_database_integrity(conn: &Connection) -> Result<()> {
    validate_database_key(conn)?;
    for pragma in ["cipher_integrity_check", "integrity_check"] {
        let mut failures = Vec::new();
        conn.pragma_query(None, pragma, |row| {
            let message = row.get::<_, String>(0)?;
            if !message.eq_ignore_ascii_case("ok") {
                failures.push(message);
            }
            Ok(())
        })
        .with_context(|| format!("执行 {pragma} 失败"))?;
        if !failures.is_empty() {
            bail!("{pragma} 未通过: {}", failures.join("; "));
        }
    }
    Ok(())
}

struct StagedDatabase {
    path: PathBuf,
}

impl StagedDatabase {
    fn new(db_path: &Path) -> Result<Self> {
        let parent = db_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Memory 数据库缺少父目录"))?;
        let path = parent.join(format!(".metadata.db.key-migration-{}.tmp", scru128::new()));
        if path.exists() {
            bail!("Memory 密钥迁移临时文件已存在: {}", path.display());
        }
        Ok(Self { path })
    }
}

impl Drop for StagedDatabase {
    fn drop(&mut self) {
        for path in database_files(&self.path) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn database_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut path = db_path.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

fn database_files(db_path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![db_path.to_path_buf()];
    paths.extend(
        ["-wal", "-shm", "-journal"]
            .into_iter()
            .map(|suffix| database_sidecar_path(db_path, suffix)),
    );
    paths
}

fn install_migrated_database(db_path: &Path, staged: &StagedDatabase) -> Result<Connection> {
    let parent = db_path.parent().context("Memory 数据库缺少父目录")?;
    let backup_path = parent.join(format!("{DB_KEY_BACKUP_PREFIX}{}.bak", scru128::new()));
    // 先复制并落盘备份，原库始终留在原位置，最后以同目录 rename 原子替换。
    // 进程在替换前退出仍读取原库，替换后退出则读取已校验的明文库。
    let mut backup = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup_path)?;
    std::io::copy(&mut std::fs::File::open(db_path)?, &mut backup)?;
    backup.sync_all()?;
    drop(backup);
    std::fs::rename(&staged.path, db_path)
        .with_context(|| "启用 Memory 明文数据库失败，原库及备份已保留")?;
    let conn = open_connection_with_key(db_path, "").and_then(configure_connection)?;
    tracing::info!(database = %db_path.display(), backup = %backup_path.display(), "Memory 已转换为普通 SQLite，原加密库备份已保留");
    Ok(conn)
}

/// 兼容 #521 先移动原库、后安装副本的中断状态。恢复完成前绝不创建空库。
fn recover_interrupted_migration(
    db_path: &Path,
    current_password: &str,
    legacy_passwords: &[String],
) -> Result<()> {
    if db_path.try_exists()? {
        return Ok(());
    }
    let parent = db_path.parent().context("Memory 数据库缺少父目录")?;
    let mut backups = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if (name.starts_with(LEGACY_BACKUP_PREFIX) || name.starts_with(DB_KEY_BACKUP_PREFIX))
            && name.ends_with(".bak")
        {
            backups.push(entry.path());
        }
    }
    // SCRU128 后缀按时间排序，不按备份格式排序。最新备份损坏时明确失败，不静默退回更旧数据。
    backups.sort_by_key(|path| {
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .rsplit('-')
            .next()
            .unwrap()
            .to_string()
    });
    let Some(backup) = backups.last() else {
        if database_files(db_path)
            .iter()
            .skip(1)
            .any(|path| path.exists())
        {
            bail!("Memory 主库缺失但存在日志文件，拒绝创建空数据库");
        }
        return Ok(());
    };
    let staged = StagedDatabase::new(db_path)?;
    if std::fs::metadata(backup)?.len() == 0 {
        bail!("Memory 迁移备份为空，拒绝创建空库");
    }
    std::fs::copy(backup, &staged.path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let moved = database_sidecar_path(backup, suffix);
        let original = database_sidecar_path(db_path, suffix);
        if !moved.try_exists()? && original.try_exists()? {
            // 旧流程可能只移动了主库。先把尚未移动的日志保存在备份旁，
            // 使本次恢复在清理旧日志后再次退出时仍有完整的重试来源。
            let saved = StagedDatabase::new(db_path)?;
            std::fs::copy(&original, &saved.path)?;
            std::fs::OpenOptions::new()
                .write(true)
                .open(&saved.path)?
                .sync_all()?;
            std::fs::rename(&saved.path, &moved)?;
        }
        if moved.try_exists()? {
            std::fs::copy(moved, database_sidecar_path(&staged.path, suffix))?;
        }
    }
    let mut recovered = None;
    for password in std::iter::once("")
        .chain(std::iter::once(current_password))
        .chain(legacy_passwords.iter().map(String::as_str))
    {
        if let Ok(conn) = open_connection_with_key(&staged.path, password) {
            recovered = Some(conn);
            break;
        }
    }
    let conn = recovered.context("Memory 迁移备份无法读取；已保留备份，拒绝创建空库")?;
    verify_database_integrity(&conn)?;
    checkpoint_wal(&conn)?;
    drop(conn);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&staged.path)?
        .sync_all()?;
    for path in database_files(db_path).iter().skip(1) {
        if path.try_exists()? {
            std::fs::remove_file(path)?;
        }
    }
    std::fs::rename(&staged.path, db_path)?;
    tracing::info!(backup = %backup.display(), "已恢复中断的 Memory 数据库迁移");
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;

    pub(crate) fn create_empty_encrypted_database(data_dir: &Path, use_v2: bool) {
        std::fs::create_dir_all(data_dir).unwrap();
        let password = if use_v2 {
            derive_db_password()
        } else {
            legacy_db_passwords().into_iter().next().unwrap()
        };
        let conn = open_connection_with_key(&data_dir.join("metadata.db"), &password).unwrap();
        schema::init_schema(&conn).unwrap();
    }

    /// 创建仅用于测试的内存数据库（不加密）
    pub(crate) fn open_in_memory() -> Result<MemoryDb> {
        let conn = Connection::open_in_memory().with_context(|| "创建测试内存数据库失败")?;
        schema::init_schema(&conn)?;
        Ok(MemoryDb { conn })
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::open_in_memory;
    use super::{
        DB_KEY_BACKUP_PREFIX, derive_current_db_password, open_connection_with_key,
        open_plaintext_conn_with_passwords,
    };
    use crate::types::{Episode, EpisodeOutcome};

    fn make_episode(session_id: &str) -> Episode {
        Episode::new(
            session_id.to_string(),
            "测试标题".to_string(),
            "测试摘要".to_string(),
            EpisodeOutcome::Success,
            vec!["关键词A".to_string(), "关键词B".to_string()],
            vec!["tool_call_1".to_string()],
            0.7,
        )
    }

    #[test]
    #[ignore = "手动提供已有数据库路径，在副本上验证，不修改原数据"]
    fn existing_database_copy_converts_to_plaintext() {
        let source = std::path::PathBuf::from(
            std::env::var_os("MEMORY_PROBE_DATABASE").expect("需要数据库路径"),
        );
        let storage = std::path::PathBuf::from(
            std::env::var_os("MEMORY_PROBE_STORAGE_ROOT").expect("需要原存储根"),
        );
        let original = std::fs::read(&source).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let copy = temp.path().join("metadata.db");
        std::fs::write(&copy, &original).unwrap();
        let wal = super::database_sidecar_path(&source, "-wal");
        if wal.exists() {
            std::fs::copy(wal, super::database_sidecar_path(&copy, "-wal")).unwrap();
        }
        let conn = open_plaintext_conn_with_passwords(
            &copy,
            &derive_current_db_password(&storage),
            &super::legacy_db_passwords(),
        )
        .unwrap();
        super::verify_database_integrity(&conn).unwrap();
        let nodes: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_nodes", [], |row| row.get(0))
            .unwrap();
        eprintln!("已有数据库副本完整性通过，记忆节点数={nodes}");
        super::checkpoint_wal(&conn).unwrap();
        drop(conn);
        assert_eq!(&std::fs::read(&copy).unwrap()[..16], b"SQLite format 3\0");
        if let Some(output) = std::env::var_os("MEMORY_PROBE_OUTPUT") {
            std::fs::copy(&copy, output).unwrap();
        }
        assert_eq!(std::fs::read(source).unwrap(), original);
    }

    fn create_encrypted_test_database(path: &std::path::Path, password: &str) {
        let conn = rusqlite::Connection::open(path).expect("创建加密测试数据库失败");
        conn.pragma_update(None, "key", password)
            .expect("设置加密测试数据库密钥失败");
        conn.pragma_update(None, "journal_mode", "WAL")
            .expect("设置加密测试数据库日志模式失败");
        conn.execute_batch(
            "CREATE TABLE migration_probe (value TEXT NOT NULL);\
             INSERT INTO migration_probe (value) VALUES ('preserved');",
        )
        .expect("写入加密测试数据库失败");
    }

    #[cfg(windows)]
    #[test]
    fn stable_password_ignores_windows_verbatim_prefix_and_case() {
        let regular = derive_current_db_password(std::path::Path::new(r"C:\Users\EDY\.tiangong"));
        let sandbox =
            derive_current_db_password(std::path::Path::new(r"\\?\c:\users\edy\.tiangong\"));
        assert_eq!(regular, sandbox);
    }

    #[test]
    fn legacy_database_is_decrypted_from_copy_and_original_is_backed_up() {
        let root = tempfile::tempdir().expect("创建迁移测试目录失败");
        let db_path = root.path().join("metadata.db");
        let legacy_password = "legacy-test-password".to_string();
        let current_password = "stable-v2-test-password";
        create_encrypted_test_database(&db_path, &legacy_password);
        let original = std::fs::read(&db_path).expect("读取迁移前数据库失败");

        let conn = open_plaintext_conn_with_passwords(
            &db_path,
            current_password,
            std::slice::from_ref(&legacy_password),
        )
        .expect("迁移旧密钥数据库失败");
        let value: String = conn
            .query_row("SELECT value FROM migration_probe", [], |row| row.get(0))
            .expect("读取迁移后数据失败");
        assert_eq!(value, "preserved");
        drop(conn);

        let backup = std::fs::read_dir(root.path())
            .expect("读取迁移测试目录失败")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(DB_KEY_BACKUP_PREFIX))
            })
            .expect("迁移后未保留原数据库备份");
        assert_eq!(
            std::fs::read(&backup).expect("读取旧数据库备份失败"),
            original
        );
        let backup_conn =
            open_connection_with_key(&backup, &legacy_password).expect("旧密钥无法打开迁移备份");
        let backup_value: String = backup_conn
            .query_row("SELECT value FROM migration_probe", [], |row| row.get(0))
            .expect("读取迁移备份数据失败");
        assert_eq!(backup_value, "preserved");
        drop(backup_conn);

        assert!(open_connection_with_key(&db_path, "").is_ok());
        assert!(open_connection_with_key(&db_path, &legacy_password).is_err());
    }

    #[test]
    fn plaintext_new_and_v2_databases_reopen_without_password() {
        for password in [None, Some("stable-v2-test-password")] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("metadata.db");
            if let Some(password) = password {
                create_encrypted_test_database(&path, password);
            }
            let conn =
                open_plaintext_conn_with_passwords(&path, "stable-v2-test-password", &[]).unwrap();
            conn.execute_batch("CREATE TABLE IF NOT EXISTS plain_probe (id TEXT PRIMARY KEY); INSERT INTO plain_probe VALUES ('readable');").unwrap();
            drop(conn);
            assert_eq!(&std::fs::read(&path).unwrap()[..16], b"SQLite format 3\0");
            for _ in 0..2 {
                let conn = open_plaintext_conn_with_passwords(&path, "unrelated-storage-root", &[])
                    .unwrap();
                assert_eq!(
                    conn.query_row("SELECT id FROM plain_probe", [], |row| row
                        .get::<_, String>(0))
                        .unwrap(),
                    "readable"
                );
            }
        }
    }

    #[test]
    fn interrupted_old_migration_recovers_backup_and_preserves_it() {
        for password in ["legacy-test-password", "stable-v2-test-password"] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("metadata.db");
            create_encrypted_test_database(&path, password);
            let backup = root.path().join(format!(
                "{}{}.bak",
                super::LEGACY_BACKUP_PREFIX,
                scru128::new()
            ));
            std::fs::rename(&path, &backup).unwrap();
            let bytes = std::fs::read(&backup).unwrap();
            for _ in 0..2 {
                let conn = open_plaintext_conn_with_passwords(
                    &path,
                    "stable-v2-test-password",
                    &["legacy-test-password".into()],
                )
                .unwrap();
                assert_eq!(
                    conn.query_row("SELECT value FROM migration_probe", [], |row| row
                        .get::<_, String>(0))
                        .unwrap(),
                    "preserved"
                );
            }
            assert_eq!(std::fs::read(&backup).unwrap(), bytes);
        }
    }

    #[test]
    fn interrupted_migration_recovers_unmoved_wal() {
        let source = tempfile::tempdir().unwrap();
        let source_path = source.path().join("metadata.db");
        let conn = open_connection_with_key(&source_path, "legacy-test-password").unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch("CREATE TABLE migration_probe (value TEXT); INSERT INTO migration_probe VALUES ('preserved-in-wal');").unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("metadata.db");
        let backup = root.path().join(format!(
            "{}{}.bak",
            super::LEGACY_BACKUP_PREFIX,
            scru128::new()
        ));
        std::fs::copy(&source_path, &backup).unwrap();
        std::fs::copy(
            super::database_sidecar_path(&source_path, "-wal"),
            super::database_sidecar_path(&path, "-wal"),
        )
        .unwrap();
        let recovered =
            open_plaintext_conn_with_passwords(&path, "v2", &["legacy-test-password".into()])
                .unwrap();
        assert_eq!(
            recovered
                .query_row("SELECT value FROM migration_probe", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "preserved-in-wal"
        );
        assert!(super::database_sidecar_path(&backup, "-wal").exists());
    }

    #[test]
    fn unreadable_migration_backup_never_creates_empty_database() {
        for bytes in [b"".as_slice(), b"corrupted backup".as_slice()] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("metadata.db");
            let backup = root.path().join(format!(
                "{}{}.bak",
                super::LEGACY_BACKUP_PREFIX,
                scru128::new()
            ));
            std::fs::write(&backup, bytes).unwrap();
            assert!(open_plaintext_conn_with_passwords(&path, "v2", &[]).is_err());
            assert!(!path.exists());
            assert_eq!(std::fs::read(&backup).unwrap(), bytes);
        }
    }

    #[test]
    fn windows_path_search_and_count_treat_backslashes_and_wildcards_literally() {
        let db = open_in_memory().unwrap();
        let mut episode = make_episode("path-session");
        episode.title = r"D:\work_100%\notes\memory.txt".into();
        db.insert_episode(&episode, None).unwrap();
        for query in [episode.title.as_str(), r"D:\", r"work_100%\", r"\notes\"] {
            assert_eq!(
                db.list_memory_nodes(None, Some(query), None, None, 0, 10)
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(
                db.count_memory_nodes(None, Some(query), None, None)
                    .unwrap(),
                1
            );
        }
        assert_eq!(
            db.count_memory_nodes(None, Some(r"workX100Y\"), None, None)
                .unwrap(),
            0
        );
    }

    #[test]
    fn unknown_legacy_key_does_not_modify_original_database() {
        let root = tempfile::tempdir().expect("创建失败迁移测试目录失败");
        let db_path = root.path().join("metadata.db");
        let legacy_password = "actual-legacy-test-password";
        create_encrypted_test_database(&db_path, legacy_password);
        let original = std::fs::read(&db_path).expect("读取失败迁移前数据库失败");

        let result = open_plaintext_conn_with_passwords(
            &db_path,
            "stable-v2-test-password",
            &["wrong-legacy-test-password".to_string()],
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(&db_path).expect("读取失败迁移后数据库失败"),
            original
        );
        assert!(open_connection_with_key(&db_path, legacy_password).is_ok());
    }

    #[test]
    fn insert_episode_stores_workspace_id_in_scope_id() {
        let db = open_in_memory().unwrap();
        let episode = make_episode("sess-001");
        let workspace_id = "ws-project-x";

        db.insert_episode(&episode, Some(workspace_id)).unwrap();

        // 验证 scope_id 正确写入
        let stored: Option<String> = db
            .conn
            .query_row(
                "SELECT scope_id FROM memory_nodes WHERE id = ?1",
                rusqlite::params![episode.id],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(stored.as_deref(), Some(workspace_id));
    }

    #[test]
    fn insert_episode_scope_id_is_null_when_no_workspace() {
        let db = open_in_memory().unwrap();
        let episode = make_episode("sess-002");

        db.insert_episode(&episode, None).unwrap();

        let stored: Option<String> = db
            .conn
            .query_row(
                "SELECT scope_id FROM memory_nodes WHERE id = ?1",
                rusqlite::params![episode.id],
                |row| row.get(0),
            )
            .unwrap();

        assert!(stored.is_none(), "无 workspace_id 时 scope_id 应为 NULL");
    }

    #[test]
    fn recent_episode_summaries_returns_correct_count() {
        let db = open_in_memory().unwrap();
        for i in 0..5 {
            let ep = make_episode(&format!("sess-{i}"));
            db.insert_episode(&ep, Some("ws-test")).unwrap();
        }

        let summaries = db.recent_episode_summaries(3).unwrap();
        assert_eq!(summaries.len(), 3, "应返回最近 3 条");
    }

    #[test]
    fn recent_episodes_filters_by_workspace_scope() {
        let db = open_in_memory().unwrap();
        let episode_a = make_episode("sess-ws-a");
        let episode_b = make_episode("sess-ws-b");
        let id_a = episode_a.id.clone();
        let id_b = episode_b.id.clone();

        db.insert_episode(&episode_a, Some("ws-a")).unwrap();
        db.insert_episode(&episode_b, Some("ws-b")).unwrap();

        let ws_a = db.recent_episodes(Some("ws-a"), 10).unwrap();
        let ws_b = db.recent_episodes(Some("ws-b"), 10).unwrap();

        assert_eq!(ws_a.len(), 1);
        assert_eq!(ws_a[0].id, id_a);
        assert_eq!(ws_b.len(), 1);
        assert_eq!(ws_b[0].id, id_b);
    }

    #[test]
    fn recent_episodes_for_session_filters_by_workspace_and_session() {
        let db = open_in_memory().unwrap();
        let episode_a1 = make_episode("session-a");
        let episode_a2 = make_episode("session-a");
        let episode_b = make_episode("session-b");
        let id_a1 = episode_a1.id.clone();
        let id_a2 = episode_a2.id.clone();

        db.insert_episode(&episode_a1, Some("ws-shared")).unwrap();
        db.insert_episode(&episode_b, Some("ws-shared")).unwrap();
        db.insert_episode(&episode_a2, Some("ws-shared")).unwrap();

        let items = db
            .recent_episodes_for_session(Some("ws-shared"), "session-a", 10)
            .unwrap();

        assert_eq!(items.len(), 2);
        assert!(
            items
                .iter()
                .all(|episode| episode.session_id == "session-a")
        );
        let ids = items
            .iter()
            .map(|episode| episode.id.as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&id_a1.as_str()));
        assert!(ids.contains(&id_a2.as_str()));
    }

    #[test]
    fn load_expanded_memories_returns_full_content_in_requested_order() {
        let db = open_in_memory().unwrap();
        let episode_a = make_episode("sess-depth2-a");
        let episode_b = make_episode("sess-depth2-b");
        let id_a = episode_a.id.clone();
        let id_b = episode_b.id.clone();

        db.insert_episode(&episode_a, Some("ws-depth2")).unwrap();
        db.insert_episode(&episode_b, Some("ws-depth2")).unwrap();

        let expanded = db
            .load_expanded_memories(&[id_b.clone(), "missing-node".to_string(), id_a.clone()])
            .unwrap();

        assert_eq!(expanded.len(), 2);
        assert_eq!(expanded[0].node_id, id_b);
        assert_eq!(expanded[1].node_id, id_a);
        assert!(expanded[0].full_content.contains("sess-depth2-b"));
        assert!(expanded[1].full_content.contains("sess-depth2-a"));
    }

    #[test]
    fn load_recall_hits_by_ids_returns_metadata_for_relation_trace() {
        let db = open_in_memory().unwrap();
        let episode = make_episode("sess-relation-source");
        let id = episode.id.clone();
        let title = episode.title.clone();

        db.insert_episode(&episode, Some("ws-relation")).unwrap();

        let hits = db
            .load_recall_hits_by_ids(&["missing-node".to_string(), id.clone()])
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node_id, id);
        assert_eq!(hits[0].title, title);
        assert_eq!(hits[0].kind, crate::types::MemoryKind::Episode);
        assert!(hits[0].depth1_loaded);
    }
}
