//! 流式文本的真实 token 计数（实时 TPS 数据源）。
//!
//! provider 流式 API（DeepSeek Responses / OpenAI 系）逐 delta 事件不携带
//! token 数——usage 只在流末尾整体给出；仅 Anthropic 的 `message_delta`
//! 逐事件带官方累计值。为让 UI 在任何 provider 下都有实时速度可显示，
//! 这里在 llm crate 用真实 BPE tokenizer（tiktoken `o200k_base`）对流式
//! 增量做分词计数：计数是真切的 tokenizer 计算，而非字符近似。
//!
//! 流式增量按批到达，批边界未必是 token 边界（`hel` + `lo` 是同一个
//! token `hello`）。计数器保留末尾未定界缓冲：每次只把「后缀再拼接一批
//! 仍不可能变化的稳定前缀」计入累计，尾部留给下一批合并后重新定界；
//! 流结束时 [`StreamTokenCounter::finish`] 把剩余缓冲全部计入。因此
//! 各批产出之和恒等于整段文本一次分词的 token 数。

use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;
use tiktoken_rs::o200k_base;

fn bpe() -> &'static CoreBPE {
    static BPE: OnceLock<CoreBPE> = OnceLock::new();
    BPE.get_or_init(|| o200k_base().expect("内嵌 o200k_base BPE 构建失败"))
}

/// 一次完整文本的真实 token 数（整段直接分词）。
pub fn count_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    bpe().encode_ordinary(text).len() as u64
}

/// 流式增量计数器：逐批 push，产出「本批新定界的 token 数」。
///
/// BPE 分词先按预分词正则切成 pretoken（连续词块 / 数字块 / 标点串 /
/// 空白串），再逐段编码。任意字节位置截断会把 pretoken 切成残片，
/// 残片分词数 ≥ 合并后的整段，且累计只增不减 → 高估。因此提交点只落在
/// 「分隔符 run（连续空白 / 标点）的起点」：分隔符是 pretoken 的天然
/// 分界，其前的文本分词结果不再因后续内容改变；分隔符自身留给未定界
/// 区，与后续词组合（如 `" word"` 的前导空格）后再定界。
/// 已定界前缀定期裁剪（[`TRIM_BYTES`]）控制窗口内存与重复分词开销。
pub struct StreamTokenCounter {
    /// 自上次裁剪以来的累计文本（含未定界尾部）。
    text: String,
    /// `text` 内已定界计入的字节偏移。
    committed: usize,
    /// `text[..committed]` 的 token 数缓存（push 差分基准）。
    committed_tokens: usize,
    /// 已定界计入的总 token 数。
    total: u64,
}

/// 窗口裁剪阈值（字节）：超出后丢弃已定界前缀，控制单次分词开销。
const TRIM_BYTES: usize = 8 * 1024;

impl Default for StreamTokenCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamTokenCounter {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            committed: 0,
            committed_tokens: 0,
            total: 0,
        }
    }

    /// 计入一段增量，返回本批新定界的 token 数。
    pub fn push(&mut self, delta: &str) -> u64 {
        if delta.is_empty() {
            return 0;
        }
        self.text.push_str(delta);
        let stable_end = stable_prefix_end(&self.text);
        if stable_end <= self.committed {
            // 没有新的安全提交点（仍在同一词块内）：等后续批次或 finish。
            return 0;
        }
        let stable_tokens = bpe().encode_ordinary(&self.text[..stable_end]).len();
        let produced = stable_tokens.saturating_sub(self.committed_tokens);
        self.total += produced as u64;
        self.committed = stable_end;
        self.committed_tokens = stable_tokens;

        // 裁剪已定界前缀：未定界尾部保留，窗口重置。
        if self.committed > TRIM_BYTES {
            let keep = self.text.split_off(self.committed);
            self.text = keep;
            self.committed = 0;
            self.committed_tokens = 0;
        }
        produced as u64
    }

    /// 流结束：把剩余缓冲全部定界计入，返回最后一笔产出。
    pub fn finish(&mut self) -> u64 {
        if self.text.len() <= self.committed {
            self.text.clear();
            self.committed = 0;
            self.committed_tokens = 0;
            return 0;
        }
        let all = bpe().encode_ordinary(&self.text).len();
        let produced = all.saturating_sub(self.committed_tokens);
        self.total += produced as u64;
        self.text.clear();
        self.committed = 0;
        self.committed_tokens = 0;
        produced as u64
    }

    /// 已定界计入的总 token 数（含 finish 后的最终值）。
    pub fn total(&self) -> u64 {
        self.total
    }
}

/// 稳定提交点：最后一个「分隔符 run」的起点（字节偏移）。
///
/// 分隔符 = 非字母非数字字符（空白 / 标点 / 符号，Unicode 口径），
/// 与 BPE 预分词的 pretoken 分界一致。连续分隔符合并为一个 run，
/// 提交点取 run 起点而非 run 内部（run 可能与后续词构成同一 pretoken，
/// 如 `" word"` 的前导空格）。没有分隔符时返回 0（全部未定界）。
fn stable_prefix_end(text: &str) -> usize {
    let mut boundary = 0usize;
    let mut in_run = false;
    for (idx, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            in_run = false;
        } else if !in_run {
            boundary = idx;
            in_run = true;
        }
    }
    boundary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_delta_counts_zero() {
        let mut counter = StreamTokenCounter::new();
        assert_eq!(counter.push(""), 0);
        assert_eq!(counter.finish(), 0);
        assert_eq!(counter.total(), 0);
    }

    #[test]
    fn stream_total_matches_single_shot_encoding() {
        let text = "你好，世界！Hello, world. 这是一个流式 token 计数测试。";
        let mut counter = StreamTokenCounter::new();
        // 按 3 字符切块模拟任意批边界。
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(3) {
            let piece: String = chunk.iter().collect();
            counter.push(&piece);
        }
        counter.finish();
        assert_eq!(counter.total(), count_tokens(text));
    }

    #[test]
    fn cross_batch_merge_not_overcounted() {
        // "hello" 被切成两批：两批产出之和等于整词一次分词的 token 数。
        let mut counter = StreamTokenCounter::new();
        let first = counter.push("hel");
        let rest = counter.push("lo world");
        let tail = counter.finish();
        let produced = first + rest + tail;
        assert_eq!(produced, count_tokens("hello world"));
    }

    #[test]
    fn long_stream_counts_progressively() {
        // 长文本逐字符 push：最终总数与整段一致（每批产出多为 0，稳定段定界推进）。
        let text =
            "The quick brown fox jumps over the lazy dog. 敏捷的棕色狐狸跳过了懒狗。".repeat(3);
        let mut counter = StreamTokenCounter::new();
        for ch in text.chars() {
            counter.push(&ch.to_string());
        }
        counter.finish();
        assert_eq!(counter.total(), count_tokens(&text));
    }
}
