/**
 * 中枢对话测试（POC 阶段 1：中枢对话单独存储，独立于普通会话体系）。
 *
 * 中枢对话固定 Session ID（`central-hub`）、后端独立 storage_root：
 * - 进入：从中枢存储加载历史（尚未创建按空对话），activeSessionId 固定
 *   指向中枢对话，记录进入前的普通模式上下文；
 * - 退出：恢复进入前的普通会话 / 新对话草稿；
 * - 普通链路隔离：loadSessions 不把激活的中枢对话判为失效；
 * - startNewConversation 的 reuseId / keepCurrentDraft 选项（恢复草稿用）。
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
const { CENTRAL_DEFAULT_GREETING } = await import('@/components/central/centralGreeting');
const getSessionsMock = vi.mocked(api.getSessions);
const getDefaultReasoningEffortMock = vi.mocked(api.getDefaultReasoningEffort);
const getReasoningEffortMock = vi.mocked(api.getReasoningEffort);
const newSessionIdMock = vi.mocked(api.newSessionId);
const loadSessionMock = vi.mocked(api.loadSession);
const getInputCacheMock = vi.mocked(api.getInputCache);
const prewarmMock = vi.mocked(api.prewarmWorkspaceIndex);
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
  prewarmMock.mockResolvedValue(undefined);
  vi.mocked(api.setReasoningEffort).mockResolvedValue(undefined);
  resetStore();
});

describe('中枢对话常量与问候', () => {
  it('固定 Session ID 与后端约定一致（src-tauri central.rs）', async () => {
    expect(CENTRAL_SESSION_ID).toBe('central-hub');
    expect(CENTRAL_DEFAULT_GREETING).toContain('我是天工中枢');
  });
});

describe('ensureCentralConversation', () => {
  it('首次进入（中枢对话未创建）：空对话 + 固定 ID + 记录进入前上下文', async () => {
    resetStore({ activeSessionId: 'work-1', sessions: [session('work-1')] });
    await useStore.getState().ensureCentralConversation();
    const state = useStore.getState();
    expect(state.activeSessionId).toBe(CENTRAL_SESSION_ID);
    expect(state.isNewConversation).toBe(false);
    expect(state.newConversationId).toBeNull();
    expect(state.messages).toEqual([]);
    expect(state.preCentralContext).toEqual({
      activeSessionId: 'work-1',
      newConversationId: null,
    });
  });

  it('已创建：从中枢存储加载历史', async () => {
    loadSessionMock.mockImplementation(async (id: string) => {
      if (id === CENTRAL_SESSION_ID) return loadedSession(id, 3);
      return loadedSession(id);
    });
    await useStore.getState().ensureCentralConversation();
    expect(useStore.getState().activeSessionId).toBe(CENTRAL_SESSION_ID);
    expect(useStore.getState().messages.length).toBe(3);
  });

  it('已在中枢对话：幂等，不覆盖已记录的上下文', async () => {
    resetStore({
      activeSessionId: CENTRAL_SESSION_ID,
      preCentralContext: { activeSessionId: 'work-1', newConversationId: null },
    });
    loadSessionMock.mockClear();
    await useStore.getState().ensureCentralConversation();
    expect(loadSessionMock).not.toHaveBeenCalled();
    expect(useStore.getState().preCentralContext?.activeSessionId).toBe('work-1');
  });

  it('turn 进行中切回：运行状态从 sessionRunStatuses 恢复', async () => {
    resetStore({
      activeSessionId: 'work-1',
      sessionRunStatuses: { [CENTRAL_SESSION_ID]: 'executing' },
    });
    await useStore.getState().ensureCentralConversation();
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
    const ensurePromise = useStore.getState().ensureCentralConversation();
    // 加载挂起期间用户退出中枢：restore 递增版本，作废在途 ensure
    await useStore.getState().restoreFromCentralConversation();
    releaseLoad(new Error('cancelled'));
    await ensurePromise;
    expect(useStore.getState().activeSessionId).toBe('work-1');
  });
});

describe('restoreFromCentralConversation', () => {
  it('进入前是活跃会话：切回原会话', async () => {
    resetStore({
      activeSessionId: CENTRAL_SESSION_ID,
      sessions: [session('work-1'), session(CENTRAL_SESSION_ID)],
      preCentralContext: { activeSessionId: 'work-1', newConversationId: null },
    });
    await useStore.getState().restoreFromCentralConversation();
    const state = useStore.getState();
    expect(state.activeSessionId).toBe('work-1');
    expect(state.preCentralContext).toBeNull();
  });

  it('进入前是新对话：恢复原草稿 ID（中枢草稿不清理）', async () => {
    resetStore({
      isNewConversation: true,
      activeSessionId: null,
      newConversationId: CENTRAL_SESSION_ID,
      preCentralContext: { activeSessionId: null, newConversationId: 'draft-1' },
    });
    useStore.setState((s) => ({
      inputCaches: {
        ...s.inputCaches,
        [CENTRAL_SESSION_ID]: { ...s.inputCaches[CENTRAL_SESSION_ID], text: '中枢草稿' },
        'draft-1': { ...s.inputCaches['draft-1'], text: '普通草稿' },
      },
    }));
    await useStore.getState().restoreFromCentralConversation();
    const state = useStore.getState();
    expect(state.isNewConversation).toBe(true);
    expect(state.newConversationId).toBe('draft-1');
    expect(state.inputCaches[CENTRAL_SESSION_ID]?.text).toBe('中枢草稿');
    expect(newSessionIdMock).not.toHaveBeenCalled();
  });

  it('无上下文记录（异常路径兜底）：不动作，保持现状', async () => {
    resetStore({ activeSessionId: CENTRAL_SESSION_ID, preCentralContext: null });
    await useStore.getState().restoreFromCentralConversation();
    // 未记录进入前上下文时不得猜测目标（正常路径 ensure 总会先记录）
    expect(useStore.getState().activeSessionId).toBe(CENTRAL_SESSION_ID);
    expect(newSessionIdMock).not.toHaveBeenCalled();
  });

  it('进入前会话已被删除：回新对话', async () => {
    resetStore({
      activeSessionId: CENTRAL_SESSION_ID,
      sessions: [session('other-1')],
      preCentralContext: { activeSessionId: 'work-deleted', newConversationId: null },
    });
    getSessionsMock.mockResolvedValue([session('other-1')]);
    newSessionIdMock.mockResolvedValue('fresh-after-restore');
    await useStore.getState().restoreFromCentralConversation();
    expect(useStore.getState().newConversationId).toBe('fresh-after-restore');
  });

  it('列表尚未加载（空）时不误判原会话已删除：刷新后切回', async () => {
    resetStore({
      activeSessionId: CENTRAL_SESSION_ID,
      sessions: [],
      preCentralContext: { activeSessionId: 'work-1', newConversationId: null },
    });
    getSessionsMock.mockResolvedValue([session('work-1')]);
    await useStore.getState().restoreFromCentralConversation();
    expect(useStore.getState().activeSessionId).toBe('work-1');
    expect(newSessionIdMock).not.toHaveBeenCalled();
  });
});

describe('普通会话链路隔离', () => {
  it('中枢对话激活期间 loadSessions 不判失效、不自动迁移', async () => {
    resetStore({ activeSessionId: CENTRAL_SESSION_ID, sessions: [] });
    getSessionsMock.mockResolvedValue([session('other-1')]);
    await useStore.getState().loadSessions();
    // 中枢对话不在普通列表但不算失效：不切到 other-1
    expect(useStore.getState().activeSessionId).toBe(CENTRAL_SESSION_ID);
  });

  it('普通会话失效语义不变（非中枢 ID 仍触发迁移）', async () => {
    resetStore({ activeSessionId: 'work-gone', sessions: [] });
    getSessionsMock.mockResolvedValue([session('other-1')]);
    await useStore.getState().loadSessions();
    expect(useStore.getState().activeSessionId).toBe('other-1');
  });
});

describe('startNewConversation 选项', () => {
  it('reuseId：不生成新 ID，直接以该 ID 进入新对话', async () => {
    await useStore.getState().startNewConversation(undefined, { reuseId: 'reused-1' });
    const state = useStore.getState();
    expect(state.newConversationId).toBe('reused-1');
    expect(newSessionIdMock).not.toHaveBeenCalled();
  });

  it('keepCurrentDraft：切换时不清理当前新对话草稿缓存', async () => {
    resetStore({ isNewConversation: true, newConversationId: 'draft-old' });
    useStore.setState((s) => ({
      inputCaches: { ...s.inputCaches, 'draft-old': { ...s.inputCaches['draft-old'], text: '草稿内容' } },
    }));
    await useStore.getState().startNewConversation(undefined, { reuseId: 'draft-new', keepCurrentDraft: true });
    expect(useStore.getState().newConversationId).toBe('draft-new');
    expect(useStore.getState().inputCaches['draft-old']?.text).toBe('草稿内容');
  });

  it('默认行为不变：切换时清理旧草稿缓存（无 keepCurrentDraft）', async () => {
    resetStore({ isNewConversation: true, newConversationId: 'draft-old' });
    useStore.setState((s) => ({
      inputCaches: { ...s.inputCaches, 'draft-old': { ...s.inputCaches['draft-old'], text: '草稿内容' } },
    }));
    await useStore.getState().startNewConversation(undefined, { reuseId: 'draft-new' });
    expect(useStore.getState().inputCaches['draft-old']).toBeUndefined();
  });
});
