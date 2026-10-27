//! 文本工具调用协议的流式收集（chat 与 responses 共用）。
//!
//! 正常情况下 DeepSeek 通过结构化字段返回工具调用（chat 的 `tool_calls`、
//! responses 的 `function_call` item）。但部分场景下模型会把工具调用写进
//! 正文文本（原生 `<｜tool▁calls▁begin｜>` 协议或 DSML 协议，见 [`crate::dsml`]）。
//!
//! 策略：**正文一律正常透传（不拦截、不撤回），同时旁路收集完整文本，
//! 流末统一识别**——识别为工具调用协议则在终止事件前转出工具调用，
//! 由上层正常发起；识别失败则什么都不做（正文已经流出，无任何影响）。
//! 这避免了长参数工具调用期间「确认协议后憋流」导致的流式中断观感，
//! 也避免了流意外中断时缓冲文本丢失。
//!
//! 出现结构化工具调用后停止收集（模型已走结构化通道，正文中出现的
//! 协议样文本按普通内容对待，防止把协议讨论误判为调用）。

use crate::dsml::{self, DsmlToolCall};

/// 文本协议流式旁路收集器。
#[derive(Default)]
pub(crate) struct TextProtocolBuffer {
    /// 是否已出现过结构化工具调用（出现则停止收集）。
    has_structured_tool_call: bool,
    /// 旁路收集的完整正文。
    collected: String,
}

impl TextProtocolBuffer {
    /// 正文增量：透传由调用方负责，这里只旁路收集。
    pub(crate) fn push_text(&mut self, delta: &str) {
        if !self.has_structured_tool_call {
            self.collected.push_str(delta);
        }
    }

    /// 出现结构化工具调用：停止收集（正文继续由调用方透传）。
    pub(crate) fn note_structured_tool_call(&mut self) {
        self.has_structured_tool_call = true;
    }

    /// 流末统一识别：命中文本工具调用协议则返回解析出的调用组，
    /// 未命中或已出现结构化调用时返回空。消费已收集文本，重复调用幂等。
    pub(crate) fn finish(&mut self) -> Vec<DsmlToolCall> {
        if self.has_structured_tool_call {
            return Vec::new();
        }
        let collected = std::mem::take(&mut self.collected);
        if collected.trim().is_empty() {
            return Vec::new();
        }
        match dsml::parse_dsml_tool_calls(collected.trim()) {
            Some(calls) if !calls.is_empty() => {
                tracing::info!(
                    count = calls.len(),
                    "工具调用文本协议在流末识别成功，转为结构化调用"
                );
                calls
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_finishes_empty() {
        let mut buffer = TextProtocolBuffer::default();
        buffer.push_text("普通回复，含 <div> 标签也不拦截");
        assert!(buffer.finish().is_empty());
    }

    #[test]
    fn dsml_text_resolves_to_tool_calls_at_finish() {
        let dsml = "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"read_file\">\n<｜｜DSML｜｜parameter name=\"path\" string=\"true\">/tmp/x</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>";
        let mut buffer = TextProtocolBuffer::default();
        buffer.push_text("我来读取文件。\n");
        buffer.push_text(dsml);
        assert_eq!(
            buffer.finish(),
            vec![DsmlToolCall {
                name: "read_file".to_string(),
                arguments: "{\"path\":\"/tmp/x\"}".to_string(),
            }]
        );
        // 重复 finish 幂等（收集器已消费）。
        assert!(buffer.finish().is_empty() || buffer.finish().is_empty());
    }

    #[test]
    fn structured_tool_call_disables_collection() {
        let mut buffer = TextProtocolBuffer::default();
        buffer.push_text("<｜｜DSML｜｜invoke 残缺");
        buffer.note_structured_tool_call();
        buffer.push_text("结构化调用之后出现的协议样文本按正文对待");
        assert!(buffer.finish().is_empty());
    }

    #[test]
    fn malformed_protocol_text_is_ignored_at_finish() {
        // 确认样似协议但解析失败：正文已透传，流末无动作。
        let mut buffer = TextProtocolBuffer::default();
        buffer.push_text("<｜｜DSML｜｜invoke 残缺，不是完整调用");
        assert!(buffer.finish().is_empty());
    }
}
