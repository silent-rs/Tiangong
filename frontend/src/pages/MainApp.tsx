import { useEffect, useRef, useState, useCallback } from 'react';
import { useStore } from '@/store/useStore';
import {
  api,
  type AvailablePlugin,
  type SandboxKind,
  type SessionStreamEvent,
  type TabKind,
} from '@/api/tauri';
import { AppSidebar } from '@/components/AppSidebar';
import { BackgroundPluginHost, type BackgroundPluginInstance } from '@/components/BackgroundPluginHost';
import { DefaultPluginOnboarding } from '@/components/DefaultPluginOnboarding';
import { FirstRunModelSetup } from '@/components/FirstRunModelSetup';
import { SidebarProvider } from '@/components/ui/sidebar';
import { LazyMessageList, LazyMessageInput, LazyStatusPanel } from '@/components/LazyComponents';
import {
  TabsContainer,
  type AppTabCommand,
} from '@/components/TabsContainer';
import { ExtensionMatrix } from '@/components/ExtensionMatrix';
import { InteractionPluginHost } from '@/components/InteractionPluginHost';
import { InputOverlayPluginHost } from '@/components/InputOverlayPluginHost';
import { useToast } from '@/components/Toast';
import { ensureDesktopNotificationPermission } from '@/utils/desktopNotification';
import { useUpdateCheck } from '@/hooks/useUpdateCheck';
import type { UnlistenFn } from '@tauri-apps/api/event';
import { getCurrentWindow, LogicalSize, currentMonitor } from '@tauri-apps/api/window';

/** 拓展区面板最小宽度，低于此值自动关闭 */
const MIN_BROWSER_WIDTH = 200;

/** 对话面板最小宽度 */
const MIN_CHAT_WIDTH = 400;

/** 侧边栏自动隐藏/恢复阈值：窄于此窗口时侧边栏转为浮层展示 */
const SIDEBAR_RESTORE_THRESHOLD = 656;

/** 屏幕工作区四周保留的边距，避免初始窗口贴边 */
const SCREEN_EDGE_MARGIN = 32;

/** 启动时裁剪窗口的最小高度兜底 */
const MIN_INITIAL_HEIGHT = 480;

/** 启动时按当前屏幕工作区裁剪窗口：仅当默认/上次的窗口尺寸超出屏幕可视区时才缩小 */
async function fitWindowToScreen(
  lock: () => void,
  unlock: () => void,
  onNarrow?: (logicalW: number) => void,
): Promise<void> {
  try {
    const appWindow = getCurrentWindow();
    const [monitor, scaleFactor, innerSize] = await Promise.all([
      currentMonitor(),
      appWindow.scaleFactor(),
      appWindow.innerSize(),
    ]);
    if (!monitor) return;

    const monitorW = monitor.size.width / scaleFactor;
    const monitorH = monitor.size.height / scaleFactor;
    const maxW = Math.max(MIN_CHAT_WIDTH, monitorW - SCREEN_EDGE_MARGIN * 2);
    const maxH = Math.max(MIN_INITIAL_HEIGHT, monitorH - SCREEN_EDGE_MARGIN * 2);

    let logicalW = innerSize.width / scaleFactor;
    let logicalH = innerSize.height / scaleFactor;
    let changed = false;
    if (logicalW > maxW) { logicalW = maxW; changed = true; }
    if (logicalH > maxH) { logicalH = maxH; changed = true; }
    if (!changed) return;

    lock();
    await appWindow.setSize(new LogicalSize(logicalW, logicalH));
    unlock();
    onNarrow?.(logicalW);
  } catch (error) {
    console.warn('适配窗口到屏幕失败:', error);
  }
}

function browserPluginSessionId(sessionId?: string | null): string {
  if (!sessionId) return '';
  const prefix = 'webview:browser:';
  return sessionId.startsWith(prefix) ? sessionId.slice(prefix.length) : sessionId;
}

