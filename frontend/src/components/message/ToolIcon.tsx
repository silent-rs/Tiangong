import { useEffect, useState } from "react";
import { createLucideIcon, type LucideIcon } from "lucide-react";
import { DynamicIcon, iconNames, type IconName } from "lucide-react/dynamic";
import { api, type ToolIconEntry } from "@/api/tauri";
import { baseToolName } from "./toolDisplayModel";

/**
 * 工具行图标：来自插件运行时的工具图标查询表（插件声明优先，其次 runtime
 * 内置表）。查不到时使用默认图标「工具交叉」。
 */

/** 默认图标的标识（data-tool-icon）。 */
export const DEFAULT_ICON = "tools-crossed";

/** 默认工具图标：扳手与螺丝刀交叉。 */
export const ToolsCrossedIcon: LucideIcon = createLucideIcon("tools-crossed", [
  [
    "path",
    {
      d: "M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.106-3.105c.32-.322.863-.22.983.218a6 6 0 0 1-8.259 7.057l-7.91 7.91a1 1 0 0 1-2.999-3l7.91-7.91a6 6 0 0 1 7.057-8.259c.438.12.54.662.219.984z",
      key: "wrench",
    },
  ],
  ["path", { d: "M3 3l1.5 1.5", key: "tip" }],
  ["path", { d: "M4.5 4.5 13 13", key: "shaft" }],
  [
    "path",
    {
      d: "m13 15 2-2a1.4 1.4 0 0 1 2 0l3.5 3.5a1.4 1.4 0 0 1 0 2l-2 2a1.4 1.4 0 0 1-2 0L13 17a1.4 1.4 0 0 1 0-2z",
      key: "handle",
    },
  ],
]);

type ToolIconTable = Record<string, ToolIconEntry>;

let tableTask: Promise<ToolIconTable> | null = null;
const tableListeners = new Set<(table: ToolIconTable) => void>();
let pluginChangeListening = false;

function loadTable(): Promise<ToolIconTable> {
  // 包一层 Promise：调用本身同步抛错（非宿主环境）时同样回落默认图标。
  const task = Promise.resolve().then(() => api.listToolIcons()).catch((error) => {
    console.warn("[tool-icons] 读取工具图标表失败，使用默认图标", error);
    return {} as ToolIconTable;
  });
  tableTask = task;
  void task.then((table) => {
    if (tableTask === task) tableListeners.forEach((listener) => listener(table));
  });
  return task;
}

function ensurePluginChangeListening() {
  if (pluginChangeListening) return;
  pluginChangeListening = true;
  // 插件安装/启停/升级后重新拉取，并清空资源图标缓存。
  window.addEventListener("tiangong:plugin-changed", () => {
    resourceUrlCache.clear();
    void loadTable();
  });
}

/** 订阅工具图标查询表（模块级共享，未加载完成前为空表）。 */
export function useToolIconTable(): ToolIconTable {
  const [table, setTable] = useState<ToolIconTable>({});
  useEffect(() => {
    let disposed = false;
    ensurePluginChangeListening();
    const listener = (next: ToolIconTable) => setTable(next);
    tableListeners.add(listener);
    void (tableTask ?? loadTable()).then((loaded) => {
      if (!disposed) setTable(loaded);
    });
    return () => {
      disposed = true;
      tableListeners.delete(listener);
    };
  }, []);
  return table;
}

/** 按工具名查表：先对外名（可能带插件前缀），再按原名；查不到返回 null。 */
export function lookupToolIcon(table: ToolIconTable, toolName: string): ToolIconEntry | null {
  if (!toolName) return null;
  return table[toolName] ?? table[baseToolName(toolName)] ?? null;
}

const ICON_NAMES: ReadonlySet<string> = new Set(iconNames);

function isIconResource(icon: string): boolean {
  return icon.includes("/") || icon.includes(".");
}

const resourceUrlCache = new Map<string, Promise<string | null>>();

function loadResourceUrl(pluginId: string, icon: string): Promise<string | null> {
  const key = `${pluginId}:${icon}`;
  let task = resourceUrlCache.get(key);
  if (!task) {
    task = api
      .pluginReadToolIcon(pluginId, icon)
      .then((resource) =>
        URL.createObjectURL(new Blob([new Uint8Array(resource.data)], { type: resource.mime })),
      )
      .catch((error) => {
        console.warn(`插件 ${pluginId} 工具图标 ${icon} 加载失败：`, error);
        return null;
      });
    resourceUrlCache.set(key, task);
  }
  return task;
}

function ResourceIcon({ pluginId, icon, className }: { pluginId: string; icon: string; className: string }) {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    let active = true;
    void loadResourceUrl(pluginId, icon).then((next) => {
      if (active) setUrl(next);
    });
    return () => {
      active = false;
    };
  }, [pluginId, icon]);
  if (!url) return <ToolsCrossedIcon className={className} data-tool-icon={DEFAULT_ICON} />;
  return (
    <img src={url} alt="" className={`${className} object-contain`} draggable={false} data-tool-icon={icon} />
  );
}

/** 工具行图标：查表得到的图标名/资源，查不到或加载失败时为默认图标。 */
export function ToolIcon({ toolName, className }: { toolName: string; className: string }) {
  const table = useToolIconTable();
  const entry = lookupToolIcon(table, toolName);
  if (entry && isIconResource(entry.icon)) {
    return entry.plugin_id ? (
      <ResourceIcon pluginId={entry.plugin_id} icon={entry.icon} className={className} />
    ) : (
      <ToolsCrossedIcon className={className} data-tool-icon={DEFAULT_ICON} />
    );
  }
  if (entry && ICON_NAMES.has(entry.icon)) {
    return (
      <DynamicIcon
        name={entry.icon as IconName}
        className={className}
        data-tool-icon={entry.icon}
        fallback={() => <ToolsCrossedIcon className={className} data-tool-icon={DEFAULT_ICON} />}
      />
    );
  }
  return <ToolsCrossedIcon className={className} data-tool-icon={DEFAULT_ICON} />;
}

/** 测试辅助：清空图标表缓存。 */
export function resetToolIconCache() {
  tableTask = null;
  resourceUrlCache.clear();
}
