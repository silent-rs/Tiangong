/**
 * 普通模式冒烟测试：MainApp 以真实聊天组件（ChatPanel / MessageList /
 * MessageInput / 插件宿主，不经 LazyComponents mock）渲染，验证中枢模式
 * 改造没有破坏普通模式主区。宿主环境与中枢链路均以 api mock 提供。
 */
import { act, type ReactNode } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => {
  const appWindow = {
    scaleFactor: vi.fn(() => Promise.resolve(1)),
    innerSize: vi.fn(() => Promise.resolve({ width: 1200, height: 800 })),
    setSize: vi.fn(() => Promise.resolve()),
    onResized: vi.fn(() => Promise.resolve(() => {})),
  };
  const api = {
    getSessions: vi.fn(() => Promise.resolve([])),
    getReasoningEffort: vi.fn(() => Promise.resolve('medium')),
    getDefaultReasoningEffort: vi.fn(() => Promise.resolve('medium')),
    newSessionId: vi.fn(() => Promise.resolve('normal-fresh-1')),
    loadSession: vi.fn(() => Promise.reject(new Error('会话不存在'))),
    getInputCache: vi.fn(() => Promise.resolve(null)),
    setInputCache: vi.fn((_k: string, cache: unknown) => Promise.resolve(cache)),
    removeInputCache: vi.fn(() => Promise.resolve()),
    prewarmWorkspaceIndex: vi.fn(() => Promise.resolve()),
    setReasoningEffort: vi.fn(() => Promise.resolve()),
    sendMessage: vi.fn(() => Promise.resolve()),
    getWorkspaceDir: vi.fn(() => Promise.resolve('/workspace')),
    pluginOpenEntry: vi.fn(() => Promise.resolve('<div>stub-plugin-page</div>')),
    onStreamEvent: vi.fn(() => Promise.resolve(() => {})),
    listSlotContributions: vi.fn(() => Promise.resolve([])),
    browserHide: vi.fn(() => Promise.resolve()),
  };
  return { api, appWindow };
});

vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>();
  return { ...actual, api: { ...actual.api, ...mocks.api } };
});
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));
vi.mock('@tauri-apps/api/window', () => ({
  currentMonitor: vi.fn(() => Promise.resolve(null)),
  getCurrentWindow: vi.fn(() => mocks.appWindow),
  LogicalSize: class LogicalSize {
    constructor(public width: number, public height: number) {}
  },
}));
vi.mock('@/api/remote', () => ({
  isRemoteHost: () => !!(window as { __TIANGONG_REMOTE__?: unknown }).__TIANGONG_REMOTE__,
  getRemoteStatus: () => ({ kind: 'ready' }),
  onRemoteStatus: () => () => {},
  remoteInvoke: vi.fn(),
  remoteListen: vi.fn(),
}));
vi.mock('@/components/AppSidebar', () => ({
  AppSidebar: () => <aside data-testid="app-sidebar">app-sidebar</aside>,
}));
vi.mock('@/components/BackgroundPluginHost', () => ({ BackgroundPluginHost: () => null }));
vi.mock('@/components/DefaultPluginOnboarding', () => ({ DefaultPluginOnboarding: () => null }));
vi.mock('@/components/FirstRunModelSetup', () => ({ FirstRunModelSetup: () => null }));
vi.mock('@/components/TabsContainer', () => ({ TabsContainer: () => null }));
vi.mock('@/components/ExtensionMatrix', () => ({ ExtensionMatrix: () => null }));
vi.mock('@/components/RemoteStatusBanner', () => ({ RemoteStatusBanner: () => null }));
vi.mock('@/components/ui/sidebar', () => ({
  SidebarProvider: ({ children }: { children: ReactNode }) => <>{children}</>,
  useSidebar: () => ({ toggleSidebar: vi.fn(), open: false }),
}));
vi.mock('@/components/Toast', () => ({ useToast: () => ({ showWarning: vi.fn() }) }));
vi.mock('@/hooks/useUpdateCheck', () => ({ useUpdateCheck: () => undefined }));
vi.mock('@/utils/desktopNotification', () => ({
  ensureDesktopNotificationPermission: vi.fn(() => Promise.resolve()),
  notifyBackgroundSessionCompleted: vi.fn(() => Promise.resolve()),
}));
vi.mock('@tauri-apps/plugin-dialog', () => ({ open: vi.fn() }));
vi.mock('@tauri-apps/api/webview', () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: vi.fn(() => Promise.resolve(() => {})),
  }),
}));
// MessageList 虚拟化依赖
vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });

const { useStore } = await import('@/store/useStore');
const { CENTRAL_MODE_STORAGE_KEY } = await import('@/lib/centralModePreference');

let cleanup: (() => void) | undefined;

beforeEach(() => {
  delete (window as { __TIANGONG_WEB__?: unknown }).__TIANGONG_WEB__;
  delete (window as { __TIANGONG_REMOTE__?: unknown }).__TIANGONG_REMOTE__;
  localStorage.removeItem(CENTRAL_MODE_STORAGE_KEY);
});

afterEach(() => {
  cleanup?.();
  cleanup = undefined;
});

async function mountMainApp() {
  const { MainApp } = await import('@/pages/MainApp');
  const container = document.createElement('div');
  document.body.appendChild(container);
  let root!: Root;
  await act(async () => {
    root = createRoot(container);
    root.render(<MainApp />);
  });
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
  cleanup = () => {
    act(() => root.unmount());
    container.remove();
  };
}

describe('普通模式主区冒烟（真实聊天组件）', () => {
  it('默认普通模式：消息列表与输入区渲染，无崩溃', async () => {
    await mountMainApp();
    expect(useStore.getState().centralMode).toBe(false);
    // 侧栏在
    expect(document.querySelector('[data-testid="app-sidebar"]')).not.toBeNull();
    // 真实 MessageInput 的编辑器挂载（普通输入区存在）
    const editor = document.querySelector('textarea, [contenteditable="true"]');
    expect(editor).not.toBeNull();
    // 普通空态欢迎语（真实 MessageList）
    expect(document.body.textContent).toContain('欢迎使用天工');
  });
});
