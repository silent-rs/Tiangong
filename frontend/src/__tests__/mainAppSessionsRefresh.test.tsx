import { act, StrictMode, type ReactNode } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { Session } from '@/api/tauri';
import { reasoningOf } from '@/api/message';

const mocks = vi.hoisted(() => {
  const eventHandlers = new Map<string, Set<(event: { payload: unknown }) => void>>();
  const streamEventHandlers = new Set<(event: unknown) => void>();
  const unlisteners: ReturnType<typeof vi.fn>[] = [];
  // 注册是否立即完成。默认 true：注册 Promise 同步 resolve，保持现有用例行为。
  // StrictMode 竞态用例临时置为 false，让注册延迟完成，模拟"清理早于注册完成"。
  let deferRegistrations = false;
  // 待 resolve 的注册 Promise：defer 模式下调用方 hold 住 resolve，按需放行。
  type Pending = { resolve: (un: ReturnType<typeof vi.fn>) => void; unlisten: ReturnType<typeof vi.fn> };
  const pending: Pending[] = [];
  const makeUnlistener = () => {
    const unlisten = vi.fn();
    unlisteners.push(unlisten);
    return unlisten;
  };
  // 返回注册 Promise。immediate 模式下立即 resolve；defer 模式下入队 pending。
  const makeRegistration = () => {
    const unlisten = makeUnlistener();
    if (!deferRegistrations) {
      return Promise.resolve(unlisten);
    }
    let resolveFn!: (un: ReturnType<typeof vi.fn>) => void;
    const promise = new Promise<ReturnType<typeof vi.fn>>((resolve) => {
      resolveFn = resolve;
    });
    pending.push({ resolve: resolveFn, unlisten });
    return promise;
  };
  const appWindow = {
    scaleFactor: vi.fn(() => Promise.resolve(1)),
    innerSize: vi.fn(() => Promise.resolve({ width: 1200, height: 800 })),
    setSize: vi.fn(() => Promise.resolve()),
    onResized: vi.fn(() => makeRegistration()),
  };
  const api = {
    getSessions: vi.fn(),
    getReasoningEffort: vi.fn(() => Promise.resolve('medium')),
    getWorkspaceDir: vi.fn(() => Promise.resolve('/workspace')),
    pluginOpenEntry: vi.fn(() => Promise.resolve('<div>stub-plugin-page</div>')),
    onStreamEvent: vi.fn((callback: (event: unknown) => void) => {
      streamEventHandlers.add(callback);
      return makeRegistration().then((unlisten) => {
        unlisten.mockImplementation(() => streamEventHandlers.delete(callback));
        return unlisten;
      });
    }),
    browserHide: vi.fn(() => Promise.resolve()),
  };
  const listen = vi.fn((eventName: string, handler: (event: { payload: unknown }) => void) => {
    let handlers = eventHandlers.get(eventName);
    if (!handlers) {
      handlers = new Set();
      eventHandlers.set(eventName, handlers);
    }
    handlers.add(handler);
    return makeRegistration().then((unlisten) => {
      unlisten.mockImplementation(() => handlers!.delete(handler));
      return unlisten;
    });
  });
  return { api, appWindow, eventHandlers, listen, makeUnlistener, unlisteners, pending, streamEventHandlers, setDefer: (v: boolean) => { deferRegistrations = v; } };
});

vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>();
  return { ...actual, api: { ...actual.api, ...mocks.api } };
});

vi.mock('@tauri-apps/api/event', () => ({ listen: mocks.listen }));
vi.mock('@tauri-apps/api/window', () => ({
  currentMonitor: vi.fn(() => Promise.resolve(null)),
  getCurrentWindow: vi.fn(() => mocks.appWindow),
  LogicalSize: class LogicalSize {
    constructor(public width: number, public height: number) {}
  },
}));
vi.mock('@/components/AppSidebar', () => ({ AppSidebar: () => null }));
vi.mock('@/components/DefaultPluginOnboarding', () => ({
  DefaultPluginOnboarding: () => null,
}));
vi.mock('@/components/InteractionPluginHost', () => ({
  InteractionPluginHost: () => null,
}));
vi.mock('@/components/LazyComponents', () => ({
  LazyMessageInput: () => null,
  LazyMessageList: () => null,
  LazyStatusPanel: () => null,
}));
vi.mock('@/components/TabsContainer', () => ({ TabsContainer: () => null }));
vi.mock('@/components/ui/sidebar', () => ({
  SidebarProvider: ({ children }: { children: ReactNode }) => children,
}));
// Toast 桩：MainApp 直渲染无 ToastProvider，仅提供窄窗口提示所需的方法
vi.mock('@/components/Toast', () => ({
  useToast: () => ({ showWarning: vi.fn() }),
}));
vi.mock('@/hooks/useUpdateCheck', () => ({ useUpdateCheck: () => undefined }));
vi.mock('@/utils/desktopNotification', () => ({
  ensureDesktopNotificationPermission: vi.fn(() => Promise.resolve()),
}));

