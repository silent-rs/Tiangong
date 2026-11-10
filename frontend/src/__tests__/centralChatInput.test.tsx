/**
 * 中枢输入区发送链路测试：真实 store + mock api，从输入到点击发送走完整
 * sendMessage 路径，验证 revision/claim 契约与参数正确性。
 */
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({
  api: {
    setInputCache: vi.fn((_cacheKey: string, cache: unknown) => Promise.resolve(
      JSON.parse(JSON.stringify(cache)),
    )),
    removeInputCache: vi.fn(() => Promise.resolve()),
    sendMessage: vi.fn(() => Promise.resolve()),
    appendMessage: vi.fn(() => Promise.resolve(true)),
    cancelTurn: vi.fn(() => Promise.resolve(true)),
    loadSession: vi.fn(() => Promise.reject(new Error('会话不存在'))),
    getSessions: vi.fn(() => Promise.resolve([])),
    getReasoningEffort: vi.fn(() => Promise.resolve('medium')),
    getDefaultReasoningEffort: vi.fn(() => Promise.resolve('medium')),
    newSessionId: vi.fn(() => Promise.resolve('fresh-1')),
    prewarmWorkspaceIndex: vi.fn(() => Promise.resolve()),
    setReasoningEffort: vi.fn(() => Promise.resolve()),
    getInputCache: vi.fn(() => Promise.resolve(null)),
  },
}));

vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>();
  return { ...actual, api: { ...actual.api, ...mocks.api } };
});

const { CentralChatInput } = await import('@/components/central/CentralChatInput');
const { CENTRAL_SESSION_ID } = await import('@/lib/centralSession');
const { useStore } = await import('@/store/useStore');

function mountInput(): { unmount: () => void; textarea: () => HTMLTextAreaElement; send: () => void } {
  const container = document.createElement('div');
  document.body.appendChild(container);
  let root!: Root;
  act(() => {
    root = createRoot(container);
    root.render(<CentralChatInput />);
  });
  const textarea = () => container.querySelector('textarea')!;
  const send = () => {
    const button = container.querySelector<HTMLButtonElement>('button[aria-label="发送"]');
    expect(button).toBeTruthy();
    act(() => {
      button?.click();
    });
  };
  return {
    unmount: () => {
      act(() => root.unmount());
      container.remove();
    },
    textarea,
    send,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  mocks.api.sendMessage.mockResolvedValue(undefined);
  // setInputCache 必须回传缓存对象（后端语义）：返回值会进入
  // mergeStoredInputCache / sendMessage 的投递快照
  mocks.api.setInputCache.mockImplementation((_key: string, cache: unknown) =>
    Promise.resolve(JSON.parse(JSON.stringify(cache)) as never));
  const initial = useStore.getInitialState();
  useStore.setState({
    ...initial,
    isNewConversation: false,
    activeSessionId: null,
    newConversationId: null,
    messages: [],
  }, true);
});

