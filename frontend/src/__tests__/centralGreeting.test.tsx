/**
 * 中枢视图测试：空态呈现中枢默认问候（§4.2），有消息/生成中复用基础
 * 组件 MessageList（经 LazyComponents，测试中以标记 mock）。中枢输入区
 * 与征询宿主的挂载一并覆盖。
 */
import { act, type ReactNode } from 'react';
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
vi.mock('@/components/LazyComponents', () => ({
  LazyMessageList: () => <div data-testid="central-message-list">message-list</div>,
}));
vi.mock('@/components/InteractionPluginHost', () => ({
  InteractionPluginHost: () => null,
}));
vi.mock('@/components/central/CentralChatInput', () => ({
  CentralChatInput: () => <div data-testid="central-chat-input">chat-input</div>,
}));

const { CentralModeView } = await import('@/components/central/CentralModeView');
const { useStore } = await import('@/store/useStore');
const { CENTRAL_SESSION_ID } = await import('@/lib/centralSession');

function mountView(): () => void {
  const container = document.createElement('div');
  document.body.appendChild(container);
  let root!: Root;
  act(() => {
    root = createRoot(container);
    root.render(<CentralModeView />);
  });
  return () => {
    act(() => root.unmount());
    container.remove();
  };
}

let cleanup: (() => void) | undefined;

beforeEach(() => {
  useStore.setState({
    activeSessionId: CENTRAL_SESSION_ID,
    messages: [],
    streamingMessageId: null,
    streamingContent: '',
    streamingReasoningContent: '',
    runStatus: 'idle',
  });
});

afterEach(() => {
  cleanup?.();
  cleanup = undefined;
});

describe('CentralModeView', () => {
  it('空对话：显示中枢默认问候，不挂消息列表', () => {
    cleanup = mountView();
    expect(document.body.textContent).toContain('我是天工中枢');
    expect(document.querySelector('[data-testid="central-message-list"]')).toBeNull();
    // 专属输入区挂载
    expect(document.querySelector('[data-testid="central-chat-input"]')).not.toBeNull();
  });

  it('有消息：复用 MessageList 渲染，问候不出现', () => {
    act(() => {
      useStore.setState({
        messages: [{
          id: 'm1',
          role: 'user',
          content: [{ type: 'text', text: '帮我看看这个问题' }],
          created_at: '2026-11-20 12:00:00',
        }],
      });
    });
    cleanup = mountView();
    expect(document.querySelector('[data-testid="central-message-list"]')).not.toBeNull();
    expect(document.body.textContent).not.toContain('我是天工中枢');
  });

  it('生成中（首条已投递）：交给 MessageList 的 typing 态，不显示问候', () => {
    act(() => {
      useStore.setState({ runStatus: 'executing' });
    });
    cleanup = mountView();
    expect(document.querySelector('[data-testid="central-message-list"]')).not.toBeNull();
    expect(document.body.textContent).not.toContain('我是天工中枢');
  });
});
