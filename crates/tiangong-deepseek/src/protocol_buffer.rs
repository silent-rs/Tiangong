//! 文本工具调用协议的流式缓冲（chat 与 responses 共用）。
//!
//! 正常情况下 DeepSeek 通过结构化字段返回工具调用（chat 的 `tool_calls`、
//! responses 的 `function_call` item）。但部分场景下模型会把工具调用写进
//! 正文文本（原生 `<｜tool▁calls▁begin｜>` 协议或 DSML 协议，见 [`crate::dsml`]）。
//! 协议标记属于机器指令，不应作为正文展示给用户或进入对话历史，因此命中
//! 协议后需要缓冲并在流末剥离，只转出剥离后的可见文本与结构化工具调用。
//!
//! 流的两阶段定性：
//!
//! - **探测阶段**（流的开头）：普通文本即时透传；遇到 `<` 进入窗口探测
//!   （上限 [`PROBE_WINDOW_CHARS`] 字符）。命中协议前缀 → `Confirmed`；
//!   窗口耗尽仍未命中 → 该流定性为普通文本流，**锁定 `Passthrough`**：
//!   后续内容（包括中途出现的协议样文本）一律原样透传，不再识别——
//!   长文正中途的幻觉协议宁可原文透出，也不误转工具调用。
//!   注意：不含 `<` 的纯文本前导（如「我来读取文件。」）即时透传但**不**
//!   锁定——「简短说明 + 调用块」是模型走文本协议的常见形态，仍可识别。
//! - **协议阶段**（`Confirmed`）：后续所有内容（含协议块之后的尾巴文本）
//!   一并缓冲，流末统一解析——成功则剥离协议块、转出可见文本与工具调用；
//!   失败则**整体抛弃**（残缺协议对用户无意义，不回放）。流异常中断时
//!   同样先尝试解析，失败即抛弃。
//!
//! 探测只匹配两套协议的精确前缀，避免把 `<toolbar>` 等普通内容误判。

use crate::dsml::{self, DsmlToolCall};

/// Probing 窗口：`<` 后最多探测多少字符。覆盖两套协议的标记前缀
/// （原生 `<｜tool` 约 5 字符，DSML `<｜｜DSML` 约 6 字符），留足余量。
const PROBE_WINDOW_CHARS: usize = 10;

/// 流末统一解析结果。
#[derive(Debug, Default, PartialEq)]
pub(crate) struct TextProtocolOutcome {
    /// 剥离协议块后的可见文本（协议前的说明文字在探测阶段已透传，
    /// 此处为协议块之间或之后的残余可见文本）。
    pub visible_text: String,
    /// 整条流对调用方可见的全部文本（已透传部分 + `visible_text`）。
    /// 终止事件重写 output 时使用，保证 `output_text()` 与流式输出一致。
    pub total_visible_text: String,
    /// 解析出的工具调用（名称 + 参数 JSON 文本）。未命中协议时为空。
    pub tool_calls: Vec<DsmlToolCall>,
    /// 本次 finish 是否处于 Confirmed（命中协议）状态——解析失败的
    /// 残缺协议也在此列，调用方据此重写终止事件携带的原始 output。
    pub was_confirmed: bool,
}

/// 文本协议流式缓冲状态机。
#[derive(Default)]
pub(crate) struct TextProtocolBuffer {
    phase: BufferPhase,
    /// Probing/Confirmed 期间暂存的文本。
    pending: String,
    /// 已作为正文透传的全部文本（终止事件重写 output 时拼接）。
    streamed_out: String,
}

#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum BufferPhase {
    #[default]
    Idle,
    /// 遇到 `<`，在窗口内探测协议关键词。
    Probing,
    /// 确认走文本协议，缓冲到流末。
    Confirmed,
    /// 已定性为普通文本流：后续一律透传，不再识别。
    Passthrough,
}

