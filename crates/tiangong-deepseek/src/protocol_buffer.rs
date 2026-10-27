//! 文本工具调用协议的流式缓冲（chat 与 responses 共用）。
//!
//! 正常情况下 DeepSeek 通过结构化字段返回工具调用（chat 的 `tool_calls`、
//! responses 的 `function_call` item）。但部分场景下模型会把工具调用写进
//! 正文文本（原生 `<｜tool▁calls▁begin｜>` 协议或 DSML 协议，见 [`crate::dsml`]）。
//! 协议标记属于机器指令，不应作为正文展示给用户或进入对话历史，因此命中
//! 协议后需要缓冲并在流末剥离，只转出剥离后的可见文本与结构化工具调用。
//!
//! 三态（兼顾流畅与不泄漏）：
//! - `Idle`：正常透传。遇到 `<` 切到 `Probing`，暂存从 `<` 起的文本。
//! - `Probing`：窗口探测（上限 [`PROBE_WINDOW_CHARS`] 字符）。出现已知协议
//!   前缀 → `Confirmed`；窗口满仍未出现 → 整块吐出回 `Idle`（普通文本/
//!   代码块零延迟，不会被误吞）。
//! - `Confirmed`：确认走文本协议，缓冲到流末统一解析：剥离协议块后的
//!   可见文本 + 结构化工具调用。流异常中断时经 [`TextProtocolBuffer::abort`]
//!   把缓冲原文冲刷回正文，不丢内容。
//!
//! 探测只匹配两套协议的精确前缀，避免把 `<toolbar>` 等普通内容误判。

use crate::dsml::{self, DsmlToolCall};

/// Probing 窗口：`<` 后最多探测多少字符。覆盖两套协议的标记前缀
/// （原生 `<｜tool` 约 5 字符，DSML `<｜｜DSML` 约 6 字符），留足余量。
const PROBE_WINDOW_CHARS: usize = 10;

/// 流末统一解析结果。
#[derive(Debug, Default, PartialEq)]
pub(crate) struct TextProtocolOutcome {
    /// 剥离协议块后的可见文本（含协议前的说明文字；解析失败时为整块原文）。
    pub visible_text: String,
    /// 解析出的工具调用（名称 + 参数 JSON 文本）。未命中协议时为空。
    pub tool_calls: Vec<DsmlToolCall>,
}

/// 文本协议流式缓冲状态机。
#[derive(Default)]
pub(crate) struct TextProtocolBuffer {
    /// 是否已出现过结构化工具调用（出现则不再缓冲，正文直接透传）。
    has_structured_tool_call: bool,
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

    /// 出现结构化工具调用：此后正文直接透传，冲刷并返回暂存文本。
    pub(crate) fn note_structured_tool_call(&mut self) -> Vec<String> {
        self.has_structured_tool_call = true;
        self.flush_pending()
    }

    /// 流末统一解析：Confirmed 则剥离协议块并转出工具调用，其余冲刷。
    pub(crate) fn finish(&mut self) -> TextProtocolOutcome {
        match self.phase {
            BufferPhase::Confirmed => {
                self.phase = BufferPhase::Idle;
                let pending = self.pending.trim();
                if let Some(calls) = dsml::parse_dsml_tool_calls(pending) {
                    tracing::info!(count = calls.len(), "工具调用文本协议兜底解析出工具调用");
                    let visible_text = dsml::strip_tool_call_block(pending);
                    self.pending.clear();
                    return TextProtocolOutcome {
                        visible_text,
                        tool_calls: calls,
                    };
                }
                tracing::warn!(
                    len = self.pending.len(),
                    "工具调用文本缓冲解析失败，整块补发缓冲文本"
                );
                TextProtocolOutcome {
                    visible_text: std::mem::take(&mut self.pending),
                    tool_calls: Vec::new(),
                }
            }
            BufferPhase::Probing | BufferPhase::Idle => TextProtocolOutcome {
                visible_text: std::mem::take(&mut self.pending),
                tool_calls: Vec::new(),
            },
        }
    }

    /// 流异常中断：把暂存文本原样冲刷回正文，不解析、不转出工具调用。
    pub(crate) fn abort(&mut self) -> Vec<String> {
        self.flush_pending()
    }

