import { useEffect, useState, useCallback } from 'react';
import { api, type SlotContributionEntry } from '../api/tauri';
import { PluginIframe } from './PluginIframe';
import { PluginSandbox } from './PluginSandbox';

/**
 * 插件设置面板：settings.plugin-page 挂载点的通用容器。
 *
 * 天工不处理插件页面内容，只提供容器和桥接：
 * - 经 listSlotContributions 按 Slot 读取贡献（v1 WASM 声明 + v2 manifest 声明）
 * - 用户点击进入时按贡献来源取页面 HTML → 渲染到 iframe（srcdoc）
 * - iframe 内 JS 经 postMessage → 本组件 → bridgeCall("plugin.*") → WASM
 *
 * 所有业务逻辑在 WASM 插件内部处理，天工完全通用。
 */
export function PluginSettingsPanel() {
  const [contributions, setContributions] = useState<SlotContributionEntry[]>([]);
  const [selected, setSelected] = useState<SlotContributionEntry | null>(null);
  const [html, setHtml] = useState<string>('');
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    api.listSlotContributions('settings.plugin-page')
      .then((entries) => {
        setContributions(entries);
      })
      .catch(() => setContributions([]))
      .finally(() => setLoading(false));
  }, []);

  // 用户选中某个插件时，按贡献来源获取页面 HTML：
  // wasm 来源走插件自身 open-view，manifest 来源读清单声明的 entry 文件。
  const selectContribution = useCallback(async (entry: SlotContributionEntry) => {
    setSelected(entry);
    if (!entry.has_view) {
      setHtml('');
      return;
    }
    try {
      const pageHtml = entry.source === 'manifest'
        ? await api.pluginOpenEntry(entry.plugin_id, entry.contribution_id)
        : await api.pluginOpenEntry(entry.plugin_id, entry.contribution_id);
      setHtml(pageHtml);
    } catch {
      setHtml('<p style="padding:16px;color:#888">页面加载失败</p>');
    }
  }, []);

  if (loading) {
    return <div className="p-4 text-sm text-muted-foreground">加载插件列表…</div>;
  }

  if (contributions.length === 0) {
    return (
      <div className="p-4 text-sm text-muted-foreground">
        暂无已加载的 WASM 插件。
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 min-w-0 overflow-hidden">
      {/* 左侧：插件列表 */}
      <div className="w-48 border-r pr-2 space-y-1">
        {contributions.map((entry) => (
          <button
            key={`${entry.plugin_id}:${entry.contribution_id}`}
            onClick={() => selectContribution(entry)}
            className={`w-full text-left px-3 py-2 rounded-md text-sm transition-colors ${
              selected?.plugin_id === entry.plugin_id
                ? 'bg-accent text-accent-foreground'
                : 'hover:bg-accent/50'
            }`}
          >
            {entry.title}
          </button>
        ))}
      </div>

      {/* 右侧：插件页面（沙箱容器） */}
      <div className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
        {selected && html ? (
          selected.source === 'manifest' ? (
            <PluginSandbox
              pluginId={selected.plugin_id}
              contributionId={selected.contribution_id}
              sandbox={selected.sandbox}
              html={html}
            />
          ) : (
            <PluginIframe pluginId={selected.plugin_id} html={html} />
          )
        ) : selected ? (
          <div className="p-4 text-sm text-muted-foreground">该插件无可配置页面。</div>
        ) : (
          <div className="p-4 text-sm text-muted-foreground">请从左侧选择一个插件。</div>
        )}
      </div>
    </div>
  );
}
