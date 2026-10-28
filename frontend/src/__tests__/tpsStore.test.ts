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

  it('user_message / done 清空窗口（轮次边界）', () => {
    feed('e2e-1', [
      { type: 'react_text', message_id: 'm1', content: 'x', tokens: 5 },
      { type: 'react_text', message_id: 'm1', content: 'y', tokens: 9 },
    ]);
    expect(liveTpsTracker.snapshot('e2e-1').active).toBe(true);
    feed('e2e-1', [{ type: 'done', usage: null }]);
    expect(liveTpsTracker.snapshot('e2e-1').active).toBe(false);
  });
});
