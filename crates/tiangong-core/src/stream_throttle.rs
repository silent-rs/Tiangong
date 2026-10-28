//! LLM 流式输出节流。
//!
//! 高速本地或局域网模型可能在极短时间内产生大量 token delta。
//! 这里将 token 先累积到内存缓冲，再按固定节奏向外发送，避免前端事件队列被打爆。
//!
//! 实时 TPS 数据源也在此层透传：provider 流上出现的官方 usage 累计
//! （`Usage` 事件的 `completion_tokens`，Anthropic 逐 message_delta 携带、
//! DeepSeek / OpenAI 系仅在流末）被记录，随下一批正文事件的 `tokens`
//! 字段下发（语义 = 该消息截至本批的累计 output tokens）。前端按相邻
//! 事件差分得到实时速度；流中无官方数据（DeepSeek 正文期间）时字段
//! 缺失，前端只显示进行态。流读取路径不做任何 token 计算。

use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tiangong_types::StreamEvent;

use tiangong_llm::ModelStreamChunk;

const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// 流式文本所属的阶段，决定 flush 时发送的 StreamEvent 变体。
#[derive(Debug, Clone, Copy)]
pub(crate) enum StreamTextKind {
    /// 普通文本增量（向后兼容，发送 Delta）。保留供未标记阶段的调用方使用。
    #[allow(dead_code)]
    Delta,
    /// ReAct 工具执行阶段的过程性文本（发送 ReactText）
    React,
    /// 历史总结阶段的最终回复（发送 SummaryText）。任务 15 起不再发起独立
    /// Summary 请求，该变体保留以维持前端事件契约；最终回复现在以 ReactText
    /// 流出并经消息 upsert（phase=Summary）提交，任务 18 前端联调后定去留。
    #[allow(dead_code)]
    Summary,
}

#[derive(Default)]
struct StreamBuffers {
    content: String,
    reasoning: String,
    /// provider 流上最近一次官方 usage 的累计 output tokens。
    /// None = 本流尚未出现 usage（该 provider 流中不提供实时计量）。
    cumulative_tokens: Option<u64>,
}

impl StreamBuffers {
    fn new() -> Self {
        Self::default()
    }
}

/// 将 LLM token delta 合并为固定节奏的 StreamEvent。
pub(crate) struct ThrottledStreamSink {
    message_id: String,
    tx: Sender<StreamEvent>,
    text_kind: StreamTextKind,
    buffers: Arc<Mutex<StreamBuffers>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ThrottledStreamSink {
    /// 创建默认（Delta）文本类型的 sink。保留作为向后兼容入口。
    #[allow(dead_code)]
    pub(crate) fn new(message_id: String, tx: Sender<StreamEvent>) -> Self {
        Self::with_text_kind(message_id, tx, StreamTextKind::Delta)
    }

    /// 指定文本阶段类型，决定 flush 时发送 ReactText / SummaryText / Delta。
    pub(crate) fn with_text_kind(
        message_id: String,
        tx: Sender<StreamEvent>,
        text_kind: StreamTextKind,
    ) -> Self {
        Self::with_interval(message_id, tx, text_kind, DEFAULT_FLUSH_INTERVAL)
    }

    fn with_interval(
        message_id: String,
        tx: Sender<StreamEvent>,
        text_kind: StreamTextKind,
        interval: Duration,
    ) -> Self {
        let buffers = Arc::new(Mutex::new(StreamBuffers::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let worker_buffers = buffers.clone();
        let worker_stop = stop.clone();
        let worker_tx = tx.clone();
        let worker_message_id = message_id.clone();
        let worker = thread::Builder::new()
            .name("tiangong-llm-stream-throttle".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    thread::sleep(interval);
                    flush_buffers(&worker_buffers, &worker_tx, &worker_message_id, text_kind);
                }
                flush_buffers(&worker_buffers, &worker_tx, &worker_message_id, text_kind);
            })
            .ok();

        Self {
            message_id,
            tx,
            text_kind,
            buffers,
            stop,
            worker,
        }
    }

    pub(crate) fn push_chunk(&self, chunk: &ModelStreamChunk) {
        let Ok(mut buffers) = self.buffers.lock() else {
            return;
        };
        // 官方 usage 累计（无论是否伴随正文）都记录，供下一批正文事件携带。
        if let Some(usage) = &chunk.usage {
            buffers.cumulative_tokens = Some(usage.completion_tokens as u64);
        }
        if chunk.content.is_empty() && chunk.reasoning_content.is_empty() {
            return;
        }
        buffers.content.push_str(&chunk.content);
        buffers.reasoning.push_str(&chunk.reasoning_content);
    }

    pub(crate) fn finish(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        } else {
            flush_buffers(&self.buffers, &self.tx, &self.message_id, self.text_kind);
        }
    }
}

