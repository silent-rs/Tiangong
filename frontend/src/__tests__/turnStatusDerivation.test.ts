import { beforeEach, describe, expect, it } from 'vitest';

import { useStore } from '@/store/useStore';

let sessionId = '';

function apply(event: Record<string, unknown>) {
  useStore.getState().applyStreamEvents([
    { session_id: sessionId, event: event as never },
  ]);
}

function statusOf(id: string) {
  return useStore.getState().messages.find((message) => message.id === id)?.turn_status;
}

describe('用户消息的轮次状态（与后端同一规则推导）', () => {
  let caseIndex = 0;
  beforeEach(() => {
    // 会话视图缓存按会话 ID 常驻模块内：每个用例使用独立会话。
    caseIndex += 1;
    sessionId = `session-turn-status-${caseIndex}`;
    useStore.setState({
      activeSessionId: sessionId,
      isNewConversation: false,
      messages: [],
      runStatus: 'idle',
      toolCallStartedAt: {},
      toolCallFinished: {},
    });
  });

  it('起轮消息为 processing，引导消息不带状态，终态写回起轮消息', () => {
    apply({ type: 'user_message', message_id: 'u1', content: '任务' });
    expect(statusOf('u1')).toBe('processing');

    apply({ type: 'user_message', message_id: 'u2', content: '引导' });
    expect(statusOf('u2')).toBeUndefined();

    apply({ type: 'done' });
    expect(statusOf('u1')).toBe('success');
    expect(statusOf('u2')).toBeUndefined();

    apply({ type: 'user_message', message_id: 'u3', content: '新任务' });
    expect(statusOf('u3')).toBe('processing');
  });

  it('上一轮意外中断（残留 processing）：新消息作为引导消息接续该轮', () => {
    useStore.setState({
      messages: [
        {
          id: 'crashed',
          role: 'user',
          content: [{ type: 'text', text: '中断的任务' }],
          reasoning_content: '',
          created_at: '2026-01-01 00:00:00',
          turn_status: 'processing',
        },
      ],
    });
    apply({ type: 'user_message', message_id: 'next', content: '继续' });
    expect(statusOf('next')).toBeUndefined();

    apply({ type: 'error', message: '执行失败' });
    expect(statusOf('crashed')).toBe('failed');
  });
});
