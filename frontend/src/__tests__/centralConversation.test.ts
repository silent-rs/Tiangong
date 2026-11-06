/**
 * 中枢对话绑定测试（POC 阶段 1：中枢对话复用现有 Session 持久化）。
 *
 * 覆盖 store.ensureCentralConversation / restoreFromCentralConversation 与
 * startNewConversation 的 reuseId / keepCurrentDraft 选项：
 * - 首次进入：生成新 ID 绑定中枢对话（新对话态），绑定写入 localStorage；
 * - 已创建：按绑定 ID 切换回中枢对话（不新建）；
 * - 绑定会话被删除：重新绑定新 ID，不误切空对话；
 * - 会话列表加载失败：放弃本次切换（避免把已创建的中枢对话当成新对话）；
 * - 退出恢复：进入前的活跃会话 / 新对话草稿原样恢复，中枢对话草稿保留。
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
const {
  CENTRAL_SESSION_STORAGE_KEY,
  isCentralSessionId,
  persistCentralSessionId,
  readCentralSessionId,
} = await import('@/lib/centralModePreference');
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

function loadedSession(id: string): LoadedSession {
  return {
    id,
    messages: [{
      id: `${id}-message`,
      role: 'assistant',
      content: [{ type: 'text', text: `message from ${id}` }],
      created_at: '2026-11-20 00:00:00',
    }],
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
  loadSessionMock.mockImplementation(async (id: string) => loadedSession(id));
  getInputCacheMock.mockResolvedValue(null);
  vi.mocked(api.switchSession).mockResolvedValue(undefined);
  vi.mocked(api.setInputCache).mockResolvedValue(undefined);
  vi.mocked(api.removeInputCache).mockResolvedValue(undefined);
  prewarmMock.mockResolvedValue(undefined);
  vi.mocked(api.setReasoningEffort).mockResolvedValue(undefined);
  resetStore();
});

describe('中枢偏好存取（centralModePreference）', () => {
  it('persist / read 往返；非法值回退 null', () => {
    expect(readCentralSessionId()).toBeNull();
    persistCentralSessionId('central-1');
    expect(readCentralSessionId()).toBe('central-1');
    expect(isCentralSessionId('central-1')).toBe(true);
    expect(isCentralSessionId('other')).toBe(false);
    expect(isCentralSessionId(null)).toBe(false);
    localStorage.setItem(CENTRAL_SESSION_STORAGE_KEY, '   ');
    expect(readCentralSessionId()).toBeNull();
  });

  it('默认问候包含中枢身份（设计稿 §4.2 原文锚点）', () => {
    expect(CENTRAL_DEFAULT_GREETING).toContain('我是天工中枢');
  });
});

describe('ensureCentralConversation', () => {
  it('无绑定：生成新 ID 进入新对话态并持久化绑定', async () => {
    newSessionIdMock.mockResolvedValue('central-new');
    await useStore.getState().ensureCentralConversation();
    const state = useStore.getState();
    expect(state.isNewConversation).toBe(true);
    expect(state.activeSessionId).toBeNull();
    expect(state.newConversationId).toBe('central-new');
    expect(readCentralSessionId()).toBe('central-new');
  });

  it('记录进入前的普通模式上下文（幂等：重复 ensure 不覆盖）', async () => {
    const list = [session('work-1'), session('central-1')];
    getSessionsMock.mockResolvedValue(list);
    resetStore({ activeSessionId: 'work-1', sessions: list });
    persistCentralSessionId('central-1');
    await useStore.getState().ensureCentralConversation();
    expect(useStore.getState().preCentralContext).toEqual({
      activeSessionId: 'work-1',
      newConversationId: null,
    });
    // 第二次进入（未退出）不覆盖记录
    await useStore.getState().ensureCentralConversation();
    expect(useStore.getState().preCentralContext?.activeSessionId).toBe('work-1');
  });

  it('已创建的中枢对话：按绑定 ID 切换（不新建）', async () => {
    const list = [session('work-1'), session('central-1')];
    getSessionsMock.mockResolvedValue(list);
    resetStore({ activeSessionId: 'work-1', sessions: list });
    persistCentralSessionId('central-1');
    await useStore.getState().ensureCentralConversation();
    const state = useStore.getState();
    expect(state.activeSessionId).toBe('central-1');
    expect(state.isNewConversation).toBe(false);
    expect(newSessionIdMock).not.toHaveBeenCalled();
    // 历史可见（首次走 api.loadSession，同文件内已 hydrate 时走缓存）
    expect(state.messages.length).toBeGreaterThan(0);
  });

  it('已在中枢对话中：幂等，不重复切换', async () => {
    const list = [session('central-1')];
    getSessionsMock.mockResolvedValue(list);
    resetStore({
      activeSessionId: 'central-1',
      sessions: list,
      preCentralContext: { activeSessionId: 'work-1', newConversationId: null },
    });
    persistCentralSessionId('central-1');
    await useStore.getState().ensureCentralConversation();
    expect(useStore.getState().activeSessionId).toBe('central-1');
    expect(loadSessionMock).not.toHaveBeenCalled();
    // 已记录的上下文不被覆盖
    expect(useStore.getState().preCentralContext?.activeSessionId).toBe('work-1');
  });

  it('处于绑定 ID 的新对话态（未发送）：沿用不换 ID（草稿保留）', async () => {
    const list: Session[] = [];
    getSessionsMock.mockResolvedValue(list);
    persistCentralSessionId('central-draft');
    resetStore({ isNewConversation: true, activeSessionId: null, newConversationId: 'central-draft', sessions: list });
    useStore.setState((s) => ({
      inputCaches: { ...s.inputCaches, 'central-draft': { ...s.inputCaches['central-draft'], text: '未发送的中枢草稿' } },
    }));
    await useStore.getState().ensureCentralConversation();
    const state = useStore.getState();
    expect(state.newConversationId).toBe('central-draft');
    expect(state.inputCaches['central-draft']?.text).toBe('未发送的中枢草稿');
    expect(newSessionIdMock).not.toHaveBeenCalled();
    expect(readCentralSessionId()).toBe('central-draft');
  });

  it('绑定会话已被删除：重新绑定新 ID（新对话态）', async () => {
    const list = [session('work-1')];
    getSessionsMock.mockResolvedValue(list);
    resetStore({ sessions: list });
    persistCentralSessionId('central-deleted');
    newSessionIdMock.mockResolvedValue('central-fresh');
    await useStore.getState().ensureCentralConversation();
    const state = useStore.getState();
    expect(state.isNewConversation).toBe(true);
    expect(state.newConversationId).toBe('central-fresh');
    expect(readCentralSessionId()).toBe('central-fresh');
  });

  it('会话列表加载失败：放弃本次（不误把已创建对话当新对话）', async () => {
    resetStore({ activeSessionId: 'work-1', sessions: [session('work-1')] });
    persistCentralSessionId('central-1');
    getSessionsMock.mockRejectedValue(new Error('backend down'));
    await useStore.getState().ensureCentralConversation();
    // 未切走、未新建、绑定未被覆盖
    expect(useStore.getState().activeSessionId).toBe('work-1');
    expect(newSessionIdMock).not.toHaveBeenCalled();
    expect(readCentralSessionId()).toBe('central-1');
    // preCentralContext 也未写入（本次进入未生效）
    expect(useStore.getState().preCentralContext).toBeNull();
  });

  it('重启直入：偏好记录仍指向中枢对话时直接恢复（历史可见）', async () => {
    // 模拟重启后的空 store：sessions 空、active 空；后端已有中枢对话
    persistCentralSessionId('central-1');
    getSessionsMock.mockResolvedValue([session('central-1'), session('work-1')]);
    await useStore.getState().ensureCentralConversation();
    const state = useStore.getState();
    expect(state.activeSessionId).toBe('central-1');
    expect(state.messages.length).toBeGreaterThan(0);
    // 重启直入无普通模式上下文可记录
    expect(state.preCentralContext).toEqual({ activeSessionId: null, newConversationId: null });
  });
});

describe('restoreFromCentralConversation', () => {
  it('进入前是活跃会话：切回原会话', async () => {
    resetStore({
      activeSessionId: 'central-1',
      sessions: [session('work-1'), session('central-1')],
      preCentralContext: { activeSessionId: 'work-1', newConversationId: null },
    });
    await useStore.getState().restoreFromCentralConversation();
    const state = useStore.getState();
    expect(state.activeSessionId).toBe('work-1');
    expect(state.preCentralContext).toBeNull();
  });

  it('进入前是新对话：恢复原草稿 ID（中枢草稿不清理）', async () => {
    // 中枢对话自身有草稿缓存，恢复普通新对话时不得清掉
    resetStore({
      isNewConversation: true,
      activeSessionId: null,
      newConversationId: 'central-1',
      preCentralContext: { activeSessionId: null, newConversationId: 'draft-1' },
    });
    useStore.setState((s) => ({
      inputCaches: {
        ...s.inputCaches,
        'central-1': { ...s.inputCaches['central-1'], text: '中枢草稿' },
        'draft-1': { ...s.inputCaches['draft-1'], text: '普通草稿' },
      },
    }));
    await useStore.getState().restoreFromCentralConversation();
    const state = useStore.getState();
    expect(state.isNewConversation).toBe(true);
    expect(state.newConversationId).toBe('draft-1');
    expect(state.inputCaches['central-1']?.text).toBe('中枢草稿');
    expect(newSessionIdMock).not.toHaveBeenCalled();
  });

  it('无上下文记录：不动作（异常路径兜底）', async () => {
    resetStore({ activeSessionId: 'central-1', preCentralContext: null });
    await useStore.getState().restoreFromCentralConversation();
    expect(useStore.getState().activeSessionId).toBe('central-1');
  });

  it('进入前会话已被删除：回新对话', async () => {
    resetStore({
      activeSessionId: 'central-1',
      sessions: [session('central-1')],
      preCentralContext: { activeSessionId: 'work-deleted', newConversationId: null },
    });
    newSessionIdMock.mockResolvedValue('fresh-after-restore');
    await useStore.getState().restoreFromCentralConversation();
    const state = useStore.getState();
    expect(state.isNewConversation).toBe(true);
    expect(state.newConversationId).toBe('fresh-after-restore');
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