const { MainApp } = await import('@/pages/MainApp');
const { useStore } = await import('@/store/useStore');
const initialState = useStore.getInitialState();
const getSessionsMock = vi.mocked(mocks.api.getSessions);

function session(id: string, messageCount: number): Session {
  return {
    id,
    title: id,
    created_at: '2026-07-21 00:00:00',
    updated_at: '2026-07-21 00:00:00',
    message_count: messageCount,
    cwd: '/tmp',
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

async function flushMicrotasks() {
  for (let index = 0; index < 30; index += 1) {
    await Promise.resolve();
  }
}

function emitSessionsUpdated() {
  for (const handler of mocks.eventHandlers.get('sessions_updated') ?? []) {
    handler({ payload: undefined });
  }
}

async function advance(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

let container: HTMLDivElement | null = null;
let root: Root | null = null;

async function mountMainApp() {
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root!.render(<MainApp />);
    await flushMicrotasks();
  });
  expect(mocks.eventHandlers.get('sessions_updated')?.size).toBe(1);
  getSessionsMock.mockClear();
}

describe('MainApp sessions_updated scheduling contract', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    mocks.eventHandlers.clear();
    mocks.streamEventHandlers.clear();
    mocks.pending.length = 0;
    mocks.unlisteners.length = 0;
    mocks.listen.mockClear();
    mocks.appWindow.onResized.mockClear();
    mocks.api.onStreamEvent.mockClear();
    mocks.api.getWorkspaceDir.mockClear();
    mocks.api.getReasoningEffort.mockClear();
    mocks.api.getReasoningEffort.mockResolvedValue('medium');
    getSessionsMock.mockReset();
    getSessionsMock.mockResolvedValue([session('a', 1)]);
    useStore.setState({
      ...initialState,
      sessions: [session('a', 1)],
      activeSessionId: 'a',
      isNewConversation: false,
    }, true);
  });

  afterEach(async () => {
    if (root) {
      await act(async () => root!.unmount());
    }
    root = null;
    container?.remove();
    container = null;
    vi.clearAllTimers();
    vi.useRealTimers();
    delete (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT;
  });

  it('一次事件突发只触发一次 protective 刷新', async () => {
    await mountMainApp();

    act(() => emitSessionsUpdated());
    await advance(40);
    act(() => emitSessionsUpdated());
    await advance(79);
    act(() => emitSessionsUpdated());
    // 尾沿去抖：最后一次事件静默 120ms 后才触发，故需再推进一个完整窗口。
    await advance(120);

    expect(getSessionsMock).toHaveBeenCalledTimes(1);
  });

  it('去抖应等待最后一个事件静默 120ms 后再刷新', async () => {
    await mountMainApp();

    act(() => emitSessionsUpdated());
    await advance(119);
    act(() => emitSessionsUpdated());
    await advance(1);
    const callsAtFirstWindow = getSessionsMock.mock.calls.length;
    await advance(119);

    expect(callsAtFirstWindow).toBe(0);
    expect(getSessionsMock).toHaveBeenCalledTimes(1);
  });

  it('后端返回错误时保留旧列表，下一次事件重新刷新后收敛', async () => {
    // 请求失败时保留旧列表，收敛由下一次 sessions_updated 事件自然触发。
    await mountMainApp();
    getSessionsMock
      .mockRejectedValueOnce(new Error('read_dir failed'))
      .mockResolvedValueOnce([session('a', 2)]);
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined);

    act(() => emitSessionsUpdated());
    await advance(120);
    // 失败时保留挂载时的旧列表，不被错误清空。
    expect(useStore.getState().sessions[0].message_count).toBe(1);
    expect(getSessionsMock).toHaveBeenCalledTimes(1);

    act(() => emitSessionsUpdated());
    await advance(120);
    expect(getSessionsMock).toHaveBeenCalledTimes(2);
    expect(useStore.getState().sessions[0].message_count).toBe(2);
    consoleError.mockRestore();
  });

  it('刷新进行中收到新事件时不并发读取，并在 settle 后执行一次 dirty rerun', async () => {
    await mountMainApp();
    const first = deferred<Session[]>();
    const second = deferred<Session[]>();
    let inFlight = 0;
    let maxInFlight = 0;
    getSessionsMock
      .mockImplementationOnce(async () => {
        inFlight += 1;
        maxInFlight = Math.max(maxInFlight, inFlight);
        const result = await first.promise;
        inFlight -= 1;
        return result;
      })
      .mockImplementationOnce(async () => {
        inFlight += 1;
        maxInFlight = Math.max(maxInFlight, inFlight);
        const result = await second.promise;
        inFlight -= 1;
        return result;
      });

    act(() => emitSessionsUpdated());
    await advance(120);
    act(() => emitSessionsUpdated());
    await advance(120);
    const observedMaxInFlight = maxInFlight;

    first.resolve([session('a', 2)]);
    second.resolve([session('a', 3)]);
    await act(async () => flushMicrotasks());

    expect(observedMaxInFlight).toBe(1);
    expect(getSessionsMock).toHaveBeenCalledTimes(2);
  });

  it('unmount 会取消 pending timer 并注销所有已注册 listener', async () => {
    await mountMainApp();
    act(() => emitSessionsUpdated());
    const registeredUnlisteners = [...mocks.unlisteners];

    await act(async () => root!.unmount());
    root = null;
    await advance(120);

    expect(getSessionsMock).not.toHaveBeenCalled();
    // stream + 7 个事件监听 + resize。
    expect(registeredUnlisteners.length).toBe(9);
    for (const unlisten of registeredUnlisteners) {
      expect(unlisten).toHaveBeenCalledTimes(1);
    }
    expect(mocks.eventHandlers.get('sessions_updated')?.size ?? 0).toBe(0);
  });

  it('plugin_reloaded 卸载该插件的后台执行壳，下次后台挂载重建', async () => {
    mocks.api.pluginOpenEntry.mockClear();
    await mountMainApp();
    useStore.setState({ activeSessionId: 's1' });
    const openEntry = vi.mocked(mocks.api.pluginOpenEntry);
    const emit = (name: string, payload: unknown) => {
      act(() => {
        for (const handler of mocks.eventHandlers.get(name) ?? []) {
          handler({ payload });
        }
      });
    };
    const openBackground = () => emit('app:open_plugin', {
      plugin_id: 'terminal',
      contribution_id: 'app',
      background: true,
    });

    openBackground();
    await act(async () => {
      await flushMicrotasks();
    });
    expect(openEntry).toHaveBeenCalledTimes(1);

    // 同插件同会话再次后台挂载命中判重，不重建实例。
    openBackground();
    await act(async () => {
      await flushMicrotasks();
    });
    expect(openEntry).toHaveBeenCalledTimes(1);

    // 热加载事件卸载旧壳后判重不再命中，重新挂载会拉取磁盘最新页面。
    emit('plugin_reloaded', 'terminal');
    await act(async () => {
      await flushMicrotasks();
    });
    openBackground();
    await act(async () => {
      await flushMicrotasks();
    });
    expect(openEntry).toHaveBeenCalledTimes(2);
  });

  it('后台会话的终端 App 不挂后台执行壳，其他插件仍隐藏挂壳', async () => {
    mocks.api.pluginOpenEntry.mockClear();
    await mountMainApp();
    useStore.setState({ activeSessionId: 's1' });
    const openEntry = vi.mocked(mocks.api.pluginOpenEntry);
    const emit = (name: string, payload: unknown) => {
      act(() => {
        for (const handler of mocks.eventHandlers.get(name) ?? []) {
          handler({ payload });
        }
      });
    };

    // 非当前激活会话的终端拉起：工具编排已下沉 sidecar 直连执行，
    // 不占任何前端资源，等切回该会话时经 terminalListByScope 恢复标签。
    emit('app:open_plugin', {
      plugin_id: 'terminal',
      contribution_id: 'terminal',
      session_id: 's-other',
      background: true,
      instance_id: 'tty-1',
    });
    await act(async () => {
      await flushMicrotasks();
    });
    expect(openEntry).not.toHaveBeenCalled();

    // 其他插件（如浏览器）在后台会话仍隐藏挂壳，保证其工具有人执行。
    emit('app:open_plugin', {
      plugin_id: 'browser',
      contribution_id: 'browser',
      session_id: 's-other',
      background: true,
      instance_id: 'wv-1',
    });
    await act(async () => {
      await flushMicrotasks();
    });
    expect(openEntry).toHaveBeenCalledTimes(1);
  });
});

