/**
 * 中枢模式前端入口测试（设计稿 §4.1 / §16）。
 *
 * 关键约束（用户与设计稿共同要求）：
 * - 中枢模式仅在 desktop（Tauri 桌面窗口）宿主下可进入——浏览器配置页
 *   （web host）与远程 H5（remote host）不提供入口、快捷键不生效，
 *   store 状态被置位也不切换视图；
 * - 进入后状态持久化（localStorage），重启直接回到中枢模式；
 * - 中枢模式下顶栏标题显示「中枢」，隐藏新对话 / 搜索 / 插件管理 / 拓展区，
 *   退出复用顶栏进入按钮（toggle）。
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
    newSessionId: vi.fn(() => Promise.resolve('central-entry-1')),
    loadSession: vi.fn(() => Promise.reject(new Error('no session'))),
    getInputCache: vi.fn(() => Promise.resolve(null)),
    setInputCache: vi.fn(() => Promise.resolve()),
    removeInputCache: vi.fn(() => Promise.resolve()),
    prewarmWorkspaceIndex: vi.fn(() => Promise.resolve()),
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
// 顶栏渲染真实 StatusPanel：覆盖中枢模式标题 / 按钮隐藏 / 退出按钮
vi.mock('@/components/LazyComponents', async () => {
  const { StatusPanel } = await import('@/components/StatusPanel');
  return {
    LazyMessageInput: () => null,
    LazyMessageList: () => null,
    LazyStatusPanel: () => <StatusPanel onToggleExtension={() => {}} />,
  };
});
vi.mock('@/components/GlobalStatusPluginHost', () => ({
  GlobalStatusPluginHost: () => null,
}));
vi.mock('@/components/TabsContainer', () => ({ TabsContainer: () => null }));
vi.mock('@/components/ExtensionMatrix', () => ({ ExtensionMatrix: () => null }));
vi.mock('@/components/RemoteStatusBanner', () => ({ RemoteStatusBanner: () => null }));
vi.mock('@/components/ui/sidebar', () => ({
  SidebarProvider: ({ children }: { children: ReactNode }) => <>{children}</>,
  useSidebar: () => ({ toggleSidebar: vi.fn(), open: false }),
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
const {
  CENTRAL_MODE_STORAGE_KEY,
  CENTRAL_SESSION_STORAGE_KEY,
  readPersistedCentralMode,
} = await import('@/lib/centralModePreference');
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

/** 挂载 MainApp（真实布局分支 + 真实 CentralModeView + 真实 StatusPanel）。 */
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

function queryByTitle(title: string): HTMLElement | null {
  return document.querySelector<HTMLElement>(`[title="${title}"]`);
}

beforeEach(() => {
  setHost('desktop');
  localStorage.removeItem(CENTRAL_MODE_STORAGE_KEY);
  localStorage.removeItem(CENTRAL_SESSION_STORAGE_KEY);
  useStore.setState({
    centralMode: false,
    preCentralContext: null,
    activeSessionId: null,
    newConversationId: null,
    isNewConversation: true,
    messages: [],
    sessions: [],
  });
});

afterEach(() => {
  setHost('desktop');
});

describe('isDesktopHost 宿主判定', () => {
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

  afterEach(() => {
    cleanup?.();
    cleanup = undefined;
    vi.useRealTimers();
  });

  it('默认渲染普通模式：侧栏在，中枢视图不在', async () => {
    cleanup = await mountMainApp();
    expect(textIncludes('app-sidebar')).toBe(true);
    expect(document.querySelector('[data-testid="central-mode-view"]')).toBeNull();
  });

  it('开启 centralMode 后隐藏侧栏并进入中枢视图', async () => {
    cleanup = await mountMainApp();
    act(() => {
      useStore.getState().setCentralMode(true);
    });
    expect(textIncludes('app-sidebar')).toBe(false);
    // 中枢视图挂载（对话链路复用 ChatPanel，空态问候由 MessageList 呈现）
    expect(document.querySelector('[data-testid="central-mode-view"]')).not.toBeNull();
    // 进入即建立中枢对话绑定（新对话态，绑定 ID 写入 localStorage）
    await act(async () => {
      await Promise.resolve();
    });
    expect(useStore.getState().newConversationId).toBe('central-entry-1');
    expect(localStorage.getItem(CENTRAL_SESSION_STORAGE_KEY)).toBe('central-entry-1');
  });

  it('非 desktop 宿主下置位 centralMode 不切换视图（双保险）', async () => {
    setHost('web');
    cleanup = await mountMainApp();
    act(() => {
      useStore.getState().setCentralMode(true);
    });
    expect(textIncludes('app-sidebar')).toBe(true);
    expect(document.querySelector('[data-testid="central-mode-view"]')).toBeNull();
  });

  it('退出复用顶栏进入按钮（toggle）：点击后恢复普通模式', async () => {
    cleanup = await mountMainApp();
    act(() => {
      useStore.getState().setCentralMode(true);
    });
    expect(textIncludes('app-sidebar')).toBe(false);
    // 顶栏中枢按钮（进入后语义变为退出）
    const exitButton = document.querySelector<HTMLElement>('[aria-label="退出中枢模式"]');
    expect(exitButton).toBeTruthy();
    act(() => {
      exitButton?.click();
    });
    expect(useStore.getState().centralMode).toBe(false);
    expect(textIncludes('app-sidebar')).toBe(true);
  });
});

