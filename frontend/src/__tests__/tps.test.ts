import { describe, expect, it } from 'vitest';
import { messageTpsOf, textTpsOf } from '@/api/message';
import type { Message } from '@/api/tauri';
import { formatTps, LiveTpsTracker, round1 } from '@/utils/tps';

function assistantMessage(fields: {
  textTps?: number | null;
  completionTokens?: number;
  textElapsedMs?: number;
} = {}): Message {
  return {
    id: 'm1',
    created_at: '2026-02-06T10:00:00+08:00',
    role: 'assistant',
    content: [{ type: 'text', text: '回复' }],
    ...(fields.textTps !== undefined || fields.completionTokens !== undefined
      ? {
          usage:
            fields.completionTokens !== undefined
              ? {
                  prompt_tokens: 10,
                  completion_tokens: fields.completionTokens,
                  total_tokens: 10 + fields.completionTokens,
                  model: 'deepseek-chat',
                  agent_id: '',
                  turn_id: null,
                  source: 'chat',
                  status: 'success',
                }
              : undefined,
        }
      : {}),
    ...(fields.textTps !== undefined ? { text_tps: fields.textTps } : {}),
    ...(fields.textElapsedMs !== undefined ? { text_elapsed_ms: fields.textElapsedMs } : {}),
  } as unknown as Message;
}

describe('messageTpsOf（持久化优先 + 旧数据折算兼容）', () => {
  it('优先读取后端持久化的 text_tps（历史复现）', () => {
    const message = assistantMessage({ textTps: 87.45, completionTokens: 999, textElapsedMs: 1 });
    expect(textTpsOf(message)).toBe(87.45);
    expect(messageTpsOf(message)).toBe(87.5);
  });

  it('旧数据无 text_tps：按 usage/耗时折算', () => {
    expect(messageTpsOf(assistantMessage({ completionTokens: 175, textElapsedMs: 2000 }))).toBe(87.5);
  });

  it('缺 usage / 耗时 / 耗时非正数时返回 null（不展示不占位）', () => {
    expect(messageTpsOf(assistantMessage({}))).toBeNull();
    expect(messageTpsOf(assistantMessage({ completionTokens: 100 }))).toBeNull();
    expect(messageTpsOf(assistantMessage({ textElapsedMs: 2000 }))).toBeNull();
    expect(messageTpsOf(assistantMessage({ completionTokens: 100, textElapsedMs: 0 }))).toBeNull();
    expect(messageTpsOf(assistantMessage({ textTps: null, completionTokens: 100, textElapsedMs: 0 }))).toBeNull();
  });

  it('非法持久化值（0 / 负数 / NaN）回退折算', () => {
    expect(messageTpsOf(assistantMessage({ textTps: 0, completionTokens: 100, textElapsedMs: 1000 }))).toBe(100);
    expect(messageTpsOf(assistantMessage({ textTps: Number.NaN, completionTokens: 100, textElapsedMs: 1000 }))).toBe(100);
  });

  it('非 assistant 消息恒为 null', () => {
    const notice = { id: 'n', created_at: '', role: 'notice', content: [] } as unknown as Message;
    expect(messageTpsOf(notice)).toBeNull();
    expect(textTpsOf(notice)).toBeNull();
  });
});

describe('LiveTpsTracker（真实 token 计数入窗）', () => {
  it('按后端 tokens 字段累计并计算窗口速度', () => {
    const tracker = new LiveTpsTracker();
    tracker.recordTokens('s1', 100, 0);
    tracker.recordTokens('s1', 100, 1000);
    const snap = tracker.snapshot('s1', 2000);
    expect(snap.active).toBe(true);
    expect(snap.elapsedMs).toBe(2000);
    // 窗口内 2 样本共 200 tokens / 2s 跨度 = 100 tok/s
    expect(round1(snap.tps)).toBe(100);
  });

  it('同批（≤1ms）事件合并为一个样本，不堆积', () => {
    const tracker = new LiveTpsTracker();
    tracker.recordTokens('s1', 10, 1000);
    tracker.recordTokens('s1', 15, 1000);
    const snap = tracker.snapshot('s1', 1000);
    expect(snap.active).toBe(true);
    // 单样本不足计算速度：tps 为 0，但 elapsed 已开始。
    expect(snap.tps).toBe(0);
  });

  it('tokens 缺失 / 非法（旧后端、插件反馈）不计样本', () => {
    const tracker = new LiveTpsTracker();
    tracker.recordTokens('s1', undefined, 0);
    tracker.recordTokens('s1', null, 0);
    tracker.recordTokens('s1', Number.NaN, 0);
    tracker.recordTokens('s1', 0, 0);
    expect(tracker.snapshot('s1', 1000).active).toBe(false);
  });

  it('reset 清空窗口；旧样本随时间滑出窗口', () => {
    const tracker = new LiveTpsTracker();
    tracker.recordTokens('s1', 50, 0);
    tracker.recordTokens('s1', 50, 500);
    tracker.reset('s1');
    expect(tracker.snapshot('s1', 600).active).toBe(false);

    tracker.recordTokens('s2', 100, 0);
    tracker.recordTokens('s2', 100, 1000);
    // 3.5s 后：前两个样本已滑出窗口，只剩新样本。
    tracker.recordTokens('s2', 40, 3500);
    const snap = tracker.snapshot('s2', 3500);
    expect(snap.active).toBe(true);
    expect(snap.elapsedMs).toBe(3500);
  });

  it('开局分母下限：极短跨度不产生数值尖峰', () => {
    const tracker = new LiveTpsTracker();
    tracker.recordTokens('s1', 10, 0);
    tracker.recordTokens('s1', 10, 50);
    const snap = tracker.snapshot('s1', 50);
    // 分母下限 300ms：20 tokens / 0.3s ≈ 66.7，而不是 20/0.05=400。
    expect(round1(snap.tps)).toBe(66.7);
  });
});

describe('formatTps', () => {
  it('≥100 取整，<100 保留一位小数', () => {
    expect(formatTps(99.96)).toBe('100.0 tok/s');
    expect(formatTps(100)).toBe('100 tok/s');
    expect(formatTps(87.34)).toBe('87.3 tok/s');
  });
});
