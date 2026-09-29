import { act, type ReactNode } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { Session } from '@/api/tauri';

const mocks = vi.hoisted(() => {
  const appWindow = {
    scaleFactor: vi.fn(() => Promise.resolve(1)),
    innerSize: vi.fn(() => Promise.resolve({ width: 1200, height: 800 })),
    setSize: vi.fn(() => Promise.resolve()),
    onResized: vi.fn(() => Promise.resolve(() => {})),
  };
  const api = {
    getSessions: vi.fn(() => Promise.resolve([] as Session[])),
    getReasoningEffort: vi.fn(() => Promise.resolve('medium')),
    getWorkspaceDir: vi.fn(() => Promise.resolve('/workspace')),
    browserHide: vi.fn(() => Promise.resolve()),
    browserSwitchSession: vi.fn(() => Promise.resolve({ session_id: null, tabs: [], active_tab_id: null })),
    listExtensionApps: vi.fn(() => Promise.resolve([
      {
        plugin_id: '__builtin__',
        contribution_id: 'browser',
        official: true,
        name: '浏览器',
        title: '浏览器',
        description: '嵌入式浏览器',
        icon: 'globe',
        open_mode: 'singleton',
        sandbox: 'native',
      },
      {
        plugin_id: '__builtin__',
        contribution_id: 'terminal',
        official: true,
        name: '终端',
        title: '终端',
        description: '嵌入式终端',
        icon: 'terminal',
        open_mode: 'multi',
        sandbox: 'native',
      },
    ])),
    onStreamEvent: vi.fn(() => Promise.resolve(() => {})),
  };
  const listen = vi.fn(() => Promise.resolve(() => {}));
  return {
    api,
    appWindow,
    listen,
  };
});

vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>();
  return { ...actual, api: { ...actual.api, ...mocks.api } };
});

vi.mock('@tauri-apps/api/event', () => ({ listen: mocks.listen }));
vi.mock('@tauri-apps/api/window', () => ({
  currentMonitor: vi.fn(() => Promise.resolve(null)),
  getCurrentWindow: vi.fn(() => mocks.appWindow),
  LogicalSize: class LogicalSize {
    constructor(public width: number, public height: number) {}
  },
}));
vi.mock('@/components/AppSidebar', () => ({ AppSidebar: () => null }));
vi.mock('@/components/DefaultPluginOnboarding', () => ({
  DefaultPluginOnboarding: () => null,
}));
vi.mock('@/components/InteractionPluginHost', () => ({
  InteractionPluginHost: () => null,
}));
// StatusPanel 桩：渲染拓展按钮转发 onToggleExtension（不依赖真实图标布局）
vi.mock('@/components/LazyComponents', () => ({
  LazyMessageInput: () => null,
  LazyMessageList: () => null,
  LazyStatusPanel: ({ onToggleExtension }: { onToggleExtension?: () => void }) => (
    <button data-testid="extension-toggle" onClick={onToggleExtension}>拓展区</button>
  ),
}));
// TabsContainer 桩：暴露 mode/kind、内嵌矩阵内容与启动台按钮
vi.mock('@/components/TabsContainer', () => ({
  TabsContainer: ({
    initialTabKind,
    mode,
    matrix,
    onShowMatrix,
  }: {
    initialTabKind: string;
    mode?: string;
    matrix?: ReactNode;
    onShowMatrix?: () => void;
  }) => (
    <div data-testid="tabs-container" data-kind={initialTabKind} data-mode={mode ?? 'app'}>
      {mode === 'matrix' && matrix}
      <button data-testid="show-matrix" onClick={onShowMatrix}>启动台</button>
    </div>
  ),
}));
vi.mock('@/components/ui/sidebar', () => ({
  SidebarProvider: ({ children }: { children: ReactNode }) => children,
}));
// Toast 桩：MainApp 直渲染无 ToastProvider，仅提供窄窗口提示所需的方法
vi.mock('@/components/Toast', () => ({
  useToast: () => ({ showWarning: vi.fn() }),
}));
vi.mock('@/hooks/useUpdateCheck', () => ({ useUpdateCheck: () => undefined }));
vi.mock('@/utils/desktopNotification', () => ({
  ensureDesktopNotificationPermission: vi.fn(() => Promise.resolve()),
}));

const { MainApp } = await import('@/pages/MainApp');
const { useStore } = await import('@/store/useStore');

let container: HTMLDivElement | null = null;
let root: Root | null = null;

async function flushMicrotasks() {
  for (let index = 0; index < 30; index += 1) {
    await Promise.resolve();
  }
}

async function renderMainApp() {
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
  root.render(<MainApp />);
  await act(async () => {});
  await act(async () => {});
}

afterEach(() => {
  if (root) act(() => root!.unmount());
  container?.remove();
  container = null;
  root = null;
  vi.clearAllMocks();
});

const click = async (selector: string) => {
  const element = container!.querySelector<HTMLButtonElement>(selector);
  expect(element, `应存在 ${selector}`).toBeTruthy();
  await act(async () => {
    element!.click();
    await flushMicrotasks();
  });
};

describe('拓展区三态状态机（T008）', () => {
  it('无已打开 tab：点击拓展按钮进入矩阵态，可从矩阵打开官方 App', async () => {
    await renderMainApp();

    expect(container!.querySelector('[data-testid="tabs-container"]')).toBeNull();
    await click('[data-testid="extension-toggle"]');

    // 矩阵态：tab 栏容器保留，矩阵作为内容区渲染（含官方卡片）
    const tabsHost = container!.querySelector('[data-testid="tabs-container"]');
    expect(tabsHost, '矩阵态 tab 栏容器应保留').toBeTruthy();
    expect(tabsHost!.getAttribute('data-mode')).toBe('matrix');
    expect(tabsHost!.textContent).toContain('浏览器');
    expect(tabsHost!.textContent).toContain('终端');

    // 从矩阵打开终端 → App 态
    const terminalCard = [...container!.querySelectorAll('button')]
      .find((button) => button.textContent?.trim() === '终端');
    expect(terminalCard).toBeTruthy();
    await act(async () => {
      terminalCard!.click();
      await flushMicrotasks();
    });

    const tabs = container!.querySelector('[data-testid="tabs-container"]');
    expect(tabs, '应进入 App 态渲染 TabsContainer').toBeTruthy();
    expect(tabs!.getAttribute('data-mode')).toBe('app');
    // 终端已迁移为插件贡献（terminal），打开的 tab kind 为 plugin
    expect(tabs!.getAttribute('data-kind')).toBe('plugin');
  });

  it('历史会话不恢复拓展区 tab，打开后直接进入矩阵态并可收起', async () => {
    useStore.setState({ activeSessionId: 'session-1' });
    await renderMainApp();

    // 即使是历史会话，拓展区也不读取旧 tab，直接进入矩阵态。
    await click('[data-testid="extension-toggle"]');
    const tabsHost = container!.querySelector('[data-testid="tabs-container"]')!;
    expect(tabsHost.getAttribute('data-mode')).toBe('matrix');
    expect(tabsHost.textContent).toContain('浏览器');

    // 矩阵态再点拓展按钮 → 收起（关闭态）。
    await click('[data-testid="extension-toggle"]');
    await act(async () => {
      await flushMicrotasks();
    });
    const panelHost = container!.querySelector('[data-testid="tabs-container"]')?.parentElement;
    expect(panelHost?.className).toContain('hidden');
  });
});
