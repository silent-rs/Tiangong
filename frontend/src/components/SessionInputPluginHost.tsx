import { useCallback, useEffect, useState } from 'react';
import { api, type SlotContributionEntry } from '@/api/tauri';
import { useStore } from '@/store/useStore';
import { PluginSandbox } from './PluginSandbox';

interface SessionInputPluginHostProps {
  slot:
    | 'session.input-action'
    | 'session.before-input'
    | 'session.after-input'
    | 'session.input-status';
}

/** 会话输入区 Slot 宿主：挂载已安装插件声明的输入辅助贡献。 */
export function SessionInputPluginHost({ slot }: SessionInputPluginHostProps) {
  const [items, setItems] = useState<Array<SlotContributionEntry & { html: string }>>([]);
  // 输入区贡献（如分支指示器）依赖当前会话工作区，与 extension.tab 宿主同源。
  const activeSessionId = useStore((s) => s.activeSessionId);
  const sessionCwd = useStore((s) => s.sessionCwd);
  const workspaceDir = useStore((s) => s.workspaceDir);

  const refresh = useCallback(async () => {
    const contributions = await api.listSlotContributions(slot);
    const loaded = await Promise.all(contributions.map(async (item) => ({
      ...item,
      html: item.source === 'manifest'
        ? await api.pluginOpenEntry(item.plugin_id, item.contribution_id)
        : await api.pluginOpenEntry(item.plugin_id, item.contribution_id),
    })));
    setItems(loaded);
  }, [slot]);

  useEffect(() => {
    let disposed = false;
    const reloadAll = () => {
      void refresh().catch((error) => {
        if (!disposed) console.warn(`[session-input-plugin] 加载 ${slot} 失败`, error);
      });
    };
    const reloadPlugin = (event: Event) => {
      const { pluginId } = (event as CustomEvent<{ pluginId: string }>).detail;
      void api.listSlotContributions(slot).then(async (contributions) => {
        if (disposed) return;
        const target = contributions.filter((item) => item.plugin_id === pluginId);
        const loaded = await Promise.all(target.map(async (item) => ({
          ...item,
          html: item.source === 'manifest'
            ? await api.pluginOpenEntry(item.plugin_id, item.contribution_id)
            : await api.pluginOpenEntry(item.plugin_id, item.contribution_id),
        })));
        if (!disposed) {
          setItems((current) => [
            ...current.filter((item) => item.plugin_id !== pluginId),
            ...loaded,
          ]);
        }
      }).catch((error) => {
        if (!disposed) console.warn(`[session-input-plugin] 刷新 ${pluginId} 失败`, error);
      });
    };
    reloadAll();
    window.addEventListener('tiangong:plugin-changed', reloadPlugin);
    return () => {
      disposed = true;
      window.removeEventListener('tiangong:plugin-changed', reloadPlugin);
    };
  }, [refresh, slot]);

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
          workspace={sessionCwd || workspaceDir || null}
          className={
            slot === 'session.input-action'
              ? 'h-8 w-8 shrink-0 overflow-hidden'
              : slot === 'session.input-status'
                ? 'relative inline-flex h-5 shrink-0 items-center overflow-visible'
                : undefined
          }
        />
      ))}
    </>
  );
}