impl Drop for ThrottledStreamSink {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn flush_buffers(
    buffers: &Arc<Mutex<StreamBuffers>>,
    tx: &Sender<StreamEvent>,
    message_id: &str,
    text_kind: StreamTextKind,
) {
    // 持锁直到事件发送完成，使显式 flush 返回时与定时 flush 建立严格先后关系。
    let Ok(mut buffers) = buffers.lock() else {
        return;
    };
    let content = mem::take(&mut buffers.content);
    let reasoning = mem::take(&mut buffers.reasoning);

    if !content.is_empty() {
        // 累计值随正文批透传（快照语义：批内多次 usage 更新取最新）；
        // 不存在「取走累计却无事件可搭」的丢失路径——累计保存在缓冲，
        // 一直等到下一批正文（或流结束随最终 usage / 持久化 text_tps 呈现）。
        let tokens = buffers.cumulative_tokens;
        let event = match text_kind {
            StreamTextKind::Delta => StreamEvent::Delta {
                message_id: message_id.to_string(),
                content,
                tokens,
            },
            StreamTextKind::React => StreamEvent::ReactText {
                message_id: message_id.to_string(),
                content,
                tokens,
            },
            StreamTextKind::Summary => StreamEvent::SummaryText {
                message_id: message_id.to_string(),
                content,
                tokens,
            },
        };
        let _ = tx.send(event);
    }
    if !reasoning.is_empty() {
        let _ = tx.send(StreamEvent::Reasoning {
            message_id: message_id.to_string(),
            content: reasoning,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use tiangong_llm::usage::TokenUsageData;

    fn push_and_flush(
        sink_buffers: &Arc<Mutex<StreamBuffers>>,
        chunks: &[ModelStreamChunk],
    ) -> Vec<StreamEvent> {
        let (tx, rx) = mpsc::channel();
        for chunk in chunks {
            let mut buffers = sink_buffers.lock().unwrap();
            if let Some(usage) = &chunk.usage {
                buffers.cumulative_tokens = Some(usage.completion_tokens as u64);
            }
            buffers.content.push_str(&chunk.content);
            buffers.reasoning.push_str(&chunk.reasoning_content);
        }
        flush_buffers(sink_buffers, &tx, "msg-1", StreamTextKind::React);
        drop(tx);
        rx.into_iter().collect()
    }

    fn text_delta(text: &str) -> ModelStreamChunk {
        ModelStreamChunk {
            content: text.to_string(),
            reasoning_content: String::new(),
            usage: None,
        }
    }

    fn usage_chunk(completion_tokens: usize) -> ModelStreamChunk {
        ModelStreamChunk {
            content: String::new(),
            reasoning_content: String::new(),
            usage: Some(TokenUsageData::new(10, completion_tokens)),
        }
    }

    fn tokens_of(event: &StreamEvent) -> Option<u64> {
        match event {
            StreamEvent::Delta { tokens, .. }
            | StreamEvent::ReactText { tokens, .. }
            | StreamEvent::SummaryText { tokens, .. } => *tokens,
            _ => None,
        }
    }

    #[test]
    fn usage_updates_carry_through_next_text_batch() {
        // Anthropic 形态：usage 累计随 message_delta 持续更新。事件携带的
        // 是 flush 时点的最新官方累计（批内正文先到、usage 后到也取最新），
        // 前端按相邻事件差分得到实时速度。
        let buffers = Arc::new(Mutex::new(StreamBuffers::new()));
        let first = push_and_flush(&buffers, &[text_delta("你好"), usage_chunk(12)]);
        assert_eq!(tokens_of(&first[0]), Some(12));

        let second = push_and_flush(
            &buffers,
            &[usage_chunk(20), text_delta("，世界"), usage_chunk(30)],
        );
        assert_eq!(tokens_of(&second[0]), Some(30));

        let third = push_and_flush(&buffers, &[text_delta("！")]);
        assert_eq!(tokens_of(&third[0]), Some(30), "无新 usage 时沿用最近累计");
    }

    #[test]
    fn text_without_any_usage_has_no_tokens() {
        // DeepSeek 形态：正文期间流上无官方计量，事件不带 tokens
        // （前端只显示进行态）；流末 usage 由最终 usage 事件 / 持久化
        // text_tps 呈现，不经正文事件。
        let buffers = Arc::new(Mutex::new(StreamBuffers::new()));
        let events = push_and_flush(&buffers, &[text_delta("纯正文")]);
        assert_eq!(tokens_of(&events[0]), None);
    }

    #[test]
    fn empty_flush_emits_no_event() {
        let buffers = Arc::new(Mutex::new(StreamBuffers::new()));
        let events = push_and_flush(&buffers, &[usage_chunk(99)]);
        assert!(events.is_empty(), "无正文不发文本事件");
    }
}
