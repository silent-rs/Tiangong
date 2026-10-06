//! 会话元数据：UI 展示与配置构建所需的轻量视图。
//!
//! 完整 `Session`（含 messages/context_summary/token 累计等）是磁盘真相源，
//! app-state 不再持有完整列表。本结构只承载 UI 列表展示与会话级配置构建
//! 必需的字段，作为磁盘的 **只读缓存视图**（可短暂数秒延迟）。

use std::path::Path;

use serde::{Deserialize, Serialize};
use tiangong_core::session::{Session, SessionCwdMode};
use tiangong_types::TrustMode;

/// 会话元数据：UI 展示 + 配置构建必需集。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub trust_mode: TrustMode,
    /// 会话级思考强度；为空时使用应用级默认值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<tiangong_llm::request::ReasoningEffort>,
    /// 会话级工作目录（工具执行时的根目录）。
    #[serde(default)]
    pub cwd: String,
    /// 工作目录模式。
    #[serde(default)]
    pub cwd_mode: SessionCwdMode,
    /// 消息条数（UI 列表展示）。P3 移除完整 Session 后需另从磁盘/缓存取。
    #[serde(default)]
    pub message_count: usize,
    /// 父会话 ID（Worker 子会话标注；UI 列表按此过滤掉子会话）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}

impl From<&Session> for SessionMetadata {
    fn from(session: &Session) -> Self {
        Self {
            id: session.id.clone(),
            title: session.title.clone(),
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            trust_mode: session.trust_mode,
            reasoning_effort: session.reasoning_effort,
            cwd: session.cwd.clone(),
            cwd_mode: session.cwd_mode.clone(),
            message_count: session.messages.len(),
            parent_session_id: session.parent_session_id.clone(),
        }
    }
}

impl SessionMetadata {
    /// 从磁盘 session 文件构造元数据（只读浅字段，不反序列化 messages 等重组件）。
    ///
    /// 复用 `Session::session_file_path` 的路径校验，但读取后只解析为
    /// `serde_json::Value` 取浅字段，避免反序列化完整的 `Session`（尤其 `messages`）。
    pub fn load_from_storage(storage_root: &Path, session_id: &str) -> Result<Self, String> {
        let path = crate::session_file_path(storage_root, session_id)?;
        let content = std::fs::read(&path)
            .map_err(|error| format!("读取会话文件失败（{}）：{error}", path.display()))?;
        Self::from_slice(&content)
            .map_err(|error| format!("解析会话文件失败（{}）：{error}", path.display()))
    }

    /// 从会话文件内容解析元数据。
    ///
    /// 只构造浅字段；`messages` 逐条跳过只计数，其余大字段（上下文摘要等）
    /// 直接跳过，不为整份会话建立 JSON 树——会话列表需要扫描全部会话文件，
    /// 构造完整 `Value` 会让侧栏刷新耗时随历史体量线性放大。
    pub fn from_slice(content: &[u8]) -> Result<Self, serde_json::Error> {
        let raw: RawMetadata = serde_json::from_slice(content)?;
        let pick_str = |value: Option<serde_json::Value>| -> String {
            match value {
                Some(serde_json::Value::String(text)) => text,
                _ => String::new(),
            }
        };
        Ok(Self {
            id: pick_str(raw.id),
            title: pick_str(raw.title),
            created_at: pick_str(raw.created_at),
            updated_at: pick_str(raw.updated_at),
            trust_mode: raw
                .trust_mode
                .and_then(|item| serde_json::from_value(item).ok())
                .unwrap_or_default(),
            reasoning_effort: raw
                .reasoning_effort
                .as_ref()
                .and_then(|item| item.as_str())
                .map(tiangong_llm::request::ReasoningEffort::parse_flexible),
            cwd: pick_str(raw.cwd),
            cwd_mode: raw
                .cwd_mode
                .and_then(|item| serde_json::from_value(item).ok())
                .unwrap_or_default(),
            message_count: raw.messages.0,
            parent_session_id: raw
                .parent_session_id
                .as_ref()
                .and_then(|item| item.as_str())
                .map(str::to_string),
        })
    }
}

