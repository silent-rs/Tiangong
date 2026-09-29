import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { api } from '@/api/tauri';
import { useStore } from '@/store/useStore';
import { useSlotContributions } from '@/hooks/useSlotContributions';
import { PluginSandbox } from './PluginSandbox';

interface InputOverlayPluginHostProps {
  /** 征询交互显示时让位：覆盖层隐藏但插件实例保持挂载。 */
  suppressed?: boolean;
  inputHeight?: number;
  onVisibilityChange?: (visible: boolean) => void;
}

/**
 * 输入覆盖层宿主（`session.input-overlay`，单实例）。
 *
 * 与征询插件宿主同构：插件页面始终挂载以保持自身状态，显示时覆盖输入区。
 * 显隐由插件经 `session.input.showOverlay / hideOverlay` 申请，宿主按会话
 * 记录；切换会话即回到该会话自己的状态，不解析插件业务。
 */
export function InputOverlayPluginHost({
  suppressed = false,
  inputHeight = 0,
  onVisibilityChange,
}: InputOverlayPluginHostProps) {
  const items = useSlotContributions('session.input-overlay');
  const handler = items[0] ?? null;
  const activeSessionId = useStore((s) => s.activeSessionId);
  const newConversationId = useStore((s) => s.newConversationId);
  const currentSessionId = activeSessionId ?? newConversationId ?? null;
  const [visibleSessions, setVisibleSessions] = useState<Set<string>>(() => new Set());
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!handler) {
      setVisibleSessions(new Set());
      return;
    }
    let disposed = false;
    let stop: (() => void) | null = null;
    void api.onSessionInputOverlay((event) => {
      if (event.plugin_id !== handler.plugin_id) return;
      const sessionId = event.session_id || useStore.getState().activeSessionId
        || useStore.getState().newConversationId;
      if (!sessionId) return;
      setVisibleSessions((current) => {
        const next = new Set(current);
        if (event.visible) next.add(sessionId);
        else next.delete(sessionId);
        return next;
      });
    }).then((unlisten) => {
      if (disposed) unlisten();
      else stop = unlisten;
    }).catch((error) => console.warn('[input-overlay] 监听覆盖层请求失败', error));
    return () => {
      disposed = true;
      stop?.();
    };
  }, [handler]);

  const requested = Boolean(handler && currentSessionId && visibleSessions.has(currentSessionId));
  const visible = requested && !suppressed;

  useLayoutEffect(() => {
    onVisibilityChange?.(visible);
  }, [onVisibilityChange, visible]);
  useEffect(() => () => {
    onVisibilityChange?.(false);
  }, [onVisibilityChange]);

  // 显示时把焦点交给插件内容（Shadow 容器内首个可聚焦元素 / iframe），
  // 插件内的键盘交互（如空格按住说话）才能生效；找不到时聚焦外层容器。
  useEffect(() => {
    if (!visible) return;
    const container = containerRef.current;
    if (!container) return;
    const frame = window.requestAnimationFrame(() => {
      const shadowHost = container.querySelector('[data-plugin-shadow-host]');
      const inner = shadowHost?.shadowRoot?.querySelector<HTMLElement>('[tabindex], button, input, textarea');
      const iframe = container.querySelector('iframe');
      (inner ?? iframe ?? container).focus({ preventScroll: true });
    });
    return () => window.cancelAnimationFrame(frame);
  }, [visible]);

  if (!handler) return null;
  return (
    <div
      ref={containerRef}
      tabIndex={-1}
      aria-hidden={!visible}
      aria-label={handler.title || '输入模式'}
      className={visible
        ? 'absolute inset-x-0 bottom-0 z-[55] overflow-hidden bg-background opacity-100 outline-none transition-opacity duration-150'
        : 'pointer-events-none invisible absolute inset-x-0 bottom-0 z-[55] overflow-hidden opacity-0 transition-opacity duration-150'}
      style={{ height: inputHeight > 0 ? `${inputHeight}px` : undefined }}
    >
      <div className="mx-auto box-border h-full w-full max-w-3xl px-4 py-3">
        <PluginSandbox
          pluginId={handler.plugin_id}
          contributionId={handler.contribution_id}
          sandbox={handler.sandbox}
          html={handler.html}
          sessionId={currentSessionId}
          className="block h-full min-h-0 w-full overflow-hidden"
        />
      </div>
    </div>
  );
}
