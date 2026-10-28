import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useStore } from '@/store/useStore';
import { liveTpsTracker } from '@/utils/tps';
import { StreamingTpsRow } from '@/components/message/StreamingTpsRow';
import type { SessionStreamEvent } from '@/api/tauri';

function feed(sessionId: string, events: Array<Record<string, unknown>>) {
  useStore.getState().applyStreamEvents(
    events.map((event) => ({ session_id: sessionId, event: { ...event } } as SessionStreamEvent)),
  );
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

  function renderRow() {
    act(() => {
      root!.render(<StreamingTpsRow />);
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

  it('无速度样本时行占位、内容为空（不显示文案与秒数）', () => {
    useStore.setState({ streamingMessageId: 'm2' });
    renderRow();

    act(() => {
      feed('render-1', [{ type: 'react_text', message_id: 'm2', content: 'x' }]);
      vi.advanceTimersByTime(700);
    });

    const status = container!.querySelector('[role="status"]');
    expect(status).not.toBeNull();
    expect(status!.textContent).toBe('');
    expect(status!.getAttribute('aria-label')).toContain('正在生成');
  });
});
