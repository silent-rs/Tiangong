/**
 * 中枢模式前端入口测试（设计稿 §4.1 / §16）。
 *
 * 关键约束（用户与设计稿共同要求）：中枢模式仅在 desktop（Tauri 桌面窗口）
 * 宿主下可进入——浏览器配置页（web host）与远程 H5（remote host）不提供
 * 入口、快捷键不生效，即便 store 状态被置位也不切换视图。
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
    getWorkspaceDir: vi.fn(() => Promise.resolve('/workspace')),
    pluginOpenEntry: vi.fn(() => Promise.resolve('<div>stub-plugin-page</div>')),
    onStreamEvent: vi.fn(() => Promise.resolve(() => {})),
    browserHide: vi.fn(() => Promise.resolve()),
  };
  const listen = vi.fn(() => Promise.resolve(() => {}));
  return { api, appWindow, listen };
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
// remote 传输层：isRemoteHost 由 window.__TIANGONG_REMOTE__ 注入控制
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
vi.mock('@/components/BackgroundPluginHost', () => ({
  BackgroundPluginHost: () => null,
}));
vi.mock('@/components/DefaultPluginOnboarding', () => ({
  DefaultPluginOnboarding: () => null,
}));
vi.mock('@/components/FirstRunModelSetup', () => ({
  FirstRunModelSetup: () => null,
}));
vi.mock('@/components/InteractionPluginHost', () => ({
  InteractionPluginHost: () => null,
}));
vi.mock('@/components/InputOverlayPluginHost', () => ({
  InputOverlayPluginHost: () => null,
}));
vi.mock('@/components/LazyComponents', () => ({
  LazyMessageInput: () => null,
  LazyMessageList: () => null,
  LazyStatusPanel: () => null,
}));
vi.mock('@/components/TabsContainer', () => ({ TabsContainer: () => null }));
vi.mock('@/components/ExtensionMatrix', () => ({ ExtensionMatrix: () => null }));
vi.mock('@/components/RemoteStatusBanner', () => ({ RemoteStatusBanner: () => null }));
vi.mock('@/components/ui/sidebar', () => ({
  SidebarProvider: ({ children }: { children: ReactNode }) => <>{children}</>,
}));
vi.mock('@/components/Toast', () => ({
  useToast: () => ({ showWarning: vi.fn() }),
}));
vi.mock('@/hooks/useUpdateCheck', () => ({ useUpdateCheck: () => undefined }));
vi.mock('@/utils/desktopNotification', () => ({
  ensureDesktopNotificationPermission: vi.fn(() => Promise.resolve()),
}));

const { isDesktopHost } = await import('@/api/host');
const { useCentralMode, useCentralModeHotkey } = await import('@/hooks/useCentralMode');
const { useStore } = await import('@/store/useStore');

/** 宿主形态注入：desktop（默认）/ web 配置页 / 远程 H5。 */
function setHost(kind: 'desktop' | 'web' | 'remote') {
  const w = window as unknown as {
    __TIANGONG_WEB__?: unknown;
    __TIANGONG_REMOTE__?: unknown;
  };
  delete w.__TIANGONG_WEB__;
  delete w.__TIANGONG_REMOTE__;
  if (kind === 'web') w.__TIANGONG_WEB__ = {};
  if (kind === 'remote') w.__TIANGONG_REMOTE__ = {};
}

function pressCentralHotkey() {
  window.dispatchEvent(
    new KeyboardEvent('keydown', { key: 'j', shiftKey: true, metaKey: true, bubbles: true }),
  );
}

/** 挂载 MainApp（真实布局分支 + 真实 CentralModeView），返回清理函数。 */
async function mountMainApp(): Promise<() => void> {
  const { MainApp } = await import('@/pages/MainApp');
  const container = document.createElement('div');
  document.body.appendChild(container);
  let root!: Root;
  await act(async () => {
    root = createRoot(container);
    root.render(<MainApp />);
  });
  return () => {
    act(() => root.unmount());
    container.remove();
  };
}

function textIncludes(text: string): boolean {
  return document.body.textContent?.includes(text) ?? false;
}

describe('isDesktopHost 宿主判定', () => {
  afterEach(() => setHost('desktop'));

  it('desktop（无注入）为 true', () => {
    setHost('desktop');
    expect(isDesktopHost()).toBe(true);
  });

  it('浏览器配置页（web host）为 false', () => {
    setHost('web');
    expect(isDesktopHost()).toBe(false);
  });

  it('远程 H5（remote host）为 false', () => {
    setHost('remote');
    expect(isDesktopHost()).toBe(false);
  });
});

