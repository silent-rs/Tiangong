import { describe, expect, it } from 'vitest';
import { latestTpsOf, messageTpsOf, textTpsOf } from '@/api/message';
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

describe('latestTpsOf（空闲回退最近完成速度）', () => {
  it('从末尾往前取第一条有效速度，跳过流式中 / 无数据消息', () => {
    const messages = [
      assistantMessage({ completionTokens: 100, textElapsedMs: 1000 }),
      assistantMessage({}),
      { id: 't1', role: 'tool', content: [] } as unknown as Message,
    ];
    expect(latestTpsOf(messages)).toBe(100);
  });

  it('无任何有效速度时返回 null（不显示、不占位）', () => {
    expect(latestTpsOf([assistantMessage({})])).toBeNull();
    expect(latestTpsOf([])).toBeNull();
  });
});

describe('LiveTpsTracker（累计样本差分 + 独立跳秒计时）', () => {
  it('按累计值差分计算窗口速度', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', 100, 0);
    tracker.record('s1', 300, 2000);
    const snap = tracker.snapshot('s1', 2000);
    expect(snap.active).toBe(true);
    expect(snap.elapsedMs).toBe(2000);
    // 窗口内累计 100→300 = 200 tokens / 2s = 100 tok/s
    expect(round1(snap.tps)).toBe(100);
  });

  it('无累计（tokens 缺失）仍跳秒，速度为 0（review：无 usage 仍跳秒）', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', undefined, 0);
    tracker.record('s1', null, 1200);
    const snap = tracker.snapshot('s1', 3000);
    expect(snap.active).toBe(true);
    expect(snap.tps).toBe(0);
    expect(snap.elapsedMs).toBe(3000);
  });

  it('持续生成超过 2 秒：旧样本滑出窗口，跳秒不回退（review：长流秒数反复回退）', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', 10, 0);
    tracker.record('s1', 200, 1500);
    tracker.record('s1', 400, 3000);
    tracker.record('s1', 600, 4500);
    const snap = tracker.snapshot('s1', 5000);
    // 跳秒从首个事件起累计（独立于速度窗口）。
    expect(snap.elapsedMs).toBe(5000);
    // 速度窗口内（cutoff=3000，保留 3000 与 4500 两个样本）：
    // 400→600 = 200 tokens / 1.5s 跨度。
    expect(round1(snap.tps)).toBe(133.3);
  });

  it('累计值回退（轮内新一次 LLM 请求）重开样本窗口，保留最近速度', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', 100, 0);
    tracker.record('s1', 300, 2000);
    const streaming = tracker.snapshot('s1', 2000);
    expect(round1(streaming.tps)).toBe(100);

    // 工具执行后新一次 LLM 请求：counter 从 1 重新累计（回退）。
    tracker.record('s1', 1, 8000);
    const waiting = tracker.snapshot('s1', 8000);
    // 等待/新流爬升期沿用最近显示，不跳动。
    expect(round1(waiting.tps)).toBe(100);
    // 新窗口累计出速度后自然覆盖（219 tokens / 2s）。
    tracker.record('s1', 220, 10000);
    const next = tracker.snapshot('s1', 10000);
    expect(round1(next.tps)).toBe(109.5);
  });

  it('官方计量校准只对齐基准：上升不产生尖峰，下降不出现负差分', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', 10, 0);
    tracker.record('s1', 30, 2000);
    expect(round1(tracker.snapshot('s1', 2000).tps)).toBe(10);

    // 官方 usage 高于 delta 计数（粗粒度 delta）：差值不计入产量。
    tracker.calibrate('s1', 200, 2500);
    expect(round1(tracker.snapshot('s1', 2500).tps)).toBe(10);

    // 校准后正文从官方基准继续累计：差分从校准点算起。
    tracker.record('s1', 220, 3000);
    expect(round1(tracker.snapshot('s1', 3000).tps)).toBe(40);

    // 官方 usage 低于当前累计（估算偏高）：只重置基准，不产生负差分。
    tracker.calibrate('s1', 15, 4000);
    const after = tracker.snapshot('s1', 4000);
    expect(after.tps).toBe(40);
  });

  it('校准早于首次采样时先用已有样本补算速度（短请求结束即校准）', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', 1, 0);
    tracker.record('s1', 2, 100);
    // 模拟 600ms 采样尚未触发：从未 snapshot，lastTps 仍为 0。
    tracker.calibrate('s1', 19, 200);
    // 校准清空窗口前补算：1 token / 300ms ≈ 3.3 tok/s，而不是丢失显示。
    expect(round1(tracker.snapshot('s1', 200).tps)).toBe(3.3);
  });

  it('校准前的补算只用窗口内样本：已滑出窗口的旧样本不冒充本轮速度', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', 1, 0);
    tracker.record('s1', 2, 100);
    // 工具执行 / 等待期没有任何新样本，UI 也未采样（切走会话等）。
    // 此刻到达官方 usage：两个样本已滑出 2s 窗口，补算必须为 0——
    // 否则上一轮的 3.3 tok/s 会被当成本轮速度，而本轮其实毫无输出。
    tracker.calibrate('s1', 19, 5000);
    expect(tracker.snapshot('s1', 5000).tps).toBe(0);
  });

  it('官方结算值覆盖最近速度（消息定稿回灌），非法值忽略', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', 10, 0);
    tracker.record('s1', 30, 2000);
    expect(round1(tracker.snapshot('s1', 2000).tps)).toBe(10);

    tracker.setLastTps('s1', 87.5);
    expect(round1(tracker.snapshot('s1', 9000).tps)).toBe(87.5);

    tracker.setLastTps('s1', 0);
    tracker.setLastTps('s1', Number.NaN);
    expect(round1(tracker.snapshot('s1', 9000).tps)).toBe(87.5);
  });

  it('等待期（无新样本，旧样本滑出窗口）保留最后显示的速度', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', 100, 0);
    tracker.record('s1', 300, 2000);
    expect(round1(tracker.snapshot('s1', 2000).tps)).toBe(100);
    // 工具执行 10s：无新样本，速度保留而非归零/消失。
    expect(round1(tracker.snapshot('s1', 12000).tps)).toBe(100);
    // 轮次边界 reset 才清空。
    tracker.reset('s1');
    expect(tracker.snapshot('s1', 12100).tps).toBe(0);
  });

  it('tokens 非法值不计样本，但事件仍驱动跳秒', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', Number.NaN, 0);
    tracker.record('s1', -1, 500);
    const snap = tracker.snapshot('s1', 900);
    expect(snap.active).toBe(true);
    expect(snap.tps).toBe(0);
    expect(snap.elapsedMs).toBe(900);
  });

  it('reset 清空窗口与计时', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', 100, 0);
    tracker.record('s1', 200, 1000);
    tracker.reset('s1');
    expect(tracker.snapshot('s1', 600).active).toBe(false);
  });

  it('同批（≤1ms）样本合并，不堆积', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', 100, 1000);
    tracker.record('s1', 150, 1000);
    const snap = tracker.snapshot('s1', 1000);
    expect(snap.active).toBe(true);
    // 单样本（合并后）不足计算速度。
    expect(snap.tps).toBe(0);
  });

  it('开局分母下限：极短跨度不产生数值尖峰', () => {
    const tracker = new LiveTpsTracker();
    tracker.record('s1', 0, 0);
    tracker.record('s1', 20, 50);
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
