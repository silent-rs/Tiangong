/**
 * 中枢消息列表测试：空态问候（§4.2）、消息渲染（用户/助手）、流式增量。
 * 中枢 UI 独立于普通模式 MessageList，本文件只测 CentralMessageList。
 */
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>();
  return {
    ...actual,
    api: {
      ...actual.api,
      getSessions: vi.fn(() => Promise.resolve([])),
      loadSessionMessages: vi.fn(() => Promise.resolve({ messages: [], start: 0 })),
    },
  };
});
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}));

const { CentralMessageList } = await import('@/components/central/CentralMessageList');
const { useStore } = await import('@/store/useStore');
const { CENTRAL_SESSION_ID } = await import('@/lib/centralSession');

function userMessage(id: string, text: string) {
  return {
    id,
    role: 'user',
    content: [{ type: 'text', text }],
    created_at: '2026-11-20 12:00:00',
  };
}

function assistantMessage(id: string, text: string) {
  return {
    id,
    role: 'assistant',
    content: [{ type: 'text', text }],
    reasoning_content: '',
    created_at: '2026-11-20 12:00:01',
  };
}

function mountList(): () => void {
  const container = document.createElement('div');
  document.body.appendChild(container);
  let root!: Root;
  act(() => {
    root = createRoot(container);
    root.render(<CentralMessageList />);
  });
  return () => {
    act(() => root.unmount());
    container.remove();
  };
}

let cleanup: (() => void) | undefined;

beforeEach(() => {
  useStore.setState({
    messages: [],
    streamingMessageId: null,
    streamingContent: '',
    streamingReasoningContent: '',
    runStatus: 'idle',
    activeSessionId: CENTRAL_SESSION_ID,
  });
});

afterEach(() => {
  cleanup?.();
  cleanup = undefined;
});

describe('CentralMessageList', () => {
  it('空对话：显示中枢默认问候', () => {
    cleanup = mountList();
    expect(document.body.textContent).toContain('我是天工中枢');
  });

  it('有消息：渲染用户与助手消息', () => {
    act(() => {
      useStore.setState({
        messages: [
          userMessage('u1', '帮我看看这个问题'),
          assistantMessage('a1', '好的，我先了解一下情况。'),
        ],
      });
    });
    cleanup = mountList();
    expect(document.body.textContent).toContain('帮我看看这个问题');
    expect(document.body.textContent).toContain('好的，我先了解一下情况。');
  });

  it('生成中：流式增量与思考过程可见', () => {
    act(() => {
      useStore.setState({
        messages: [userMessage('u1', '继续')],
        streamingMessageId: 'a-streaming',
        streamingContent: '正在生成的部分回复',
        streamingReasoningContent: '思考片段',
        runStatus: 'executing',
      });
    });
    cleanup = mountList();
    expect(document.body.textContent).toContain('思考过程');
    expect(document.body.textContent).toContain('思考片段');
    expect(document.body.textContent).toContain('正在生成的部分回复');
  });
});
