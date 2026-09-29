import { useEffect, useRef } from 'react';
import { api, type SandboxKind } from '../api/tauri';
import { PluginIframe } from './PluginIframe';
import {
  hostContext,
  type HostMessageContext,
  type PluginHostContext,
} from './pluginHostContext';

/**
 * 标准 Slot 沙箱容器：按贡献声明的 sandbox 级别分发渲染。
 *
 * - shadow（默认）：挂载主 DOM 树的 Shadow DOM 容器，插件脚本以注入的
 *   `bridge` 参数访问宿主桥接（设计文档 6.2 ①）。
 * - iframe：等价既有 srcdoc + postMessage 模式的强隔离容器（6.2 ②）。
 * - native：官方原生容器（6.2 ③），M2 内置插件迁移时提供。
 *
 * 宿主只负责容器与桥接通道，不解析插件页面内容。
 */
export interface PluginSandboxProps {
  pluginId: string;
  contributionId: string;
  sandbox: SandboxKind;
  html: string;
  className?: string;
  sessionId?: string | null;
  /** 当前会话工作目录（无活跃会话时为全局工作区）。 */
  workspace?: string | null;
  /** extension.tab 顶部标签实例编号。 */
  instanceId?: string;
  /** 当前实例是否是拓展区正在显示的标签。 */
  visible?: boolean;
  /** 消息级 Slot 的消息上下文。 */
  message?: HostMessageContext;
}

export function PluginSandbox({
  sandbox,
  pluginId,
  contributionId,
  html,
  className,
  sessionId,
  workspace,
  instanceId,
  visible,
  message,
}: PluginSandboxProps) {
  // webview 贡献（如浏览器插件）：管理界面（地址栏/工具栏）跑 shadow 容器——
  // 页面本体是宿主原生 webview 实例，管理界面需要主文档坐标同步其位置，
  // iframe 内拿不到主窗口坐标，故与 shadow 同路径渲染。
  if (sandbox === 'shadow' || sandbox === 'webview') {
    return (
      <ShadowContainer
        pluginId={pluginId}
        contributionId={contributionId}
        html={html}
        className={className}
        sessionId={sessionId}
        workspace={workspace}
        instanceId={instanceId}
        visible={visible}
        message={message}
      />
    );
  }
  if (sandbox === 'native') {
    return (
      <div className="p-4 text-sm text-muted-foreground">
        该贡献声明 native 容器，仅官方签名插件可用，当前版本暂未提供。
      </div>
    );
  }
  return (
    <PluginIframe
      pluginId={pluginId}
      html={html}
      sessionId={sessionId}
      workspace={workspace}
      instanceId={instanceId}
      visible={visible}
      message={message}
    />
  );
}

/** 宿主注入插件脚本的桥接对象（设计文档 6.3 的 Shadow 容器子集）。 */
export interface HostBridge {
  call(method: string, payload: string): Promise<string>;
  on(channel: string, handler: (payload: string) => void): () => void;
}

interface DisposableHostBridge extends HostBridge {
  dispose(): void;
}

type ShadowCleanup = () => void;
type ShadowBeforeClose = () => void | Promise<void>;
type HostContextHandler = (context: PluginHostContext) => void;

const beforeCloseHandlersByInstance = new Map<string, Set<ShadowBeforeClose>>();

export async function runPluginBeforeClose(instanceId: string): Promise<void> {
  const handlers = [...(beforeCloseHandlersByInstance.get(instanceId) ?? [])];
  await Promise.all(handlers.map((handler) => handler()));
}

interface ShadowRuntimeState {
  updateContext(context: PluginHostContext): void;
}

function currentRootTheme(): 'light' | 'dark' {
  return document.documentElement.classList.contains('dark') ? 'dark' : 'light';
}

/**
 * 创建绑定单个插件贡献的桥接：call 走 T003 白名单通道；
 * on 经 bridge_subscribe 订阅 + 全局 bridge_event 分发，dispose 时全部退订。
 */
