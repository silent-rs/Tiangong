/**
 * 当前任务常驻横幅：存在活跃工作时显示最新一条（状态 + 任务 + 模型），
 * 无活跃不渲染；事件驱动刷新。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type React from 'react';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';

const listeners = new Map<string, (event: { payload: unknown }) => void>();
const listenMock = vi.fn((event: string, cb: (event: { payload: unknown }) => void) => {
  listeners.set(event, cb);
  return Promise.resolve(() => listeners.delete(event));
});
const statusMock = vi.fn();
vi.mock('@/api/host', () => ({
  listen: (...args: unknown[]) => listenMock(...(args as [string, never])),
}));
vi.mock('@/api/tauri', () => ({
  api: { getCentralWorkStatus: () => statusMock() },
}));

const { CentralActiveTaskBanner } = await import(
  '@/features/central/components/CentralActiveTaskBanner'
);
let container: HTMLDivElement | null = null;
let root: Root | null = null;
async function mount() {
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root!.render(<CentralActiveTaskBanner />);
    for (let i = 0; i < 6; i += 1) await Promise.resolve();
  });
}
function fire(event: string) {
  listeners.get(event)?.({ payload: undefined });
}

describe('CentralActiveTaskBanner', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    listeners.clear();
  });
  afterEach(() => {
    if (root) act(() => root!.unmount());
    root = null;
    container?.remove();
    container = null;
  });

  it('无活跃工作不渲染；执行中任务出现时显示状态+任务+模型', async () => {
    statusMock.mockResolvedValue([
      { work_id: 'w1', session_id: 's1', task: '已完成的历史', model: null, dispatched_at: '2026-11-16 10:00:00', state: 'completed' },
    ]);
    await mount();
    expect(container!.querySelector('[data-testid="central-active-task-banner"]')).toBeNull();
    statusMock.mockResolvedValue([
      { work_id: 'w2', session_id: 's2', task: '第三十四轮 review：请继续 review feature/central-refactor 分支', model: 'glm-5.3', dispatched_at: '2026-11-16 13:50:00', state: 'running' },
    ]);
    await act(async () => {
      fire('central-work-updated');
      for (let i = 0; i < 6; i += 1) await Promise.resolve();
    });
    const banner = container!.querySelector<HTMLButtonElement>(
      '[data-testid="central-active-task-banner"]',
    );
    expect(banner).not.toBeNull('活跃任务必须有常驻横幅');
    expect(banner.textContent).toContain('正在执行');
    expect(banner.textContent).toContain('第三十四轮 review');
    expect(banner.textContent).toContain('glm-5.3');
    expect(banner.querySelector('span.truncate')).not.toBeNull('任务文本一行截断');
  });

  it('等你决定状态高亮显示；全部终态后横幅消失', async () => {
    statusMock.mockResolvedValue([
      { work_id: 'w3', session_id: 's3', task: '迁移数据库？', model: null, dispatched_at: '2026-11-16 14:00:00', state: 'waiting_user' },
    ]);
    await mount();
    const banner = container!.querySelector('[data-testid="central-active-task-banner"]');
    expect(banner!.textContent).toContain('等你决定');
    statusMock.mockResolvedValue([
      { work_id: 'w3', session_id: 's3', task: '迁移数据库？', model: null, dispatched_at: '2026-11-16 14:00:00', state: 'completed' },
    ]);
    await act(async () => {
      fire('central-work-updated');
      for (let i = 0; i < 6; i += 1) await Promise.resolve();
    });
    expect(
      container!.querySelector('[data-testid="central-active-task-banner"]'),
    ).toBeNull('终态后横幅消失');
  });
});