    /// Probing 状态下检查窗口：命中关键词 → Confirmed；窗口超限 → 整块吐出
    /// 回 Idle；否则继续等下一个 delta。
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
            tracing::info!("DeepSeek 流式确认走工具调用文本协议，缓冲至流末统一解析");
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

    const DSML: &str = "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"read_file\">\n<｜｜DSML｜｜parameter name=\"path\" string=\"true\">/tmp/x</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>";

    #[test]
    fn plain_text_passes_through_immediately() {
        let mut buffer = TextProtocolBuffer::default();
        assert_eq!(buffer.push_text("普通"), vec!["普通".to_string()]);
        assert_eq!(buffer.push_text("回复"), vec!["回复".to_string()]);
        let outcome = buffer.finish();
        assert_eq!(outcome.visible_text, "");
        assert!(outcome.tool_calls.is_empty());
    }

    #[test]
    fn angle_bracket_and_code_block_text_flush_after_window() {
        // 含 < 的正常文本与代码块：窗口内无协议前缀即整块吐出（普通文本零延迟；
        // 代码块尾部 < 后不足窗口时最迟在流末冲刷），内容零丢失、不转工具。
        let mut buffer = TextProtocolBuffer::default();
        let mut visible = buffer.push_text("比较 a < b 这是正常文本无关键词").concat();
        visible.push_str(
            &buffer
                .push_text("```html\n<div><span>hello</span></div>\n```")
                .concat(),
        );
        visible.push_str(&buffer.finish().visible_text);
        assert!(
            visible.contains("比较 a < b"),
            "尖括号文本不得丢失：{visible:?}"
        );
        assert!(
            visible.contains("<div><span>hello</span></div>"),
            "代码块不得丢失：{visible:?}"
        );
    }

    #[test]
    fn dsml_protocol_text_is_buffered_not_leaked() {
        // 命中协议后缓冲：协议原文不透传（说明文字在 Idle 阶段已正常流出），
        // 流末剥离协议块并转出工具调用。
        let mut buffer = TextProtocolBuffer::default();
        let passed = buffer.push_text("我来读取文件。\n").concat();
        assert_eq!(passed, "我来读取文件。\n");
        let leaked = buffer.push_text(DSML);
        assert!(leaked.is_empty(), "协议原文不应透传：{leaked:?}");
        let outcome = buffer.finish();
        assert_eq!(outcome.visible_text, "", "缓冲区内协议块应被整体剥离");
        assert_eq!(
            outcome.tool_calls,
            vec![DsmlToolCall {
                name: "read_file".to_string(),
                arguments: "{\"path\":\"/tmp/x\"}".to_string(),
            }]
        );
    }

    #[test]
    fn malformed_confirmed_text_replays_verbatim_at_finish() {
        // 确认样似协议但解析失败：整块原文补发，不吞内容。
        let mut buffer = TextProtocolBuffer::default();
        buffer.push_text("<｜｜DSML｜｜invoke 残缺，不是完整调用");
        let outcome = buffer.finish();
        assert_eq!(
            outcome.visible_text,
            "<｜｜DSML｜｜invoke 残缺，不是完整调用"
        );
        assert!(outcome.tool_calls.is_empty());
    }

    #[test]
    fn structured_tool_call_disables_buffering() {
        let mut buffer = TextProtocolBuffer::default();
        buffer.push_text("<｜｜DSML｜｜invoke 残缺");
        let flushed = buffer.note_structured_tool_call();
        assert!(flushed.iter().any(|text| text.contains("残缺")));
        assert_eq!(buffer.push_text("<div>"), vec!["<div>".to_string()]);
        assert!(buffer.finish().tool_calls.is_empty());
    }

    #[test]
    fn abort_flushes_pending_as_plain_text() {
        // 流异常中断：缓冲原文冲刷回正文，不解析转出。
        let mut buffer = TextProtocolBuffer::default();
        buffer.push_text("说明文字\n");
        buffer.push_text(DSML);
        let flushed = buffer.abort();
        assert_eq!(flushed.len(), 1);
        assert!(flushed[0].contains("我来读取") || flushed[0].contains("DSML"));
        assert!(buffer.finish().tool_calls.is_empty());
    }

    #[test]
    fn text_call_ids_are_globally_unique() {
        let a = dsml::next_text_call_id();
        let b = dsml::next_text_call_id();
        assert_ne!(a, b);
        assert!(a.starts_with("textcall_"));
    }
}
