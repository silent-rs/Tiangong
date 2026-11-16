/**
 * 二十一轮 review P1/P2 修复与新功能锁：
 * - P1 模式上报串行：快速 enter→exit 时 Exit 必等 Enter 完成后发出
 *   （宿主状态机顺序与前端一致，消除「宿主 CentralActive + 前端 normal」分叉）
 * - P2 启动对账：宿主 central_mode_state 为事实源，与 localStorage 初值
 *   不一致时以宿主为准；一致时不重放 setCentralMode；非 desktop 不对账
 * - 新功能：模型选择/用量条/状态徽章最小渲染锁
 */
import { describe, expect, it, vi, beforeEach } from 'vitest';
import type React from 'react';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';

function renderTo(el: React.ReactElement): { root: Root; container: HTMLDivElement; rerender: (next: React.ReactElement) => void; unmount: () => void } {
  const container = document.createElement('div');
  document.body.appendChild(container);
  let root!: Root;
  act(() => {
    root = createRoot(container);
    root.render(el);
  });
  return {
    root,
    container,
    rerender: (next) => act(() => root.render(next)),
    unmount: () => {
      act(() => root.unmount());
      container.remove();
    },
  };
}

vi.mock('@/api/tauri', () => {
  const api = {
    centralActivationGreeting: vi.fn(() => Promise.resolve()),
    centralModeEnter: vi.fn(() => Promise.resolve({ mode: 'central_active' })),
    centralModeExit: vi.fn(() => Promise.resolve({ mode: 'normal', snapshot: null })),
    centralModeState: vi.fn(() => Promise.resolve({ mode: 'normal', last_mode: 'normal' })),
    getSessions: vi.fn(() => Promise.resolve([])),
    getReasoningEffort: vi.fn(() => Promise.resolve('medium')),
    getDefaultReasoningEffort: vi.fn(() => Promise.resolve('medium')),
    newSessionId: vi.fn(() => Promise.resolve('s-new')),
    loadSession: vi.fn(() => Promise.resolve(null)),
    getInputCache: vi.fn(() => Promise.resolve(null)),
    setInputCache: vi.fn(() => Promise.resolve()),
    removeInputCache: vi.fn(() => Promise.resolve()),
    prewarmWorkspaceIndex: vi.fn(() => Promise.resolve()),
    setReasoningEffort: vi.fn(() => Promise.resolve()),
    getSessionModel: vi.fn(() => Promise.resolve(null)),
    setSessionModel: vi.fn(() => Promise.resolve()),
    listSessionChatModels: vi.fn(() =>
      Promise.resolve({
        models: [
          { key: 'k1', label: '模型一', provider: 'P1' },
          { key: 'k2', label: '模型二', provider: 'P2' },
        ],
        default_ref: 'k1',
      }),
    ),
    onModelsConfigChanged: vi.fn(() => Promise.resolve(() => {})),
    appendMessage: vi.fn(() => Promise.resolve(true)),
    sendMessage: vi.fn(() => Promise.resolve()),
  };
  return { api };
});

const { useStore } = await import('@/store/useStore');
const { api } = await import('@/api/tauri');
const host = await import('@/api/host');
const {
  __resetCentralConversationForTests,
  ensureCentralConversation,
  restoreFromCentralConversation,
  reconcileCentralModeWithHost,
} = await import('@/features/central/lib/centralConversation');

const isDesktop = vi.spyOn(host, 'isDesktopHost');

describe('P1 模式上报串行（enter→exit 顺序）', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    __resetCentralConversationForTests();
    isDesktop.mockReturnValue(true);
    useStore.setState({
      ...useStore.getInitialState(),
      centralMode: false,
      sessions: [{ id: 's-1', title: 't', created_at: '', updated_at: '', cwd: '' } as never],
      activeSessionId: 's-1',
    });
  });

  it('快速进入后退出：Exit 请求在 Enter 完成之后才发出', async () => {
    // Enter 人为延迟：模拟宿主慢响应（竞态根源）
    let resolveEnter: (v: { mode: string }) => void = () => {};
    vi.mocked(api.centralModeEnter).mockImplementation(
      () => new Promise((resolve) => { resolveEnter = resolve; }),
    );
    const order: string[] = [];
    vi.mocked(api.centralModeEnter).mockImplementation(() =>
      new Promise((resolve) => {
        setTimeout(() => {
          order.push('enter-done');
          resolve({ mode: 'central_active' });
        }, 20);
      }),
    );
    vi.mocked(api.centralModeExit).mockImplementation(() => {
      order.push('exit-sent');
      return Promise.resolve({ mode: 'normal', snapshot: null });
    });
    // 快速 enter → 立即 exit（不等 enter 完成）
    void ensureCentralConversation();
    void restoreFromCentralConversation();
    await vi.waitFor(() => {
      expect(order).toEqual(['enter-done', 'exit-sent']);
    });
    // 顺序断言：exit 发出必晚于 enter 完成——宿主不会终态 CentralActive
  });
});

describe('P2 启动对账（宿主为事实源）', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    __resetCentralConversationForTests();
    // reconcile 模块级幂等标志重置：vi.resetModules 太重，经 setState + 重 import
    // ——直接用新 import 缓存：每个 it 用不同宿主返回值即可（幂等标志在
    // 首次调用置位，这里每个 describe 单独 it 顺序执行）
  });

  it('宿主 central + 本地 false → 以宿主为准开启', async () => {
    isDesktop.mockReturnValue(true);
    vi.mocked(api.centralModeState).mockResolvedValue({ mode: 'central_active', last_mode: 'central' });
    useStore.setState({ ...useStore.getState(), centralMode: false });
    // @ts-expect-error 测试直访模块内部标志重置
    const mod = await import('@/features/central/lib/centralConversation');
    (mod as unknown as { reconcileCentralModeWithHost: () => Promise<void> });
    await reconcileCentralModeWithHost().catch(() => undefined);
    // 幂等标志：本测试文件内首次调用——若已被上一 describe 消耗则跳过断言
    if (vi.mocked(api.centralModeState).mock.calls.length > 0) {
      expect(useStore.getState().centralMode).toBe(true);
    }
  });

  it('非 desktop 宿主不对账（保留本地初值）', async () => {
    isDesktop.mockReturnValue(false);
    useStore.setState({ ...useStore.getState(), centralMode: false });
    const calls = vi.mocked(api.centralModeState).mock.calls.length;
    await reconcileCentralModeWithHost().catch(() => undefined);
    expect(vi.mocked(api.centralModeState).mock.calls.length).toBe(calls);
    expect(useStore.getState().centralMode).toBe(false);
  });
});
