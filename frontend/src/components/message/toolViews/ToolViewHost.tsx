import { memo, useMemo } from "react";
import type { LoadedSlotContribution } from "@/hooks/useSlotContributions";
import { useSlotContributions } from "@/hooks/useSlotContributions";
import { useStore } from "@/store/useStore";
import { PluginSandbox } from "@/components/PluginSandbox";
import type { HostMessageContext } from "@/components/pluginHostContext";
import { manifestToolTitle, useToolTitleTable } from "../toolTitles";

const MESSAGE_ITEM_SLOT = "session.message-item" as const;

/** 交给工具视图的调用数据（`render.data`）：工具调用的原始信息，宿主不加工。 */
export interface ToolViewData {
  tool_call_id: string;
  /** 工具对外名（`{插件id}__{工具名}`）。 */
  tool_name: string;
  arguments: unknown;
  status: "running" | "ok" | "error";
  /** 工具原始输出（已去掉宿主结果抬头）；运行中为 null。 */
  output: string | null;
  duration_ms: number | null;
}

/** 特指键 `对外名:参数=取值` 是否命中本次调用参数（0 未命中，1 通用，2 特指）。 */
export function matchesKey(key: string, toolName: string, args: unknown): 0 | 1 | 2 {
  const separator = key.indexOf(":");
  if (separator < 0) return key === toolName ? 1 : 0;
  if (key.slice(0, separator) !== toolName) return 0;
  const [arg, ...rest] = key.slice(separator + 1).split("=");
  const expected = rest.join("=");
  if (!args || typeof args !== "object" || Array.isArray(args)) return 0;
  const actual = (args as Record<string, unknown>)[arg];
  return (typeof actual === "string" || typeof actual === "number" || typeof actual === "boolean")
    && String(actual) === expected
    ? 2
    : 0;
}

/**
 * 找到接管该工具调用的插件视图：特指声明优先于通用声明；均未声明返回 null。
 */
export function findToolView(
  items: Pick<LoadedSlotContribution, "render" | "tools">[],
  toolName: string,
  args: unknown,
): number | null {
  let best: { index: number; score: number } | null = null;
  items.forEach((item, index) => {
    if (item.render !== "replace") return;
    for (const key of item.tools ?? []) {
      const score = matchesKey(key, toolName, args);
      if (score > (best?.score ?? 0)) best = { index, score };
    }
  });
  return best === null ? null : (best as { index: number }).index;
}

/**
 * 插件在工具视图中声明的工具行标题：特指声明优先于通用声明，未声明返回 null
 * （宿主显示通用标题「工具调用」）。
 */
export function findToolTitle(
  items: Pick<LoadedSlotContribution, "render" | "titles">[],
  toolName: string,
  args: unknown,
): string | null {
  let best: { title: string; score: number } | null = null;
  for (const item of items) {
    if (item.render !== "replace") continue;
    for (const [key, title] of Object.entries(item.titles ?? {})) {
      const score = matchesKey(key, toolName, args);
      if (title && score > (best?.score ?? 0)) best = { title, score };
    }
  }
  return best === null ? null : (best as { title: string }).title;
}

/**
 * 当前工具行标题查询函数：先查工具视图声明的 `titles`（随视图演进的插件），
 * 再查清单 `tool_titles` 查询表（无视图插件），均未声明返回 null（通用标题）。
 */
export function useToolTitle(): (toolName: string, args: unknown) => string | null {
  const items = useSlotContributions(MESSAGE_ITEM_SLOT);
  const manifestTable = useToolTitleTable();
  return useMemo(
    () => (toolName: string, args: unknown) =>
      findToolTitle(items, toolName, args) ?? manifestToolTitle(manifestTable, toolName, args),
    [items, manifestTable],
  );
}

interface ToolViewHostProps {
  data: ToolViewData;
  /** 插件未提供视图时的文本默认显示。 */
  fallback: React.ReactNode;
}

/**
 * 工具调用展开区：处理该工具的插件若声明了工具视图（`session.message-item`
 * 的 replace 贡献 + `tools`），挂载其视图并把调用原始数据经消息上下文的
 * `render.data` 交给插件渲染；否则显示 fallback 文本。
 */
function ToolViewHostView({ data, fallback }: ToolViewHostProps) {
  const items = useSlotContributions(MESSAGE_ITEM_SLOT);
  const activeSessionId = useStore((s) => s.activeSessionId);
  const index = findToolView(items, data.tool_name, data.arguments);
  const item = index === null ? null : items[index];
  const dataKey = JSON.stringify(data);
  const context = useMemo<HostMessageContext | null>(
    () => item
      ? {
          id: data.tool_call_id,
          role: "tool",
          text: data.output ?? "",
          attachments: [],
          render: { view: item.contribution_id, data },
        }
      : null,
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [item?.plugin_id, item?.contribution_id, dataKey],
  );
  if (!item || !context) return <>{fallback}</>;
  return (
    <div className="block w-full" data-tool-view={`${item.plugin_id}:${item.contribution_id}`}>
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

export const ToolViewHost = memo(ToolViewHostView);
