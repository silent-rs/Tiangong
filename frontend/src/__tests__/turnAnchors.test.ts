import { describe, expect, it } from 'vitest';
import type { Message } from '@/api/tauri';
import { latestTurnAnchorIndex, turnAnchorIds } from '@/api/message';
import { legacyMessage, type LegacyMessageFields } from './legacyMessage';

function message(id: string, role: string, extra: LegacyMessageFields = {}): Message {
  return legacyMessage({ id, role, content: id, created_at: '2026-10-11 00:00:00', ...extra });
}

describe('轮次锚点（只有起轮消息，引导消息不是锚点）', () => {
  const messages = [
    message('a1', 'user', { turn_status: 'success', elapsed_ms: 10 }),
    message('r1', 'assistant'),
    message('g1', 'user'), // 执行中追加的引导消息
    message('r2', 'assistant'),
    message('a2', 'user', { turn_status: 'failed', elapsed_ms: 10 }),
    message('r3', 'assistant'),
    message('g2', 'user', { turn_status: 'cancelled', elapsed_ms: 5 }), // 失败后接续（历史数据）
    message('r4', 'assistant'),
    message('a3', 'user', { turn_status: 'success', elapsed_ms: 10 }), // 取消后另起一轮
    message('r5', 'assistant'),
  ];

  it('引导消息与失败后接续的消息不是锚点；取消后的消息是锚点', () => {
    expect([...turnAnchorIds(messages)]).toEqual(['a1', 'a2', 'a3']);
  });

  it('最后一个锚点：跳过引导消息与失败接续链', () => {
    expect(latestTurnAnchorIndex(messages)).toBe(8);
    expect(latestTurnAnchorIndex(messages.slice(0, 8))).toBe(4);
    expect(latestTurnAnchorIndex(messages.slice(0, 4))).toBe(0);
    expect(latestTurnAnchorIndex([])).toBe(-1);
  });

  it('轮次状态出现前的旧数据：每条用户消息都是锚点', () => {
    const legacy = [message('u1', 'user'), message('x1', 'assistant'), message('u2', 'user')];
    expect([...turnAnchorIds(legacy)]).toEqual(['u1', 'u2']);
    expect(latestTurnAnchorIndex(legacy)).toBe(2);
  });

  it('执行中：起轮 processing 之后的引导消息不是锚点', () => {
    const live = [
      message('a', 'user', { turn_status: 'processing' }),
      message('x', 'assistant'),
      message('g', 'user'),
    ];
    expect([...turnAnchorIds(live)]).toEqual(['a']);
    expect(latestTurnAnchorIndex(live)).toBe(0);
  });
});
