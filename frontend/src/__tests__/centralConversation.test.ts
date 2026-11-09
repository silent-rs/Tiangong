/**
 * 中枢对话测试（POC 阶段 1：中枢对话单独存储，独立于普通会话体系）。
 *
 * 切换逻辑位于 lib/centralConversation（独立于共用 store）：固定 Session
 * ID、进入加载中枢存储历史（未创建按空对话）、退出恢复普通上下文、
 * 列表未加载不误判原会话已删除、快速来回切换竞态防护。
 */
import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { LoadedSession, Session } from '@/api/tauri';

vi.mock('@/api/tauri', () => {
  const api = {
    getSessions: vi.fn(),
    getReasoningEffort: vi.fn(),
    getDefaultReasoningEffort: vi.fn(),
    newSessionId: vi.fn(),
    loadSession: vi.fn(),
    getInputCache: vi.fn(),
    switchSession: vi.fn(),
    setInputCache: vi.fn(),
    removeInputCache: vi.fn(),
    prewarmWorkspaceIndex: vi.fn(),
    setReasoningEffort: vi.fn(),
  };
  return { api };
});

const { useStore } = await import('@/store/useStore');
const { api } = await import('@/api/tauri');
const { CENTRAL_SESSION_ID } = await import('@/lib/centralSession');
const {
  __resetCentralConversationForTests,
  ensureCentralConversation,
  restoreFromCentralConversation,
} = await import('@/lib/centralConversation');
const { CENTRAL_DEFAULT_GREETING } = await import('@/components/central/centralGreeting');
const getSessionsMock = vi.mocked(api.getSessions);
const getDefaultReasoningEffortMock = vi.mocked(api.getDefaultReasoningEffort);
const getReasoningEffortMock = vi.mocked(api.getReasoningEffort);
const newSessionIdMock = vi.mocked(api.newSessionId);
const loadSessionMock = vi.mocked(api.loadSession);
const getInputCacheMock = vi.mocked(api.getInputCache);
const initialState = useStore.getInitialState();

type StoreState = ReturnType<typeof useStore.getState>;

function session(id: string): Session {
  return {
    id,
    title: id,
    created_at: '2026-11-20 00:00:00',
    updated_at: '2026-11-20 00:00:00',
    message_count: 2,
    cwd: '/tmp',
  };
}

function loadedSession(id: string, messageCount = 1): LoadedSession {
  return {
    id,
    messages: Array.from({ length: messageCount }, (_, index) => ({
      id: `${id}-message-${index}`,
      role: 'assistant',
      content: [{ type: 'text', text: `message ${index} from ${id}` }],
      created_at: '2026-11-20 00:00:00',
    })),
    token_stats: null,
    cwd: '/tmp',
    reasoning_effort: 'medium',
  } as LoadedSession;
}

function resetStore(overrides: Partial<StoreState> = {}) {
  useStore.setState({
    ...initialState,
    isNewConversation: false,
    ...overrides,
  }, true);
}

beforeEach(() => {
  vi.clearAllMocks();
  __resetCentralConversationForTests();
  localStorage.clear();
  getSessionsMock.mockResolvedValue([]);
  getDefaultReasoningEffortMock.mockResolvedValue('medium');
  getReasoningEffortMock.mockResolvedValue('medium');
  newSessionIdMock.mockImplementation(async () => `fresh-${Math.random().toString(36).slice(2, 8)}`);
  // 默认：中枢对话尚未创建（loadSession 报错，按空对话处理）
  loadSessionMock.mockImplementation(async (id: string) => {
    if (id === CENTRAL_SESSION_ID) throw new Error('会话不存在');
    return loadedSession(id);
  });
  getInputCacheMock.mockResolvedValue(null);
  vi.mocked(api.switchSession).mockResolvedValue(undefined);
  vi.mocked(api.setInputCache).mockResolvedValue(undefined);
  vi.mocked(api.removeInputCache).mockResolvedValue(undefined);
  vi.mocked(api.prewarmWorkspaceIndex).mockResolvedValue(undefined);
  vi.mocked(api.setReasoningEffort).mockResolvedValue(undefined);
  resetStore();
});

describe('中枢对话常量', () => {
  it('固定 Session ID 与后端约定一致（src-tauri central.rs）', async () => {
    expect(CENTRAL_SESSION_ID).toBe('central-hub');
    expect(CENTRAL_DEFAULT_GREETING).toContain('我是天工中枢');
  });
});

