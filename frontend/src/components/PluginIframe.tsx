import { useEffect, useRef, useCallback, useMemo } from 'react';
import { api } from '../api/tauri';
import { useResolvedTheme } from '../hooks/useTheme';
import { usePluginMask } from '../hooks/usePluginMask';
import { hostContext, type HostMessageContext } from './pluginHostContext';

const pluginCallQueues = new Map<string, Promise<void>>();

/**
 * 插件 iframe 容器 + postMessage 桥接（sandbox: "iframe" 强隔离模式）。
 *
 * iframe 内的 JS 经 window.parent.postMessage({ type: 'plugin_call', ... }) 发消息：
 * - 完整命名空间方法（plugin./storage./session./…）：直接透传宿主桥接；
 * - 裸方法名（旧版插件页面协议）：补 plugin. 前缀转发到本插件 WASM。
 * 订阅消息（plugin_subscribe/plugin_unsubscribe）对应宿主事件通道，
 * 宿主 bridge_event 经 onBridgeEvent 回推本 iframe。天工不解析业务负载。
 */
const BRIDGE_METHOD_NAMESPACES = ['plugin.', 'storage.', 'session.', 'tool.', 'sidecar.'];

/** 旧协议裸方法名补 plugin. 前缀；SDK 的完整命名空间方法透传。 */
export function normalizeBridgeMethod(method: string): string {
  if (BRIDGE_METHOD_NAMESPACES.some((prefix) => method.startsWith(prefix))) {
    return method;
  }
  return `plugin.${method}`;
}
export function PluginIframe({
  pluginId,
  html,
  sessionId,
  workspace,
  instanceId,
  visible,
  message,
}: {
  pluginId: string;
  html: string;
  sessionId?: string | null;
  workspace?: string | null;
  instanceId?: string;
  visible?: boolean;
  message?: HostMessageContext;
}) {
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const theme = useResolvedTheme();
  const channel = useMemo(() => crypto.randomUUID(), [pluginId, html]);
  const maskColor = usePluginMask(iframeRef, channel);
  const sendHostContext = useCallback(() => {
    iframeRef.current?.contentWindow?.postMessage(
      hostContext(
        theme,
        channel,
        sessionId,
        workspace,
        instanceId && typeof visible === 'boolean'
          ? { instance_id: instanceId, visible }
          : undefined,
        message,
      ),
      '*',
    );
  }, [channel, instanceId, message, sessionId, theme, visible, workspace]);

  useEffect(() => {
    sendHostContext();
  }, [sendHostContext]);

  // 宿主事件回推：bridge_event 按插件过滤后转发给本 iframe
  useEffect(() => {
    let disposed = false;
    let stop: (() => void) | null = null;
    void api.onBridgeEvent((bridgeEvent) => {
      if (bridgeEvent.plugin_id !== pluginId) return;
      iframeRef.current?.contentWindow?.postMessage(
        {
          type: 'bridge_event',
          channel: bridgeEvent.channel,
          payload: bridgeEvent.payload,
        },
        '*',
      );
    }).then((unlisten) => {
      if (disposed) {
        unlisten();
      } else {
        stop = unlisten;
      }
    });
    return () => {
      disposed = true;
      stop?.();
    };
  }, [pluginId]);

  useEffect(() => {
    const source = iframeRef.current?.contentWindow;
    if (!source) return;
    const handler = (event: MessageEvent) => {
      if (event.source !== source) return;
      const data = event.data;
      // 事件订阅/退订（iframe 容器经 postMessage 发起，宿主做能力校验）
      if (data && data.type === 'plugin_subscribe' && typeof data.event === 'string'
        && data.event.length > 0 && data.event.length <= 128 && data.channel === channel) {
        api.bridgeSubscribe(pluginId, data.event).catch(console.error);
        return;
      }
      if (data && data.type === 'plugin_unsubscribe' && typeof data.event === 'string'
        && data.channel === channel) {
        api.bridgeUnsubscribe(pluginId, data.event).catch(console.error);
        return;
      }
      if (!data || data.type !== 'plugin_call' || data.channel !== channel) return;
      const { id, method, payload } = data;
      if (
        typeof id !== 'string'
        || typeof method !== 'string'
        || typeof payload !== 'string'
        || id.length > 200
        || method.length > 100
        || payload.length > 2_000_000
      ) return;

      const queue = pluginCallQueues.get(pluginId) ?? Promise.resolve();
      pluginCallQueues.set(pluginId, queue.then(async () => {
        try {
          const bridgeMethod = normalizeBridgeMethod(method);
          const result = sessionId
            ? await api.bridgeCall(pluginId, bridgeMethod, payload, sessionId)
            : await api.bridgeCall(pluginId, bridgeMethod, payload);
          source.postMessage({ id, channel, result }, '*');
        } catch (e) {
          source.postMessage(
            { id, channel, error: String(e) },
            '*',
          );
        }
      }));
    };

    window.addEventListener('message', handler);
    return () => {
      window.setTimeout(() => window.removeEventListener('message', handler), 1000);
    };
  }, [channel, pluginId, sessionId]);

  return (
    <div className="flex h-full min-h-0 min-w-0 w-full flex-1 flex-col overflow-hidden">
      {maskColor && (
        <div
          aria-hidden="true"
          data-plugin-host-mask
          className="fixed inset-0 z-[90]"
          style={{ backgroundColor: maskColor }}
        />
      )}
      <iframe
        ref={iframeRef}
        title="plugin-view"
        srcDoc={html}
        className={`block min-h-0 min-w-0 w-full flex-1 border-0 ${maskColor ? 'relative z-[91]' : ''}`}
        sandbox="allow-scripts"
        onLoad={sendHostContext}
      />
    </div>
  );
}
