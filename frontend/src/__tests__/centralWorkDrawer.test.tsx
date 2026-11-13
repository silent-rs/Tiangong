/**
 * CentralWorkDrawer 单测（R4 工作抽屉，方案 §3.9 / §6.4）：
 * - 时间线从事件日志加载渲染（人读类型名）
 * - waiting_user 展示「回中枢对话答复」提示
 * - Esc 关闭：只影响视图，不触碰会话
 * - 「打开原始工作会话」经显式回调
 * - 时间线加载失败：显示错误而非静默
 */
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { CentralWorkDrawer } from '@/components/central/CentralWorkDrawer';

const timelineMock = vi.fn();

vi.mock('@/api/tauri', () => ({
  api: {
    centralWorkTimeline: (...args: unknown[]) => timelineMock(...args),
  },
}));

function makeItem(overrides: Record<string, unknown> = {}) {
  return {
    work_id: 'work-drawer',
    session_id: 'sess-worker',
    task: '重构认证模块',
    model: 'k3',
    dispatched_at: '2026-11-13 10:00:00',
    state: 'running',
    reused_session: false,
    ...overrides,
  };
}

describe('CentralWorkDrawer', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    timelineMock.mockReset();
    // 默认空时间线（个别用例覆盖）；未设置时 undefined.then 会崩
    timelineMock.mockResolvedValue([]);
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });
  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  function renderDrawer(
    work: ReturnType<typeof makeItem>,
    handlers: { onClose?: () => void; onOpenSession?: (sessionId: string) => void } = {},
  ) {
    act(() => {
      root.render(
        <CentralWorkDrawer
          work={work}
          onClose={handlers.onClose ?? (() => {})}
          onOpenSession={handlers.onOpenSession ?? (() => {})}
        />,
      );
    });
  }

  it('渲染工作详情并从事件日志加载时间线（人读类型名）', async () => {
    timelineMock.mockResolvedValue([
      { at: '2026-11-13 10:00:00', kind: 'work_item.started', note: '派发投递成功' },
      { at: '2026-11-13 10:05:00', kind: 'work_item.completed', note: '工作完成' },
    ]);
    renderDrawer(makeItem());
    expect(timelineMock).toHaveBeenCalledWith('work-drawer');
    expect(container.textContent).toContain('重构认证模块');
    await act(async () => {
      await Promise.resolve();
    });
    expect(container.textContent).toContain('开始/恢复');
    expect(container.textContent).toContain('完成');
    expect(container.textContent).toContain('派发投递成功');
  });

  it('waiting_user：提示回中枢对话答复（不在抽屉内决定）', async () => {
    timelineMock.mockResolvedValue([]);
    renderDrawer(makeItem({ state: 'waiting_user' }));
    await act(async () => {
      await Promise.resolve();
    });
    expect(container.textContent).toContain('回到中枢对话直接答复');
  });

  it('Esc 关闭：只影响视图，不触碰会话', async () => {
    const onClose = vi.fn();
    const onOpenSession = vi.fn();
    renderDrawer(makeItem(), { onClose, onOpenSession });
    await act(async () => {
      window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    });
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(onOpenSession).not.toHaveBeenCalled();
  });

  it('「打开原始工作会话」经显式回调携带会话 ID', async () => {
    const onOpenSession = vi.fn();
    renderDrawer(makeItem(), { onOpenSession });
    await act(async () => {
      await Promise.resolve();
    });
    const button = Array.from(container.querySelectorAll('button')).find((btn) =>
      btn.textContent?.includes('打开原始工作会话'),
    );
    expect(button).toBeDefined();
    await act(async () => {
      button!.click();
    });
    expect(onOpenSession).toHaveBeenCalledWith('sess-worker');
  });


  it('视口级抽屉：遮罩点击关闭（fixed 定位，不再受角标容器尺寸影响）', async () => {
    timelineMock.mockResolvedValue([]);
    const onClose = vi.fn();
    renderDrawer(makeItem(), { onClose });
    const overlay = container.querySelector('[data-testid="central-work-drawer-overlay"]');
    expect(overlay).not.toBeNull();
    const drawer = container.querySelector('[data-testid="central-work-drawer"]')!;
    // fixed 定位验证（修复：此前 absolute 挂在角标小容器内尺寸塌陷）
    expect(drawer.className).toContain('fixed');
    await act(async () => {
      overlay!.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('时间线加载失败：显示错误而非静默', async () => {
    timelineMock.mockRejectedValue(new Error('disk io'));
    renderDrawer(makeItem());
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(container.textContent).toContain('disk io');
  });
});
