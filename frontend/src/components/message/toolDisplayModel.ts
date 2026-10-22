import { textContent } from "@/api/tauri";
import {
  durationMsOf,
  messageKind,
  toolIsError,
  toolNameOf,
} from "@/api/message";
import type { MessageItem } from "./types";
import type { ToolArgsPresentation } from "@/api/tauri";
import { EMPTY_ARGS_PRESENTATION } from "./toolArgsPresentation";

/**
 * 工具调用展示模型。
 *
 * 工具行通用展示为「工具名 + 参数摘要」，展开区由处理该工具的插件视图
 * （`session.message-item` 的 replace 贡献声明 `tools`）渲染调用与结果；
 * 插件未提供视图时以文本默认显示参数与结果。宿主不按工具名或参数映射
 * 展示方式。模型是纯函数，不碰 React。
 */

export type ToolDisplayState = "running" | "ok" | "error";

export interface ToolDisplayModel {
  /** 工具对外名（可能带插件前缀），按此查插件提供的图标；未知为空串。 */
  toolName: string;
  /** 行标题：插件声明的标题，未声明时为通用标题「工具调用」。 */
  title: string;
  /** 单行摘要：参数摘要（无参数时取结果首行）；失败时被 errorSummary 顶替显示。 */
  summary: string;
  state: ToolDisplayState;
  /** 失败结果的第一行，折叠行直接以错误色显示。 */
  errorSummary: string | null;
  /** 调用参数文本（文本默认显示用）；无参数为 null。 */
  argsText: string | null;
  /** 结果文本（工具原始输出，已去掉插件结果抬头）；运行中为 null。 */
  outputText: string | null;
  /** 单次调用耗时（毫秒）：优先事件携带值，历史消息从 trace 文本解析。 */
  durationMs: number | null;
}

/**
 * 去掉插件工具的插件前缀（`{插件id}__{工具名}` → 工具名），用于按原名查图标。
 * MCP 工具（`mcp__{server}__{tool}`）是插件自身的命名，保持不变。
 */
export function baseToolName(toolName: string): string {
  if (toolName.startsWith("mcp__")) return toolName;
  const index = toolName.indexOf("__");
  return index > 0 ? toolName.slice(index + 2) : toolName;
}

/**
 * 去掉插件为模型添加的结果抬头（首行「调用工具 X：成功」/「调用插件 P 的 X：失败」），
 * 界面只展示工具的原始输出。
 */
export function stripToolResultHeader(content: string): string {
  return content.replace(/^调用(?:工具 \S+|插件 \S+ 的 \S+)：(?:成功|失败)\n?/, "");
}

/** 摘要字符上限：多行文本压缩为单行后截断防撑爆。 */
const SUMMARY_MAX_CHARS = 500;

function firstNonEmptyLine(text: string | null | undefined): string {
  if (!text) return "";
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (trimmed) return trimmed;
  }
  return "";
}

function clamp(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

/** 多行文本压缩为单行。 */
function squeezeToSingleLine(text: string): string {
  return text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean)
    .join(" ");
}

function visibleArgs(args: unknown[], hints: ToolArgsPresentation): unknown[] {
  return args.filter(
    (item) => !(typeof item === "string" && hints.hidden_value_prefixes.some((prefix) => item.startsWith(prefix))),
  );
}

/**
 * 通用参数摘要：位置参数取各字符串项，对象参数取 `键=值`。排在前面的参数、
 * 跳过的配置参数与隐藏的内部取值均来自运行时提供的展示提示，不按工具名区分。
 */
export function argsSummary(
  args: unknown,
  hints: ToolArgsPresentation = EMPTY_ARGS_PRESENTATION,
): string | null {
  if (args === undefined || args === null) return null;
  if (typeof args === "string") return args ? clamp(squeezeToSingleLine(args), SUMMARY_MAX_CHARS) : null;
  if (Array.isArray(args)) {
    const parts = visibleArgs(args, hints)
      .filter((item): item is string => typeof item === "string" && item.length > 0)
      .map(squeezeToSingleLine);
    return parts.length ? clamp(parts.join(" "), SUMMARY_MAX_CHARS) : null;
  }
  if (typeof args !== "object") return null;
  const lead = (key: string) => {
    const index = hints.lead_args.indexOf(key);
    return index < 0 ? Number.MAX_SAFE_INTEGER : index;
  };
  const entries = Object.entries(args as Record<string, unknown>)
    .filter(([key, value]) => !hints.skip_args.includes(key) && value !== null && value !== undefined && value !== "")
    .sort(([a], [b]) => lead(a) - lead(b));
  if (entries.length === 0) return null;
  const parts = entries.map(([key, value]) => {
    const text = typeof value === "string" ? value : JSON.stringify(value);
    return hints.lead_args.includes(key) ? squeezeToSingleLine(text) : `${key}=${squeezeToSingleLine(text ?? "")}`;
  });
  return clamp(parts.join(" "), SUMMARY_MAX_CHARS);
}