function createHostBridge(
  pluginId: string,
  getContext: () => PluginHostContext,
): DisposableHostBridge {
  let disposed = false;
  const channelHandlers = new Map<string, Set<(payload: string) => void>>();
  const requestedChannels = new Set<string>();
  const registeredChannels = new Set<string>();
  const pendingSubscriptions = new Set<Promise<void>>();
  let unlistenEvent: (() => void) | null = null;
  let eventListeningTask: Promise<void> | null = null;

  const ensureEventListening = () => {
    if (unlistenEvent || disposed) return Promise.resolve();
    if (eventListeningTask) return eventListeningTask;
    eventListeningTask = api.onBridgeEvent((event) => {
      if (event.plugin_id !== pluginId) return;
      channelHandlers.get(event.channel)?.forEach((handler) => handler(event.payload));
    }).then((stop) => {
      if (disposed) {
        stop();
      } else {
        unlistenEvent = stop;
      }
    }).finally(() => {
      eventListeningTask = null;
    });
    return eventListeningTask;
  };

  return {
    async call(method, payload) {
      if (disposed) throw new Error('bridge 已随容器卸载');
      // 插件常在 bridge.on 后立即启动会产生通知的 sidecar。先等宿主订阅
      // 真正生效，避免首批输出在 invoke 与 subscribe 的竞态中丢失。
      await Promise.all([...pendingSubscriptions]);
      if (disposed) throw new Error('bridge 已随容器卸载');
      const sessionId = getContext().session?.id;
      return sessionId
        ? api.bridgeCall(pluginId, method, payload, sessionId)
        : api.bridgeCall(pluginId, method, payload);
    },
    on(channel, handler) {
      let handlers = channelHandlers.get(channel);
      if (!handlers) {
        handlers = new Set();
        channelHandlers.set(channel, handlers);
      }
      handlers.add(handler);
      if (!requestedChannels.has(channel)) {
        requestedChannels.add(channel);
        // bridgeSubscribe 会同步触发待处理工具调用重放，必须先让全局事件
        // 监听器就绪，否则首次后台挂载时重放事件可能在监听建立前丢失。
        const subscription = ensureEventListening()
          .then(async () => {
            if (disposed || !requestedChannels.has(channel)) return;
            await api.bridgeSubscribe(pluginId, channel);
            // 容器可能在异步订阅期间卸载。只有仍存活的请求才登记；否则
            // 立即成对退订，避免宿主留下没有前端处理器的虚假订阅者。
            if (disposed || !requestedChannels.has(channel)) {
              await api.bridgeUnsubscribe(pluginId, channel).catch(() => {});
              return;
            }
            registeredChannels.add(channel);
          })
          .catch((error) => {
            console.warn(`[plugin-sandbox] 订阅 ${channel} 失败:`, error);
          })
          .then(() => undefined)
          .finally(() => pendingSubscriptions.delete(subscription));
        pendingSubscriptions.add(subscription);
      }
      return () => {
        handlers?.delete(handler);
        if (handlers?.size === 0) {
          channelHandlers.delete(channel);
          requestedChannels.delete(channel);
          if (registeredChannels.delete(channel)) {
            api.bridgeUnsubscribe(pluginId, channel).catch(() => {});
          }
        }
      };
    },
    dispose() {
      disposed = true;
      channelHandlers.clear();
      requestedChannels.clear();
      pendingSubscriptions.clear();
      for (const channel of registeredChannels) {
        api.bridgeUnsubscribe(pluginId, channel).catch(() => {});
      }
      registeredChannels.clear();
      unlistenEvent?.();
      unlistenEvent = null;
    },
  };
}

/** Shadow DOM 沙箱容器：入口 HTML 注入 shadow root，脚本受控执行。 */
function ShadowContainer({
  pluginId,
  contributionId,
  html,
  className,
  sessionId,
  workspace,
  instanceId,
  visible,
  message,
}: Omit<PluginSandboxProps, 'sandbox'>) {
  const hostRef = useRef<HTMLDivElement>(null);
  const runtimeRef = useRef<ShadowRuntimeState | null>(null);

  // 挂载/重建：解析 HTML → 取回外链资源 → 注入 shadow root → 按序执行脚本。
  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const shadow = host.shadowRoot ?? host.attachShadow({ mode: 'open' });
    let currentContext = hostContext(
      currentRootTheme(),
      `shadow:${pluginId}:${contributionId}`,
      sessionId,
      workspace,
      instanceId && typeof visible === 'boolean'
        ? { instance_id: instanceId, visible }
        : undefined,
      message,
    );
    const bridge = createHostBridge(pluginId, () => currentContext);
    let cancelled = false;
    const contextHandlers = new Set<HostContextHandler>();
    const cleanups: ShadowCleanup[] = [];
    const beforeCloseHandlers = new Set<ShadowBeforeClose>();
    if (instanceId) beforeCloseHandlersByInstance.set(instanceId, beforeCloseHandlers);

    const runCleanup = (cleanup: ShadowCleanup) => {
      try {
        cleanup();
      } catch (error) {
        console.warn('[plugin-sandbox] 插件卸载回调执行失败:', error);
      }
    };

    const registerCleanup = (cleanup: ShadowCleanup) => {
      if (cancelled) {
        runCleanup(cleanup);
        return;
      }
      cleanups.push(cleanup);
    };

    const registerBeforeClose = (handler: ShadowBeforeClose) => {
      if (!cancelled) beforeCloseHandlers.add(handler);
    };

    const onHostContextChange = (handler: HostContextHandler) => {
      if (cancelled) return () => {};
      contextHandlers.add(handler);
      handler(currentContext);
      return () => contextHandlers.delete(handler);
    };

    const runtime: ShadowRuntimeState = {
      updateContext(context) {
        const contextChanged = currentContext.session?.id !== context.session?.id
          || currentContext.session?.workspace !== context.session?.workspace
          || currentContext.app?.instance_id !== context.app?.instance_id
          || currentContext.app?.visible !== context.app?.visible
          || currentContext.message?.id !== context.message?.id
          || currentContext.message?.text !== context.message?.text;
        currentContext = context;
        if (!contextChanged) return;
        contextHandlers.forEach((handler) => handler(context));
      },
    };
    runtimeRef.current = runtime;

    void mountShadowContent(
      shadow,
      html,
      pluginId,
      contributionId,
      bridge,
      () => currentContext,
      onHostContextChange,
      registerCleanup,
      registerBeforeClose,
      () => cancelled,
    );

    return () => {
      cancelled = true;
      if (runtimeRef.current === runtime) runtimeRef.current = null;
      if (instanceId && beforeCloseHandlersByInstance.get(instanceId) === beforeCloseHandlers) {
        beforeCloseHandlersByInstance.delete(instanceId);
      }
      beforeCloseHandlers.clear();
      for (const cleanup of cleanups.reverse()) runCleanup(cleanup);
      contextHandlers.clear();
      bridge.dispose();
      shadow.innerHTML = '';
    };
    // 主题变量从 App 根节点自动继承，主题和会话变化都不重建插件页面。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pluginId, contributionId, html]);

  // 会话切换时刷新运行时上下文；主题样式由 CSS 继承自动更新。
  useEffect(() => {
    runtimeRef.current?.updateContext(
      hostContext(
        currentRootTheme(),
        `shadow:${pluginId}:${contributionId}`,
        sessionId,
        workspace,
        instanceId && typeof visible === 'boolean'
          ? { instance_id: instanceId, visible }
          : undefined,
        message,
      ),
    );
  }, [contributionId, instanceId, message, pluginId, sessionId, visible, workspace]);

  return (
    <div
      ref={hostRef}
      data-plugin-shadow-host={`${pluginId}:${contributionId}`}
      className={className ?? 'block h-full min-h-0 min-w-0 w-full flex-1 overflow-auto'}
    />
  );
}