describe('中枢模式顶栏（StatusPanel）', () => {
  let cleanup: (() => void) | undefined;

  afterEach(() => {
    cleanup?.();
    cleanup = undefined;
    vi.useRealTimers();
  });

  it('中枢模式下标题显示「中枢」，普通模式显示会话标题', async () => {
    cleanup = await mountMainApp();
    expect(textIncludes('新对话')).toBe(true); // 普通模式默认标题
    act(() => {
      useStore.getState().setCentralMode(true);
    });
    expect(document.querySelector('[data-testid="central-title"]')).not.toBeNull();
    expect(textIncludes('中枢')).toBe(true);
    expect(textIncludes('新对话')).toBe(false);
  });

  it('中枢模式下隐藏新对话 / 搜索 / 插件管理 / 拓展区，普通模式均在', async () => {
    cleanup = await mountMainApp();
    // 普通模式全部可见
    expect(queryByTitle('新对话')).not.toBeNull();
    expect(queryByTitle('搜索消息')).not.toBeNull();
    expect(queryByTitle('插件管理')).not.toBeNull();
    expect(queryByTitle('打开拓展区')).not.toBeNull();

    act(() => {
      useStore.getState().setCentralMode(true);
    });
    expect(queryByTitle('新对话')).toBeNull();
    expect(queryByTitle('搜索消息')).toBeNull();
    expect(queryByTitle('插件管理')).toBeNull();
    expect(queryByTitle('打开拓展区')).toBeNull();
    // 中枢按钮本身保留（toggle 退出入口）
    expect(document.querySelector('[aria-label="退出中枢模式"]')).not.toBeNull();
  });
});

describe('中枢模式快捷键（Cmd/Ctrl+Shift+J）', () => {
  let cleanup: (() => void) | undefined;

  afterEach(() => {
    cleanup?.();
    cleanup = undefined;
  });

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

describe('中枢模式状态持久化', () => {
  it('setCentralMode 写入 localStorage，重启读取恢复', async () => {
    act(() => {
      useStore.getState().setCentralMode(true);
    });
    expect(localStorage.getItem(CENTRAL_MODE_STORAGE_KEY)).toBe('true');
    act(() => {
      useStore.getState().setCentralMode(false);
    });
    expect(localStorage.getItem(CENTRAL_MODE_STORAGE_KEY)).toBe('false');
  });

  it('readPersistedCentralMode：非法值与缺键回退 false', () => {
    expect(readPersistedCentralMode()).toBe(false);
    localStorage.setItem(CENTRAL_MODE_STORAGE_KEY, 'garbage');
    expect(readPersistedCentralMode()).toBe(false);
    localStorage.setItem(CENTRAL_MODE_STORAGE_KEY, 'true');
    expect(readPersistedCentralMode()).toBe(true);
    localStorage.removeItem(CENTRAL_MODE_STORAGE_KEY);
    expect(readPersistedCentralMode()).toBe(false);
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

// 重启直入：必须放最后（vi.resetModules 会影响后续动态 import 的模块图）
describe('重启直接进入中枢模式', () => {
  it('localStorage 预置 true 时，新 store 实例初始化即中枢模式', async () => {
    localStorage.setItem(CENTRAL_MODE_STORAGE_KEY, 'true');
    vi.resetModules();
    const { useStore: freshStore } = await import('@/store/useStore');
    expect(freshStore.getState().centralMode).toBe(true);

    localStorage.setItem(CENTRAL_MODE_STORAGE_KEY, 'false');
    vi.resetModules();
    const { useStore: normalStore } = await import('@/store/useStore');
    expect(normalStore.getState().centralMode).toBe(false);
  });
});

// 引用保持（避免未使用告警；hotkey 副作用经 MainApp 覆盖）
void useCentralModeHotkey;
