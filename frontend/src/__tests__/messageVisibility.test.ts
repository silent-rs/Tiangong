import { describe, expect, it } from 'vitest';
import type { Message } from '@/api/tauri';
import { groupMessages } from '@/components/message';
import { findSearchMatches } from '@/utils/search';
import { legacyMessage } from './legacyMessage';

function message(
  id: string,
  role: string,
  text: string,
  phase = 'normal',
): Message {
  return legacyMessage({ id, role, content: text, phase, created_at: '2026-07-19 00:00:00' });
}

describe('压缩续接消息可见性', () => {
  const messages = [
    message('user-1', 'user', '第一条真实问题'),
    message('assistant-1', 'assistant', '第一条真实回答'),
    message('resume', 'user', '仅供模型使用的续接内容', 'compressedresume'),
    message('user-2', 'user', '第二条真实问题'),
    message('assistant-2', 'assistant', '第二条真实回答'),
  ];

  it('分组时只隐藏续接消息并保留完整真实对话', () => {
    const visibleIds = groupMessages(messages).flatMap((group) =>
      group.messages.map((item) => item.id),
    );

    expect(visibleIds).toEqual(['user-1', 'assistant-1', 'user-2', 'assistant-2']);
  });

  it('所有搜索范围都排除续接消息', () => {
    expect(findSearchMatches(messages, '续接内容', [], 'messages')).toEqual([]);
    expect(findSearchMatches(messages, '续接内容', [], 'withThinking')).toEqual([]);
    expect(findSearchMatches(messages, '续接内容', [], 'all')).toEqual([]);
    expect(findSearchMatches(messages, '第二条真实问题', [], 'messages')).toHaveLength(1);
  });
});

describe('宿主注入媒体消息（RFC 0017）', () => {
  const messages = [
    message('user-1', 'user', '总结微信群聊'),
    message('assistant-1', 'assistant', '好的，我先截取屏幕'),
    message('inject-1', 'user', '[injected-images provenance]', 'hostinjected'),
    message('assistant-2', 'assistant', '我看到了截图内容'),
  ];

  it('分组时注入消息归入助手轮次组（assistant 侧展示）', () => {
    const groups = groupMessages(messages);
    const visibleIds = groups.flatMap((group) =>
      group.messages.map((item) => item.id),
    );

    expect(visibleIds).toEqual(['user-1', 'assistant-1', 'inject-1', 'assistant-2']);
    const injectGroup = groups.find((group) =>
      group.messages.some((item) => item.id === 'inject-1'),
    );

    expect(injectGroup?.type).toBe('agent_turn');
    expect(injectGroup?.messages.some((item) => item.id === 'assistant-1')).toBe(true);
  });

  it('搜索范围排除注入消息', () => {
    expect(findSearchMatches(messages, 'injected-images', [], 'messages')).toEqual([]);
    expect(findSearchMatches(messages, 'injected-images', [], 'all')).toEqual([]);
  });
});