/**
 * 把入口 HTML 渲染进 shadow root：
 * 1. DOMParser 解析；`<link rel=stylesheet>` 与 `<script src>` 经宿主按
 *    插件目录安全读取（外链资源不落任意 URL，见 read_manifest_resource）；
 * 2. 其余节点（含内联 style）按序导入 shadow root；
 * 3. 脚本（内联 + 取回的外链）按文档顺序拼接后受控执行。旧脚本仍可只使用
 *    `bridge`；新脚本还可使用 `pluginRoot`、宿主上下文订阅与卸载登记。
 *
 * 图片等媒体资源暂不代理（后续按需扩展），当前面向脚本与样式驱动的页面。
 */
async function mountShadowContent(
  shadow: ShadowRoot,
  html: string,
  pluginId: string,
  contributionId: string,
  bridge: HostBridge,
  getHostContext: () => PluginHostContext,
  onHostContextChange: (handler: HostContextHandler) => () => void,
  registerCleanup: (cleanup: ShadowCleanup) => void,
  registerBeforeClose: (handler: ShadowBeforeClose) => void,
  isCancelled: () => boolean,
) {
  const doc = new DOMParser().parseFromString(html, 'text/html');
  const nodes = [...doc.head.childNodes, ...doc.body.childNodes];
  const scripts: string[] = [];

  const fetchText = async (path: string): Promise<string> => {
    const resource = await api.pluginReadEntryResource(pluginId, contributionId, path);
    // 字节数组 → 文本（脚本/样式按 UTF-8 解码）
    return new TextDecoder().decode(new Uint8Array(resource.data));
  };

  for (const node of nodes) {
    if (isCancelled()) return;
    if (!(node instanceof Element)) {
      if (node.textContent?.trim()) shadow.appendChild(document.importNode(node, true));
      continue;
    }
    if (node.tagName === 'SCRIPT') {
      const src = node.getAttribute('src');
      if (src) {
        try {
          const code = await fetchText(src);
          scripts.push(code);
        } catch (error) {
          console.warn(`[plugin-sandbox] 加载脚本 ${src} 失败:`, error);
        }
      } else if (node.textContent) {
        scripts.push(node.textContent);
      }
      continue;
    }
    if (node.tagName === 'LINK' && node.getAttribute('rel')?.toLowerCase() === 'stylesheet') {
      const href = node.getAttribute('href');
      if (href) {
        try {
          const css = await fetchText(href);
          const style = document.createElement('style');
          style.setAttribute('data-source-href', href);
          style.textContent = css;
          shadow.appendChild(style);
        } catch (error) {
          console.warn(`[plugin-sandbox] 加载样式 ${href} 失败:`, error);
        }
      }
      continue;
    }
    shadow.appendChild(document.importNode(node, true));
  }

  if (isCancelled() || scripts.length === 0) return;
  try {
    const combined = scripts.join('\n;\n');
    const runner = new Function(
      'bridge',
      'pluginRoot',
      'hostContext',
      'onHostContextChange',
      'registerCleanup',
      'registerBeforeClose',
      `"use strict";\n${combined}`,
    );
    const returnedCleanup = runner.call(
      shadow,
      bridge,
      shadow,
      getHostContext(),
      onHostContextChange,
      registerCleanup,
      registerBeforeClose,
    );
    if (typeof returnedCleanup === 'function') registerCleanup(returnedCleanup as ShadowCleanup);
  } catch (error) {
    console.error('[plugin-sandbox] 插件脚本执行失败:', error);
  }
}
