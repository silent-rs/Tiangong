//! 文本工具调用协议的流式缓冲（chat 与 responses 共用）。
//!
//! 正常情况下 DeepSeek 通过结构化字段返回工具调用（chat 的 `tool_calls`、
//! responses 的 `function_call` item）。但部分场景下模型会把工具调用写进
//! 正文文本（原生 `<｜tool▁calls▁begin｜>` 协议或 DSML 协议，见 [`crate::dsml`]），
//! 流式时这些协议标记按字符分片到达，必须缓冲拼接后才能整体解析。
//!
//! 本模块只做文本层面的三态缓冲，不感知具体事件类型；chat / responses 的
//! 流适配层各自负责把透传文本与解析结果包装回自己的事件类型。
//!
//! 三态：
//! - `Idle`：正常透传。遇到 `<` 切到 `Probing`，暂存从 `<` 起的文本。
//! - `Probing`：窗口探测。累积 `<` 后的文本（窗口上限 [`PROBE_WINDOW_CHARS`]
//!   字符），出现已知协议前缀 → `Confirmed`；窗口满仍未出现 → 整块吐出回 `Idle`。
//! - `Confirmed`：确认走文本协议，持续缓冲到完整响应结束后统一解析。
//!
//! 探测只匹配两套协议的精确前缀，避免把 `<toolbar>` 等普通内容误判为工具调用。

use crate::dsml::{self, DsmlToolCall};

/// Probing 窗口：`<` 后最多探测多少字符。覆盖两套协议的标记前缀
/// （原生 `<｜tool` 约 5 字符，DSML `<｜｜DSML` 约 6 字符），留足余量。
const PROBE_WINDOW_CHARS: usize = 10;

/// 完整响应结束后的缓冲解析结果。
#[derive(Debug, Default, PartialEq)]
pub(crate) struct TextProtocolOutcome {
    /// 非工具调用文本：协议外的说明文字，或解析失败时回退的整块原文。
    pub leftover_text: String,
    /// 解析出的工具调用（名称 + 参数 JSON 文本）。非协议文本时为空。
    pub tool_calls: Vec<DsmlToolCall>,
}

/// 文本协议流式缓冲状态机。
#[derive(Default)]
pub(crate) struct TextProtocolBuffer {
    /// 是否已出现过结构化工具调用（出现则永不缓冲）。
    has_structured_tool_call: bool,
    /// 当前状态。
    phase: BufferPhase,
    /// Probing/Confirmed 期间暂存的文本。
    pending: String,
}

#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum BufferPhase {
    #[default]
    Idle,
    /// 遇到 `<`，在窗口内探测协议关键词。
    Probing,
    /// 确认走文本协议，缓冲到流末。
    Confirmed,
}

impl TextProtocolBuffer {
    /// 流式文本增量进入缓冲，返回应立即透传的文本块（0~N 个）。
    pub(crate) fn push_text(&mut self, delta: &str) -> Vec<String> {
        if self.has_structured_tool_call {
            return vec![delta.to_string()];
        }
        match self.phase {
            BufferPhase::Idle => {
                if delta.contains('<') {
                    // 含 `<`：暂存并进入窗口探测。
                    self.pending.push_str(delta);
                    self.phase = BufferPhase::Probing;
                    self.check_probe_window()
                } else {
                    vec![delta.to_string()]
                }
            }
            BufferPhase::Probing => {
                self.pending.push_str(delta);
                self.check_probe_window()
            }
            BufferPhase::Confirmed => {
                self.pending.push_str(delta);
                Vec::new()
            }
        }
    }

    /// 出现结构化工具调用：此后文本协议不可能再出现，冲刷 pending 并停用缓冲。
    pub(crate) fn note_structured_tool_call(&mut self) -> Vec<String> {
        self.has_structured_tool_call = true;
        self.flush_pending()
    }

    /// 完整响应结束：确认协议则整体解析为工具调用组，否则冲刷 pending。
    pub(crate) fn finish(&mut self) -> TextProtocolOutcome {
        match self.phase {
            BufferPhase::Confirmed => {
                self.phase = BufferPhase::Idle;
                let pending = self.pending.trim();
                if let Some(calls) = dsml::parse_dsml_tool_calls(pending) {
                    tracing::info!(count = calls.len(), "工具调用文本协议兜底解析出工具调用");
                    let leftover_text = dsml::strip_tool_call_block(pending);
                    self.pending.clear();
                    return TextProtocolOutcome {
                        leftover_text,
                        tool_calls: calls,
                    };
                }
                tracing::warn!(
                    len = self.pending.len(),
                    "工具调用文本缓冲解析失败，整块补发缓冲文本"
                );
                TextProtocolOutcome {
                    leftover_text: std::mem::take(&mut self.pending),
                    tool_calls: Vec::new(),
                }
            }
            BufferPhase::Probing => TextProtocolOutcome {
                leftover_text: std::mem::take(&mut self.pending),
                tool_calls: Vec::new(),
            },
            BufferPhase::Idle => TextProtocolOutcome::default(),
        }
    }