describe('中枢模式布局分支', () => {
  let cleanup: (() => void) | undefined;

  beforeEach(() => {
    setHost('desktop');
    useStore.setState({ centralMode: false });
  });

  afterEach(() => {
    cleanup?.();
    cleanup = undefined;
  });

  it('默认渲染普通模式：侧栏在，中枢视图不在', async () => {
    cleanup = await mountMainApp();
    expect(textIncludes('app-sidebar')).toBe(true);
    expect(textIncludes('天工中枢')).toBe(false);
  });

  it('开启 centralMode 后隐藏侧栏并进入中枢视图', async () => {
    cleanup = await mountMainApp();
    act(() => {
      useStore.getState().setCentralMode(true);
    });
    expect(textIncludes('app-sidebar')).toBe(false);
    expect(textIncludes('天工中枢')).toBe(true);
    // 无上下文默认问候（设计稿 §4.2 原文）
    expect(textIncludes('我是天工中枢')).toBe(true);
  });

  it('非 desktop 宿主下置位 centralMode 不切换视图（双保险）', async () => {
    setHost('web');
    cleanup = await mountMainApp();
    act(() => {
      useStore.getState().setCentralMode(true);
    });
    expect(textIncludes('app-sidebar')).toBe(true);
    expect(textIncludes('天工中枢')).toBe(false);
  });

  it('中枢视图点击退出按钮恢复普通模式', async () => {
    cleanup = await mountMainApp();
    act(() => {
      useStore.getState().setCentralMode(true);
    });
    const exitButton = Array.from(document.querySelectorAll('button')).find((button) =>
      button.textContent?.includes('退出中枢模式'),
    );
    expect(exitButton).toBeTruthy();
    act(() => {
      exitButton?.click();
    });
    expect(useStore.getState().centralMode).toBe(false);
    expect(textIncludes('app-sidebar')).toBe(true);
  });
});

describe('中枢模式快捷键（Cmd/Ctrl+Shift+J）', () => {
  let cleanup: (() => void) | undefined;

  beforeEach(() => {
    setHost('desktop');
    useStore.setState({ centralMode: false });
  });

  afterEach(() => {
    cleanup?.();
    cleanup = undefined;
    setHost('desktop');
  });

  /** 快捷键注册在 MainApp（useCentralModeHotkey），渲染 MainApp 生效。 */
  it('desktop 下切换开关，重复按键切回', async () => {
    cleanup = await mountMainApp();
    act(() => pressCentralHotkey());
    expect(useStore.getState().centralMode).toBe(true);
    act(() => pressCentralHotkey());
    expect(useStore.getState().centralMode).toBe(false);
  });

  it('非 desktop 宿主快捷键不生效', async () => {
    setHost('web');
    cleanup = await mountMainApp();
    act(() => pressCentralHotkey());
    expect(useStore.getState().centralMode).toBe(false);
  });
});

describe('useCentralMode hook 行为', () => {
  let cleanup: (() => void) | undefined;

  /** 挂载探针组件获取 hook 返回值（hooks 不能在组件外直接调用）。 */
  function mountProbe() {
    let hookApi: ReturnType<typeof useCentralMode> | undefined;
    function HookProbe() {
      hookApi = useCentralMode();
      return null;
    }
    const container = document.createElement('div');
    document.body.appendChild(container);
    const root = createRoot(container);
    act(() => root.render(<HookProbe />));
    expect(hookApi).toBeTruthy();
    const api = hookApi!;
    cleanup = () => {
      act(() => root.unmount());
      container.remove();
    };
    return api;
  }

  beforeEach(() => {
    setHost('desktop');
    useStore.setState({ centralMode: false });
  });

  afterEach(() => {
    cleanup?.();
    cleanup = undefined;
  });

  it('toggleCentral 在 desktop 下切换；web 宿主下 no-op', () => {
    const desktop = mountProbe();
    desktop.toggleCentral();
    expect(useStore.getState().centralMode).toBe(true);

    cleanup?.();
    setHost('web');
    const web = mountProbe();
    expect(web.canUseCentral).toBe(false);
    expect(web.centralActive).toBe(false);
    web.toggleCentral();
    expect(useStore.getState().centralMode).toBe(true); // 未被切回 false
  });

  it('exitCentral 无条件退出（任何宿主都允许退出）', () => {
    useStore.setState({ centralMode: true });
    setHost('web');
    const web = mountProbe();
    web.exitCentral();
    expect(useStore.getState().centralMode).toBe(false);
  });
});

// 引用保持（避免未使用告警；hotkey 副作用经 MainApp 覆盖）
void useCentralModeHotkey;
