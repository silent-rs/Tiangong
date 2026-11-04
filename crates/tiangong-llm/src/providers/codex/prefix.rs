//! Codex 请求前缀稳定度观测：判读 prompt cache 为什么命中/不命中。
//!
//! 上游的块级缓存（128 token 块）只在**请求前缀逐字节一致**时才复用：同一会话
//! 的下一轮请求必须保留上一轮已发送的全部内容，且 `instructions` 与历史消息的
//! 序列化结果不变。任何前置内容变动（例如每轮往 system 里塞时间、改工具顺序、
//! 重写历史消息）都会让其后所有块失效。
//!
//! 本模块只做结构比对、不落任何 prompt 内容：记录每个会话上一轮请求体，给出
//! 「instructions 是否相同」「第一个不同的 input 项下标与角色」「历史是否只是
//! 追加」。日志据此可判断缓存命中的前提是否成立。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

/// 单次比对结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrefixReport {
    /// 上一轮是否可比较（同一会话的首个请求为 false）。
    pub comparable: bool,
    /// `instructions` 是否与上一轮逐字相同。
    pub instructions_same: bool,
    /// 第一个不同的 `input` 项下标；`None` 表示历史项完全相同（纯追加）或不可比。
    pub first_diff_item: Option<usize>,
    /// 上一轮 / 本轮 `input` 项数。
    pub prev_items: usize,
    pub cur_items: usize,
    /// 历史是否纯追加（前 `prev_items` 项完全一致）。
    pub append_only: bool,
}

/// 参与比较的会话上限，避免长进程里无界增长。
const MAX_TRACKED_SESSIONS: usize = 256;

static LAST_PAYLOADS: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();

fn last_payloads() -> &'static Mutex<HashMap<String, Value>> {
    LAST_PAYLOADS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 记录本轮请求体并与同会话上一轮比对。
pub(crate) fn observe(session_id: &str, payload: &Value) -> PrefixReport {
    let mut store = last_payloads().lock().expect("Codex 前缀观测锁");
    let previous = store.get(session_id).cloned();
    if store.len() >= MAX_TRACKED_SESSIONS && !store.contains_key(session_id) {
        store.clear();
    }
    store.insert(session_id.to_string(), payload.clone());

    let Some(previous) = previous else {
        return PrefixReport {
            comparable: false,
            instructions_same: true,
            first_diff_item: None,
            prev_items: item_count(payload),
            cur_items: item_count(payload),
            append_only: true,
        };
    };
    compare(&previous, payload)
}

fn item_count(payload: &Value) -> usize {
    payload
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

fn items(payload: &Value) -> &[Value] {
    payload
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn compare(previous: &Value, current: &Value) -> PrefixReport {
    let previous_instructions = previous.get("instructions");
    let current_instructions = current.get("instructions");
    let (prev, cur) = (items(previous), items(current));
    let first_diff_item = prev
        .iter()
        .zip(cur.iter())
        .position(|(left, right)| left != right);
    PrefixReport {
        comparable: true,
        instructions_same: previous_instructions == current_instructions,
        first_diff_item,
        prev_items: prev.len(),
        cur_items: cur.len(),
        // 历史纯追加：本轮以前一轮的每一项都原样保留为前缀。
        append_only: first_diff_item.is_none() && cur.len() >= prev.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(instructions: &str, items: Vec<Value>) -> Value {
        json!({ "instructions": instructions, "input": items })
    }

    fn user(text: &str) -> Value {
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
    }

    fn assistant(text: &str) -> Value {
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})
    }

    #[test]
    fn first_request_is_not_comparable() {
        let report = observe("prefix-test-first", &payload("sys", vec![user("a")]));
        assert!(!report.comparable);
        assert!(report.append_only);
    }

    #[test]
    fn append_only_history_is_stable() {
        let session = "prefix-test-append";
        observe(session, &payload("sys", vec![user("a")]));
        let report = observe(
            session,
            &payload("sys", vec![user("a"), assistant("b"), user("c")]),
        );
        assert!(report.comparable);
        assert!(report.instructions_same);
        assert_eq!(report.first_diff_item, None);
        assert!(report.append_only);
        assert_eq!((report.prev_items, report.cur_items), (1, 3));
    }

    #[test]
    fn changed_instructions_breaks_prefix() {
        let session = "prefix-test-sys";
        observe(session, &payload("sys v1", vec![user("a")]));
        let report = observe(session, &payload("sys v2", vec![user("a"), user("b")]));
        assert!(!report.instructions_same);
        assert!(report.append_only, "历史仍是纯追加，断点在 system");
    }

    #[test]
    fn rewritten_history_points_at_first_diff_item() {
        let session = "prefix-test-rewrite";
        observe(session, &payload("sys", vec![user("a"), assistant("b")]));
        let report = observe(
            session,
            &payload("sys", vec![user("a"), assistant("改写了"), user("c")]),
        );
        assert_eq!(report.first_diff_item, Some(1));
        assert!(!report.append_only);
    }

    #[test]
    fn dropped_history_is_not_append_only() {
        let session = "prefix-test-drop";
        observe(session, &payload("sys", vec![user("a"), assistant("b")]));
        let report = observe(session, &payload("sys", vec![user("a")]));
        assert_eq!(report.first_diff_item, None);
        assert!(!report.append_only);
    }
}
