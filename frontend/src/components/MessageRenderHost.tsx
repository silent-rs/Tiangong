import { memo, useMemo, type ReactNode } from 'react';
import type { MessageRender } from '@/api/message';
import { useStore } from '@/store/useStore';
import { useSlotContributions } from '@/hooks/useSlotContributions';
import { PluginSandbox } from './PluginSandbox';
import { cn } from '@/lib/utils';
import type { HostMessageContext } from './pluginHostContext';

const MESSAGE_ITEM_SLOT = 'session.message-item' as const;
/**
 * 插件接管消息的统一上下留白：插件视图只负责卡片本身，与相邻消息的间距
 * 由宿主统一给出（虚拟列表里每条消息单独包裹，`first:mt-0` 类间距不生效）。
 */
export const MESSAGE_RENDER_SPACING = 'py-3';

interface MessageRenderHostProps {
  /** 消息的插件渲染声明（`meta.render`）。 */
  render: MessageRender;
  message: HostMessageContext;
  /** 插件缺失、停用或未声明该视图时的默认显示。 */
  fallback: ReactNode;
  /** 容器宽度等布局类；上下留白由宿主统一追加，插件与调用方无需处理。 */
  className?: string;
}

/**
 * 插件接管的消息显示：按 `render.plugin` / `render.view` 找到声明
 * `render: "replace"` 的 `session.message-item` 贡献并挂载，替换默认显示；
 * 找不到时原样渲染 `fallback`。渲染数据经消息上下文的 `render.data` 交给插件。
 * 插件接管整条消息（含附件，经消息上下文 `attachments` 读取），宿主只提供
 * 容器与统一的上下留白。
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
    <div className={cn(className ?? 'block w-full', MESSAGE_RENDER_SPACING)} data-message-render={`${item.plugin_id}:${item.contribution_id}`}>
      <PluginSandbox
        pluginId={item.plugin_id}
        contributionId={item.contribution_id}
        sandbox={item.sandbox}
        html={item.html}
        sessionId={activeSessionId ?? null}
        message={context}
        className="block w-full"
      />
    </div>
  );
}

export const MessageRenderHost = memo(MessageRenderHostView);