describe('MainApp StrictMode 异步监听注册竞态', () => {
  // 事件名清单：每类都应只保留一个有效监听器。
  const LISTENED_EVENTS = [
    'sessions_updated',
    'desktop_notification_open_session',
    'browser:open',
  ] as const;

  function emitStreamDelta(messageId: string, text: string) {
    for (const handler of mocks.streamEventHandlers) {
      handler({ session_id: 'a', event: { type: 'delta', message_id: messageId, content: text } });
    }
  }

  function emitStreamReasoning(messageId: string, text: string) {
    for (const handler of mocks.streamEventHandlers) {
      handler({ session_id: 'a', event: { type: 'reasoning', message_id: messageId, content: text } });
    }
  }

  async function mountMainAppStrict() {
    mocks.setDefer(true);
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    await act(async () => {
      root!.render(
        <StrictMode>
          <MainApp />
        </StrictMode>,
      );
      await flushMicrotasks();
    });
  }

  async function resolveAllPending() {
    await act(async () => {
      for (let i = 0; i < 40; i += 1) {
        if (mocks.pending.length === 0) {
          await flushMicrotasks();
          if (mocks.pending.length === 0) break;
        }
        while (mocks.pending.length > 0) {
          const entry = mocks.pending.pop()!;
          entry.resolve(entry.unlisten);
        }
        await flushMicrotasks();
      }
    });
  }

  beforeEach(() => {
    vi.useFakeTimers();
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    mocks.eventHandlers.clear();
    mocks.streamEventHandlers.clear();
    mocks.pending.length = 0;
    mocks.unlisteners.length = 0;
    mocks.setDefer(false);
    mocks.listen.mockClear();
    mocks.appWindow.onResized.mockClear();
    mocks.api.onStreamEvent.mockClear();
    mocks.api.getWorkspaceDir.mockClear();
    mocks.api.getReasoningEffort.mockClear();
    mocks.api.getReasoningEffort.mockResolvedValue('medium');
    getSessionsMock.mockReset();
    getSessionsMock.mockResolvedValue([session('a', 1)]);
    useStore.setState({
      ...initialState,
      sessions: [session('a', 1)],
      activeSessionId: 'a',
      isNewConversation: false,
    }, true);
  });

  afterEach(async () => {
    if (root) {
      await act(async () => root!.unmount());
    }
    root = null;
    container?.remove();
    container = null;
    vi.clearAllTimers();
    vi.useRealTimers();
    mocks.setDefer(false);
    delete (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT;
  });

  it('StrictMode 挂载→清理→再挂载后，每类事件最终只剩一个有效监听器', async () => {
    await mountMainAppStrict();
    // defer 模式下注册串行进行：每轮 effect 卡在首个 await（onStreamEvent）。
    // StrictMode 挂载→清理→再挂载会跑两轮 effect，故至少有 2 个未 resolve 的注册。
    // 此时第一轮 effect 已被清理，但注册尚未完成；resolve 后第一轮应在 guard 处放弃。
    expect(mocks.pending.length).toBeGreaterThanOrEqual(2);

    await resolveAllPending();

    // stream_event（onStreamEvent）+ 3 个关键 listen 事件，每类恰好一个 handler。
    // 第一轮的孤儿监听已被 guard 放弃时取消，不会重复消费。
    expect(mocks.streamEventHandlers.size).toBe(1);
    for (const eventName of LISTENED_EVENTS) {
      expect(mocks.eventHandlers.get(eventName)?.size ?? 0).toBe(1);
    }
  });

  it('一个后端流事件只被消费一次，正文与 reasoning 各追加一次', async () => {
    await mountMainAppStrict();
    await resolveAllPending();

    const messageId = 'msg-1';
    // 发送一次正文增量，flush 后应只追加一次。
    act(() => emitStreamDelta(messageId, 'hello'));
    await advance(16);
    let message = useStore.getState().messages.find((item) => item.id === messageId);
    expect(message?.content).toEqual([{ type: 'text', text: 'hello' }]);

    // 再发一次 reasoning 增量，只追加一次。
    act(() => emitStreamReasoning(messageId, 'thinking'));
    await advance(16);
    message = useStore.getState().messages.find((item) => item.id === messageId);
    expect(message ? reasoningOf(message) : undefined).toBe('thinking');
  });

  it('卸载后所有迟到的 unlisten 都被调用，且再发事件不改变状态', async () => {
    await mountMainAppStrict();
    await resolveAllPending();
    const registeredUnlisteners = [...mocks.unlisteners];

    await act(async () => root!.unmount());
    root = null;

    // 卸载后再发送事件，store 状态不应变化（无遗留监听）。
    const before = useStore.getState().messages.length;
    act(() => emitStreamDelta('late-msg', 'late'));
    await advance(16);
    expect(useStore.getState().messages.length).toBe(before);

    // 第一轮的迟到 unlisten 在 guard 放弃时被立即调用；第二轮在 cleanup 中释放。
    // 每个注册产生的 unlisten 至少被调用一次。
    for (const unlisten of registeredUnlisteners) {
      expect(unlisten).toHaveBeenCalled();
    }
    expect(mocks.streamEventHandlers.size).toBe(0);
    for (const eventName of LISTENED_EVENTS) {
      expect(mocks.eventHandlers.get(eventName)?.size ?? 0).toBe(0);
    }
  });
});
