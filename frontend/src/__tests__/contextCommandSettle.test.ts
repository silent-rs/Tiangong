import { beforeEach, describe, expect, it } from 'vitest';
import { useStore } from '@/store/useStore';

let sessionId = '';

function apply(event: Record<string, unknown>) {
  useStore.getState().applyStreamEvents([
    { session_id: sessionId, event: event as never },
  ]);
}

/**
 * 手动整理上下文的运行态只由事件流终态 `context_compressed` 收尾：
 * 命令持锁返回时，本次压缩的用量 / 通知事件可能尚未转发，前端不能据
 * 返回值提前回到空闲（否则晚到的事件会把会话重新拉回执行中且再无终态）。
 */
describe('手动整理上下文的运行态收尾', () => {
  let caseIndex = 0;
  beforeEach(() => {
    caseIndex += 1;
    sessionId = `session-context-settle-${caseIndex}`;
    useStore.setState({
      activeSessionId: sessionId,
      isNewConversation: false,
      messages: [],
      runStatus: 'idle',
      runSummary: '',
      sessionRunStatuses: {},
      toolCallStartedAt: {},
      toolCallFinished: {},
    });
  });

  it.each(['failed', 'cancelled', 'compress', 'noop'])(
    '压缩以 %s 结束：终态之前的事件保持执行中，终态事件回到空闲',
    (action) => {
      useStore.getState().beginContextManagement('正在压缩早期上下文...');
      apply({ type: 'context_compressing', summary_up_to: 0, total_messages: 4 });
      apply({ type: 'token_usage', source: 'manual_context_compress_failed' });
      apply({
        type: 'session_message_upsert',
        message: { id: 'usage-1', role: 'notice', content: [], created_at: '2026-01-01 00:00:00' },
      });
      apply({ type: 'agent_notification', content: '手动压缩上下文失败：boom', level: 'error' });
      expect(useStore.getState().runStatus).toBe('executing');
      expect(useStore.getState().sessionRunStatuses[sessionId]).toBe('executing');

      apply({ type: 'context_compressed', action, summary_up_to: 0, remaining_messages: 4 });

      const state = useStore.getState();
      expect(state.runStatus).toBe('idle');
      expect(state.sessionRunStatuses[sessionId]).toBeUndefined();
    },
  );

  it('终态事件先于命令返回到达时同样回到空闲', () => {
    useStore.getState().beginContextManagement('正在压缩早期上下文...');
    apply({ type: 'context_compressed', action: 'failed', summary_up_to: 0, remaining_messages: 4 });
    expect(useStore.getState().runStatus).toBe('idle');
  });
});