describe('CentralChatInput 发送链路', () => {
  it('输入并点击发送：以固定中枢会话 ID 调 sendMessage，成功后清空草稿', async () => {
    const view = mountInput();
    const target = view.textarea();
    await act(async () => {
      target.focus();
    });
    // 模拟逐字输入（驱动 inputCache 创建与 revision 递增）
    const change = (value: string) => act(() => {
      const setter = Object.getOwnPropertyDescriptor(
        window.HTMLTextAreaElement.prototype,
        'value',
      )!.set!;
      setter.call(target, value);
      target.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await change('帮我看看这个问题');
    expect(view.textarea().value).toBe('帮我看看这个问题');
    // revision 已随输入递增
    const revision = useStore.getState().inputCaches[CENTRAL_SESSION_ID]?.revision;
    expect(revision).toBeGreaterThan(0);

    await act(async () => {
      view.send();
      // 让 sendMessage 的 await 链走完（sync claim + api.sendMessage + settle）
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    await act(async () => {
      await Promise.resolve();
    });

    expect(mocks.api.sendMessage).toHaveBeenCalledTimes(1);
    const args = mocks.api.sendMessage.mock.calls[0];
    expect(args[0]).toBe(CENTRAL_SESSION_ID);
    expect(args[1]).toBe('帮我看看这个问题');
    expect(args[2]).toEqual([]);
    expect(args[3]).toBe(revision);
    // 中枢固定会话：不携带 cwd / trust / effort / model
    expect(args[4]).toBeUndefined();
    expect(args[5]).toBeUndefined();
    // 发送成功后草稿清空
    expect(useStore.getState().inputCaches[CENTRAL_SESSION_ID]?.text).toBe('');
    view.unmount();
  });

  it('执行中（runStatus=executing）继续发送走引导 appendMessage，不被拒', async () => {
    const view = mountInput();
    const target = view.textarea();
    await act(async () => {
      target.focus();
    });
    const change = (value: string) => act(() => {
      const setter = Object.getOwnPropertyDescriptor(
        window.HTMLTextAreaElement.prototype,
        'value',
      )!.set!;
      setter.call(target, value);
      target.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await change('执行中的补充');
    act(() => {
      useStore.setState({ activeSessionId: CENTRAL_SESSION_ID, runStatus: 'executing' });
    });
    await act(async () => {
      // 执行中按钮区显示「停止」，发送走 Enter（与真实操作一致）
      target.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(mocks.api.appendMessage).toHaveBeenCalledTimes(1);
    const args = mocks.api.appendMessage.mock.calls[0];
    expect(args[0]).toBe(CENTRAL_SESSION_ID);
    expect(args[1]).toBe('执行中的补充');
    // 不得走新开轮（会被后端拒绝）
    expect(mocks.api.sendMessage).not.toHaveBeenCalled();
    view.unmount();
  });

  it('append 落空（后端 turn 刚结束）回落新开轮，消息必达', async () => {
    const view = mountInput();
    const target = view.textarea();
    await act(async () => {
      target.focus();
    });
    const change = (value: string) => act(() => {
      const setter = Object.getOwnPropertyDescriptor(
        window.HTMLTextAreaElement.prototype,
        'value',
      )!.set!;
      setter.call(target, value);
      target.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await change('竞态窗口消息');
    act(() => {
      useStore.setState({ activeSessionId: CENTRAL_SESSION_ID, runStatus: 'executing' });
    });
    // 后端无活跃 turn：append 返回 false → 应回落 sendMessage
    mocks.api.appendMessage.mockResolvedValueOnce(false);
    await act(async () => {
      target.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    await act(async () => {
      await Promise.resolve();
    });
    expect(mocks.api.appendMessage).toHaveBeenCalledTimes(1);
    expect(mocks.api.sendMessage).toHaveBeenCalledTimes(1);
    expect(mocks.api.sendMessage.mock.calls[0][1]).toBe('竞态窗口消息');
    view.unmount();
  });

  it('Enter 触发发送，Shift+Enter 不发送', async () => {
    const view = mountInput();
    const target = view.textarea();
    await act(async () => {
      target.focus();
    });
    const change = (value: string) => act(() => {
      const setter = Object.getOwnPropertyDescriptor(
        window.HTMLTextAreaElement.prototype,
        'value',
      )!.set!;
      setter.call(target, value);
      target.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await change('hi');
    act(() => {
      target.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', shiftKey: true, bubbles: true }));
    });
    expect(mocks.api.sendMessage).not.toHaveBeenCalled();
    act(() => {
      target.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    });
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(mocks.api.sendMessage).toHaveBeenCalledTimes(1);
    view.unmount();
  });

  it('发送失败：草稿保留并回滚执行态', async () => {
    mocks.api.sendMessage.mockRejectedValue(new Error('后端错误'));
    const view = mountInput();
    const target = view.textarea();
    await act(async () => {
      target.focus();
    });
    const change = (value: string) => act(() => {
      const setter = Object.getOwnPropertyDescriptor(
        window.HTMLTextAreaElement.prototype,
        'value',
      )!.set!;
      setter.call(target, value);
      target.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await change('会失败的消息');
    await act(async () => {
      view.send();
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    await act(async () => {
      await Promise.resolve();
    });
    expect(mocks.api.sendMessage).toHaveBeenCalledTimes(1);
    // 失败：草稿保留、执行态回滚、行内错误提示可见
    expect(useStore.getState().inputCaches[CENTRAL_SESSION_ID]?.text).toBe('会失败的消息');
    expect(useStore.getState().runStatus).toBe('idle');
    expect(document.body.textContent).toContain('发送失败');
    view.unmount();
  });
});