/// 会话文件中元数据所需的浅字段；未列出的字段由 serde 跳过。
/// 字段取 `Value` 保持对异常取值的容错（类型不符时回落默认值而非整体失败）。
#[derive(Deserialize)]
struct RawMetadata {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    title: Option<serde_json::Value>,
    #[serde(default)]
    created_at: Option<serde_json::Value>,
    #[serde(default)]
    updated_at: Option<serde_json::Value>,
    #[serde(default)]
    trust_mode: Option<serde_json::Value>,
    #[serde(default)]
    reasoning_effort: Option<serde_json::Value>,
    #[serde(default)]
    cwd: Option<serde_json::Value>,
    #[serde(default)]
    cwd_mode: Option<serde_json::Value>,
    #[serde(default)]
    messages: ElementCount,
    #[serde(default)]
    parent_session_id: Option<serde_json::Value>,
}

/// 只统计 JSON 数组元素个数、不构造元素的反序列化目标；非数组按 0 计。
#[derive(Default)]
struct ElementCount(usize);

impl<'de> Deserialize<'de> for ElementCount {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};

        struct CountVisitor;

        impl<'de> Visitor<'de> for CountVisitor {
            type Value = usize;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("消息数组")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<usize, A::Error> {
                let mut count = 0;
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    count += 1;
                }
                Ok(count)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<usize, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(0)
            }

            fn visit_unit<E>(self) -> Result<usize, E> {
                Ok(0)
            }

            fn visit_none<E>(self) -> Result<usize, E> {
                Ok(0)
            }

            fn visit_bool<E>(self, _: bool) -> Result<usize, E> {
                Ok(0)
            }

            fn visit_i64<E>(self, _: i64) -> Result<usize, E> {
                Ok(0)
            }

            fn visit_u64<E>(self, _: u64) -> Result<usize, E> {
                Ok(0)
            }

            fn visit_f64<E>(self, _: f64) -> Result<usize, E> {
                Ok(0)
            }

            fn visit_str<E>(self, _: &str) -> Result<usize, E> {
                Ok(0)
            }
        }

        deserializer.deserialize_any(CountVisitor).map(ElementCount)
    }
}

impl SessionMetadata {
    /// 索引 ID（用于按 id 查找）。
    pub fn id(&self) -> &str {
        &self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_roundtrips_via_serde() {
        let meta = SessionMetadata {
            id: "s1".into(),
            title: "t".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-02T00:00:00Z".into(),
            trust_mode: TrustMode::FullTrust,
            reasoning_effort: Some(tiangong_llm::request::ReasoningEffort::High),
            cwd: "/tmp".into(),
            cwd_mode: SessionCwdMode::Inherit,
            message_count: 3,
            parent_session_id: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: SessionMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, back);
    }

    #[test]
    fn from_slice_counts_messages_and_tolerates_odd_fields() {
        let content = br#"{
            "id": "s1", "title": "t", "created_at": "c", "updated_at": "u",
            "trust_mode": "bogus", "cwd": "/w", "context_summary": {"big": [1,2,3]},
            "messages": [{"role":"user","content":[{"text":"a"}]}, {"role":"assistant"}, 3],
            "parent_session_id": "p"
        }"#;
        let meta = SessionMetadata::from_slice(content).unwrap();
        assert_eq!(meta.id, "s1");
        assert_eq!(meta.cwd, "/w");
        assert_eq!(meta.message_count, 3);
        assert_eq!(meta.trust_mode, TrustMode::default());
        assert_eq!(meta.parent_session_id.as_deref(), Some("p"));

        let empty = SessionMetadata::from_slice(br#"{"id": 1, "messages": null}"#).unwrap();
        assert_eq!(empty.id, "");
        assert_eq!(empty.message_count, 0);
        assert!(SessionMetadata::from_slice(b"{broken").is_err());
    }

    #[test]
    fn metadata_omits_none_reasoning_effort() {
        let meta = SessionMetadata {
            id: "s1".into(),
            title: "t".into(),
            created_at: "c".into(),
            updated_at: "u".into(),
            trust_mode: TrustMode::default(),
            reasoning_effort: None,
            cwd: String::new(),
            cwd_mode: SessionCwdMode::Inherit,
            message_count: 0,
            parent_session_id: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("reasoning_effort"));
    }
}
