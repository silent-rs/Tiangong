import { memo, useMemo, type ReactNode } from 'react';
import type { MessageRender } from '@/api/message';
import { useStore } from '@/store/useStore';
import { useSlotContributions } from '@/hooks/useSlotContributions';
import { PluginSandbox } from './PluginSandbox';
import type { HostMessageContext } from './pluginHostContext';

const MESSAGE_ITEM_SLOT = 'session.message-item' as const;

interface MessageRenderHostProps {
  /** 消息的插件渲染声明（`meta.render`）。 */
  render: MessageRender;
  message: HostMessageContext;
  /** 插件缺失、停用或未声明该视图时的默认显示。 */
  fallback: ReactNode;
  className?: string;
}

/**
 * 插件接管的消息显示：按 `render.plugin` / `render.view` 找到声明
 * `render: "replace"` 的 `session.message-item` 贡献并挂载，替换默认显示；
 * 找不到时原样渲染 `fallback`。渲染数据经消息上下文的 `render.data` 交给插件。
 */
function MessageRenderHostView({ render, message, fallback, className }: MessageRenderHostProps) {
  const items = useSlotContributions(MESSAGE_ITEM_SLOT);
  const activeSessionId = useStore((s) => s.activeSessionId);
  const item = items.find((candidate) =>
    candidate.render === 'replace'
    && candidate.plugin_id === render.plugin
    && candidate.contribution_id === render.view,
  );
  const dataKey = JSON.stringify(render.data ?? null);
  const context = useMemo<HostMessageContext>(
    () => ({ ...message, render: { view: render.view, data: render.data } }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [message.id, message.role, message.text, JSON.stringify(message.attachments), render.view, dataKey],
  );
  if (!item) return <>{fallback}</>;
  return (
    <PluginSandbox
      pluginId={item.plugin_id}
      contributionId={item.contribution_id}
      sandbox={item.sandbox}
      html={item.html}
      sessionId={activeSessionId ?? null}
      message={context}
      className={className ?? 'block w-full'}
    />
  );
}

export const MessageRenderHost = memo(MessageRenderHostView);
