/**
 * 中枢通知桥测试（review 机检：通知桥 4 条具名用例）：
 * - info/warning/error 三级路由到对应 toast 方法
 * - 无 title 的事件忽略；卸载后取消订阅（不再触发）
 */
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

type Listener = (event: { payload: unknown }) => void;
let listeners: Listener[] = [];
const off = vi.fn();
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn((_name: string, cb: Listener) => {
    listeners.push(cb);
    return Promise.resolve(() => {
      off();
      const index = listeners.indexOf(cb);
      if (index >= 0) listeners.splice(index, 1);
    });
  }),
}));

const showInfo = vi.fn();
const showWarning = vi.fn();
const showError = vi.fn();
vi.mock('@/components/Toast', () => ({
  useToast: () => ({ showInfo, showWarning, showError }),
}));

const { CentralNotificationBridge } = await import(
  '@/components/central/CentralNotificationBridge'
);

function mount(): HTMLElement {
  const container = document.createElement('div');
  document.body.appendChild(container);
  let root!: Root;
  act(() => {
    root = createRoot(container);
    root.render(<CentralNotificationBridge />);
  });
  return container;
}

function emit(payload: unknown) {
  for (const listener of [...listeners]) {
    act(() => listener({ payload }));
  }
}

describe('CentralNotificationBridge', () => {
  let container: HTMLElement | undefined;

  beforeEach(() => {
    vi.clearAllMocks();
    listeners = [];
    container = undefined;
  });

  afterEach(() => {
    if (container) {
      container.remove();
      container = undefined;
    }
    listeners = [];
  });

  it('info 通知路由到 showInfo', () => {
    mount();
    emit({ title: '任务完成', message: '第十轮 review 已完成', level: 'info' });
    expect(showInfo).toHaveBeenCalledWith('任务完成', '第十轮 review 已完成');
    expect(showWarning).not.toHaveBeenCalled();
    expect(showError).not.toHaveBeenCalled();
  });

  it('warning/error 分别路由到对应级别', () => {
    mount();
    emit({ title: '注意', message: '需要决策', level: 'warning' });
    emit({ title: '失败', message: '模型不可用', level: 'error' });
    expect(showWarning).toHaveBeenCalledWith('注意', '需要决策');
    expect(showError).toHaveBeenCalledWith('失败', '模型不可用');
    expect(showInfo).not.toHaveBeenCalled();
  });

  it('缺 title 的事件忽略（不弹 toast）', () => {
    mount();
    emit({ message: '无标题', level: 'info' });
    emit(undefined);
    expect(showInfo).not.toHaveBeenCalled();
  });

  it('卸载后取消订阅（不再触发 toast）', async () => {
    // 真正 unmount（root.unmount）——仅 remove DOM 不会触发 React 清理
    const host = document.createElement('div');
    document.body.appendChild(host);
    let root!: ReturnType<typeof createRoot>;
    act(() => {
      root = createRoot(host);
      root.render(<CentralNotificationBridge />);
    });
    act(() => {
      root.unmount();
    });
    host.remove();
    // flush 微任务（组件的 listen().then 在微任务里执行取消订阅）
    await act(async () => {
      await Promise.resolve();
    });
    emit({ title: '卸载后', level: 'info' });
    expect(showInfo).not.toHaveBeenCalled();
    expect(off).toHaveBeenCalled();
  });

  it('未知 level 回退 info', () => {
    mount();
    emit({ title: '默认级别', level: 'critical' });
    expect(showInfo).toHaveBeenCalledWith('默认级别', undefined);
  });
});