/** 展开用的参数文本：对象 pretty JSON，数组逐项标注位置参数。 */
function argsToText(args: unknown): string | null {
  if (args === undefined || args === null) return null;
  if (Array.isArray(args)) {
    if (args.length === 0) return null;
    return args
      .map((item, index) => {
        const value = typeof item === "string" ? item : JSON.stringify(item);
        return `[${index}] ${value ?? ""}`;
      })
      .join("\n");
  }
  if (typeof args === "object") {
    return Object.keys(args as object).length ? JSON.stringify(args, null, 2) : null;
  }
  if (typeof args === "string" && args) return args;
  return null;
}

/** 插件未声明工具行标题时的通用标题（工具名在悬停提示与分组摘要中可见）。 */
export const DEFAULT_TOOL_TITLE = "工具调用";

/** 调用部分（标题、参数摘要、参数文本）。 */
function callPart(args: unknown, hints: ToolArgsPresentation) {
  return { title: DEFAULT_TOOL_TITLE, summary: argsSummary(args, hints) ?? "", argsText: argsToText(args) };
}

/** 解析后端系统 trace 消息（旧会话 formatting.rs 产出的「工具执行 [...]」格式）。 */
function parseTrace(content: string) {
  const header = content.match(/^工具执行 \[(.+?)\]/);
  if (!header) return null;
  const status = content.match(/^ok=(\S+) exit_code=(\S+) duration_ms=(\d+)$/m);
  return {
    toolName: header[1],
    ok: status ? status[1] !== "false" : true,
    durationMs: status ? Number(status[3]) : null,
    summary: content.match(/^summary: (.+)$/m)?.[1] ?? null,
  };
}

/**
 * 构建单条工具消息（已完成）的展示模型。
 *
 * @param msg 工具结果消息（role:'tool'，或旧会话的工具 trace / 插件注入系统消息）。
 * @param args 配对到的调用参数（来自 assistant.tool_calls，按 tool_call_id 配对）。
 */
export function buildToolDisplayModel(
  msg: MessageItem,
  args?: unknown,
  hints: ToolArgsPresentation = EMPTY_ARGS_PRESENTATION,
): ToolDisplayModel {
  const content = textContent(msg);
  const toolName = toolNameOf(msg) || "";

  // 旧会话的系统 trace 消息：通用展示。
  const trace = messageKind(msg) === "system" ? parseTrace(content) : null;
  if (trace) {
    const call = callPart(args, hints);
    return {
      toolName: trace.toolName,
      ...call,
      summary: call.summary || clamp(trace.summary ?? "", 120),
      state: trace.ok ? "ok" : "error",
      errorSummary: trace.ok ? null : clamp(trace.summary || "执行失败", 160),
      outputText: content,
      durationMs: trace.durationMs,
    };
  }

  const isError = toolIsError(msg);
  const outputText = (messageKind(msg) === "tool" ? stripToolResultHeader(content) : content) || null;
  const call = callPart(args, hints);
  return {
    toolName,
    ...call,
    summary: call.summary || clamp(firstNonEmptyLine(outputText) || "执行完成", 80),
    state: isError ? "error" : "ok",
    errorSummary: isError && outputText ? clamp(firstNonEmptyLine(outputText) || "执行失败", 160) : null,
    outputText,
    durationMs: durationMsOf(msg),
  };
}

/** 运行中工具调用的展示模型（结果未到达，展开查看调用）。 */
export function buildRunningToolModel(
  name: string,
  args?: unknown,
  hints: ToolArgsPresentation = EMPTY_ARGS_PRESENTATION,
): ToolDisplayModel {
  return {
    toolName: name,
    ...callPart(args, hints),
    state: "running",
    errorSummary: null,
    outputText: null,
    durationMs: null,
  };
}

/**
 * 已执行完成、结果尚待按序提交的工具调用展示模型。
 *
 * 同批工具结果按模型声明顺序提交，先完成的后序工具需等待前序工具；此期间
 * 停止运行计时，以 `tool_finished` 事件携带的真实耗时展示。结果文本尚未到达，
 * 失败时只能给出通用提示，详细错误随 `tool_result` 到达后替换整行。
 */
export function buildFinishedToolModel(
  name: string,
  args: unknown,
  finished: { ok: boolean; durationMs: number | null },
  hints: ToolArgsPresentation = EMPTY_ARGS_PRESENTATION,
): ToolDisplayModel {
  const model = buildRunningToolModel(name, args, hints);
  return {
    ...model,
    state: finished.ok ? "ok" : "error",
    errorSummary: finished.ok ? null : "执行失败",
    durationMs: finished.durationMs,
  };
}
