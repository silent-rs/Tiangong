import { useEffect, useState } from "react";
import { api, type ToolTitleEntry } from "@/api/tauri";
import { baseToolName } from "./toolDisplayModel";

/**
 * 工具行标题：来自插件运行时的标题查询表（plugin.json 顶层 `tool_titles`
 * 声明）。与 `session.message-item` 视图贡献的 `titles` 同键语义，供无视图
 * 插件声明；调用方先查视图声明再查本表，均未声明时使用通用标题「工具调用」。
 */

type ToolTitleTable = Record<string, ToolTitleEntry>;

let tableTask: Promise<ToolTitleTable> | null = null;
const tableListeners = new Set<(table: ToolTitleTable) => void>();
let pluginChangeListening = false;

function loadTable(): Promise<ToolTitleTable> {
  const task = Promise.resolve().then(() => api.listToolTitles()).catch((error) => {
    console.warn("[tool-titles] 读取工具标题表失败，使用通用标题", error);
    return {} as ToolTitleTable;
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
  window.addEventListener("tiangong:plugin-changed", () => void loadTable());
}

/** 订阅工具标题查询表（模块级共享，未加载完成前为空表）。 */
export function useToolTitleTable(): ToolTitleTable {
  const [table, setTable] = useState<ToolTitleTable>({});
  useEffect(() => {
    let disposed = false;
    ensurePluginChangeListening();
    const listener = (next: ToolTitleTable) => setTable(next);
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

/** 特指声明 `参数名 → 取值 → 标题` 是否命中本次调用参数。 */
function variantTitle(entry: ToolTitleEntry, args: unknown): string | null {
  if (!entry.variants || !args || typeof args !== "object" || Array.isArray(args)) return null;
  const record = args as Record<string, unknown>;
  for (const [arg, values] of Object.entries(entry.variants)) {
    const value = record[arg];
    if (value === null || value === undefined) continue;
    if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {
      const title = values[String(value)];
      if (title) return title;
    }
  }
  return null;
}

/** 按工具名查表并按调用参数选标题：先对外名再原名；未声明返回 null。 */
export function manifestToolTitle(
  table: ToolTitleTable,
  toolName: string,
  args?: unknown,
): string | null {
  if (!toolName) return null;
  const entry = table[toolName] ?? table[baseToolName(toolName)] ?? null;
  if (!entry) return null;
  return variantTitle(entry, args) ?? (entry.title || null);
}

/** 测试辅助：清空标题表缓存。 */
export function resetToolTitleCache() {
  tableTask = null;
}
