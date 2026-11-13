/**
 * CentralCardsBar 单测（R5 消息卡片）：
 * - 授权卡片：宿主事件到达渲染；点击「授权本次」调用
 *   centralApproveWorkspace（一次性票据语义）；成功转已授权态；
 *   同目录同任务去重。
 * - 决定卡片：work status 的 waiting_user 驱动渲染。
 */
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { CentralCardsBar } from '@/components/central/CentralCardsBar';

type EventCallback = (event: { payload: unknown }) => void;
const listeners = new Map<string, EventCallback>();
const listenMock = vi.fn((name: string, callback: EventCallback) => {
  listeners.set(name, callback);
  return Promise.resolve(() => listeners.delete(name));
});

vi.mock('@tauri-apps/api/event', () => ({
  listen: (...args: unknown[]) => listenMock(...(args as [string, EventCallback])),
}));

const approveMock = vi.fn();
const statusMock = vi.fn();

vi.mock('@/api/tauri', () => ({
  api: {
    centralApproveWorkspace: (...args: unknown[]) => approveMock(...args),
    getCentralWorkStatus: () => statusMock(),
  },
}));

function fire(event: string, payload: unknown) {
  listeners.get(event)?.({ payload });
}

describe('CentralCardsBar', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    listeners.clear();
    approveMock.mockReset().mockResolvedValue({ approval_id: 'a1', workspace_id: '/ws/x', expires_at: 't' });
    statusMock.mockReset().mockResolvedValue([]);
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });
  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  function mount() {
    act(() => {
      root.render(<CentralCardsBar />);
    });
  }

  it('授权事件到达渲染卡片；点击授权调用票据命令并转已授权', async () => {
    mount();
    await act(async () => { await Promise.resolve(); });
    act(() => {
      fire('central-approval-required', {
        cwd: '/ws/proj',
        task: '发布模块 v2',
        work_id: 'work-ap',
      });
    });
    const card = container.querySelector('[data-testid="central-approval-card"]');
    expect(card).not.toBeNull();
    expect(card!.textContent).toContain('/ws/proj');
    expect(card!.textContent).toContain('发布模块 v2');

    const button = Array.from(card!.querySelectorAll('button')).find((btn) =>
      btn.textContent?.trim() === '授权',
    );
    vi.useFakeTimers();
    await act(async () => {
      button!.click();
    });
    expect(approveMock).toHaveBeenCalledWith('/ws/proj', '发布模块 v2', false);
    expect(
      container.querySelector('[data-testid="central-approval-card-approved"]'),
    ).not.toBeNull();
    expect(
      container.querySelector('[data-testid="central-approval-card-approved"]')!.textContent,
    ).toContain('正在重新派发');
    // 已授权卡片 3.5s 后自动收起（不再长期占据页面）
    await act(async () => {
      await Promise.resolve();
    });
    act(() => {
      vi.advanceTimersByTime(3600);
    });
    vi.useRealTimers();
    expect(container.querySelector('[data-testid="central-approval-card-approved"]')).toBeNull();
  });

  it('「授权并记住」传 persist=true', async () => {
    mount();
    await act(async () => { await Promise.resolve(); });
    act(() => {
      fire('central-approval-required', { cwd: '/ws/r', task: '任务 R', work_id: 'w-r' });
    });
    const card = container.querySelector('[data-testid="central-approval-card"]')!;
    const button = Array.from(card.querySelectorAll('button')).find((btn) =>
      btn.textContent?.trim() === '记住',
    );
    await act(async () => {
      button!.click();
    });
    expect(approveMock).toHaveBeenCalledWith('/ws/r', '任务 R', true);
  });

  it('同目录同任务去重（模型重试拒绝只保留一张）', async () => {
    mount();
    await act(async () => { await Promise.resolve(); });
    act(() => {
      fire('central-approval-required', { cwd: '/ws/d', task: '任务 D', work_id: 'w1' });
      fire('central-approval-required', { cwd: '/ws/d', task: '任务 D', work_id: 'w2' });
    });
    expect(container.querySelectorAll('[data-testid="central-approval-card"]').length).toBe(1);
  });

  it('决定卡片由 waiting_user 状态驱动；状态变化自动消失', async () => {
    statusMock.mockResolvedValue([
      { work_id: 'w-wait', session_id: 's1', task: '迁移数据库？', state: 'waiting_user' },
    ]);
    mount();
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    const decision = container.querySelector('[data-testid="central-decision-card"]');
    expect(decision).not.toBeNull();
    expect(decision!.textContent).toContain('迁移数据库？');

    // 答复后状态恢复：刷新无 waiting_user → 卡片消失
    statusMock.mockResolvedValue([
      { work_id: 'w-wait', session_id: 's1', task: '迁移数据库？', state: 'running' },
    ]);
    await act(async () => {
      fire('central-work-updated', undefined);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(container.querySelector('[data-testid="central-decision-card"]')).toBeNull();
  });
});
