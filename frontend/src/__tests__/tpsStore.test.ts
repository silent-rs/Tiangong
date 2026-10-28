import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useStore } from '@/store/useStore';
import { liveTpsTracker } from '@/utils/tps';
import type { SessionStreamEvent } from '@/api/tauri';

/** 端到端：流事件（后端累计 tokens）→ store 喂点 → tracker 快照。 */
function feed(sessionId: string, events: Array<Record<string, unknown>>) {
  useStore.getState().applyStreamEvents(
    events.map((event) => ({ session_id: sessionId, event: { ...event } } as SessionStreamEvent)),
  );
}

describe('实时 TPS 全链路（applyStreamEvents → LiveTpsTracker）', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
    liveTpsTracker.reset('e2e-1');
    liveTpsTracker.reset('e2e-2');
  });

  it('带累计 tokens 的正文事件产生速度与跳秒（模拟后端 100ms 批节奏）', () => {
    // 后端节流层每 100ms flush 一批正文事件；前端按批到达间隔入窗。
    feed('e2e-1', [
      { type: 'react_text', message_id: 'm1', content: '你好', tokens: 8 },
    ]);
    vi.advanceTimersByTime(100);
    feed('e2e-1', [
      { type: 'react_text', message_id: 'm1', content: '，世界', tokens: 21 },
    ]);
    vi.advanceTimersByTime(100);
    feed('e2e-1', [
      { type: 'react_text', message_id: 'm1', content: '！', tokens: 33 },
    ]);

    const snap = liveTpsTracker.snapshot('e2e-1');
    expect(snap.active).toBe(true);
    expect(snap.elapsedMs).toBe(200);
    // 窗口内累计 8→33 = 25 tokens / 200ms（分母下限 300ms）≈ 83.3 tok/s。
    expect(snap.tps).toBeGreaterThan(0);
  });

  it('缺 tokens 字段的事件仍驱动跳秒但不产生速度（旧后端 / 插件反馈）', () => {
    feed('e2e-2', [
      { type: 'react_text', message_id: 'm1', content: 'a' },
      { type: 'delta', message_id: 'm2', content: 'b' },
    ]);
    const snap = liveTpsTracker.snapshot('e2e-2');
    expect(snap.active).toBe(true);
    expect(snap.elapsedMs).not.toBeNull();
    expect(snap.tps).toBe(0);
  });

  it('轮次结束（done）速度保留显示，不清空', () => {
    feed('e2e-1', [
      { type: 'react_text', message_id: 'm1', content: 'x', tokens: 5 },
      { type: 'react_text', message_id: 'm1', content: 'y', tokens: 9 },
    ]);
    const before = liveTpsTracker.snapshot('e2e-1');
    expect(before.active).toBe(true);
    expect(before.tps).toBeGreaterThanOrEqual(0);
    feed('e2e-1', [{ type: 'done', usage: null }]);
    // 轮次边界不再清空：底部统计区保留最近速度（仅 app 重启 / core
    // 实例销毁才随内存消失）。
    const after = liveTpsTracker.snapshot('e2e-1');
    expect(after.active).toBe(true);
    expect(after.tps).toBe(before.tps);
  });

  it('token_usage 事件的官方计量补记为校准样本（流末校准传导）', () => {
    // 节流层的最终校准可能没有后续正文事件可搭；经 token_usage 事件
    // 到达的官方 completion_tokens 补记，单流即消息级累计。
    feed('e2e-1', [{ type: 'react_text', message_id: 'm1', content: 'x', tokens: 2 }]);
    vi.advanceTimersByTime(100);
    feed('e2e-1', [{ type: 'react_text', message_id: 'm1', content: 'y', tokens: 4 }]);
    vi.advanceTimersByTime(100);
    feed('e2e-1', [{
      type: 'token_usage',
      usage: { prompt_tokens: 10, completion_tokens: 6, total_tokens: 16 },
      current_tokens: null,
      compression_threshold_tokens: null,
      context_limit_tokens: null,
      source: 'chat',
      agent_id: null,
    }]);
    const snap = liveTpsTracker.snapshot('e2e-1');
    expect(snap.active).toBe(true);
    // 2→6（校准）= 4 tokens / 200ms（分母下限 300ms）≈ 13.3 tok/s。
    expect(snap.tps).toBeGreaterThan(0);
  });
});