impl TextProtocolBuffer {
    /// 流式文本增量进入缓冲，返回应立即透传的文本块（0~N 个）。
    pub(crate) fn push_text(&mut self, delta: &str) -> Vec<String> {
        match self.phase {
            BufferPhase::Idle => {
                if delta.contains('<') {
                    self.pending.push_str(delta);
                    self.phase = BufferPhase::Probing;
                    self.check_probe_window()
                } else {
                    // 纯文本前导：即时透传但不锁定——说明文字之后
                    // 仍可能跟协议调用块（模型常见形态）。
                    self.streamed_out.push_str(delta);
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
            BufferPhase::Passthrough => {
                self.streamed_out.push_str(delta);
                vec![delta.to_string()]
            }
        }
    }

    /// 出现结构化工具调用：该流定性为普通文本流，冲刷暂存并锁定透传。
    pub(crate) fn note_structured_tool_call(&mut self) -> Vec<String> {
        self.phase = BufferPhase::Passthrough;
        if self.pending.is_empty() {
            Vec::new()
        } else {
            vec![std::mem::take(&mut self.pending)]
        }
    }

    /// 流末统一解析。Confirmed 时命中协议则剥离并转出工具调用；
    /// 解析失败则**整体抛弃**（残缺协议对用户无意义）。
    pub(crate) fn finish(&mut self) -> TextProtocolOutcome {
        let total_visible_text = std::mem::take(&mut self.streamed_out);
        match self.phase {
            BufferPhase::Confirmed => {
                let was_confirmed = true;
                self.phase = BufferPhase::Idle;
                let pending = self.pending.trim();
                if let Some(calls) = dsml::parse_dsml_tool_calls(pending) {
                    tracing::info!(count = calls.len(), "工具调用文本协议兜底解析出工具调用");
                    let visible_text = dsml::strip_tool_call_block(pending);
                    self.pending.clear();
                    let mut total = total_visible_text;
                    total.push_str(&visible_text);
                    return TextProtocolOutcome {
                        visible_text,
                        total_visible_text: total,
                        tool_calls: calls,
                        was_confirmed,
                    };
                }
                tracing::warn!(
                    len = self.pending.len(),
                    "工具调用文本缓冲解析失败，已确认协议的残缺内容整体抛弃"
                );
                self.pending.clear();
                TextProtocolOutcome {
                    total_visible_text,
                    was_confirmed,
                    ..Default::default()
                }
            }
            BufferPhase::Probing | BufferPhase::Idle | BufferPhase::Passthrough => {
                let visible_text = std::mem::take(&mut self.pending);
                let mut total = total_visible_text;
                total.push_str(&visible_text);
                TextProtocolOutcome {
                    visible_text,
                    total_visible_text: total,
                    tool_calls: Vec::new(),
                    was_confirmed: false,
                }
            }
        }
    }

    /// 流异常中断：Confirmed 的缓冲内容直接抛弃（不回放残缺协议原文，
    /// 也不尝试转出工具调用）；探测期未定性文本冲刷回正文，不丢内容。
    pub(crate) fn abort(&mut self) -> Vec<String> {
        if self.phase == BufferPhase::Confirmed {
            self.phase = BufferPhase::Idle;
            self.pending.clear();
            self.streamed_out.clear();
            Vec::new()
        } else {
            self.flush_pending()
        }
    }

    /// Probing 状态下检查窗口：命中关键词 → Confirmed；窗口超限 → 定性为
    /// 普通文本流，整块吐出并锁定 Passthrough；否则继续等下一个 delta。
    fn check_probe_window(&mut self) -> Vec<String> {
        // 取最后一个 `<` 之后的文本作为探测窗口。
        let lt_pos = match self.pending.rfind('<') {
            Some(pos) => pos,
            None => return self.lock_passthrough(),
        };
        let after_lt = &self.pending[lt_pos + 1..];
        let window_chars = after_lt.chars().count();
        // 前缀只匹配最后一个 `<` 之后：协议前缀必然紧随协议起始 `<`，
        // `<` 之前出现的 `｜tool` 等字样属于正文讨论，不得据此定性，
        // 否则整段正文会被误判为协议并在解析失败后整体抛弃。
        if contains_protocol_prefix(after_lt) {
            self.phase = BufferPhase::Confirmed;
            tracing::info!("DeepSeek 流式确认走工具调用文本协议，缓冲至流末统一解析");
            Vec::new()
        } else if window_chars >= PROBE_WINDOW_CHARS {
            // 窗口耗尽仍无关键词 → 不是协议：定性为普通文本流并锁定。
            self.lock_passthrough()
        } else {
            // 继续等下一个 delta。
            Vec::new()
        }
    }

    /// 定性为普通文本流：整块吐出 pending 并锁定 Passthrough。
    fn lock_passthrough(&mut self) -> Vec<String> {
        self.phase = BufferPhase::Passthrough;
        if self.pending.is_empty() {
            Vec::new()
        } else {
            self.streamed_out.push_str(&self.pending);
            vec![std::mem::take(&mut self.pending)]
        }
    }

    /// 整块吐出 pending 并回 Idle。
    fn flush_pending(&mut self) -> Vec<String> {
        self.phase = BufferPhase::Idle;
        if self.pending.is_empty() {
            Vec::new()
        } else {
            self.streamed_out.push_str(&self.pending);
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
    fn protocol_mention_before_angle_bracket_is_not_misjudged() {
        // 第四轮 review P2-3 回归：正文里合法讨论「｜tool 标记的写法」再跟
        // 普通尖括号内容——前缀匹配只看最后一个 < 之后，整段正文不得被
        // 误判为协议并在解析失败后整体抛弃。
        let mut buffer = TextProtocolBuffer::default();
        let out = buffer.push_text("讨论 ｜tool 标记的写法 <div>正文继续");
        let mut visible = out.concat();
        visible.push_str(&buffer.finish().visible_text);
        assert!(
            visible.contains("讨论 ｜tool 标记的写法") && visible.contains("<div>正文继续"),
            "正文必须完整透传：{visible:?}"
        );
        // 真协议不受影响：紧随 < 的前缀仍识别。
        let mut buffer = TextProtocolBuffer::default();
        assert!(
            buffer
                .push_text("说明 <｜｜DSML｜｜invoke name=\"f\">")
                .is_empty()
        );
        assert!(buffer.finish().was_confirmed);
    }

    #[test]
    fn text_stream_locks_passthrough_after_window_miss() {
        // 含 < 的正常文本：窗口耗尽即定性为普通文本流，整块吐出并锁定——
        // 后续即使出现完整协议块也不再识别（原样透传，不误转工具）。
        let mut buffer = TextProtocolBuffer::default();
        let out = buffer.push_text("比较 a < b 这是正常文本无关键词");
        assert!(out.iter().any(|text| text.contains("比较 a < b")));
        let out = buffer.push_text(DSML);
        assert_eq!(
            out,
            vec![DSML.to_string()],
            "锁定透传后协议样文本应原样透传：{out:?}"
        );
        let outcome = buffer.finish();
        assert!(outcome.tool_calls.is_empty());
        assert!(outcome.visible_text.is_empty());
    }

    #[test]
    fn plain_text_preamble_does_not_lock_protocol_detection() {
        // 纯文本前导（不含 <）：即时透传但不锁定——其后紧跟的协议块
        // 仍应被识别（「说明 + 调用块」是模型走文本协议的常见形态）。
        let mut buffer = TextProtocolBuffer::default();
        assert_eq!(
            buffer.push_text("我来读取文件。\n"),
            vec!["我来读取文件。\n".to_string()]
        );
        assert!(buffer.push_text(DSML).is_empty(), "协议块应进入缓冲");
        let outcome = buffer.finish();
        assert!(
            outcome
                .tool_calls
                .iter()
                .any(|call| call.name == "read_file")
        );
    }

    #[test]
    fn dsml_protocol_text_is_buffered_not_leaked() {
        // 命中协议后缓冲：协议原文不透传，流末剥离并转出工具调用。
        let mut buffer = TextProtocolBuffer::default();
        let leaked = buffer.push_text(DSML);
        assert!(leaked.is_empty(), "协议原文不应透传：{leaked:?}");
        let outcome = buffer.finish();
        assert_eq!(outcome.visible_text, "");
        assert_eq!(
            outcome.tool_calls,
            vec![DsmlToolCall {
                name: "read_file".to_string(),
                arguments: "{\"path\":\"/tmp/x\"}".to_string(),
            }]
        );
    }

    #[test]
    fn malformed_confirmed_text_is_discarded_at_finish() {
        // 已确认协议但解析失败：残缺协议整体抛弃，不作为正文回放。
        let mut buffer = TextProtocolBuffer::default();
        buffer.push_text("<｜｜DSML｜｜invoke 残缺，不是完整调用");
        let outcome = buffer.finish();
        assert_eq!(outcome.visible_text, "", "残缺协议应被抛弃");
        assert!(outcome.tool_calls.is_empty());
    }

    #[test]
    fn confirmed_tailing_text_survives_strip() {
        // 协议块之后的尾巴文本：解析成功时作为可见文本保留。
        let mut buffer = TextProtocolBuffer::default();
        buffer.push_text(DSML);
        buffer.push_text("\n调用完成");
        let outcome = buffer.finish();
        assert_eq!(outcome.visible_text, "调用完成");
        assert_eq!(outcome.tool_calls.len(), 1);
    }

    #[test]
    fn structured_tool_call_locks_passthrough() {
        let mut buffer = TextProtocolBuffer::default();
        buffer.push_text("<｜｜DSML｜｜invoke 残缺");
        let flushed = buffer.note_structured_tool_call();
        assert!(flushed.iter().any(|text| text.contains("残缺")));
        assert_eq!(buffer.push_text("<div>"), vec!["<div>".to_string()]);
        assert!(buffer.finish().tool_calls.is_empty());
    }

    #[test]
    fn abort_parses_confirmed_and_discards_on_failure() {
        // 中断时已确认协议：完整可解析则照常转出（finish 路径由其他用例
        // 覆盖）；残缺则抛弃，不回放协议原文。
        let mut buffer = TextProtocolBuffer::default();
        buffer.push_text("<｜｜DSML｜｜invoke 残缺");
        assert!(buffer.abort().is_empty(), "残缺协议中断时应抛弃");
        assert!(buffer.finish().tool_calls.is_empty());
    }

    #[test]
    fn abort_flushes_probing_text_as_plain() {
        // 探测期中断：未定性文本冲刷回正文，不丢内容。
        let mut buffer = TextProtocolBuffer::default();
        buffer.push_text("正文 <未判定");
        let flushed = buffer.abort();
        assert!(
            flushed.iter().any(|text| text.contains("正文 <未判定")),
            "探测期内容应冲刷回放：{flushed:?}"
        );
    }
}
