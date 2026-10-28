//! LLM 流式输出节流。
//!
//! 高速本地或局域网模型可能在极短时间内产生大量 token delta。
//! 这里将 token 先累积到内存缓冲，再按固定节奏向外发送，避免前端事件队列被打爆。
//!
//! 实时 TPS 数据源也在此层透传：`ModelStreamChunk.cumulative_tokens`
//! （provider 层按正文 delta 事件计数、官方 usage 到达时校准的累计
//! output tokens）被记录，随下一批正文事件的 `tokens` 字段下发。
//! 前端按相邻事件差分得到实时速度。流读取路径不做任何分词计算。

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
    /// 截至当前的累计 output tokens（provider 层 delta 计数 + 官方校准）。
    /// None = 本流尚未出现正文。
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
        // 累计值（无论是否伴随正文）都记录，供下一批正文事件携带。
        if let Some(cumulative) = chunk.cumulative_tokens {
            buffers.cumulative_tokens = Some(cumulative);
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

    /// 按 chunk 序列推入并 flush 一次，收集发出的文本事件（含 tokens）。
    fn push_and_flush(
        sink_buffers: &Arc<Mutex<StreamBuffers>>,
        chunks: &[ModelStreamChunk],
    ) -> Vec<StreamEvent> {
        let (tx, rx) = mpsc::channel();
        for chunk in chunks {
            let mut buffers = sink_buffers.lock().unwrap();
            if let Some(cumulative) = chunk.cumulative_tokens {
                buffers.cumulative_tokens = Some(cumulative);
            }
            buffers.content.push_str(&chunk.content);
            buffers.reasoning.push_str(&chunk.reasoning_content);
        }
        flush_buffers(sink_buffers, &tx, "msg-1", StreamTextKind::React);
        drop(tx);
        rx.into_iter().collect()
    }

    /// 正文 delta chunk（携带 delta 事件计数的累计值，provider 层语义）。
    fn text_delta(text: &str, cumulative: u64) -> ModelStreamChunk {
        ModelStreamChunk {
            content: text.to_string(),
            reasoning_content: String::new(),
            usage: None,
            cumulative_tokens: Some(cumulative),
        }
    }

    /// usage chunk（官方计量校准后的累计值）。
    fn usage_chunk(calibrated: u64) -> ModelStreamChunk {
        ModelStreamChunk {
            content: String::new(),
            reasoning_content: String::new(),
            usage: Some(TokenUsageData::new(10, calibrated as usize)),
            cumulative_tokens: Some(calibrated),
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
    fn anthropic_realistic_sequence_streams_cumulative() {
        // 真实协议时序：message_start（usage=1）→ 正文 delta（逐事件计数）→
        // content_block_stop 后 message_delta（官方终值校准）。正文期间每批
        // 事件携带递增累计，前端差分即得实时速度。
        let buffers = Arc::new(Mutex::new(StreamBuffers::new()));
        // message_start：无正文，仅初始累计，不发文本事件。
        let start = push_and_flush(&buffers, &[usage_chunk(1)]);
        assert!(start.is_empty());

        // 正文阶段：delta 事件计数递增（批内可能聚合多个事件）。
        let b1 = push_and_flush(&buffers, &[text_delta("你", 2), text_delta("好", 3)]);
        assert_eq!(tokens_of(&b1[0]), Some(3));
        let b2 = push_and_flush(&buffers, &[text_delta("，世界", 8)]);
        assert_eq!(tokens_of(&b2[0]), Some(8));

        // 块结束后的 message_delta：官方终值校准。常规流此批已无正文，
        // 校准由最终 usage / 持久化 text_tps 呈现；若仍有正文则携带校准值。
        let b3 = push_and_flush(&buffers, &[usage_chunk(15)]);
        assert!(b3.is_empty());
        let b4 = push_and_flush(&buffers, &[text_delta("！", 15)]);
        assert_eq!(tokens_of(&b4[0]), Some(15), "校准后新正文沿用官方累计");
    }

    #[test]
    fn deepseek_sequence_counts_from_first_delta() {
        // DeepSeek：正文 delta 即 token 粒度，从首批起就有累计可差分。
        let buffers = Arc::new(Mutex::new(StreamBuffers::new()));
        let b1 = push_and_flush(
            &buffers,
            &[text_delta("a", 1), text_delta("b", 2), text_delta("c", 3)],
        );
        assert_eq!(tokens_of(&b1[0]), Some(3));
        let b2 = push_and_flush(&buffers, &[text_delta("d", 4)]);
        assert_eq!(tokens_of(&b2[0]), Some(4));
    }

    #[test]
    fn empty_flush_emits_no_event() {
        let buffers = Arc::new(Mutex::new(StreamBuffers::new()));
        let events = push_and_flush(&buffers, &[]);
        assert!(events.is_empty());
    }
}