export function MainApp() {
  const { applyStreamEvents, loadSessions, updateSessionMeta } = useStore();
  const { showWarning } = useToast();
  const activeSessionId = useStore((state) => state.activeSessionId);
  const newConversationId = useStore((state) => state.newConversationId);
  const currentSessionId = activeSessionId ?? newConversationId;
  useUpdateCheck();
  const [workspacePanelMounted, setWorkspacePanelMounted] = useState(false);
  const [interactionVisible, setInteractionVisible] = useState(false);
  const [inputOverlayVisible, setInputOverlayVisible] = useState(false);
  const [messageInputHeight, setMessageInputHeight] = useState(0);
  const [showWorkspacePanel, setShowWorkspacePanel] = useState(false);
  const [workspaceTabKind, setWorkspaceTabKind] = useState<TabKind>('browser');
  // 拓展区三态（设计文档 6.7.2）：面板展开时区分矩阵态（App 矩阵）与 App 态
  // （聚焦某类 App 实例）；关闭态由 showWorkspacePanel=false 表达。
  const [workspaceMode, setWorkspaceMode] = useState<'app' | 'matrix'>('matrix');
  // 矩阵右键菜单下发的 App 实例命令（新建实例/关闭全部），version 递增触发。
  const [appTabCommand, setAppTabCommand] = useState<AppTabCommand | null>(null);
  // 工具接应的后台插件实例（app.open mode=background）：隐藏挂载不弹面板。
  const [bgPluginInstances, setBgPluginInstances] = useState<BackgroundPluginInstance[]>([]);
  // 当前会话已接入顶部标签的 App 键集合：绿点唯一事实源（工具拉起的
  // 实例同样建立可见标签，无隐藏实例，关闭全部标签即绿点熄灭）。
  const [foregroundPluginApps, setForegroundPluginApps] = useState<string[]>([]);
  const runningPluginApps = foregroundPluginApps;
  const [workspaceOpenRequestVersion, setWorkspaceOpenRequestVersion] = useState(0);
  const [chatPanelWidth, setChatPanelWidth] = useState(MIN_CHAT_WIDTH);
  const [sidebarOpen, setSidebarOpen] = useState(true);
  // 首次启动推荐安装的缺失默认插件列表；为 null 时不显示引导对话框。
  const [onboardingMissing, setOnboardingMissing] = useState<AvailablePlugin[] | null>(null);
  // 未配置主对话模型时弹出快速配置弹窗。
  const [modelSetupOpen, setModelSetupOpen] = useState(false);
  const showWorkspacePanelRef = useRef(false);
  const workspaceTabKindRef = useRef<TabKind>('browser');
  const workspaceOpenRequestIdRef = useRef(0);
  const chatPanelWidthRef = useRef(MIN_CHAT_WIDTH);
  const isDraggingRef = useRef(false);
  const streamEventQueueRef = useRef<SessionStreamEvent[]>([]);
  const streamEventTimerRef = useRef<number | null>(null);
  // sessions_updated 在一次 done 后会被连续 emit 多次（done/标题生成/Core 退役），
  // 用尾沿去抖合并刷新，并在刷新期间收到新事件时串行补跑一次。
  const sessionsRefreshTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const sessionsRefreshInFlightRef = useRef(false);
  const sessionsRefreshDirtyRef = useRef(false);
  const preferredSidebarOpenRef = useRef(true);
  const programmaticResizeRef = useRef(false);
  const resizeLockTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    setForegroundPluginApps([]);
    setWorkspaceMode('matrix');
  }, [currentSessionId]);

  const lockResize = useCallback(() => {
    programmaticResizeRef.current = true;
    if (resizeLockTimerRef.current) clearTimeout(resizeLockTimerRef.current);
  }, []);

  const unlockResize = useCallback(() => {
    resizeLockTimerRef.current = setTimeout(() => {
      programmaticResizeRef.current = false;
    }, 200);
  }, []);

  const setSidebarOpenByLayout = useCallback((open: boolean) => {
    setSidebarOpen((current) => current === open ? current : open);
  }, []);

  const handleSidebarChange = useCallback(async (open: boolean) => {
    preferredSidebarOpenRef.current = open;
    setSidebarOpen(open);
  }, []);

  /// 展开拓展区面板：窗口宽度保持不变，在现有宽度内压缩聊天栏为拓展区腾出
  /// 空间。窗口窄到放不下「聊天栏 + 拓展区」两个最小宽度时禁止展开（返回
  /// false），仅隐藏挂载保活，插件 App 实例照常使用。矩阵态与 App 态共用；
  /// 面板已展开时不改变现有分栏宽度。
  const ensureWorkspacePanelExpanded = useCallback((): boolean => {
    if (window.innerWidth < MIN_CHAT_WIDTH + MIN_BROWSER_WIDTH) {
      setWorkspacePanelMounted(true);
      return false;
    }
    const wasOpen = showWorkspacePanelRef.current;
    showWorkspacePanelRef.current = true;
    setSidebarOpenByLayout(false);
    if (!wasOpen) {
      // 首次展开：按当前窗口宽度收敛聊天栏（侧边栏已收，可用宽度即窗口宽度），
      // 为拓展区保留最小可用宽度
      const clamped = Math.min(chatPanelWidthRef.current, window.innerWidth - MIN_BROWSER_WIDTH);
      chatPanelWidthRef.current = clamped;
      setChatPanelWidth(clamped);
    }
    setWorkspacePanelMounted(true);
    setShowWorkspacePanel(true);
    return true;
  }, [setSidebarOpenByLayout]);

  const openWorkspacePanel = useCallback(async (kind: TabKind) => {
    const requestId = workspaceOpenRequestIdRef.current + 1;
    workspaceOpenRequestIdRef.current = requestId;
    workspaceTabKindRef.current = kind;
    setWorkspaceTabKind(kind);
    setWorkspaceMode('app');
    setWorkspaceOpenRequestVersion((version) => version + 1);
    // 窗口过窄时 ensure 返回 false：标签照常建立、实例隐藏挂载保活，仅不展开面板
    ensureWorkspacePanelExpanded();
  }, [ensureWorkspacePanelExpanded]);

  const closeWorkspacePanel = useCallback(async () => {
    if (!showWorkspacePanelRef.current) return;
    workspaceOpenRequestIdRef.current += 1;
    showWorkspacePanelRef.current = false;
    setShowWorkspacePanel(false);
    // 窗口宽度足够时按用户偏好恢复侧边栏；宽度不足时保持收起，避免挤压主内容。
    if (window.innerWidth > SIDEBAR_RESTORE_THRESHOLD && preferredSidebarOpenRef.current) {
      setSidebarOpenByLayout(true);
    }
    // 面板关闭后 tab 仍保存在 TabsContainer 内存中，仅隐藏。
  }, [setSidebarOpenByLayout]);

  /// 拓展区按钮（三态切换，设计文档 6.7.2）：
  /// 面板展开 → 收起；面板收起且有已打开 tab → 回到上次 App 态；否则进入矩阵态。
  const handleToggleExtension = useCallback(() => {
    if (showWorkspacePanelRef.current) {
      void closeWorkspacePanel();
      return;
    }
    // 窗口过窄：禁止展开面板（窗口宽度与布局均保持不变），已打开的 App
    // 实例仍在后台保活可用，拉宽窗口后即可正常展开。
    if (window.innerWidth < MIN_CHAT_WIDTH + MIN_BROWSER_WIDTH) {
      showWarning(
        '窗口宽度不足，无法展开拓展区',
        `至少需要 ${MIN_CHAT_WIDTH + MIN_BROWSER_WIDTH}px。已打开的应用仍在后台保持可用。`,
      );
      return;
    }
    if (runningPluginApps.length > 0) {
      void openWorkspacePanel(workspaceTabKindRef.current);
      return;
    }
    setWorkspaceMode('matrix');
    ensureWorkspacePanelExpanded();
  }, [
    closeWorkspacePanel,
    ensureWorkspacePanelExpanded,
    openWorkspacePanel,
    runningPluginApps.length,
    showWarning,
  ]);

  /// 启动台按钮：App 态切回矩阵态（面板保持展开，App 实例隐藏保活）。
  /// 矩阵态下插件 webview 实例的隐藏由 TabsContainer 的 mode effect 处理。
  const handleShowMatrix = useCallback(() => {
    setWorkspaceMode('matrix');
  }, []);

  /// 矩阵态下点击顶部标签：直接切回 App 态聚焦被点的实例。
  const handleRequestAppMode = useCallback(() => {
    setWorkspaceMode('app');
  }, []);

  const handleWorkspaceActiveKindChange = useCallback((kind: TabKind | null) => {
    if (!kind) return;
    workspaceTabKindRef.current = kind;
    setWorkspaceTabKind(kind);
  }, []);

  const handleDividerDrag = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    isDraggingRef.current = true;
    const mainEl = document.querySelector('main');
    if (!mainEl) return;

    const cleanup = () => {
      isDraggingRef.current = false;
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
    };

    const onUp = () => { cleanup(); };

    const onMove = (ev: MouseEvent) => {
      if (!isDraggingRef.current) return;
      const rect = mainEl.getBoundingClientRect();
      const next = ev.clientX - rect.left;
      // 拖到右侧剩余宽度小于面板最小宽度时，关闭工作区面板。
      if (rect.width - next < MIN_BROWSER_WIDTH) {
        cleanup();
        void closeWorkspacePanel();
        return;
      }
      const clamped = Math.max(MIN_CHAT_WIDTH, next);
      setChatPanelWidth(clamped);
      chatPanelWidthRef.current = clamped;
    };

    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  }, [closeWorkspacePanel]);

  useEffect(() => {
    ensureDesktopNotificationPermission().catch(console.warn);

    // 启动时按当前屏幕工作区裁剪初始窗口，避免低分辨率屏幕上窗口超出可视区
    fitWindowToScreen(lockResize, unlockResize, (logicalW) => {
      if (logicalW <= SIDEBAR_RESTORE_THRESHOLD) {
        setSidebarOpenByLayout(false);
      }
    }).catch(console.warn);

    loadSessions();

    // 首次启动时检测缺失的默认插件，有缺失则弹出推荐安装引导。
    // 独立 catch，网络异常或检测失败都不影响主初始化流程。
    api.checkDefaultPlugins()
      .then((check) => {
        if (check.first_launch_pending && check.missing.length > 0) {
          setOnboardingMissing(check.missing);
        }
      })
      .catch((error) => console.warn('默认插件检测失败', error));

    // 未配置主对话模型（chat 路由）时在主页面弹出快速配置引导；
    // 独立 catch，检测失败不影响主初始化流程。
    api.getModelsConfig()
      .then((cfg) => {
        if (!cfg.routing.chat) setModelSetupOpen(true);
      })
      .catch((error) => console.warn('模型配置检测失败', error));

    const flushStreamEvents = () => {
      streamEventTimerRef.current = null;
      const events = streamEventQueueRef.current;
      streamEventQueueRef.current = [];
      applyStreamEvents(events);
    };

    const scheduleStreamEvent = (event: SessionStreamEvent) => {
      streamEventQueueRef.current.push(event);
      if (streamEventTimerRef.current !== null) return;
      streamEventTimerRef.current = window.setTimeout(flushStreamEvents, 16);
    };

    // 本轮 effect 的生命周期标记与取消函数集合。
    // 关键：不使用跨 effect 的单值引用（会被旧异步流程覆盖，导致第一轮监听器成为孤儿）。
    // 每个 await 注册完成后立即交给 track 收纳；guard 在每个 await 点之后检查 disposed，
    // 已清理则立即取消并中止后续注册，避免 StrictMode 挂载→清理→再挂载时第一轮监听器残留。
    let disposed = false;
    const cleanups: UnlistenFn[] = [];
    const track = (un: UnlistenFn) => {
      if (disposed) {
        un();
        return;
      }
      cleanups.push(un);
    };
    const guard = () => {
      if (disposed) throw new DOMException('Aborted', 'AbortError');
    };

    const setupListener = async () => {
      try {
        track(await api.onStreamEvent(scheduleStreamEvent));
        guard();

        const { listen } = await import('@tauri-apps/api/event');
      // 尾沿去抖 + in-flight 串行化 + dirty rerun。
      // - 每次事件都重置 120ms 计时器，等事件静默后再刷新；
      // - 刷新进行中来的事件标记 dirty，settle 后 rerun 一次，避免并发读取。
      // 刷新失败时由 store 保留旧列表，后续事件会再次触发收敛。
      const runProtectiveRefresh = async () => {
        if (sessionsRefreshInFlightRef.current) {
          sessionsRefreshDirtyRef.current = true;
          return;
        }
        sessionsRefreshInFlightRef.current = true;
        sessionsRefreshDirtyRef.current = false;
        try {
          await loadSessions({ protective: true });
        } finally {
          sessionsRefreshInFlightRef.current = false;
          if (sessionsRefreshDirtyRef.current) {
            sessionsRefreshDirtyRef.current = false;
            runProtectiveRefresh();
          }
        }
      };
      const scheduleSessionsRefresh = () => {
        if (sessionsRefreshTimerRef.current !== null) {
          clearTimeout(sessionsRefreshTimerRef.current);
        }
        sessionsRefreshTimerRef.current = setTimeout(() => {
          sessionsRefreshTimerRef.current = null;
          runProtectiveRefresh();
        }, 120);
      };
      // turn 结束后精确更新单条会话（消息数/时间），不全量刷新列表。
      track(await listen<string>('session_meta_updated', (event) => {
        const sessionId = event.payload;
        if (sessionId) {
          updateSessionMeta(sessionId);
        }
      }));
      guard();
      // sessions_updated 仅用于低频全量刷新（如恢复会话）。
      track(await listen('sessions_updated', () => {
        scheduleSessionsRefresh();
      }));
      guard();
      track(await listen<string>('desktop_notification_open_session', (event) => {
        const sessionId = event.payload;
        if (sessionId) {
          useStore.getState().switchSession(sessionId).catch(console.error);
        }
      }));
      guard();

      // 兼容旧宿主浏览器命令的 browser:open；插件工具（web_fetch / browser_open）
      // 已由浏览器插件通过 SDK 的 app.open 自行决定是否展开拓展区。
      track(await listen<{ session_id: string; url: string }>('browser:open', async (event) => {
        const { url } = event.payload;
        const sessionId = browserPluginSessionId(event.payload.session_id);
        if (!sessionId || useStore.getState().activeSessionId !== sessionId) return;
        let instanceId: string | undefined;
        if (url) {
          const raw = await api
            .bridgeCall(
              'browser',
              'webview.navigate',
              JSON.stringify({ url, session_id: sessionId }),
            )
            .catch((error) => {
              console.error('打开浏览器页面失败:', error);
              return null;
            });
          if (raw) {
            const result = JSON.parse(raw) as { active_tab_id?: string | null };
            instanceId = result.active_tab_id ?? undefined;
          }
        }
        setAppTabCommand({
          kind: 'plugin',
          action: 'open-plugin',
          version: Date.now(),
          sessionId,
          app: {
            pluginId: 'browser',
            contributionId: 'browser',
            title: '浏览器',
            sandbox: 'webview',
            multi: true,
            instanceId,
          },
        });
        await openWorkspacePanel('plugin');
      }));
      guard();
      // app.open 原语落地：宿主请求打开插件 App（官方与三方插件一致）。
      // 实例统一建立拓展区顶部标签（可见、可关闭、计入绿点），showPanel
      // 仅控制是否自动展开面板：background 为工具静默拉起，不弹面板
      // 打扰用户。实例挂载后 shell 订阅 tool.requested，宿主重放挂起
      // 调用，工具继续执行。
      track(await listen<{
        plugin_id: string;
        contribution_id: string;
        title?: string;
        sandbox?: string;
        multi?: boolean;
        session_id?: string;
        background?: boolean;
        instance_id?: string | null;
      }>('app:open_plugin', (event) => {
        const payload = event.payload;
        const normalizeSandbox = (): SandboxKind => (
          payload.sandbox === 'webview'
            || payload.sandbox === 'iframe'
            || payload.sandbox === 'native'
            ? payload.sandbox
            : 'shadow'
        );
        const requestedSessionId = payload.session_id
          || useStore.getState().activeSessionId
          || useStore.getState().newConversationId
          || '';
        if (!requestedSessionId) return;
        const mountBackgroundShell = () => {
          setBgPluginInstances((prev) => {
            const exists = prev.some((item) => item.pluginId === payload.plugin_id
              && item.contributionId === payload.contribution_id
              && item.sessionId === requestedSessionId);
            if (exists) return prev;
            return [...prev, {
              pluginId: payload.plugin_id,
              contributionId: payload.contribution_id,
              sandbox: normalizeSandbox(),
              sessionId: requestedSessionId,
            }];
          });
        };
        // 后台会话（Sub Agent/Bot 等）的实例无法进入当前会话的标签栏，
        // 仍隐藏挂载保证其工具有人执行，不计入前台标签与绿点。终端插件
        // 例外：工具编排已下沉 sidecar 直连执行，不需要前端壳接应；后台
        // 会话不占任何前端资源，切回该会话时由拓展区按 terminalListByScope
        // 恢复真实使用中的终端标签。
        if ((useStore.getState().activeSessionId || useStore.getState().newConversationId)
          !== requestedSessionId) {
          if (payload.plugin_id !== 'terminal') {
            mountBackgroundShell();
          }
          return;
        }
        // 宿主无订阅兜底拉起（不带实例编号）：只挂隐藏执行壳，插件工具
        // 随后会携带精确实例编号再次 app.open 建立可见标签，此处建标签
        // 会因编号不匹配产生重复空白实例。
        if (payload.background && !payload.instance_id) {
          mountBackgroundShell();
          return;
        }
        setAppTabCommand({
          kind: 'plugin',
          action: 'open-plugin',
          version: Date.now(),
          sessionId: requestedSessionId,
          app: {
            pluginId: payload.plugin_id,
            contributionId: payload.contribution_id,
            title: payload.title || payload.plugin_id,
            sandbox: normalizeSandbox(),
            multi: Boolean(payload.multi),
            focusExisting: true,
            instanceId: payload.instance_id ?? undefined,
          },
        });
        if (payload.background) {
          // 工具静默拉起：实例照常建立标签（可见、可关闭、计入绿点），
          // 仅挂载拓展区容器（隐藏保活）而不展开面板。
          setWorkspacePanelMounted(true);
          return;
        }
        void openWorkspacePanel('plugin');
      }));
      guard();
      // 插件热加载完成：卸载该插件的后台执行壳。壳内可能是目录替换前
      // 的旧页面（桥订阅已断），保留会让工具调用的"已有实例"判重吞掉
      // 重新挂载的请求，永远等不到新订阅。卸载后下次调用自动按磁盘
      // 最新页面重建并接续执行；在途调用已由后端热加载先行取消。
      track(await listen<string>('plugin_reloaded', (event) => {
        const pluginId = event.payload;
        if (!pluginId) return;
        setBgPluginInstances((prev) => prev.filter((item) => item.pluginId !== pluginId));
      }));
      guard();
      // app.close 原语落地：宿主请求关闭插件 App 实例（Agent/插件在必要
      // 时收起对应 tab）。instance_id 精确关一个实例，all 收起全部。
      track(await listen<{
        plugin_id: string;
        session_id?: string;
        instance_id?: string | null;
        all?: boolean;
      }>('app:close_plugin', (event) => {
        const payload = event.payload;
        const sessionId = payload.session_id
          || useStore.getState().activeSessionId
          || useStore.getState().newConversationId
          || '';
        if (
          payload.session_id
          && useStore.getState().activeSessionId !== payload.session_id
        ) return;
        setAppTabCommand({
          kind: 'plugin',
          action: 'close-plugin',
          version: Date.now(),
          sessionId,
          // close-plugin 只消费 pluginId / instanceId，其余字段为类型占位。
          app: {
            pluginId: payload.plugin_id,
            contributionId: '',
            title: '',
            sandbox: 'shadow',
            multi: false,
            instanceId: payload.instance_id ?? undefined,
          },
        });
      }));
      guard();
      track(await getCurrentWindow().onResized(async () => {
        if (programmaticResizeRef.current) return;

        const appWindow = getCurrentWindow();
        const innerSize = await appWindow.innerSize();
        const scaleFactor = await appWindow.scaleFactor();
        const logicalW = innerSize.width / scaleFactor;

        if (showWorkspacePanelRef.current) {
          const mainEl = document.querySelector('main');
          const sidebarW = mainEl ? mainEl.offsetLeft : 0;
          const browserSpace = logicalW - sidebarW - chatPanelWidthRef.current;
          if (browserSpace < MIN_BROWSER_WIDTH) {
            await closeWorkspacePanel();
          }
        }

        if (logicalW <= SIDEBAR_RESTORE_THRESHOLD) {
          setSidebarOpenByLayout(false);
        } else if (
          !showWorkspacePanelRef.current
          && preferredSidebarOpenRef.current
        ) {
          setSidebarOpenByLayout(true);
        }
      }));
      } catch (error) {
        // effect 已被清理（guard 抛 AbortError），释放本轮已收纳的监听。
        if ((error as DOMException)?.name !== 'AbortError') {
          throw error;
        }
      }
    };

    setupListener();

    api.getWorkspaceDir()
      .then((workspaceDir) => {
        useStore.setState((state) => ({
          workspaceDir,
          sessionCwd: state.activeSessionId ? state.sessionCwd : state.sessionCwd || workspaceDir,
        }));
      })
      .catch(console.error);

    const onOpenBrowser = async (e: Event) => {
      const url = (e as CustomEvent).detail;
      if (typeof url !== 'string' || !url.trim()) return;
      const store = useStore.getState();
      const sessionId = store.activeSessionId || store.newConversationId;
      if (!sessionId) {
        console.error('无法打开浏览器：缺少 session_id');
        return;
      }
      // 阶段 4 退役内置浏览器：链接打开走浏览器插件（插件×会话实例，
      // 面板 attach 后自动对齐显示），再聚焦插件浏览器标签
      let instanceId: string | undefined;
      try {
        const raw = await api.bridgeCall(
          'browser',
          'webview.navigate',
          JSON.stringify({ url, session_id: sessionId }),
        );
        const result = JSON.parse(raw) as { active_tab_id?: string | null };
        instanceId = result.active_tab_id ?? undefined;
      } catch (error) {
        console.error('打开浏览器地址失败:', error);
      }
      setAppTabCommand({
        kind: 'plugin',
        action: 'open-plugin',
        version: Date.now(),
        sessionId,
        app: {
          pluginId: 'browser',
          contributionId: 'browser',
          title: '浏览器',
          sandbox: 'webview',
          multi: true,
          instanceId,
        },
      });
      await openWorkspacePanel('plugin');
    };
    window.addEventListener('tiangong:open-browser', onOpenBrowser);

    return () => {
      // 先标记本轮 disposed，使尚未完成的异步注册流程在后续 guard 处自行放弃；
      // 再释放本轮已收纳的监听，避免遗留孤儿监听导致事件被重复消费。
      disposed = true;
      while (cleanups.length > 0) {
        const un = cleanups.pop();
        un?.();
      }
      if (streamEventTimerRef.current !== null) {
        window.clearTimeout(streamEventTimerRef.current);
        streamEventTimerRef.current = null;
      }
      streamEventQueueRef.current = [];
      if (sessionsRefreshTimerRef.current !== null) {
        clearTimeout(sessionsRefreshTimerRef.current);
        sessionsRefreshTimerRef.current = null;
      }
      window.removeEventListener('tiangong:open-browser', onOpenBrowser);
    };
  }, [
    setSidebarOpenByLayout,
    applyStreamEvents,
    openWorkspacePanel,
    closeWorkspacePanel,
    lockResize,
    unlockResize,
  ]);

  return (
    <SidebarProvider open={sidebarOpen} onOpenChange={handleSidebarChange}>
      {/* 工具接应的后台插件实例：隐藏挂载，仅保证插件工具有人执行 */}
      <BackgroundPluginHost instances={bgPluginInstances} />
      <div className="flex flex-col h-screen w-full overflow-hidden">
        <LazyStatusPanel
          // 高亮 = 拓展区面板展开中；绿点 = 会话存在已打开的 App 实例（在用标记）
          extensionActive={showWorkspacePanel}
          extensionAgentActive={runningPluginApps.length > 0}
          onToggleExtension={handleToggleExtension}
        />

        <div className="flex flex-1 min-h-0">
          <AppSidebar />

          <main className="flex flex-1 flex-col min-w-0 bg-background">
            <div className="flex min-w-0 flex-1 min-h-0">
              <div
                className={`relative isolate flex flex-col min-w-0 ${showWorkspacePanel ? 'shrink-0' : 'flex-1'}`}
                style={showWorkspacePanel ? { width: chatPanelWidth } : undefined}
              >
                <div className="flex-1 overflow-hidden">
                  <LazyMessageList />
                </div>

                <LazyMessageInput
                  interactionVisible={interactionVisible || inputOverlayVisible}
                  onHeightChange={setMessageInputHeight}
                />
                <InputOverlayPluginHost
                  suppressed={interactionVisible}
                  inputHeight={messageInputHeight}
                  onVisibilityChange={setInputOverlayVisible}
                />
                <InteractionPluginHost
                  inputHeight={messageInputHeight}
                  onVisibilityChange={setInteractionVisible}
                />
              </div>

              {workspacePanelMounted && showWorkspacePanel && (
                <div
                  className="w-[3px] shrink-0 cursor-col-resize bg-border hover:bg-muted-foreground/30 active:bg-muted-foreground/50 transition-colors"
                  onMouseDown={handleDividerDrag}
                />
              )}

              {workspacePanelMounted && (
                <div className={`min-w-0 flex-1 ${showWorkspacePanel ? 'flex' : 'hidden'}`}>
                  <TabsContainer
                    initialTabKind={workspaceTabKind}
                    isVisible={showWorkspacePanel}
                    openRequestVersion={workspaceOpenRequestVersion}
                    mode={workspaceMode}
                    matrix={
                      <ExtensionMatrix
                        runningPluginApps={runningPluginApps}
                        onOpenPluginApp={(app, opts) => {
                          // App 统一走插件命令通道：已有实例聚焦（含工具
                          // 静默建立的标签），无实例或显式新建时按 open_mode
                          // 分派（单例聚焦/多例新建）。
                          setAppTabCommand({
                            kind: 'plugin',
                            action: 'open-plugin',
                            version: Date.now(),
                            sessionId: currentSessionId ?? undefined,
                            app: {
                              pluginId: app.plugin_id,
                              contributionId: app.contribution_id,
                              title: app.title,
                              sandbox: app.sandbox,
                              multi: app.open_mode === 'multi',
                              focusExisting: !opts?.newInstance,
                            },
                          });
                          void openWorkspacePanel('plugin');
                        }}
                      />
                    }
                    appCommand={appTabCommand}
                    onClose={() => { void closeWorkspacePanel(); }}
                    onShowMatrix={handleShowMatrix}
                    onRequestAppMode={handleRequestAppMode}
                    onTabKindsChanged={(_kinds, pluginApps) => {
                      setForegroundPluginApps(pluginApps);
                    }}
                    onActiveKindChange={handleWorkspaceActiveKindChange}
                  />
                </div>
              )}
            </div>
          </main>
        </div>
      </div>

      <DefaultPluginOnboarding
        missing={onboardingMissing}
        onOpenChange={(open) => {
          if (!open) setOnboardingMissing(null);
        }}
        onComplete={() => {
          /* 安装后 registry 会热加载，Core 创建时按需感知已装插件，无需额外刷新 */
        }}
      />

      <FirstRunModelSetup
        open={modelSetupOpen}
        onOpenChange={setModelSetupOpen}
      />
    </SidebarProvider>
  );
}