describe('ensureCentralConversation', () => {
  it('首次进入（中枢对话未创建）：空对话 + 固定 ID', async () => {
    resetStore({ activeSessionId: 'work-1', sessions: [session('work-1')] });
    await ensureCentralConversation();
    const state = useStore.getState();
    expect(state.activeSessionId).toBe(CENTRAL_SESSION_ID);
    expect(state.isNewConversation).toBe(false);
    expect(state.newConversationId).toBeNull();
    expect(state.messages).toEqual([]);
  });

  it('已创建：从中枢存储加载历史', async () => {
    loadSessionMock.mockImplementation(async (id: string) => {
      if (id === CENTRAL_SESSION_ID) return loadedSession(id, 3);
      return loadedSession(id);
    });
    await ensureCentralConversation();
    expect(useStore.getState().activeSessionId).toBe(CENTRAL_SESSION_ID);
    expect(useStore.getState().messages.length).toBe(3);
  });

  it('已在中枢对话：幂等，不重复加载', async () => {
    resetStore({ activeSessionId: CENTRAL_SESSION_ID });
    loadSessionMock.mockClear();
    await ensureCentralConversation();
    expect(loadSessionMock).not.toHaveBeenCalled();
    expect(useStore.getState().activeSessionId).toBe(CENTRAL_SESSION_ID);
  });

  it('turn 进行中切回：运行状态从 sessionRunStatuses 恢复', async () => {
    resetStore({
      activeSessionId: 'work-1',
      sessionRunStatuses: { [CENTRAL_SESSION_ID]: 'executing' },
    });
    await ensureCentralConversation();
    const state = useStore.getState();
    expect(state.runStatus).toBe('executing');
    expect(state.runSummary).toBe('正在处理');
  });

  it('加载期间模式被切走：放弃本次写入（不误设中枢态）', async () => {
    resetStore({ activeSessionId: 'work-1', sessions: [session('work-1')] });
    let releaseLoad!: (reason?: unknown) => void;
    loadSessionMock.mockImplementation(
      () => new Promise((_resolve, reject) => {
        releaseLoad = reject;
      }),
    );
    const ensurePromise = ensureCentralConversation();
    // 加载挂起期间用户退出中枢：restore 递增版本，作废在途 ensure
    await restoreFromCentralConversation();
    releaseLoad(new Error('cancelled'));
    await ensurePromise;
    expect(useStore.getState().activeSessionId).toBe('work-1');
  });
});

describe('restoreFromCentralConversation', () => {
  it('进入前是活跃会话：切回原会话', async () => {
    const list = [session('work-1'), session(CENTRAL_SESSION_ID)];
    resetStore({ activeSessionId: CENTRAL_SESSION_ID, sessions: list });
    await ensureCentralConversation().catch(() => undefined);
    // 直接模拟「进入前是 work-1」的记录：先置回普通再进入
    __resetCentralConversationForTests();
    resetStore({ activeSessionId: 'work-1', sessions: list });
    await ensureCentralConversation();
    await restoreFromCentralConversation();
    expect(useStore.getState().activeSessionId).toBe('work-1');
  });

  it('进入前是新对话：恢复原草稿 ID（中枢草稿不清理）', async () => {
    resetStore({
      isNewConversation: true,
      activeSessionId: null,
      newConversationId: 'draft-1',
    });
    await ensureCentralConversation();
    useStore.setState((s) => ({
      inputCaches: {
        ...s.inputCaches,
        [CENTRAL_SESSION_ID]: { ...s.inputCaches[CENTRAL_SESSION_ID], text: '中枢草稿' },
        'draft-1': { ...s.inputCaches['draft-1'], text: '普通草稿' },
      },
    }));
    await restoreFromCentralConversation();
    const state = useStore.getState();
    expect(state.isNewConversation).toBe(true);
    expect(state.activeSessionId).toBeNull();
    expect(state.newConversationId).toBe('draft-1');
    expect(state.inputCaches[CENTRAL_SESSION_ID]?.text).toBe('中枢草稿');
    expect(state.inputCaches['draft-1']?.text).toBe('普通草稿');
    expect(newSessionIdMock).not.toHaveBeenCalled();
  });

  it('无上下文记录（异常路径兜底）：不动作', async () => {
    resetStore({ activeSessionId: CENTRAL_SESSION_ID });
    await restoreFromCentralConversation();
    expect(useStore.getState().activeSessionId).toBe(CENTRAL_SESSION_ID);
    expect(newSessionIdMock).not.toHaveBeenCalled();
  });

  it('进入前会话已被删除：回新对话', async () => {
    const list = [session('other-1')];
    getSessionsMock.mockResolvedValue(list);
    resetStore({ activeSessionId: 'work-gone', sessions: list });
    await ensureCentralConversation();
    newSessionIdMock.mockResolvedValue('fresh-after-restore');
    await restoreFromCentralConversation();
    expect(useStore.getState().newConversationId).toBe('fresh-after-restore');
  });

  it('列表尚未加载（空）时不误判原会话已删除：刷新后切回', async () => {
    resetStore({ activeSessionId: 'work-1', sessions: [] });
    await ensureCentralConversation();
    getSessionsMock.mockResolvedValue([session('work-1')]);
    await restoreFromCentralConversation();
    expect(useStore.getState().activeSessionId).toBe('work-1');
    expect(newSessionIdMock).not.toHaveBeenCalled();
  });
});

describe('普通会话链路隔离', () => {
  it('中枢对话激活期间 loadSessions 不判失效、不自动迁移', async () => {
    resetStore({ activeSessionId: CENTRAL_SESSION_ID, sessions: [] });
    getSessionsMock.mockResolvedValue([session('other-1')]);
    await useStore.getState().loadSessions();
    expect(useStore.getState().activeSessionId).toBe(CENTRAL_SESSION_ID);
  });

  it('普通会话失效语义不变（非中枢 ID 仍触发迁移）', async () => {
    resetStore({ activeSessionId: 'work-gone', sessions: [] });
    getSessionsMock.mockResolvedValue([session('other-1')]);
    await useStore.getState().loadSessions();
    expect(useStore.getState().activeSessionId).toBe('other-1');
  });

  it('updateSessionMeta 跳过中枢对话（不追加进普通列表）', async () => {
    const list = [session('work-1')];
    resetStore({ sessions: list });
    await useStore.getState().updateSessionMeta(CENTRAL_SESSION_ID);
    // 短路：不发 meta 请求，列表不变
    expect(useStore.getState().sessions).toEqual(list);
  });
});
