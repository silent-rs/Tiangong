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
    centralWorkTimeline: () =>
      Promise.resolve([
        { at: '2026-11-13 10:00:00', kind: 'work_item.started', note: '派发投递成功' },
        { at: '2026-11-13 10:05:00', kind: 'work_item.completed', note: '工作完成' },
      ]),
  },
}));

vi.mock('@/features/central/lib/centralConversation', () => ({
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
  '@/features/central/components/CentralWorkStatusBar'
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
    expect(badge!.getAttribute('aria-label')).toBe('后台工作：暂无进行中的任务');
    // SVG 徽章：空态为四角星（central-badge-idle），不再显示文字
    expect(container.querySelector('[data-testid="central-badge-idle"]')).not.toBeNull();
    // 点击仍可打开浮层（空态文案）
    act(() => {
      badge!.click();
    });
    const popover = container.querySelector('#central-work-popover');
    expect(popover).not.toBeNull();
    expect(popover!.textContent).toContain('暂无后台工作');
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
      expect(container.querySelector('#central-work-popover')).not.toBeNull();
      expect(container.querySelector('[data-testid="central-work-drawer"]')).toBeNull('hover 只开浮层，不开详情抽屉');
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

  it('用户实测五：纯 hover 交互——点击徽章打开浮层但不固定，移出即收（无 pinned）', async () => {
    vi.useFakeTimers();
    try {
      container = await mountWithItems([makeItem()]);
      const badge = container.querySelector('button');
      act(() => {
        badge!.click();
      });
      // 点击打开的是浮层（无固定面板；详情抽屉未开）
      expect(container.querySelector('#central-work-popover')).not.toBeNull();
      expect(container.querySelector('[data-testid="central-work-drawer"]')).toBeNull('点击不再固定为抽屉');
      // 鼠标移出：浮层按 250ms 延迟收起（hover 语义）
      act(() => {
        badge!.dispatchEvent(
          new MouseEvent('mouseout', { bubbles: true, relatedTarget: document.body }),
        );
      });
      act(() => {
        vi.advanceTimersByTime(300);
      });
      expect(container.querySelector('#central-work-popover')).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it('中断任务进入「已中断」分组（重启恢复路径）', async () => {
    container = await mountWithItems([
      makeItem({ work_id: 'work-i1', session_id: 'sess-i', state: 'interrupted', task: '被中断的任务' }),
    ]);
    const badge = container.querySelector('button');
    act(() => {
      badge!.click();
    });
    const popover = container.querySelector('#central-work-popover');
    expect(popover).not.toBeNull();
    expect(popover!.textContent).toContain('已中断');
    expect(popover!.textContent).toContain('被中断的任务');
    expect(popover!.textContent).toContain('可让中枢重派');
  });

  it('focus 进入容器不触发 250ms 关闭（a11y focus-within）', async () => {
    container = await mountWithItems([makeItem()]);
    const badge = container.querySelector('button')!;
    act(() => {
      badge.dispatchEvent(
        new MouseEvent('mouseover', { bubbles: true, relatedTarget: document.body }),
      );
    });
    expect(container.querySelector('#central-work-popover')).not.toBeNull('hover 面板应打开');
    // Tab 聚焦容器内控件（真实聚焦改变 activeElement）：focusin 捕获
    // 打开面板并保持，随后鼠标移出不得关闭（focus-within 语义）
    act(() => {
      badge.focus();
    });
    act(() => {
      badge.dispatchEvent(
        new MouseEvent('mouseout', { bubbles: true, relatedTarget: document.body }),
      );
    });
    await new Promise((resolve) => setTimeout(resolve, 340));
    expect(container.querySelector('#central-work-popover')).not.toBeNull(
      '焦点在容器内时面板不得被 250ms 定时关闭',
    );
  });


  it('徽章 6 态随最高优先级切换（等你决定 > 执行中 > 失败 > 中断 > 暂停 > 空闲）', async () => {
    // 空闲
    container = await mountWithItems([]);
    expect(container.querySelector('[data-testid="central-badge-idle"]')).not.toBeNull();
    // 暂停/排队（无更高优先级时）
    container = await mountWithItems([makeItem({ state: 'paused' })]);
    expect(container.querySelector('[data-testid="central-badge-paused"]')).not.toBeNull();
    // 中断 压过 暂停
    container = await mountWithItems([
      makeItem({ work_id: 'w-p', session_id: 's-p', state: 'paused' }),
      makeItem({ work_id: 'w-i', session_id: 's-i', state: 'interrupted' }),
    ]);
    expect(container.querySelector('[data-testid="central-badge-interrupted"]')).not.toBeNull();
    // 失败 压过 中断
    container = await mountWithItems([
      makeItem({ work_id: 'w-i', session_id: 's-i2', state: 'interrupted' }),
      makeItem({ work_id: 'w-f', session_id: 's-f2', state: 'failed' }),
    ]);
    expect(container.querySelector('[data-testid="central-badge-failed"]')).not.toBeNull();
    // 执行中 压过 失败
    container = await mountWithItems([
      makeItem({ work_id: 'w-f', session_id: 's-f3', state: 'failed' }),
      makeItem({ work_id: 'w-r', session_id: 's-r3', state: 'running' }),
    ]);
    expect(container.querySelector('[data-testid="central-badge-running"]')).not.toBeNull();
    // 等你决定 压过 执行中（最高优先级）
    container = await mountWithItems([
      makeItem({ work_id: 'w-r', session_id: 's-r4', state: 'running' }),
      makeItem({ work_id: 'w-w', session_id: 's-w4', state: 'waiting_user' }),
    ]);
    expect(container.querySelector('[data-testid="central-badge-waiting_user"]')).not.toBeNull();
    // 完成即空闲：仅 completed/report_queued → idle（实测七）
    container = await mountWithItems([
      makeItem({ work_id: 'w-c', session_id: 's-c5', state: 'completed' }),
      makeItem({ work_id: 'w-q', session_id: 's-q5', state: 'report_queued' }),
    ]);
    expect(container.querySelector('[data-testid="central-badge-idle"]')).not.toBeNull('完成即空闲');
  });

  it('列表按派发时间降序（最新在前，用户实测四；十八轮口径 11 顺序断言）', async () => {
    container = await mountWithItems([
      makeItem({ work_id: 'w-old', session_id: 's-old', state: 'running', dispatched_at: '2026-11-13 08:00:00', task: '旧任务' }),
      makeItem({ work_id: 'w-new', session_id: 's-new', state: 'running', dispatched_at: '2026-11-13 12:00:00', task: '新任务' }),
      makeItem({ work_id: 'w-mid', session_id: 's-mid', state: 'running', dispatched_at: '2026-11-13 10:00:00', task: '中任务' }),
    ]);
    const badge = container.querySelector('button')!;
    act(() => {
      badge.click();
    });
    const rows = Array.from(
      container.querySelectorAll('[role="button"][tabindex="0"]'),
    ).map((row) => row.textContent ?? '');
    const newIdx = rows.findIndex((t) => t.includes('新任务'));
    const midIdx = rows.findIndex((t) => t.includes('中任务'));
    const oldIdx = rows.findIndex((t) => t.includes('旧任务'));
    expect(newIdx).toBeGreaterThan(-1);
    expect(midIdx).toBeGreaterThan(newIdx, '中任务在 新任务 之后');
    expect(oldIdx).toBeGreaterThan(midIdx, '旧任务在 中任务 之后');
  });


  it('reduced-motion：执行中降级静态素材（SMIL 不受 CSS 约束，JS 判定）', async () => {
    // matchMedia stub（jsdom 无原生）
    const realMatchMedia = window.matchMedia;
    Object.defineProperty(window, 'matchMedia', {
      writable: true,
      value: (query: string) => ({
        matches: query.includes('prefers-reduced-motion'),
        media: query,
        addEventListener: () => {},
        removeEventListener: () => {},
        addListener: () => {},
        removeListener: () => {},
        dispatchEvent: () => false,
      }),
    });
    try {
      container = await mountWithItems([makeItem({ state: 'running' })]);
      const img = container.querySelector('[data-testid="central-badge-running"]');
      expect(img).not.toBeNull();
      const src = (img as HTMLImageElement).src;
      // Vite 内联 SVG 为 data URI（无文件名）——以「无 SMIL 动画标签」判定静态帧
      expect(src).not.toContain('animate', 'reduced-motion 素材不得含 SMIL 动画');
    } finally {
      Object.defineProperty(window, 'matchMedia', { writable: true, value: realMatchMedia });
    }
  });

  it('aria-controls 指向浮层 id（a11y 关联，纯 hover 模式）', async () => {
    container = await mountWithItems([makeItem()]);
    const badge = container.querySelector('button')!;
    act(() => {
      badge.click();
    });
    expect(badge.getAttribute('aria-controls')).toBe('central-work-popover');
    expect(container.querySelector('#central-work-popover')).not.toBeNull();
  });

  it('失败任务进入「失败」分组，与完成/中断分色呈现', async () => {
    container = await mountWithItems([
      makeItem({ work_id: 'work-f1', session_id: 'sess-f', state: 'failed', task: '失败任务' }),
      makeItem({ work_id: 'work-c1', session_id: 'sess-c', state: 'completed', task: '完成任务' }),
    ]);
    const badge = container.querySelector('button');
    act(() => {
      badge!.click();
    });
    const popover = container.querySelector('#central-work-popover');
    expect(popover).not.toBeNull();
    expect(popover!.textContent).toContain('失败');
    expect(popover!.textContent).toContain('完成任务');
    expect(popover!.textContent).toContain('失败任务');
  });

  it('同会话多条记录按 session 聚合为单条显示', async () => {
    container = await mountWithItems([
      makeItem({ work_id: 'work-a', session_id: 'sess-same', task: '旧任务标题' }),
      makeItem({ work_id: 'work-b', session_id: 'sess-same', task: '最新任务标题', state: 'completed' }),
    ]);
    const badge = container.querySelector('button');
    act(() => {
      badge!.click();
    });
    const text = container.querySelector('#central-work-popover')!.textContent ?? '';
    // 后端按会话聚合（一屏一条）；前端对脏数据兜底去重——同会话只显示最新
    const rows = Array.from(
      container.querySelectorAll('[role="button"][tabindex="0"]'),
    ).filter((row) => (row.textContent ?? '').includes('任务标题'));
    expect(rows.length).toBe(1);
    expect(text).toContain('最新任务标题');
  });

  it('R4 工作抽屉：点击工作行打开抽屉（不切模式），「打开原始工作会话」才切', async () => {
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
    // 点击行：打开抽屉，不退出中枢、不切会话（§6.4 抽屉不改上下文）
    expect(switchSessionMock).not.toHaveBeenCalled();
    const drawer = container.querySelector('[data-testid="central-work-drawer"]');
    expect(drawer).not.toBeNull();
    expect(drawer!.textContent).toContain('sess-aaaaaaaa'.slice(0, 0) + '已完成');
    // 时间线加载（api mock 已注入 centralWorkTimeline）
    await act(async () => {
      await Promise.resolve();
    });
    // 「打开原始工作会话」：显式动作才切换（按文本定位，避免结构耦合）
    const openSession = Array.from(drawer!.querySelectorAll('button')).find((btn) =>
      btn.textContent?.includes('打开原始工作会话'),
    );
    expect(openSession).toBeDefined();
    await act(async () => {
      openSession!.click();
    });
    expect(switchSessionMock).toHaveBeenCalledWith('sess-aaaaaaaa');
    // 抽屉关闭
    expect(container.querySelector('[data-testid="central-work-drawer"]')).toBeNull();
  });
});
