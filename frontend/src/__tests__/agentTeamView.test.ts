import { describe, expect, it } from 'vitest';

import type { Message } from '@/api/tauri';
import { finalReplyOf } from '@/api/message';
import { hasMessage } from '@/components/message/utils';
import { parseAgentsFromMessages, useStore } from '@/store/useStore';

function systemMessage(id: string, text: string): Message {
  return {
    id,
    role: { type: 'system' },
    content: [{ type: 'text', text }],
    created_at: '2026-07-12 00:00:00',
  };
}

describe('agent team view routing', () => {
  it('updates equal-label agents by exact agent id', () => {
    const created = [
      systemMessage('create-dev', '[Agent] Worker (dev) 已加入团队 id=agent-dev'),
      systemMessage('create-test', '[Agent] Worker (test) 已加入团队 id=agent-test'),
    ];
    const running = parseAgentsFromMessages([
      ...created,
      systemMessage(
        'agent-status:agent-test',
        '[Agent] Worker 状态变更: running id=agent-test',
      ),
    ]);

    expect(running.find((agent) => agent.agentId === 'agent-dev')?.status).toBe('idle');
    expect(running.find((agent) => agent.agentId === 'agent-test')?.status).toBe('running');

    const terminated = parseAgentsFromMessages([
      ...created,
      systemMessage(
        'agent-status:agent-test',
        '[Agent] Worker 状态变更: terminated id=agent-test',
      ),
    ]);
    expect(terminated.map((agent) => agent.agentId)).toEqual(['agent-dev']);
  });

  it('replaces the anchor user message when only final_reply changes', () => {
    const anchor: Message = {
      id: 'main-anchor',
      role: { type: 'user', turn_status: 'processing' },
      content: [{ type: 'text', text: '请审查' }],
      created_at: '2026-07-12 00:00:00',
    };
    const reply: Message = {
      id: 'main-result',
      role: { type: 'assistant' },
      content: [{ type: 'text', text: '最终审查结果' }],
      created_at: '2026-07-12 00:00:01',
    };
    const finished: Message = {
      ...anchor,
      role: { type: 'user', turn_status: 'success', final_reply: 'main-result' },
    };
    useStore.setState({
      activeSessionId: 'session-main',
      isNewConversation: false,
      messages: [anchor, reply],
      runStatus: 'idle',
      streamingMessageId: null,
      streamingContent: '',
      streamingReasoningContent: '',
    });
    useStore.getState().applyStreamEvents([{
      session_id: 'session-main',
      event: { type: 'session_message_upsert', message: finished },
    }]);

    const [result, untouched] = useStore.getState().messages;
    expect(result).not.toBe(anchor);
    expect(finalReplyOf(result)).toBe('main-result');
    expect(untouched).toBe(reply);
  });

  it('keeps message content changes and checks the requested streaming id', () => {
    const sessionId = 'session-model-exclusion';
    const visible = systemMessage('agent-process', '执行过程');
    const excluded = { ...visible, content: [{ type: 'text' as const, text: '执行过程（更新）' }] };

    useStore.setState({
      activeSessionId: sessionId,
      isNewConversation: false,
      messages: [visible],
      runStatus: 'idle',
    });
    useStore.getState().applyStreamEvents([{
      session_id: sessionId,
      event: { type: 'session_message_upsert', message: excluded },
    }]);

    expect(useStore.getState().messages[0].content).toEqual(excluded.content);
    expect(hasMessage([visible], 'missing')).toBe(false);
    expect(hasMessage([visible], visible.id)).toBe(true);
  });
});
