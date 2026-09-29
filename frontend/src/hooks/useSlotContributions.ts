import { useCallback, useEffect, useState } from 'react';
import { api, type SlotContributionEntry, type SlotId } from '@/api/tauri';

export interface LoadedSlotContribution extends SlotContributionEntry {
  html: string;
}

async function loadEntries(
  slot: SlotId,
  filter?: (item: SlotContributionEntry) => boolean,
): Promise<LoadedSlotContribution[]> {
  const contributions = (await api.listSlotContributions(slot)).filter(
    (item) => !filter || filter(item),
  );
  return Promise.all(contributions.map(async (item) => ({
    ...item,
    html: item.source === 'manifest'
      ? await api.pluginOpenEntry(item.plugin_id, item.contribution_id)
      : await api.pluginOpenView(item.plugin_id, item.contribution_id),
  })));
}

/**
 * 加载某个 Slot 的全部插件贡献（含入口 HTML），插件安装/启停/重载后按插件增量刷新。
 *
 * 同一 Slot 的多个宿主实例（如每条消息的操作区）共享同一份缓存，避免
 * 每条消息各自读取插件入口。
 */
const slotCache = new Map<SlotId, Promise<LoadedSlotContribution[]>>();
const slotListeners = new Map<SlotId, Set<(items: LoadedSlotContribution[]) => void>>();
let pluginChangeListening = false;

function publish(slot: SlotId, items: LoadedSlotContribution[]) {
  slotListeners.get(slot)?.forEach((listener) => listener(items));
}

function reloadSlot(slot: SlotId) {
  const task = loadEntries(slot);
  slotCache.set(slot, task);
  void task.then((items) => {
    if (slotCache.get(slot) === task) publish(slot, items);
  }).catch((error) => {
    console.warn(`[plugin-slot] 加载 ${slot} 失败`, error);
  });
  return task;
}

function ensurePluginChangeListening() {
  if (pluginChangeListening) return;
  pluginChangeListening = true;
  window.addEventListener('tiangong:plugin-changed', () => {
    for (const slot of slotListeners.keys()) reloadSlot(slot);
  });
}

export function useSlotContributions(slot: SlotId): LoadedSlotContribution[] {
  const [items, setItems] = useState<LoadedSlotContribution[]>([]);
  const listener = useCallback((next: LoadedSlotContribution[]) => setItems(next), []);
  useEffect(() => {
    let disposed = false;
    ensurePluginChangeListening();
    let listeners = slotListeners.get(slot);
    if (!listeners) {
      listeners = new Set();
      slotListeners.set(slot, listeners);
    }
    listeners.add(listener);
    const task = slotCache.get(slot) ?? reloadSlot(slot);
    void task.then((loaded) => {
      if (!disposed) setItems(loaded);
    }).catch(() => {});
    return () => {
      disposed = true;
      listeners?.delete(listener);
    };
  }, [listener, slot]);
  return items;
}

/** 测试辅助：清空 Slot 缓存。 */
export function resetSlotContributionCache() {
  slotCache.clear();
}
