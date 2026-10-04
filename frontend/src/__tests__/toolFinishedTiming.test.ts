import { beforeEach, describe, expect, it } from 'vitest';

import { buildFinishedToolModel } from '@/components/message/toolDisplayModel';
import { useStore } from '@/store/useStore';

const SESSION_ID = 'session-tool-finished';

function apply(event: Record<string, unknown>) {
  useStore.getState().applyStreamEvents([
    { session_id: SESSION_ID, event: event as never },
  ]);
}

describe('tool_finished 运行计时', () => {
  beforeEach(() => {
    useStore.setState({
      activeSessionId: SESSION_ID,
      isNewConversation: false,
      messages: [],
      runStatus: 'idle',
      toolCallStartedAt: {},
      toolCallFinished: {},
    });
  });

  it('后序工具先完成时记录真实耗时，结果按序提交后清除', () => {
    apply({
      type: 'tool_calls',
      message_id: 'assistant-1',
      names: ['web_fetch', 'web_fetch'],
      calls: [
        { id: 'call-slow', name: 'web_fetch', arguments: { url: 'https://a.example' } },
        { id: 'call-fast', name: 'web_fetch', arguments: { url: 'https://b.example' } },
      ],
    });
    expect(Object.keys(useStore.getState().toolCallStartedAt).sort()).toEqual([
      'call-fast',
      'call-slow',
    ]);

    // 后序工具先完成：仅记录完成信息，仍保留在运行列表中等待按序提交。
    apply({ type: 'tool_finished', tool_call_id: 'call-fast', name: 'web_fetch', ok: true, duration_ms: 1200 });
    expect(useStore.getState().toolCallFinished['call-fast']).toEqual({ ok: true, durationMs: 1200 });
    expect(useStore.getState().toolCallStartedAt['call-fast']).toBeDefined();

    // 前序工具完成并提交结果后，后序工具结果随之提交，完成信息被清除。
    apply({ type: 'tool_finished', tool_call_id: 'call-slow', name: 'web_fetch', ok: false, duration_ms: 30000 });
    apply({ type: 'tool_result', tool_call_id: 'call-slow', name: 'web_fetch', ok: false, output: 'timeout', duration_ms: 30000 });
    apply({ type: 'tool_result', tool_call_id: 'call-fast', name: 'web_fetch', ok: true, output: 'done', duration_ms: 1200 });

    const state = useStore.getState();
    expect(state.toolCallFinished).toEqual({});
    expect(state.toolCallStartedAt).toEqual({});
  });

  it('本轮结束时清空未提交的完成信息', () => {
    apply({ type: 'tool_finished', tool_call_id: 'call-x', name: 'web_fetch', ok: true, duration_ms: 10 });
    expect(useStore.getState().toolCallFinished['call-x']).toBeDefined();
    apply({ type: 'done' });
    expect(useStore.getState().toolCallFinished).toEqual({});
  });

  it('完成待提交行停止计时并显示真实耗时', () => {
    const ok = buildFinishedToolModel('web_fetch', { url: 'https://b.example' }, { ok: true, durationMs: 1200 });
    expect(ok.state).toBe('ok');
    expect(ok.durationMs).toBe(1200);
    expect(ok.errorSummary).toBeNull();

    const failed = buildFinishedToolModel('web_fetch', { url: 'https://a.example' }, { ok: false, durationMs: 30000 });
    expect(failed.state).toBe('error');
    expect(failed.errorSummary).toBe('执行失败');
  });
});
