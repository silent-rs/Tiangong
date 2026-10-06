import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { LoadedSession, Session } from '@/api/tauri';

vi.mock('@/api/tauri', () => {
  const api = {
    getSessions: vi.fn(),
    getReasoningEffort: vi.fn(),
    newSessionId: vi.fn(),
    loadSession: vi.fn(),
    getInputCache: vi.fn(),
    switchSession: vi.fn(),
    setInputCache: vi.fn(),
    removeInputCache: vi.fn(),
  };
  return { api };
});

const { useStore } = await import('@/store/useStore');
const { api } = await import('@/api/tauri');
const getSessionsMock = vi.mocked(api.getSessions);
const getReasoningEffortMock = vi.mocked(api.getReasoningEffort);
const newSessionIdMock = vi.mocked(api.newSessionId);
const loadSessionMock = vi.mocked(api.loadSession);
const getInputCacheMock = vi.mocked(api.getInputCache);
const switchSessionMock = vi.mocked(api.switchSession);
const setInputCacheMock = vi.mocked(api.setInputCache);
const removeInputCacheMock = vi.mocked(api.removeInputCache);
const initialState = useStore.getInitialState();

type StoreState = ReturnType<typeof useStore.getState>;

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

function loadedSession(id: string): LoadedSession {
  return {
    id,
    messages: [{
      id: `${id}-message`,
      role: 'assistant',
      content: [{ type: 'text', text: `message from ${id}` }],
      reasoning_content: '',
      created_at: '2026-07-21 00:00:00',
    }],
    token_stats: {
      current_tokens: 1,
      compression_threshold_tokens: 100,
      context_limit_tokens: 200,
      total_prompt_tokens: 1,
      total_completion_tokens: 1,
      total_tokens: 2,
      active_agent_current_tokens: 0,
      active_agent_id: null,
      agent_current_tokens: {},
      agent_token_usage: {},
    },
    cwd: `/workspace/${id}`,
    reasoning_effort: 'high',
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function resetStore(overrides: Partial<StoreState> = {}) {
  useStore.setState({
    ...initialState,
    isNewConversation: false,
    ...overrides,
  }, true);
}

describe('loadSessions refresh contract', () => {
  it('只补充用量的消息更新也必须进入当前会话', () => {
    const original = loadedSession('usage-live').messages[0];
    resetStore({ activeSessionId: 'usage-live', messages: [original] });
    const usage = { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110,
      prompt_cache_hit_tokens: 80, prompt_cache_miss_tokens: 20, cache_hit_rate: 0.8,
      model: 'test-model', agent_id: 'usage-live', turn_id: 'turn-1', source: 'react', status: 'success' as const };
    useStore.getState().applyStreamEvents([{ session_id: 'usage-live', event: {
      type: 'session_message_upsert', message: { ...original, usage },
    } }]);
    expect(useStore.getState().messages).toHaveLength(1);
    expect(useStore.getState().messages[0].usage).toEqual(usage);
    expect(useStore.getState().messages[0].content).toEqual(original.content);
  });

  beforeEach(() => {
    getSessionsMock.mockReset();
    getReasoningEffortMock.mockReset();
    newSessionIdMock.mockReset();
    loadSessionMock.mockReset();
    getInputCacheMock.mockReset();
    switchSessionMock.mockReset();
    setInputCacheMock.mockReset();
    removeInputCacheMock.mockReset();
    getReasoningEffortMock.mockResolvedValue('medium');
    newSessionIdMock.mockResolvedValue('new-session');
    getInputCacheMock.mockResolvedValue({
      text: '',
      attachments: [],
      is_sending: false,
      revision: 0,
    });
    switchSessionMock.mockResolvedValue(undefined);
    setInputCacheMock.mockImplementation(async (_cacheKey, cache) => cache);
    removeInputCacheMock.mockResolvedValue(undefined);
    resetStore();
  });

  it('普通刷新失败时保留现有列表和 active，并结束 loading', async () => {
    const existing = [session('a', 5), session('b', 3)];
    resetStore({ sessions: existing, activeSessionId: 'a' });
    getSessionsMock.mockRejectedValue(new Error('scan failed'));
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined);

    await useStore.getState().loadSessions();

    const state = useStore.getState();
    expect(state.sessions).toBe(existing);
    expect(state.activeSessionId).toBe('a');
    expect(state.isLoadingSessions).toBe(false);
    consoleError.mockRestore();
  });

  it('合法 Ok([]) 应清空失效 active 并初始化可发送的新对话', async () => {
    resetStore({ sessions: [session('a', 5)], activeSessionId: 'a' });
    getSessionsMock.mockResolvedValue([]);

    await useStore.getState().loadSessions({ protective: true });

    const state = useStore.getState();
    expect(state.sessions).toEqual([]);
    expect(state.activeSessionId).toBeNull();
    expect(state.isNewConversation).toBe(true);
    expect(state.newConversationId).toBe('new-session');
    expect(state.inputCaches['new-session']).toEqual({
      text: '',
      attachments: [],
      is_sending: false,
      revision: 0,
    });
    expect(newSessionIdMock).toHaveBeenCalledTimes(1);
  });

  it('active 被删除但仍有其他会话时执行完整切换并 hydration', async () => {
    resetStore({
      sessions: [session('a', 5), session('refresh-target', 3)],
      activeSessionId: 'a',
      messages: [{
        id: 'old-message',
        role: 'assistant',
        content: [{ type: 'text', text: 'old message' }],
        reasoning_content: '',
        created_at: '2026-07-21 00:00:00',
      }],
      sessionCwd: '/workspace/a',
    });
    getSessionsMock.mockResolvedValue([session('refresh-target', 3)]);
    loadSessionMock.mockResolvedValue(loadedSession('refresh-target'));

    await useStore.getState().loadSessions({ protective: true });

    const state = useStore.getState();
    expect(state.sessions.map((item) => item.id)).toEqual(['refresh-target']);
    expect(loadSessionMock).toHaveBeenCalledWith('refresh-target');
    expect(switchSessionMock).toHaveBeenCalledWith('refresh-target');
    expect(state.activeSessionId).toBe('refresh-target');
    expect(state.isNewConversation).toBe(false);
    expect(state.messages.map((message) => message.id)).toEqual(['refresh-target-message']);
    expect(state.sessionCwd).toBe('/workspace/refresh-target');
    expect(state.reasoningEffort).toBe('high');
  });

  it('edit 重发导致 message_count 合法下降时接受权威结果', async () => {
    resetStore({
      sessions: [session('a', 5), session('b', 3)],
      activeSessionId: 'a',
    });
    getSessionsMock.mockResolvedValue([session('a', 2), session('b', 3)]);

    await useStore.getState().loadSessions({ protective: true });

    expect(useStore.getState().sessions.map((item) => item.message_count)).toEqual([2, 3]);
  });

  it('protective 刷新 pending 期间不翻转 loading', async () => {
    const request = deferred<Session[]>();
    resetStore({ sessions: [session('a', 1)], activeSessionId: 'a' });
    getSessionsMock.mockReturnValue(request.promise);

    const load = useStore.getState().loadSessions({ protective: true });
    expect(useStore.getState().isLoadingSessions).toBe(false);

    request.resolve([session('a', 2)]);
    await load;
    expect(useStore.getState().isLoadingSessions).toBe(false);
  });

  it('非 protective 刷新 pending 期间翻转 loading 并在完成后复位', async () => {
    const request = deferred<Session[]>();
    resetStore({ sessions: [session('a', 1)], activeSessionId: 'a' });
    getSessionsMock.mockReturnValue(request.promise);

    const load = useStore.getState().loadSessions();
    expect(useStore.getState().isLoadingSessions).toBe(true);

    request.resolve([session('a', 2)]);
    await load;
    expect(useStore.getState().isLoadingSessions).toBe(false);
    expect(useStore.getState().sessions.map((item) => item.message_count)).toEqual([2]);
  });

  it('旧刷新晚到时不能覆盖较新的权威结果', async () => {
    const older = deferred<Session[]>();
    const newer = deferred<Session[]>();
    resetStore({ sessions: [session('a', 1)], activeSessionId: 'a' });
    getSessionsMock
      .mockReturnValueOnce(older.promise)
      .mockReturnValueOnce(newer.promise);

    const olderLoad = useStore.getState().loadSessions({ protective: true });
    const newerLoad = useStore.getState().loadSessions({ protective: true });

    newer.resolve([session('a', 3)]);
    await newerLoad;
    older.resolve([session('a', 2)]);
    await olderLoad;

    expect(useStore.getState().sessions[0].message_count).toBe(3);
  });

  it('并发普通刷新中任一请求仍 pending 时保持 loading', async () => {
    const first = deferred<Session[]>();
    const second = deferred<Session[]>();
    resetStore({ sessions: [session('a', 1)], activeSessionId: 'a' });
    getSessionsMock
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);

    const firstLoad = useStore.getState().loadSessions();
    const secondLoad = useStore.getState().loadSessions();
    expect(useStore.getState().isLoadingSessions).toBe(true);

    first.resolve([session('a', 2)]);
    await firstLoad;
    expect(useStore.getState().isLoadingSessions).toBe(true);

    second.resolve([session('a', 3)]);
    await secondLoad;
    expect(useStore.getState().isLoadingSessions).toBe(false);
  });

  it('protective 请求不能清除另一个普通刷新持有的 loading', async () => {
    const ordinary = deferred<Session[]>();
    const protective = deferred<Session[]>();
    resetStore({ sessions: [session('a', 1)], activeSessionId: 'a' });
    getSessionsMock
      .mockReturnValueOnce(ordinary.promise)
      .mockReturnValueOnce(protective.promise);

    const ordinaryLoad = useStore.getState().loadSessions();
    const protectiveLoad = useStore.getState().loadSessions({ protective: true });
    expect(useStore.getState().isLoadingSessions).toBe(true);

    protective.resolve([session('a', 2)]);
    await protectiveLoad;
    expect(useStore.getState().isLoadingSessions).toBe(true);

    ordinary.resolve([session('a', 3)]);
    await ordinaryLoad;
    expect(useStore.getState().isLoadingSessions).toBe(false);
  });

  it('一次未结束的后端切换会阻塞串行队列中的后续会话切换', async () => {
    const stalledBackendSwitch = deferred<void>();
    switchSessionMock
      .mockReturnValueOnce(stalledBackendSwitch.promise)
      .mockResolvedValueOnce(undefined);
    loadSessionMock.mockImplementation(async (id: string) => loadedSession(id));

    const stalledSwitch = useStore.getState().switchSession('stalled-session');
    await vi.waitFor(() => {
      expect(switchSessionMock).toHaveBeenCalledWith('stalled-session');
    });

    const healthySwitch = useStore.getState().switchSession('healthy-session');
    let healthySettled = false;
    void healthySwitch.finally(() => {
      healthySettled = true;
    });

    try {
      await new Promise((resolve) => globalThis.setTimeout(resolve, 100));
      expect(healthySettled).toBe(false);
      expect(switchSessionMock).toHaveBeenCalledTimes(1);
    } finally {
      stalledBackendSwitch.resolve();
      await Promise.allSettled([stalledSwitch, healthySwitch]);
    }

    expect(switchSessionMock).toHaveBeenNthCalledWith(2, 'healthy-session');
    expect(useStore.getState().activeSessionId).toBe('healthy-session');
  });

  it('切换会话加载期间给出加载提示，完成或失败后撤下', async () => {
    const pendingLoad = deferred<ReturnType<typeof loadedSession>>();
    loadSessionMock.mockReturnValueOnce(pendingLoad.promise);

    const switching = useStore.getState().switchSession('slow-session');
    expect(useStore.getState().switchingSessionId).toBe('slow-session');
    pendingLoad.resolve(loadedSession('slow-session'));
    await switching;
    expect(useStore.getState().switchingSessionId).toBeNull();
    expect(useStore.getState().activeSessionId).toBe('slow-session');

    loadSessionMock.mockRejectedValueOnce(new Error('网络中断'));
    await useStore.getState().switchSession('broken-session');
    expect(useStore.getState().switchingSessionId).toBeNull();
  });
});
