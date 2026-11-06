/**
 * 中枢对话 UI 空态测试：真实渲染 MessageList，验证中枢模式空对话呈现
 * 默认问候（设计稿 §4.2），普通模式保持原「欢迎使用天工」欢迎语。
 *
 * 中枢对话链路（绑定、进入/退出切换）由 centralConversation.test.ts 在
 * store 层覆盖；本文件聚焦空态渲染分支。
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
      loadSession: vi.fn(() => Promise.resolve(null)),
      loadSessionMessages: vi.fn(() => Promise.resolve({ messages: [], start: 0 })),
    },
  };
});
// MessageList 通过 api/host 的 listen 订阅事件；desktop 宿主走 Tauri listen，
// 浏览器测试环境不可用，mock 为空订阅。
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}));
vi.mock('@tauri-apps/plugin-dialog', () => ({ open: vi.fn() }));

const { MessageList } = await import('@/components/MessageList');
const { useStore } = await import('@/store/useStore');

// MessageList 在有消息时使用 react-virtual（依赖 ResizeObserver）；jsdom 不提供
vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });

function mountMessageList(): () => void {
  const container = document.createElement('div');
  document.body.appendChild(container);
  let root!: Root;
  act(() => {
    root = createRoot(container);
    root.render(<MessageList />);
  });
  return () => {
    act(() => root.unmount());
    container.remove();
  };
}

beforeEach(() => {
  useStore.setState({ centralMode: false, messages: [], activeSessionId: null });
});

afterEach(() => {
  useStore.setState({ centralMode: false });
});

describe('MessageList 空态：中枢问候与普通欢迎', () => {
  it('普通模式显示「欢迎使用天工」', () => {
    const cleanup = mountMessageList();
    expect(document.body.textContent).toContain('欢迎使用天工');
    expect(document.body.textContent).not.toContain('我是天工中枢');
    cleanup();
  });

  it('中枢模式（desktop）空对话显示中枢默认问候', () => {
    act(() => {
      useStore.setState({ centralMode: true });
    });
    const cleanup = mountMessageList();
    expect(document.body.textContent).toContain('我是天工中枢');
    expect(document.body.textContent).not.toContain('欢迎使用天工');
    cleanup();
  });
});
