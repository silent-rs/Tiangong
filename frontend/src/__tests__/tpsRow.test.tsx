import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useStore } from '@/store/useStore';
import { liveTpsTracker } from '@/utils/tps';
import { StreamingTpsRow } from '@/components/message/StreamingTpsRow';
import type { MessageUsage, SessionStreamEvent, TokenUsage } from '@/api/tauri';
import type { MessageItem } from '@/components/message/types';

function feed(sessionId: string, events: Array<Record<string, unknown>>) {
  useStore.getState().applyStreamEvents(
    events.map((event) => ({ session_id: sessionId, event: { ...event } } as SessionStreamEvent)),
  );
}

function usageMessage(id: string): MessageItem {
  const tokens: TokenUsage = {
    prompt_tokens: 100, completion_tokens: 10, total_tokens: 110,
    prompt_cache_hit_tokens: null, prompt_cache_miss_tokens: null,
  };
  const usage: MessageUsage = { ...tokens, model: 'test-model', agent_id: 'agent-1', turn_id: 'turn-1', source: 'react', status: 'success' };
  return { id, role: 'assistant', usage, content: [{ type: 'text', text: '回复' }], created_at: new Date().toISOString() };
}

describe('StreamingTpsRow（渲染层端到端）', () => {
  let container: HTMLDivElement | null = null;
  let root: Root | null = null;

  beforeEach(() => {
    vi.useFakeTimers();
    useStore.setState({ activeSessionId: 'render-1', newConversationId: null });
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });
  afterEach(() => {
    act(() => root?.unmount());
    container?.remove();
    vi.useRealTimers();
    liveTpsTracker.reset('render-1');
  });

  function renderRow(props: { usageMessages?: MessageItem[] } = {}) {
    act(() => {
      root!.render(<StreamingTpsRow {...props} />);
    });
  }

  function statusText(): string | null {
    return container!.querySelector('[role="status"]')?.textContent ?? null;
  }

  it('累计样本充足时渲染速度文本（与操作行同构的小字行）', () => {
    useStore.setState({ streamingMessageId: 'm1' });
    renderRow();

    act(() => {
      feed('render-1', [{ type: 'react_text', message_id: 'm1', content: '你好', tokens: 8 }]);
      vi.advanceTimersByTime(100);
      feed('render-1', [{ type: 'react_text', message_id: 'm1', content: '，世界', tokens: 21 }]);
      vi.advanceTimersByTime(100);
      feed('render-1', [{ type: 'react_text', message_id: 'm1', content: '！', tokens: 33 }]);
      // useLiveTps 600ms 采样节拍。
      vi.advanceTimersByTime(700);
    });

    const text = statusText();
    expect(text).toContain('tok/s');
    expect(text).not.toContain('正在生成');
    expect(text).not.toMatch(/\d+s/);
  });

  it('无速度样本且无用量数据时整行不渲染（轮次开始不出现空分隔线）', () => {
    useStore.setState({ streamingMessageId: 'm2' });
    renderRow();

    act(() => {
      feed('render-1', [{ type: 'react_text', message_id: 'm2', content: 'x' }]);
      vi.advanceTimersByTime(700);
    });

    expect(container!.querySelector('[role="status"]')).toBeNull();
  });

  it('已有用量数据时即使没有速度样本也渲染徽标（同行不带速度）', () => {
    useStore.setState({ streamingMessageId: 'm3' });
    renderRow({ usageMessages: [usageMessage('m3')] });

    const status = container!.querySelector('[role="status"]');
    expect(status).not.toBeNull();
    expect(status!.textContent).not.toContain('tok/s');
    expect(status!.querySelector('button')?.getAttribute('aria-label')).toBe('查看调用用量详情');
  });
});
