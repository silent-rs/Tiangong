/**
 * waiting_user 决策覆盖层（用户裁决保留协议 + 覆盖输入框防歧义）：
 * - 有 waiting_user 工作时覆盖层渲染（absolute bottom 覆盖输入区）
 * - 内容行两行截断、点击展开限高滚动（全文太长不利于决策）
 * - 卡片直答：提交 → central_answer_waiting → 状态 running → 覆盖层消失
 */
import { afterEach, describe, expect, it, vi, beforeEach } from 'vitest';
import type React from 'react';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';

const listeners = new Map<string, (event: { payload: unknown }) => void>();
const listenMock = vi.fn(
  (event: string, cb: (event: { payload: unknown }) => void) => {
    listeners.set(event, cb);
    return Promise.resolve(() => listeners.delete(event));
  },
);
const statusMock = vi.fn();
const answerMock = vi.fn(() => Promise.resolve({ work_id: '', state: 'running' }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: (...args: unknown[]) => listenMock(...(args as [string, never])),
}));
vi.mock('@/api/tauri', () => ({
  api: {
    getCentralWorkStatus: () => statusMock(),
    centralAnswerWaiting: (...args: unknown[]) =>
      answerMock(...(args as [string, string])),
  },
}));

const { CentralDecisionOverlay } = await import(
  '@/features/central/components/CentralDecisionOverlay'
);

let container: HTMLDivElement | null = null;
let root: Root | null = null;
async function mount() {
  container = document.createElement('div');
  // absolute 定位锚（模拟 CentralModeView 的 main relative）
  container.style.position = 'relative';
  container.style.height = '600px';
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root!.render(<CentralDecisionOverlay inputHeight={160} />);
    for (let i = 0; i < 6; i += 1) await Promise.resolve();
  });
}
function fire(event: string) {
  listeners.get(event)?.({ payload: undefined });
}

describe('CentralDecisionOverlay', () => {
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

  it('无 waiting 工作时不渲染；出现时覆盖层挂载（覆盖输入区）', async () => {
    statusMock.mockResolvedValue([
      { work_id: 'w1', session_id: 's1', task: '运行中', state: 'running' },
    ]);
    await mount();
    expect(container!.querySelector('[data-testid="central-decision-overlay"]')).toBeNull();
    statusMock.mockResolvedValue([
      { work_id: 'w2', session_id: 's2', task: '迁移数据库？', state: 'waiting_user' },
    ]);
    await act(async () => {
      fire('central-work-updated');
      for (let i = 0; i < 6; i += 1) await Promise.resolve();
    });
    const overlay = container!.querySelector('[data-testid="central-decision-overlay"]');
    expect(overlay).not.toBeNull('覆盖层应渲染');
    expect((overlay as HTMLElement).className).toContain('absolute');
    expect((overlay as HTMLElement).style.minHeight).toBe('160px', '至少覆盖输入框高度');
  });

  it('内容默认两行截断，点击展开；直答提交恢复后覆盖层消失', async () => {
    const longTask = '第三十四轮 review：' + '细节'.repeat(200);
    statusMock.mockResolvedValue([
      { work_id: 'w9', session_id: 's9', task: longTask, state: 'waiting_user' },
    ]);
    await mount();
    await act(async () => {
      for (let i = 0; i < 6; i += 1) await Promise.resolve();
    });
    const body = container!.querySelector<HTMLButtonElement>(
      '[data-testid="central-decision-card-body"]',
    )!;
    expect(body.className).toContain('line-clamp-2', '默认精炼两行');
    await act(async () => {
      body.click();
    });
    expect(body.className).toContain('max-h-[38vh]', '展开限高滚动');
    // 直答
    const input = container!.querySelector<HTMLInputElement>(
      '[data-testid="central-decision-card-input"]',
    )!;
    const submit = container!.querySelector<HTMLButtonElement>(
      '[data-testid="central-decision-card-submit"]',
    )!;
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
      setter.call(input, '同意，先备份');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await act(async () => {
      submit.click();
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(answerMock).toHaveBeenCalledWith('w9', '同意，先备份');
    statusMock.mockResolvedValue([
      { work_id: 'w9', session_id: 's9', task: longTask, state: 'running' },
    ]);
    await act(async () => {
      fire('central-work-updated');
      for (let i = 0; i < 6; i += 1) await Promise.resolve();
    });
    expect(
      container!.querySelector('[data-testid="central-decision-overlay"]'),
    ).toBeNull('状态 running 后覆盖层消失，普通输入框回归');
  });
});
