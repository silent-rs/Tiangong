import { useStore } from '@/store/useStore';
import { useSlotContributions } from '@/hooks/useSlotContributions';
import { PluginSandbox } from './PluginSandbox';

/**
 * 全局状态栏插件宿主（`global.status-item`）：顶部状态栏右侧的插件状态项，
 * 如自动朗读开关。插件以当前会话为上下文，宿主不解析插件行为。
 */
export function GlobalStatusPluginHost() {
  const items = useSlotContributions('global.status-item');
  const activeSessionId = useStore((s) => s.activeSessionId);
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
          className="inline-flex h-6 shrink-0 items-center overflow-visible"
        />
      ))}
    </>
  );
}
