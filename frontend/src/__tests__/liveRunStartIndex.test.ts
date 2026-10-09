import { describe, expect, it } from 'vitest';
import type { Message } from '@/api/tauri';
import { groupMessages, liveRunStartIndex } from '@/components/message';
import { legacyMessage, type LegacyMessageFields } from './legacyMessage';

function message(
  id: string,
  role: string,
  text: string,
  extra: LegacyMessageFields = {},
): Message {
  return legacyMessage({ id, role, content: text, created_at: '2026-09-20 00:00:00', ...extra });
}

describe('当前执行链起始组下标（引导消息不结束轮次）', () => {
  it('执行中注入引导消息：前序过程与引导消息同属当前执行链', () => {
    const groups = groupMessages([
      message('a1', 'user', '原始任务', { turn_status: 'processing' }),
      message('t1', 'assistant', '前序过程'),
      message('b1', 'user', '引导消息'),
      message('t2', 'assistant', '后续过程'),
    ]);
    // 组序列：[user a1][turn t1][user b1][turn t2]，链起点是起轮消息 a1。
    expect(groups.map((g) => g.type)).toEqual(['user', 'agent_turn', 'user', 'agent_turn']);
    expect(liveRunStartIndex(groups)).toBe(0);
  });

  it('多级引导链：起轮消息（processing）为起点', () => {
    const groups = groupMessages([
      message('a2', 'user', '原始任务', { turn_status: 'processing' }),
      message('t3', 'assistant', '过程一'),
      message('b2', 'user', '引导一'),
      message('t4', 'assistant', '过程二'),
      message('b3', 'user', '引导二'),
      message('t5', 'assistant', '过程三'),
    ]);
    expect(liveRunStartIndex(groups)).toBe(0);
  });

  it('历史已结算轮次不进链：新执行只覆盖自身锚点之后', () => {
    const groups = groupMessages([
      message('h1', 'user', '历史问题', { turn_status: 'success', elapsed_ms: 1200 }),
      message('h2', 'assistant', '历史过程'),
      message('a3', 'user', '新任务', { turn_status: 'processing' }),
      message('t6', 'assistant', '新过程'),
    ]);
    // 组序列：[user h1][turn h2][user a3][turn t6]，链起点是新锚点 a3（下标 2）。
    expect(liveRunStartIndex(groups)).toBe(2);
  });

  it('已结束轮次里的引导消息不扩大下一轮的链', () => {
    const groups = groupMessages([
      message('a4', 'user', '原始任务', { turn_status: 'success', elapsed_ms: 9000 }),
      message('t7', 'assistant', '过程'),
      // 上一轮的引导消息不带状态。
      message('b4', 'user', '引导消息'),
      message('t8', 'assistant', '上一轮收尾'),
      message('a5', 'user', '再起新任务', { turn_status: 'processing' }),
      message('t9', 'assistant', '新过程'),
    ]);
    // 链起点是 a5（下标 4），而非上一轮的引导消息 b4。
    expect(liveRunStartIndex(groups)).toBe(4);
  });

  it('接续意外中断的轮次：新消息作为引导消息，链起点是中断轮的起轮消息', () => {
    const groups = groupMessages([
      message('h5', 'user', '历史问题', { turn_status: 'success', elapsed_ms: 100 }),
      message('h6', 'assistant', '历史过程'),
      message('a6', 'user', '中断的任务', { turn_status: 'processing' }),
      message('t10', 'assistant', '中断前过程'),
      message('b5', 'user', '重启后的新消息'),
      message('t11', 'assistant', '接续过程'),
    ]);
    expect(liveRunStartIndex(groups)).toBe(2);
  });

  it('全部轮次已结算：无活跃组，返回组数量', () => {
    const groups = groupMessages([
      message('h3', 'user', '问题', { turn_status: 'failed', elapsed_ms: 300 }),
      message('h4', 'assistant', '过程'),
    ]);
    expect(liveRunStartIndex(groups)).toBe(groups.length);
  });

  it('空分组返回 0', () => {
    expect(liveRunStartIndex([])).toBe(0);
  });
});
