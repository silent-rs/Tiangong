import { memo, useMemo } from 'react';
import { useStore } from '@/store/useStore';
import { useSlotContributions } from '@/hooks/useSlotContributions';
import { PluginSandbox } from './PluginSandbox';
import type { HostMessageContext } from './pluginHostContext';

interface MessagePluginHostProps {
  slot: 'session.message-action' | 'session.message-item';
  message: HostMessageContext;
}

/**
 * 消息级 Slot 宿主：为单条消息挂载插件贡献（操作按钮 / 附加区），并以
 * `message` 上下文告知插件当前是哪条消息。宿主不解析插件行为。
 */
function MessagePluginHostView({ slot, message }: MessagePluginHostProps) {
  const items = useSlotContributions(slot).filter((item) => item.render !== 'replace');
  const activeSessionId = useStore((s) => s.activeSessionId);
  // 上下文对象保持稳定引用：内容不变时不触发插件上下文更新。
  const context = useMemo(
    () => message,
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [message.id, message.role, message.text, JSON.stringify(message.attachments)],
  );
  if (items.length === 0) return null;
  return (
    <>
      {items.map((item) => (
        <PluginSandbox
          key={`${item.plugin_id}:${item.contribution_id}`}
          pluginId={item.plugin_id}
          contributionId={item.contribution_id}
          sandbox={item.sandbox}
          html={item.html}
          sessionId={activeSessionId ?? null}
          message={context}
          className={
            slot === 'session.message-action'
              ? 'inline-flex h-6 shrink-0 items-center overflow-visible'
              : 'block w-full'
          }
        />
      ))}
    </>
  );
}

export const MessagePluginHost = memo(MessagePluginHostView);
