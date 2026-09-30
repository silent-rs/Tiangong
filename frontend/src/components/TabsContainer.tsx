import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { Globe, Grid3x3, Puzzle, X } from 'lucide-react';
import { api } from '@/api/tauri';
import type { PluginInstanceEntry, SandboxKind, TabKind, TabState } from '@/api/tauri';
import { useStore } from '@/store/useStore';
import { PluginAppTabContent } from './PluginAppTabContent';
import { runPluginBeforeClose } from './PluginSandbox';
import { Button } from './ui/button';
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from './ui/context-menu';

interface TabsContainerProps {
  initialTabKind: TabKind;
  isVisible: boolean;
  openRequestVersion: number;
  onClose: () => void;
  /** 点击启动台按钮：拓展区切回 App 矩阵态（面板保持展开）。 */
  onShowMatrix?: () => void;
  /** 矩阵态下点击顶部标签：请求切回 App 态显示该实例（矩阵不随 active 变化）。 */
  onRequestAppMode?: () => void;
  /** tab 集合（按类型）变化通知：宿主「已打开」绿点的即时数据源，
   *  覆盖新建、关闭和会话切换。 */
  onTabKindsChanged?: (kinds: TabKind[], pluginApps: string[]) => void;
  /** 宿主下发的 App 实例命令（矩阵右键菜单：新建实例/关闭全部实例）。 */
  appCommand?: AppTabCommand | null;
  /** 拓展区模式：app（聚焦实例）或 matrix（App 矩阵占据内容区，tab 栏保留）。 */
  mode?: 'app' | 'matrix';
  /** 矩阵态渲染到内容区的视图（由宿主注入，保持容器与矩阵解耦）。 */
  matrix?: ReactNode;
  onActiveKindChange?: (kind: TabKind | null) => void;
}

const DEFAULT_BROWSER_URL = 'about:blank';

/** 宿主（矩阵菜单等）下发的 App 实例命令，version 递增触发执行。 */
export interface AppTabCommand {
  kind: TabKind;
  action: 'close-all' | 'close-plugin' | 'open-plugin';
  version: number;
  /** 命令所属会话；仅在该会话仍为当前会话时执行。 */
  sessionId?: string;
  /** open-plugin 携带的三方 App 元数据；close-plugin 仅使用 pluginId。 */
  app?: {
    pluginId: string;
    contributionId: string;
    title: string;
    sandbox: SandboxKind;
    multi: boolean;
    /** 已由调用方创建或导航到的 webview 页面编号。 */
    instanceId?: string;
    /** 工具调用拉起等宿主侧场景置 true：已有实例时聚焦而非新建（multi 亦然）。 */
    focusExisting?: boolean;
    /** 实例持有后端资源（manifest `instance_resources`）。 */
    instanceResources?: boolean;
  };
}

interface WebviewPluginTabInfo {
  id: string;
  url: string;
  title: string;
}

interface WebviewPluginSnapshot {
  tab_id?: string;
  tabs?: WebviewPluginTabInfo[];
  active_tab_id?: string | null;
}

function isWebviewPluginTab(tab: TabState): boolean {
  return tab.kind === 'plugin'
    && tab.sandbox === 'webview'
    && Boolean(tab.plugin_id && tab.contribution_id);
}

/** 标签持有宿主统一管理的实例资源（webview 页面或声明 instance_resources）。 */
function isResourceTab(tab: TabState): boolean {
  return tab.kind === 'plugin'
    && Boolean(tab.plugin_id)
    && (tab.sandbox === 'webview' || Boolean(tab.instance_resources));
}

function liveResourceInstances(tabs: TabState[]): { plugin_id: string; instance_id: string }[] {
  return tabs
    .filter(isResourceTab)
    .map((tab) => ({ plugin_id: tab.plugin_id as string, instance_id: tab.id }));
}

function mountedBrowserTabIds(tabs: TabState[]): string[] {
  return tabs
    .filter((tab) => isWebviewPluginTab(tab) && tab.plugin_id === 'browser')
    .map((tab) => tab.id);
}

function webviewSessionId(sessionId: string): string {
  return sessionId || '__global__';
}

async function callWebviewPlugin<T>(
  pluginId: string,
  sessionId: string,
  method: string,
  payload: Record<string, unknown> = {},
): Promise<T> {
  const raw = await api.bridgeCall(
    pluginId,
    method,
    JSON.stringify({ session_id: webviewSessionId(sessionId), ...payload }),
  );
  return JSON.parse(raw) as T;
}