    /// Probing 状态下检查窗口：命中关键词 → Confirmed；窗口超限 → 整块吐出回
    /// Idle；否则继续等下一个 delta。
    fn check_probe_window(&mut self) -> Vec<String> {
        // 取最后一个 `<` 之后的文本作为探测窗口。
        let lt_pos = match self.pending.rfind('<') {
            Some(pos) => pos,
            None => return self.flush_pending(),
        };
        let after_lt = &self.pending[lt_pos + 1..];
        let window_chars = after_lt.chars().count();
        if contains_protocol_prefix(&self.pending) {
            self.phase = BufferPhase::Confirmed;
            tracing::info!("DeepSeek 流式确认走工具调用文本协议，等待完整响应");
            Vec::new()
        } else if window_chars >= PROBE_WINDOW_CHARS {
            // 窗口耗尽仍无关键词 → 不是协议，整块吐出。
            self.flush_pending()
        } else {
            // 继续等下一个 delta。
            Vec::new()
        }
    }

    /// 整块吐出 pending 并回 Idle。
    fn flush_pending(&mut self) -> Vec<String> {
        self.phase = BufferPhase::Idle;
        if self.pending.is_empty() {
            Vec::new()
        } else {
            vec![std::mem::take(&mut self.pending)]
        }
    }
}

/// 判断累计文本是否含已知协议前缀（不区分大小写）。
fn contains_protocol_prefix(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("｜tool") || lower.contains("｜｜dsml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_all(buffer: &mut TextProtocolBuffer, deltas: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for delta in deltas {
            out.extend(buffer.push_text(delta));
        }
        out
    }

    #[test]
    fn plain_text_passes_through_immediately() {
        let mut buffer = TextProtocolBuffer::default();
        assert_eq!(buffer.push_text("普通"), vec!["普通".to_string()]);
        assert_eq!(buffer.push_text("回复"), vec!["回复".to_string()]);
        let outcome = buffer.finish();
        assert_eq!(outcome.leftover_text, "");
        assert!(outcome.tool_calls.is_empty());
    }

    #[test]
    fn angle_bracket_text_flushes_after_window() {
        let mut buffer = TextProtocolBuffer::default();
        let out = push_all(&mut buffer, &["比较 a < b 这是正常文本无关键词"]);
        assert!(
            out.iter().any(|text| text.contains("比较 a < b")),
            "正常尖括号文本应完整吐出：{out:?}"
        );
        let outcome = buffer.finish();
        assert!(outcome.tool_calls.is_empty());
    }

    #[test]
    fn dsml_text_resolves_to_tool_calls_at_finish() {
        let dsml = "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"read_file\">\n<｜｜DSML｜｜parameter name=\"path\" string=\"true\">/tmp/x</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>";
        let mut buffer = TextProtocolBuffer::default();
        let out = push_all(&mut buffer, &[dsml]);
        assert!(out.is_empty(), "协议文本不应透传：{out:?}");
        let outcome = buffer.finish();
        assert_eq!(
            outcome.tool_calls,
            vec![DsmlToolCall {
                name: "read_file".to_string(),
                arguments: "{\"path\":\"/tmp/x\"}".to_string(),
            }]
        );
        assert_eq!(outcome.leftover_text, "");
    }

    #[test]
    fn structured_tool_call_disables_buffering() {
        let mut buffer = TextProtocolBuffer::default();
        assert!(push_all(&mut buffer, &["<未完"]).is_empty());
        let flushed = buffer.note_structured_tool_call();
        assert_eq!(flushed, vec!["<未完".to_string()]);
        assert_eq!(buffer.push_text("<div>"), vec!["<div>".to_string()]);
    }

    #[test]
    fn malformed_protocol_text_is_replayed_verbatim() {
        // 确认协议但解析失败：整块补发原文，不吞内容。
        let mut buffer = TextProtocolBuffer::default();
        push_all(&mut buffer, &["<｜｜DSML｜｜invoke 残缺"]);
        let outcome = buffer.finish();
        assert_eq!(outcome.leftover_text, "<｜｜DSML｜｜invoke 残缺");
        assert!(outcome.tool_calls.is_empty());
    }
}
