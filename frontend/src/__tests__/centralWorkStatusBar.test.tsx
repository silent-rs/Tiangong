/**
 * 后台工作徽章测试（review M6 状态分离回归）：
 * - hover 浮层与 pinned 抽屉互不干扰：点击固定后鼠标移出不再收起面板
 * - ESC 收起抽屉；空工作列表时徽章不渲染
 * - 工作项点击回调携带 session_id（进入普通模式）
 */
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const listenMock = vi.fn(() => Promise.resolve(() => {}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: listenMock,
}));

const getCentralWorkStatusMock = vi.fn(() => Promise.resolve([]));
vi.mock('@/api/tauri', () => ({
  api: {
    getCentralWorkStatus: (...args: unknown[]) => getCentralWorkStatusMock(...args),
  },
}));

vi.mock('@/lib/centralConversation', () => ({
  restoreFromCentralConversation: vi.fn(() => Promise.resolve()),
}));

const switchSessionMock = vi.fn(() => Promise.resolve());
vi.mock('@/store/useStore', () => {
  interface StoreState {
    centralMode: boolean;
    setCentralMode: () => void;
    switchSession: (sessionId: string) => Promise<void>;
    runStatus: string;
    messages: unknown[];
  }
  const state: StoreState = {
    centralMode: true,
    setCentralMode: () => undefined,
    switchSession: switchSessionMock,
    runStatus: 'idle',
    messages: [],
  };
  // zustand hook 形态：可带 selector 调用，且带 getState
  const useStoreMock = (selector?: (s: StoreState) => unknown) =>
    selector ? selector(state) : state;
  useStoreMock.getState = () => state;
  return { useStore: useStoreMock };
});

const { CentralWorkStatusBar } = await import(
  '@/components/central/CentralWorkStatusBar'
);

function makeItem(overrides: Partial<Record<string, unknown>> = {}) {
  return {
    work_id: 'work-00000001',
    session_id: 'sess-aaaaaaaa',
    task: 'Review 天工仓库',
    model: 'k3',
    dispatched_at: '2026-11-11 12:00:00',
    state: 'running',
    reused_session: false,
    ...overrides,
  };
}

function mount(): HTMLElement {
  const container = document.createElement('div');
  document.body.appendChild(container);
  let root!: Root;
  act(() => {
    root = createRoot(container);
    root.render(<CentralWorkStatusBar />);
  });
  return container;
}

async function mountWithItems(items: unknown[]): Promise<HTMLElement> {
  getCentralWorkStatusMock.mockImplementation(() => Promise.resolve(items));
  const host = mount();
  // 挂载后的首个 fetch promise 落定 + 渲染
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
  return host;
}

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

describe('CentralWorkStatusBar', () => {
  let container: HTMLElement | undefined;

  beforeEach(() => {
    vi.clearAllMocks();
    container = undefined;
  });

  afterEach(() => {
    if (container) container.remove();
    container = undefined;
  });

  it('无后台工作时徽章常驻显示空态（用户设计：始终显示）', async () => {
    container = await mountWithItems([]);
    const badge = container.querySelector('button');
    expect(badge).not.toBeNull('徽章必须常驻，无任务也不消失');
    expect(badge!.getAttribute('aria-label')).toBe('后台工作：暂无任务');
    expect(badge!.textContent).toContain('无后台任务');
    // 空态面板可打开，展示空态文案
    act(() => {
      badge!.click();
    });
    const drawer = container.querySelector('[role="dialog"]');
    expect(drawer).not.toBeNull();
    expect(drawer!.textContent).toContain('暂无后台工作');
  });

  it('hover 打开轻量浮层，鼠标移出后收起', async () => {
    vi.useFakeTimers();
    try {
      container = await mountWithItems([makeItem()]);
      const badge = container.querySelector('button');
      expect(badge).not.toBeNull();
      act(() => {
        // React 依据 mouseover/mouseout（带 relatedTarget）合成 enter/leave
        badge!.dispatchEvent(
          new MouseEvent('mouseover', { bubbles: true, relatedTarget: document.body }),
        );
      });
      expect(container.querySelector('[role="dialog"]')).toBeNull();
      expect(container.textContent).toContain('执行中');
      act(() => {
        badge!.dispatchEvent(
          new MouseEvent('mouseout', { bubbles: true, relatedTarget: document.body }),
        );
      });
      act(() => {
        vi.advanceTimersByTime(300);
      });
      expect(container.textContent).not.toContain('执行中');
    } finally {
      vi.useRealTimers();
    }
  });

  it('点击徽章固定为抽屉，鼠标移出不收起，ESC 收起', async () => {
    container = await mountWithItems([makeItem()]);
    const badge = container.querySelector('button');
    act(() => {
      badge!.click();
    });
    const drawer = container.querySelector('[role="dialog"]');
    expect(drawer).not.toBeNull();
    expect(drawer!.getAttribute('aria-label')).toBe('后台工作视图');
    // 固定后鼠标移出：抽屉仍在（M6 状态分离）
    act(() => {
      badge!.dispatchEvent(
        new MouseEvent('mouseout', { bubbles: true, relatedTarget: document.body }),
      );
    });
    expect(container.querySelector('[role="dialog"]')).not.toBeNull();
    // ESC 收起
    act(() => {
      window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    });
    expect(container.querySelector('[role="dialog"]')).toBeNull();
  });

  it('工作项点击携带 session_id 并经普通模式打开', async () => {
    container = await mountWithItems([makeItem({ state: 'completed' })]);
    const badge = container.querySelector('button');
    act(() => {
      badge!.click();
    });
    const row = container.querySelector('[role="button"][tabindex="0"]');
    expect(row).not.toBeNull();
    await act(async () => {
      row!.dispatchEvent(
        new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }),
      );
    });
    expect(switchSessionMock).toHaveBeenCalledWith('sess-aaaaaaaa');
  });
});
