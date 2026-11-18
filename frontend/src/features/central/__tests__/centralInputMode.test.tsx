/**
 * 中枢复用 MessageInput 的行为锁（二十一轮 review 口径 11：替换
 * CentralChatInput 后验收面不得倒退——centralMode 形态必须可用断言）：
 * - workspace 目录区隐藏 / git 拓展槽不挂载 / 模型选择与用量区保留
 * - 非 centralMode 对照：目录区存在
 */
import { describe, expect, it, vi, beforeEach } from 'vitest';
import type React from 'react';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';

const inputStatusSlots: string[] = [];
vi.mock('@/api/tauri', () => {
  const api = {
    getSessionModel: vi.fn(() => Promise.resolve(null)),
    listSessionChatModels: vi.fn(() =>
      Promise.resolve({
        models: [{ key: 'k1', label: '模型一', provider: 'P' }],
        default_ref: 'k1',
      }),
    ),
    setSessionModel: vi.fn(() => Promise.resolve()),
    onModelsConfigChanged: vi.fn(() => Promise.resolve(() => {})),
    setSessionCwd: vi.fn(() => Promise.resolve()),
    cancelTurn: vi.fn(() => Promise.resolve(true)),
  };
  // MessageInput 触达面广（信任模式/提及/流事件等）：未列方法统一回退
  // resolved null——本测试只断言中枢形态的三件事
  const proxied = new Proxy(api, {
    get(target, prop: string) {
      return target[prop] ?? vi.fn(() => Promise.resolve(null));
    },
  });
  return { api: proxied };
});
// ResizeObserver（compact 宽度检测）：jsdom 未实现——最小桩
globalThis.ResizeObserver = class {
  observe() {}
  unobserve() {}
  disconnect() {}
} as unknown as typeof ResizeObserver;

vi.mock('@tauri-apps/api/webview', () => ({
  getCurrentWebview: vi.fn(() => ({
    onDragDropEvent: vi.fn(() => Promise.resolve(() => {})),
  })),
}));
vi.mock('@/components/SessionInputPluginHost', () => ({
  SessionInputPluginHost: ({ slot }: { slot: string }) => {
    inputStatusSlots.push(slot);
    return null;
  },
}));

const { useStore } = await import('@/store/useStore');
const { MessageInput } = await import('@/components/MessageInput');

function mount(props: React.ComponentProps<typeof MessageInput>) {
  const container = document.createElement('div');
  document.body.appendChild(container);
  let root!: Root;
  act(() => {
    root = createRoot(container);
    root.render(<MessageInput {...props} />);
  });
  return {
    container,
    unmount: () => {
      act(() => root.unmount());
      container.remove();
    },
  };
}

describe('MessageInput centralMode 形态（中枢复用）', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    inputStatusSlots.length = 0;
    useStore.setState({ ...useStore.getInitialState(), activeSessionId: 's-1' });
  });

  it('中枢：workspace 目录区不渲染', () => {
    const view = mount({ centralMode: true });
    expect(
      view.container.querySelector('[title*="对话目录"], [title*="设置对话目录"]'),
    ).toBeNull();
    view.unmount();
  });

  it('中枢：git 拓展槽（session.input-status）不挂载', () => {
    const view = mount({ centralMode: true });
    expect(inputStatusSlots).not.toContain('session.input-status');
    view.unmount();
  });

  it('中枢：模型选择保留（读会话引用 + 拉取注册表）', async () => {
    const { api } = await import('@/api/tauri');
    const view = mount({ centralMode: true });
    await vi.waitFor(() => {
      expect(api.listSessionChatModels).toHaveBeenCalled();
    });
    expect(api.getSessionModel).toHaveBeenCalledWith('s-1');
    view.unmount();
  });

  it('对照（非中枢）：目录区与拓展槽均在', () => {
    const view = mount({});
    expect(
      view.container.querySelector('[title*="对话目录"], [title*="设置对话目录"], button[title="新对话"]'),
    ).not.toBeNull();
    expect(inputStatusSlots).toContain('session.input-status');
    view.unmount();
  });
});