function createWebviewPluginTab(
  page: WebviewPluginTabInfo,
  app: NonNullable<AppTabCommand['app']>,
): TabState {
  return {
    id: page.id,
    kind: 'plugin',
    title: page.title || app.title,
    url: page.url || DEFAULT_BROWSER_URL,
    created_at: nowText(),
    plugin_id: app.pluginId,
    contribution_id: app.contributionId,
    sandbox: app.sandbox,
    instance_resources: true,
  };
}

function nowText(): string {
  return new Date().toISOString();
}

function normalizeActiveTabId(tabs: TabState[], activeTabId: string | null | undefined): string {
  if (activeTabId && tabs.some((tab) => tab.id === activeTabId)) {
    return activeTabId;
  }
  return tabs[0]?.id ?? '';
}

function pickNextActiveTab(tabs: TabState[], closedIndex: number): string | null {
  if (tabs.length === 0) return null;
  const nextIndex = Math.min(closedIndex, tabs.length - 1);
  return tabs[nextIndex]?.id ?? tabs[0]?.id ?? null;
}

export function TabsContainer({
  initialTabKind,
  isVisible,
  openRequestVersion,
  onClose,
  onShowMatrix,
  onRequestAppMode,
  onTabKindsChanged,
  appCommand,
  mode = 'app',
  matrix,
  onActiveKindChange,
}: TabsContainerProps) {
  const activeSessionId = useStore((state) => state.activeSessionId);
  const newConversationId = useStore((state) => state.newConversationId);
  /** 插件 tab 的刷新代次（右键「刷新」+1 强制重挂：重拉 entry 与资源，
   *  用于插件迭代重装后快速试用新版本）。 */
  const [reloadGenerations, setReloadGenerations] = useState<Record<string, number>>({});
  const [tabs, setTabs] = useState<TabState[]>([]);
  const [activeTabId, setActiveTabId] = useState('');
  const [sessionResetVersion, setSessionResetVersion] = useState(0);
  const [activationRetryVersion, setActivationRetryVersion] = useState(0);
  const tabsRef = useRef<TabState[]>([]);
  const activeTabIdRef = useRef('');
  const initialTabKindRef = useRef<TabKind>(initialTabKind);
  const isVisibleRef = useRef(isVisible);
  const lastInitialActivationKeyRef = useRef<string | null>(null);
  const mountedSessionIdRef = useRef('');
  const mountedTabsSyncRef = useRef<Promise<void>>(Promise.resolve());
  /** tab 栏横向滚动容器：滚轮转横向滚动 + 活跃 tab 自动滚入可视区。 */
  const tabsScrollRef = useRef<HTMLDivElement>(null);
  const terminalSessionId = activeSessionId || newConversationId || '';

  const activeTab = useMemo(
    () => tabs.find((tab) => tab.id === activeTabId) ?? tabs[0] ?? null,
    [activeTabId, tabs],
  );

  const syncMountedBrowserTabs = useCallback((sessionId: string, tabIds: string[]) => {
    const sync = mountedTabsSyncRef.current
      .catch(() => {})
      .then(() => api.setWebviewMountedTabs(sessionId, tabIds));
    mountedTabsSyncRef.current = sync;
    return sync;
  }, []);

  const openWebviewPluginTab = useCallback(async (
    app: NonNullable<AppTabCommand['app']>,
    requestedInstanceId?: string,
  ) => {
    if (app.sandbox !== 'webview') return;
    const sessionId = terminalSessionId;
    if (!sessionId || mountedSessionIdRef.current !== sessionId) return;
    try {
      let snapshot = await callWebviewPlugin<WebviewPluginSnapshot>(
        app.pluginId,
        sessionId,
        'webview.tabs',
      );
      if (mountedSessionIdRef.current !== sessionId) return;
      // 已有页面（如 Agent 后台操作中的标签）时直接聚焦（协同观察）；
      // 仅在无页面或矩阵「新建实例」（multi 且非聚焦语义）时新建空白页。
      if (
        !requestedInstanceId
        && ((snapshot.tabs ?? []).length === 0 || (app.multi && !app.focusExisting))
      ) {
        snapshot = await callWebviewPlugin<WebviewPluginSnapshot>(
          app.pluginId,
          sessionId,
          'webview.tabNew',
          { url: DEFAULT_BROWSER_URL },
        );
        if (mountedSessionIdRef.current !== sessionId) return;
      }

      const instanceId = requestedInstanceId
        ?? snapshot.tab_id
        ?? snapshot.active_tab_id
        ?? snapshot.tabs?.[0]?.id;
      const page = snapshot.tabs?.find((tab) => tab.id === instanceId);
      if (!instanceId || !page) throw new Error('宿主未返回可用的浏览器页面');

      const nextTab = createWebviewPluginTab(page, app);
      const currentTabs = tabsRef.current;
      const nextTabs = currentTabs.some((tab) => tab.id === nextTab.id)
        ? currentTabs.map((tab) => tab.id === nextTab.id ? { ...tab, ...nextTab } : tab)
        : [...currentTabs, nextTab];
      tabsRef.current = nextTabs;
      activeTabIdRef.current = nextTab.id;
      setTabs(nextTabs);
      setActiveTabId(nextTab.id);
    } catch (error) {
      console.error('打开 webview 插件标签失败：', error);
    }
  }, [terminalSessionId]);

  const hideWebviewPluginTabs = useCallback((
    targetTabs: TabState[],
    sessionId = terminalSessionId,
  ) => {
    const plugins = new Set(
      targetTabs
        .filter(isWebviewPluginTab)
        .map((tab) => tab.plugin_id)
        .filter((pluginId): pluginId is string => Boolean(pluginId)),
    );
    for (const pluginId of plugins) {
      void callWebviewPlugin(pluginId, sessionId, 'webview.hide').catch(console.error);
    }
  }, [terminalSessionId]);

  // tab 类型/插件 App 集合变化 → 通知宿主更新「已打开」绿点，并同步
  // webview 页面实际挂载清单，页面事件只有命中该清单才允许进入会话。
  const onTabKindsChangedRef = useRef(onTabKindsChanged);
  onTabKindsChangedRef.current = onTabKindsChanged;
  const lastTabKindsRef = useRef<string>('');
  useEffect(() => {
    const kinds = Array.from(new Set(tabs.map((tab) => tab.kind)));
    const pluginApps = Array.from(new Set(
      tabs
        .filter((tab) => tab.kind === 'plugin' && tab.plugin_id && tab.contribution_id)
        .map((tab) => `${tab.plugin_id}:${tab.contribution_id}`),
    ));
    const mountedWebviewTabIds = mountedBrowserTabIds(tabs);
    const key = `${terminalSessionId}|${kinds.join(',')}|${pluginApps.join(',')}|${mountedWebviewTabIds.join(',')}`;
    if (key === lastTabKindsRef.current) return;
    lastTabKindsRef.current = key;
    onTabKindsChangedRef.current?.(kinds, pluginApps);
    if (terminalSessionId && mountedSessionIdRef.current === terminalSessionId) {
      void syncMountedBrowserTabs(terminalSessionId, mountedWebviewTabIds).catch(console.error);
    }
  }, [syncMountedBrowserTabs, tabs, terminalSessionId]);

  useEffect(() => {
    onActiveKindChange?.(isVisible ? activeTab?.kind ?? null : null);
  }, [activeTab?.kind, isVisible, onActiveKindChange]);

  initialTabKindRef.current = initialTabKind;
  isVisibleRef.current = isVisible;

  useEffect(() => {
    if (isVisible && mode === 'app') return;
    hideWebviewPluginTabs(tabsRef.current);
  }, [hideWebviewPluginTabs, isVisible, mode]);

  useEffect(() => {
    activeTabIdRef.current = activeTabId;
  }, [activeTabId]);

  // 鼠标滚轮（垂直）转为 tab 栏横向滚动；触控板横向滚动（deltaX）保持原生行为。
  useEffect(() => {
    const el = tabsScrollRef.current;
    if (!el) return;
    const handleWheel = (event: WheelEvent) => {
      if (Math.abs(event.deltaY) <= Math.abs(event.deltaX)) return;
      if (el.scrollWidth <= el.clientWidth) return;
      event.preventDefault();
      const step = event.deltaMode === WheelEvent.DOM_DELTA_LINE
        ? event.deltaY * 16
        : event.deltaMode === WheelEvent.DOM_DELTA_PAGE
          ? event.deltaY * el.clientWidth
          : event.deltaY;
      el.scrollLeft += step;
    };
    el.addEventListener('wheel', handleWheel, { passive: false });
    return () => el.removeEventListener('wheel', handleWheel);
  }, []);

  // 活跃 tab 变化时滚入可视区（工具拉起/命令聚焦远处 tab 时不必手动翻找）。
  useEffect(() => {
    const el = tabsScrollRef.current;
    if (!el || !activeTabId) return;
    const activeEl = el.querySelector<HTMLElement>(
      `[data-tab-id="${CSS.escape(activeTabId)}"]`,
    );
    activeEl?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  }, [activeTabId]);


  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    void api.onBridgeEvent((event) => {
      if (cancelled || event.channel !== 'webview.event') return;
      let pageEvent: {
        scope?: string;
        payload?: { tab_id?: string; url?: string; title?: string };
      };
      try {
        pageEvent = JSON.parse(event.payload);
      } catch {
        return;
      }
      const expectedScope = `webview:${event.plugin_id}:${webviewSessionId(terminalSessionId)}`;
      const tabId = pageEvent.payload?.tab_id;
      if (pageEvent.scope !== expectedScope || !tabId) return;

      let changed = false;
      const nextTabs = tabsRef.current.map((tab) => {
        if (!isWebviewPluginTab(tab) || tab.plugin_id !== event.plugin_id || tab.id !== tabId) {
          return tab;
        }
        changed = true;
        return {
          ...tab,
          url: pageEvent.payload?.url ?? tab.url,
          title: pageEvent.payload?.title || tab.title,
        };
      });
      if (changed) {
        tabsRef.current = nextTabs;
        setTabs(nextTabs);
      }
    }).then((cleanup) => {
      if (cancelled) cleanup();
      else unlisten = cleanup;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [terminalSessionId]);

  // 会话挂载后按宿主 listInstances 以同一编号恢复持有资源的实例标签
  // （终端 PTY、浏览器页面等；后台会话执行期间不占前端资源），随后提交
  // 标签集合核查，宿主直接释放无标签归属的多余资源（记 warn）。已有
  // 同编号标签（工具静默拉起抢先建好）时仅跳过。
  const restoreInstanceTabs = useCallback(async (sessionId: string) => {
    if (!sessionId) return;
    let entries: PluginInstanceEntry[];
    try {
      entries = await api.pluginInstancesList(sessionId);
    } catch (error) {
      console.error('查询插件实例失败：', error);
      return;
    }
    if (mountedSessionIdRef.current !== sessionId) return;
    let nextTabs = tabsRef.current;
    let changed = false;
    for (const entry of entries) {
      if (nextTabs.some((tab) => tab.id === entry.instance_id)) continue;
      nextTabs = [...nextTabs, {
        id: entry.instance_id,
        kind: 'plugin',
        title: entry.page_title || entry.title,
        url: entry.url,
        created_at: nowText(),
        plugin_id: entry.plugin_id,
        contribution_id: entry.contribution_id,
        sandbox: entry.sandbox,
        instance_resources: true,
      }];
      changed = true;
    }
    if (changed) {
      tabsRef.current = nextTabs;
      setTabs(nextTabs);
      // 竞态下工具拉起的标签已建好并聚焦时不抢占；正常切换路径标签栏
      // 已被清空，聚焦最后一个恢复的实例。
      if (!activeTabIdRef.current && entries.length > 0) {
        const last = entries[entries.length - 1].instance_id;
        activeTabIdRef.current = last;
        setActiveTabId(last);
      }
    }
    await api
      .pluginInstancesReconcile(sessionId, liveResourceInstances(tabsRef.current))
      .catch((error) => console.error('核查插件实例失败：', error));
  }, []);

  // 拓展区 Tab 仅驻留当前进程。会话变化时清空标签栏：无资源的插件实例
  // 直接关闭（beforeClose 保存状态）；持有资源的实例（终端、浏览器页面等）
  // 跨会话存活——离开的会话由宿主隐藏其全部 webview 实例，切回时按
  // listInstances 以同一编号恢复标签，并核查释放多余资源。
  useEffect(() => {
    if (mountedSessionIdRef.current === terminalSessionId) return;
    const previousSessionId = mountedSessionIdRef.current;
    const closingTabs = tabsRef.current;
    mountedSessionIdRef.current = terminalSessionId;

    void Promise.allSettled(
      closingTabs
        .filter((tab) => tab.kind === 'plugin' && !isResourceTab(tab))
        .map((tab) => runPluginBeforeClose(tab.id)),
    );
    if (previousSessionId) {
      void api.pluginInstancesDetach(previousSessionId).catch(console.error);
    }

    tabsRef.current = [];
    activeTabIdRef.current = '';
    setTabs([]);
    setActiveTabId('');
    setReloadGenerations({});
    setSessionResetVersion((version) => version + 1);
    setActivationRetryVersion((version) => version + 1);
    void restoreInstanceTabs(terminalSessionId);
  }, [restoreInstanceTabs, terminalSessionId]);

  const activateOrCreateTab = useCallback(async (kind: TabKind) => {
    const sessionId = terminalSessionId;
    if (!sessionId) return;
    // 所有 App（浏览器/终端/三方）均以插件形态存在：创建/聚焦统一走宿主
    // 命令通道（open-plugin），此处仅聚焦已有实例。
    if (kind === 'plugin') {
      const existing = tabsRef.current.find((tab) => tab.kind === 'plugin');
      if (existing) {
        activeTabIdRef.current = existing.id;
        setActiveTabId(existing.id);
      }
      return;
    }
  }, [terminalSessionId]);

  useEffect(() => {
    if (!isVisible) return;
    // 矩阵态 = 用户尚未选择 App：不按 initialTabKind 隐式创建 tab
    // （浏览器/终端已插件化，宿主不再默认开原生 App；显式打开走
    // openWorkspacePanel，会切到 App 态并推进 openRequestVersion 再触发本 effect）。
    if (mode === 'matrix') return;
    const activationKey = `${terminalSessionId}:${sessionResetVersion}:${initialTabKind}:${openRequestVersion}`;
    if (lastInitialActivationKeyRef.current === activationKey) {
      return;
    }
    lastInitialActivationKeyRef.current = activationKey;
    void activateOrCreateTab(initialTabKind);
  }, [
    activateOrCreateTab,
    activationRetryVersion,
    sessionResetVersion,
    initialTabKind,
    isVisible,
    mode,
    openRequestVersion,
    terminalSessionId,
  ]);

  const handleSwitchTab = useCallback((tabId: string) => {
    // 矩阵态占据内容区且不随 active 标记变化：点击标签视为请求回到
    // App 态聚焦该实例，由宿主切回 mode=app。
    if (mode === 'matrix') onRequestAppMode?.();
    const nextTab = tabs.find((tab) => tab.id === tabId);
    const currentTab = tabsRef.current.find((tab) => tab.id === activeTabIdRef.current);
    if (currentTab && currentTab.id !== tabId && isWebviewPluginTab(currentTab) && currentTab.plugin_id) {
      const switchingWithinPlugin = Boolean(
        nextTab
          && isWebviewPluginTab(nextTab)
          && nextTab.plugin_id === currentTab.plugin_id,
      );
      void callWebviewPlugin(
        currentTab.plugin_id,
        terminalSessionId,
        switchingWithinPlugin ? 'webview.instanceHide' : 'webview.hide',
        switchingWithinPlugin ? { tab_id: currentTab.id } : {},
      ).catch(console.error);
    }
    activeTabIdRef.current = tabId;
    setActiveTabId(tabId);
  }, [mode, onRequestAppMode, tabs, terminalSessionId]);

  const handleCloseTab = useCallback(async (tabId: string) => {
    let currentTabs = tabsRef.current;
    let closedIndex = currentTabs.findIndex((tab) => tab.id === tabId);
    if (closedIndex === -1) return;

    const closingTab = currentTabs[closedIndex];
    const closingBrowserTab = isWebviewPluginTab(closingTab) && closingTab.plugin_id === 'browser';
    if (closingBrowserTab) {
      const remainingMountedIds = mountedBrowserTabIds(
        currentTabs.filter((tab) => tab.id !== tabId),
      );
      try {
        await syncMountedBrowserTabs(terminalSessionId, remainingMountedIds);
      } catch (error) {
        console.error('撤销浏览器标签登记失败：', error);
        return;
      }
    }
    if (closingTab.kind === 'plugin') {
      try {
        await runPluginBeforeClose(closingTab.id);
      } catch (error) {
        if (closingBrowserTab) {
          await syncMountedBrowserTabs(
            terminalSessionId,
            mountedBrowserTabIds(currentTabs),
          ).catch(console.error);
        }
        console.error('插件关闭前处理失败：', error);
        return;
      }
      currentTabs = tabsRef.current;
      closedIndex = currentTabs.findIndex((tab) => tab.id === tabId);
      if (closedIndex === -1) return;
    }
    // 统一释放路径：标签移除后由宿主向资源方发出 instanceClosed（webview
    // 页面由宿主 webview 层关闭，sidecar 资源由插件回收）；无资源插件
    // 仅移除内存状态。

    const nextTabs = currentTabs.filter((tab) => tab.id !== tabId);
    const currentActiveId = activeTabIdRef.current;
    const nextActiveId = currentActiveId === tabId
      ? pickNextActiveTab(nextTabs, closedIndex) ?? ''
      : normalizeActiveTabId(nextTabs, currentActiveId);

    if (currentActiveId === tabId && isWebviewPluginTab(closingTab) && closingTab.plugin_id) {
      const nextActiveTab = nextTabs.find((tab) => tab.id === nextActiveId);
      const remainsInSameWebviewPlugin = Boolean(
        nextActiveTab
        && isWebviewPluginTab(nextActiveTab)
        && nextActiveTab.plugin_id === closingTab.plugin_id,
      );
      if (!remainsInSameWebviewPlugin) {
        await callWebviewPlugin(
          closingTab.plugin_id,
          terminalSessionId,
          'webview.hide',
        ).catch(console.error);
      }
    }

    tabsRef.current = nextTabs;
    activeTabIdRef.current = nextActiveId;
    setTabs(nextTabs);
    setActiveTabId(nextActiveId);

    if (isResourceTab(closingTab) && closingTab.plugin_id && terminalSessionId) {
      await api
        .pluginInstanceClosed(
          closingTab.plugin_id,
          webviewSessionId(terminalSessionId),
          closingTab.id,
        )
        // 标签已移除；释放失败由下一次会话核查兜底（宿主直接释放并记 warn）。
        .catch((error) => console.error('释放插件实例资源失败：', error));
    }

    if (nextTabs.length === 0) {
      // 拓展区三态：全部 tab 关闭后回到 App 矩阵态（面板保持展开）；
      // 未提供矩阵回调时沿用旧行为直接收起。
      if (onShowMatrix) {
        onShowMatrix();
      } else {
        onClose();
      }
      return;
    }
  }, [onClose, onShowMatrix, syncMountedBrowserTabs, terminalSessionId]);

  const handleCloseWorkspace = useCallback(() => {
    hideWebviewPluginTabs(tabsRef.current);
    onClose();
  }, [hideWebviewPluginTabs, onClose]);

  // 宿主（矩阵右键菜单）下发的实例命令：关闭全部按 id 快照逐个走
  // handleCloseTab（末 tab 关闭会触发回矩阵/收起的既有逻辑，处于矩阵态时
  // onShowMatrix 幂等）；open-plugin 按 open_mode 分派——单例聚焦已有
  // 实例，多例每次新建。
  const lastAppCommandVersionRef = useRef(0);
  useEffect(() => {
    if (!appCommand || appCommand.version === lastAppCommandVersionRef.current) return;
    // 只消费当前会话的命令；切换会话后旧命令不得重新建立标签。
    if (appCommand.sessionId && appCommand.sessionId !== terminalSessionId) {
      lastAppCommandVersionRef.current = appCommand.version;
      return;
    }
    if (!terminalSessionId || mountedSessionIdRef.current !== terminalSessionId) return;
    lastAppCommandVersionRef.current = appCommand.version;
    if (appCommand.action === 'open-plugin' && appCommand.app) {
      const { pluginId, contributionId, title, sandbox, multi, instanceId, focusExisting } =
        appCommand.app;
      if (sandbox === 'webview') {
        void openWebviewPluginTab(appCommand.app, instanceId);
        return;
      }
      // 调用方指定了实例编号时按编号幂等聚焦（重开同一实例）；未指定时
      // multi 且非聚焦语义（矩阵新建实例）才跳过查重直接新建。
      const existing = instanceId
        ? tabsRef.current.find((tab) => tab.id === instanceId)
        : multi && !focusExisting
          ? null
          : tabsRef.current.find(
            (tab) => tab.kind === 'plugin'
              && tab.plugin_id === pluginId
              && tab.contribution_id === contributionId,
          );
      if (existing) {
        activeTabIdRef.current = existing.id;
        setActiveTabId(existing.id);
        return;
      }
      const sessionAtOpen = terminalSessionId;
      void (async () => {
        // 实例编号由宿主生成（scru128），资源方以同一编号归属资源。
        let id = instanceId;
        if (!id) {
          try {
            id = await api.pluginInstanceReserve();
          } catch (error) {
            console.error('预留插件实例编号失败：', error);
            return;
          }
        }
        if (mountedSessionIdRef.current !== sessionAtOpen) return;
        if (tabsRef.current.some((tab) => tab.id === id)) {
          activeTabIdRef.current = id;
          setActiveTabId(id);
          return;
        }
        const nextTab: TabState = {
          id,
          kind: 'plugin',
          title,
          url: '',
          created_at: new Date().toISOString(),
          plugin_id: pluginId,
          contribution_id: contributionId,
          sandbox,
          instance_resources: Boolean(appCommand.app?.instanceResources),
        };
        const nextTabs = [...tabsRef.current, nextTab];
        tabsRef.current = nextTabs;
        activeTabIdRef.current = nextTab.id;
        setTabs(nextTabs);
        setActiveTabId(nextTab.id);
      })();
      return;
    }
    if (appCommand.action === 'close-plugin' && appCommand.app?.pluginId) {
      // app.close 原语落地：instanceId 精确关一个实例，缺省关闭该插件在
      // 本会话的全部实例（宿主已校验调用方显式声明 all）。
      const pluginId = appCommand.app.pluginId;
      const targetIds = appCommand.app.instanceId
        ? tabsRef.current
          .filter((tab) => tab.id === appCommand.app!.instanceId
            && tab.kind === 'plugin'
            && tab.plugin_id === pluginId)
          .map((tab) => tab.id)
        : tabsRef.current
          .filter((tab) => tab.kind === 'plugin' && tab.plugin_id === pluginId)
          .map((tab) => tab.id);
      void (async () => {
        for (const tabId of targetIds) {
          await handleCloseTab(tabId);
        }
      })();
      return;
    }
    const targetIds = tabsRef.current
      .filter((tab) => tab.kind === appCommand.kind)
      .map((tab) => tab.id);
    void (async () => {
      for (const tabId of targetIds) {
        await handleCloseTab(tabId);
      }
    })();
  }, [
    activationRetryVersion,
    appCommand,
    handleCloseTab,
    sessionResetVersion,
    openWebviewPluginTab,
    terminalSessionId,
  ]);

  return (
    <div className="flex h-full min-w-0 flex-1 flex-col bg-background">
      {/* tab 栏：启动台按钮与右侧操作区均固定（shrink-0），中间滚动区
          flex-1 min-w-0 承载溢出，tab 再多时两端按钮始终可见；滚动条完全
          隐藏（不占位、不撑高 tab 栏），滚动能力由滚轮横滑/触控板/自动滚入承担。 */}
      <div className="flex min-w-0 shrink-0 items-center gap-1 border-b px-2 py-1">
        {onShowMatrix && (
          <Button
            size="sm"
            variant="ghost"
            className={`h-7 w-7 shrink-0 p-0 ${
              mode === 'matrix'
                ? 'bg-muted text-foreground'
                : 'text-muted-foreground hover:text-foreground'
            }`}
            onClick={onShowMatrix}
            title="启动台（回到拓展区矩阵）"
            aria-label="启动台"
          >
            <Grid3x3 className="h-3.5 w-3.5" />
          </Button>
        )}
        <div
          ref={tabsScrollRef}
          className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto overflow-y-hidden [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
        >
          {tabs.map((tab) => {
            const active = tab.id === activeTab?.id;
            const webviewPlugin = isWebviewPluginTab(tab);
            const Icon = webviewPlugin ? Globe : Puzzle;
            return (
              <ContextMenu key={tab.id}>
                <ContextMenuTrigger asChild>
                  <div
                    data-tab-id={tab.id}
                    className={`group flex h-7 min-w-28 max-w-44 shrink-0 cursor-default items-center gap-1.5 rounded px-2 text-xs transition-colors ${
                      active
                        ? 'bg-muted text-foreground'
                        : 'text-muted-foreground hover:bg-muted/50 hover:text-foreground'
                    }`}
                    title={tab.title}
                  >
                    <button
                      type="button"
                      className="flex min-w-0 flex-1 items-center gap-1.5"
                      onClick={() => handleSwitchTab(tab.id)}
                    >
                      <Icon className="h-3.5 w-3.5 shrink-0" />
                      <span className="min-w-0 flex-1 truncate text-left">{tab.title}</span>
                    </button>
                    <button
                      type="button"
                      className="flex h-4 w-4 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-background hover:text-destructive"
                      onClick={(event) => {
                        event.stopPropagation();
                        void handleCloseTab(tab.id);
                      }}
                      title="关闭"
                      aria-label="关闭标签页"
                    >
                      <X className="h-3 w-3" />
                    </button>
                  </div>
                </ContextMenuTrigger>
                <ContextMenuContent>
                  {tab.kind === 'plugin' && !webviewPlugin && (
                    <ContextMenuItem
                      onClick={() => {
                        setReloadGenerations((prev) => ({
                          ...prev,
                          [tab.id]: (prev[tab.id] ?? 0) + 1,
                        }));
                      }}
                    >
                      刷新（重载插件页面）
                    </ContextMenuItem>
                  )}
                  {webviewPlugin && (
                    <ContextMenuItem onClick={() => {
                      if (tab.plugin_id && tab.contribution_id) {
                        void openWebviewPluginTab({
                          pluginId: tab.plugin_id,
                          contributionId: tab.contribution_id,
                          title: tab.plugin_id === 'browser' ? '浏览器' : tab.title,
                          sandbox: 'webview',
                          multi: true,
                        });
                      }
                    }}>
                      新建浏览器标签页
                    </ContextMenuItem>
                  )}
                  {tabs.length > 1 && webviewPlugin && (
                    <ContextMenuItem
                      onClick={() => {
                        const others = tabsRef.current.filter((item) => (
                          item.id !== tab.id
                          && item.plugin_id === tab.plugin_id
                          && item.contribution_id === tab.contribution_id
                        ));
                        void (async () => {
                          for (const other of others) {
                            await handleCloseTab(other.id);
                          }
                        })();
                      }}
                    >
                      关闭其他标签页
                    </ContextMenuItem>
                  )}
                  {(() => {
                    // 同 App = 同插件同贡献点（如浏览器的多个标签页、多例三方 App）。
                    if (tab.kind !== 'plugin' || !tab.plugin_id || !tab.contribution_id) {
                      return null;
                    }
                    const sameAppTabs = tabs.filter((item) => (
                      item.kind === 'plugin'
                      && item.plugin_id === tab.plugin_id
                      && item.contribution_id === tab.contribution_id
                    ));
                    if (sameAppTabs.length <= 1) return null;
                    return (
                      <ContextMenuItem
                        onClick={() => {
                          void (async () => {
                            for (const item of sameAppTabs) {
                              await handleCloseTab(item.id);
                            }
                          })();
                        }}
                      >
                        关闭所有同 App 标签页
                      </ContextMenuItem>
                    );
                  })()}
                  <ContextMenuSeparator className="my-1" />
                  <ContextMenuItem onClick={() => void handleCloseTab(tab.id)}>
                    关闭标签页
                  </ContextMenuItem>
                </ContextMenuContent>
              </ContextMenu>
            );
          })}
        </div>

        <div className="flex shrink-0 items-center">
          <Button
            size="sm"
            variant="ghost"
            className="h-7 w-7 p-0 text-muted-foreground hover:text-foreground"
            onClick={handleCloseWorkspace}
            title="关闭工作区"
            aria-label="关闭工作区"
          >
            <X className="h-3.5 w-3.5" />
          </Button>
        </div>
      </div>

      {/* App 实例内容：矩阵态隐藏保活（切换矩阵不销毁插件实例）。
          min-w-0 + overflow-hidden 阻断终端等插件内容的固有宽度穿透
          flex 链撑大拓展区（否则右侧按钮被挤出窗口、消息区被压缩）。 */}
      <div className={mode === 'matrix' ? 'hidden' : 'min-h-0 min-w-0 flex-1 overflow-hidden'}>
        {tabs.map((tab) => (
          tab.sandbox === 'native' && tab.plugin_id === '__builtin__' ? (
            // 官方内置 App 的 native 容器（设计 6.2 ③，仅官方）：按贡献分派组件
            <div
              key={`${terminalSessionId}:${tab.id}`}
              className={
                isVisible && mode === 'app' && tab.id === activeTab?.id
                  ? 'flex h-full min-h-0 w-full flex-1 flex-col'
                  : 'hidden'
              }
            >
              <div className="p-4 text-sm text-muted-foreground">
                该官方 App 内容组件未注册。
              </div>
            </div>
          ) : (
            <PluginAppTabContent
              key={`${terminalSessionId}:${tab.id}:${reloadGenerations[tab.id] ?? 0}`}
              tab={tab}
              isActive={isVisible && mode === 'app' && tab.id === activeTab?.id}
              sessionId={terminalSessionId || null}
              onRequestNew={isWebviewPluginTab(tab) && tab.plugin_id && tab.contribution_id
                ? () => void openWebviewPluginTab({
                  pluginId: tab.plugin_id!,
                  contributionId: tab.contribution_id!,
                  title: tab.plugin_id === 'browser' ? '浏览器' : tab.title,
                  sandbox: 'webview',
                  multi: true,
                })
                : undefined}
            />
          )
        ))}
      </div>
      {mode === 'matrix' && matrix && (
        <div className="min-h-0 flex-1">{matrix}</div>
      )}
    </div>
  );
}
