import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { AppTabCommand } from '@/components/TabsContainer';

const mocks = vi.hoisted(() => ({
  api: {
    pluginInstancesList: vi.fn(),
    pluginInstancesReconcile: vi.fn(() => Promise.resolve(0)),
    pluginInstancesDetach: vi.fn(() => Promise.resolve()),
    pluginInstanceClosed: vi.fn(() => Promise.resolve()),
    pluginInstanceReserve: vi.fn(() => Promise.resolve('01reserved0000000000000000')),
    setWebviewMountedTabs: vi.fn(() => Promise.resolve()),
    bridgeCall: vi.fn(() => Promise.resolve('{}')),
    onBridgeEvent: vi.fn(() => Promise.resolve(() => {})),
  },
  beforeClose: vi.fn(() => Promise.resolve()),
}));

vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>();
  return { ...actual, api: { ...actual.api, ...mocks.api } };
});
vi.mock('@/components/PluginAppTabContent', () => ({ PluginAppTabContent: () => null }));
vi.mock('@/components/PluginSandbox', () => ({ runPluginBeforeClose: mocks.beforeClose }));

const { TabsContainer } = await import('@/components/TabsContainer');
const { useStore } = await import('@/store/useStore');

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
if (!globalThis.CSS?.escape) {
  (globalThis as { CSS?: { escape: (value: string) => string } }).CSS = {
    escape: (value: string) => value.replace(/["\\]/g, '\\$&'),
  };
}
Element.prototype.scrollIntoView ??= () => {};

let container: HTMLDivElement | null = null;
let root: Root | null = null;

async function flush() {
  for (let index = 0; index < 30; index += 1) await Promise.resolve();
}

async function render(appCommand: AppTabCommand | null = null) {
  if (!container) {
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  }
  await act(async () => {
    root!.render(
      <TabsContainer
        initialTabKind="plugin"
        isVisible
        openRequestVersion={0}
        onClose={() => {}}
        onShowMatrix={() => {}}
        appCommand={appCommand}
      />,
    );
    await flush();
  });
}

afterEach(() => {
  if (root) act(() => root!.unmount());
  container?.remove();
  container = null;
  root = null;
  vi.clearAllMocks();
});

const terminalEntry = {
  plugin_id: 'terminal',
  contribution_id: 'terminal',
  title: '终端',
  sandbox: 'shadow' as const,
  instance_id: 'tty-a',
  url: '',
  page_title: '',
};

describe('插件实例生命周期（TabsContainer）', () => {
  it('会话挂载后按 listInstances 以同一编号恢复标签，并提交核查', async () => {
    mocks.api.pluginInstancesList.mockResolvedValue([terminalEntry]);
    useStore.setState({ activeSessionId: 'session-a' });
    await render();

    expect(mocks.api.pluginInstancesList).toHaveBeenCalledWith('session-a');
    expect(container!.querySelector('[data-tab-id="tty-a"]')).toBeTruthy();
    expect(mocks.api.pluginInstancesReconcile).toHaveBeenCalledWith('session-a', [
      { plugin_id: 'terminal', instance_id: 'tty-a' },
    ]);
  });

  it('关闭资源标签：先 beforeClose，移除标签后发出 instanceClosed', async () => {
    mocks.api.pluginInstancesList.mockResolvedValue([terminalEntry]);
    useStore.setState({ activeSessionId: 'session-b' });
    await render();
    await render({
      kind: 'plugin',
      action: 'close-plugin',
      version: 1,
      sessionId: 'session-b',
      app: {
        pluginId: 'terminal',
        contributionId: '',
        title: '',
        sandbox: 'shadow',
        multi: false,
        instanceId: 'tty-a',
      },
    });

    expect(mocks.beforeClose).toHaveBeenCalledWith('tty-a');
    expect(mocks.api.pluginInstanceClosed).toHaveBeenCalledWith('terminal', 'session-b', 'tty-a');
    expect(container!.querySelector('[data-tab-id="tty-a"]')).toBeNull();
  });

  it('切换会话：离开的会话交宿主隐藏，资源标签不触发 beforeClose 释放', async () => {
    mocks.api.pluginInstancesList.mockResolvedValue([terminalEntry]);
    useStore.setState({ activeSessionId: 'session-c' });
    await render();
    mocks.api.pluginInstancesList.mockResolvedValue([]);
    await act(async () => {
      useStore.setState({ activeSessionId: 'session-d' });
      await flush();
    });

    expect(mocks.api.pluginInstancesDetach).toHaveBeenCalledWith('session-c');
    expect(mocks.beforeClose).not.toHaveBeenCalled();
    expect(mocks.api.pluginInstanceClosed).not.toHaveBeenCalled();
  });

  it('新建插件实例使用宿主预留的 scru128 编号', async () => {
    mocks.api.pluginInstancesList.mockResolvedValue([]);
    useStore.setState({ activeSessionId: 'session-e' });
    await render();
    await render({
      kind: 'plugin',
      action: 'open-plugin',
      version: 2,
      sessionId: 'session-e',
      app: {
        pluginId: 'pomodoro',
        contributionId: 'timer',
        title: '番茄钟',
        sandbox: 'shadow',
        multi: true,
      },
    });

    expect(mocks.api.pluginInstanceReserve).toHaveBeenCalled();
    expect(container!.querySelector('[data-tab-id="01reserved0000000000000000"]')).toBeTruthy();
  });
});
