import { textContent } from "@/api/tauri";
import { durationMsOf, messageKind, toolIsError, toolNameOf } from "@/api/message";
import type { MessageItem } from "./types";

/**
 * 工具调用展示模型（参考 DeepSeek Harness 的 tool-call-model 设计）。
 *
 * 职责：把一条工具相关消息（role:'tool' 结果，或系统 trace / 记忆检索 / 插件注入消息）
 * 加上可选的配对调用参数（assistant.tool_calls.arguments），转换成纯展示用的模型：
 * 变体、标题、单行摘要、状态、错误首行与展开物料。模型是纯函数，不碰 React。
 */

export type ToolVariant =
  | "terminal"
  | "file-read"
  | "file-write"
  | "search"
  | "web"
  | "memory"
  | "plugin"
  | "other";

export type ToolDisplayState = "running" | "ok" | "error";

/** 终端卡物料：命令执行类工具的展开内容。 */
export interface TerminalMaterial {
  command: string | null;
  stdout: string | null;
  stderr: string | null;
}

export interface ToolDisplayModel {
  variant: ToolVariant;
  /** 工具对外名（可能带插件前缀），按此查插件提供的图标；未知为空串。 */
  toolName: string;
  /** 类别标题（命令执行 / 读取文件 / ……）。 */
  title: string;
  /** 单行摘要：优先从参数派生，失败时被 errorSummary 顶替显示。 */
  summary: string;
  state: ToolDisplayState;
  /** 失败结果的第一行，折叠行直接以错误色显示。 */
  errorSummary: string | null;
  /** 展开用的参数文本；无参数为 null。 */
  argsText: string | null;
  /** 结果文本（工具输出原文）。 */
  outputText: string | null;
  /** terminal 变体的展开卡物料；其余变体为 null。 */
  terminal: TerminalMaterial | null;
  /** 文件类工具的目标路径（来自参数）。 */
  filePath: string | null;
  /** 单次调用耗时（毫秒）：优先事件携带值，历史消息从 trace 文本解析。 */
  durationMs: number | null;
}

const VARIANT_TITLES: Record<ToolVariant, string> = {
  terminal: "命令执行",
  "file-read": "读取文件",
  "file-write": "写入文件",
  search: "搜索",
  web: "网页",
  memory: "记忆检索",
  plugin: "插件注入",
  other: "工具调用",
};

/** 已知工具名 → 变体；未收录的落 other。 */
const TOOL_VARIANTS: Record<string, ToolVariant> = {
  run_command: "terminal",
  run_shell: "terminal",
  terminal_send: "terminal",
  read_file: "file-read",
  list_dir: "file-read",
  tree_dir: "file-read",
  write_file: "file-write",
  replace_in_file: "file-write",
  apply_patch: "file-write",
  grep: "search",
  glob: "search",
  search: "search",
  web_fetch: "web",
  web_search: "web",
  recall_memory: "memory",
};

/**
 * 去掉插件工具的插件前缀（`{插件id}__{工具名}` → 工具名），用于按原名分类。
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

/** 以 web_ 开头的浏览器插件工具统一归 web 变体。 */
export function classifyToolName(toolName: string): ToolVariant {
  const name = baseToolName(toolName);
  if (TOOL_VARIANTS[name]) return TOOL_VARIANTS[name];
  if (name.startsWith("web_")) return "web";
  return "other";
}

/** run_command 的 shell 模式哨兵值（后端 formatting.rs 约定）。 */
const SHELL_SENTINEL = "__tiangong_shell__";
/** 后端混入参数数组的工作目录参数前缀。 */
const CWD_ARG_PREFIX = "__tiangong_cwd=";
/** 对象参数中的环境/配置键，摘要兜底遍历时跳过（不构成"这次调用做了什么"）。 */
const CONFIG_ARG_KEYS: ReadonlySet<string> = new Set([
  "cwd",
  "workdir",
  "working_dir",
  "timeout",
  "timeout_secs",
  "interactive",
  "encoding",
  "offset",
  "limit",
  "append",
  "dry_run",
  "force",
]);

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

/** 后端混入参数数组的内部执行参数（__tiangong_cwd=），展示摘要时过滤，与 formatting.rs 对齐。 */
function filterCwdArgs(args: unknown[]): unknown[] {
  return args.filter((item) => !(typeof item === "string" && item.startsWith(CWD_ARG_PREFIX)));
}

/** 摘要字符上限：多行命令压缩为单行后截断防撑爆。 */
const SUMMARY_MAX_CHARS = 500;

/** 多行文本压缩为单行：换行折叠为单个空格，供单行摘要展示。 */
function squeezeToSingleLine(text: string): string {
  return text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean)
    .join(" ");
}

/**
 * terminal 调用参数组合为完整命令文本：摘要与终端展开卡共用同一来源。
 * 兼容三种形态：位置数组 [cmd, ...args]、对象 {cmd, args}（run_command 当前 schema）、
 * 对象 {script | command}（run_shell 等）。shell 脚本保留多行原文；
 * exec 形态空格连接全部部分，与后端 formatting.rs 的命令行对齐。
 */
function terminalCommandFromArgs(args: unknown): string | null {
  if (args === undefined || args === null) return null;

  if (Array.isArray(args)) {
    const visible = filterCwdArgs(args);
    if (visible.length === 0) return null;
    if (visible[0] === SHELL_SENTINEL) {
      const script = visible[1];
      return typeof script === "string" && script ? script : null;
    }
    const parts = visible
      .filter((item): item is string => typeof item === "string" && item !== SHELL_SENTINEL)
      .map((item) => squeezeToSingleLine(item))
      .filter(Boolean);
    return parts.length > 0 ? parts.join(" ") : null;
  }

  if (typeof args === "object") {
    const record = args as Record<string, unknown>;
    // exec 对象形态：cmd 与参数列表拼接为完整命令，缺省 args 时命令即 cmd 本身。
    if (typeof record.cmd === "string" && record.cmd) {
      const parts = Array.isArray(record.args)
        ? record.args
            .filter((item): item is string => typeof item === "string")
            .map((item) => squeezeToSingleLine(item))
            .filter(Boolean)
        : [];
      return parts.length > 0 ? [record.cmd, ...parts].join(" ") : record.cmd;
    }
    const script = record.script ?? record.command;
    if (typeof script === "string" && script) return script;
    return null;
  }

  if (typeof args === "string" && args) return args;
  return null;
}

/** 从调用参数提取单行摘要（多行命令压缩去换行）；参数兼容位置数组与对象。 */
function summaryFromArgs(variant: ToolVariant, args: unknown): string | null {
  if (args === undefined || args === null) return null;

  if (Array.isArray(args)) {
    const visible = filterCwdArgs(args);
    if (variant === "terminal") {
      const command = terminalCommandFromArgs(visible);
      if (command) return clamp(squeezeToSingleLine(command), SUMMARY_MAX_CHARS);
      return null;
    }
    const first = visible.find((item) => typeof item === "string" && item !== SHELL_SENTINEL);
    if (typeof first === "string" && first) return clamp(squeezeToSingleLine(first), SUMMARY_MAX_CHARS);
    return null;
  }

  if (typeof args === "object") {
    const record = args as Record<string, unknown>;
    if (variant === "terminal") {
      const command = terminalCommandFromArgs(record);
      if (command) return clamp(squeezeToSingleLine(command), SUMMARY_MAX_CHARS);
    }
    const keyPreference: Record<ToolVariant, readonly string[]> = {
      terminal: ["script", "cmd", "command", "description"],
      "file-read": ["path", "file_path", "url"],
      "file-write": ["path", "file_path"],
      search: ["query", "pattern", "keyword"],
      web: ["url", "query", "title"],
      memory: ["query"],
      plugin: ["command", "url", "title"],
      other: [],
    };
    for (const key of keyPreference[variant]) {
      const value = record[key];
      if (typeof value === "string" && value) return clamp(squeezeToSingleLine(value), SUMMARY_MAX_CHARS);
    }
    // 兜底取第一个内容字符串，跳过环境/配置键（cwd、timeout 等），避免摘要显示工作目录。
    for (const [key, value] of Object.entries(record)) {
      if (CONFIG_ARG_KEYS.has(key)) continue;
      if (typeof value === "string" && value) return clamp(squeezeToSingleLine(value), SUMMARY_MAX_CHARS);
    }
    return null;
  }

  if (typeof args === "string" && args) return clamp(squeezeToSingleLine(args), SUMMARY_MAX_CHARS);
  return null;
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
  if (typeof args === "object") return JSON.stringify(args, null, 2);
  if (typeof args === "string" && args) return args;
  return null;
}

/** 从 write_file 参数中取写入内容（位置参数 [path, content, append?]，过滤 cwd 干扰项）。 */
export function writeContentFromArgs(args: unknown): string | null {
  if (!Array.isArray(args)) {
    if (args && typeof args === "object") {
      const content = (args as Record<string, unknown>).content;
      if (typeof content === "string") return content;
    }
    return null;
  }
  const content = filterCwdArgs(args)[1];
  return typeof content === "string" ? content : null;
}

/** 解析后端系统 trace 消息（formatting.rs 产出的「工具执行 [...]」格式）。 */
export interface ToolTraceInfo {
  toolName: string;
  command: string | null;
  ok: boolean;
  exitCode: number | null;
  durationMs: number | null;
  summary: string | null;
  stdout: string | null;
  stderr: string | null;
}

export function parseToolTrace(content: string): ToolTraceInfo | null {
  const header = content.match(/^工具执行 \[(.+?)\]/);
  if (!header) return null;
  const info: ToolTraceInfo = {
    toolName: header[1],
    command: null,
    ok: true,
    exitCode: null,
    durationMs: null,
    summary: null,
    stdout: null,
    stderr: null,
  };
  const commandMatch = content.match(/^命令: (.+)$/m);
  if (commandMatch) info.command = commandMatch[1];
  const statusMatch = content.match(/^ok=(\S+) exit_code=(\S+) duration_ms=(\d+)$/m);
  if (statusMatch) {
    info.ok = statusMatch[1] !== "false";
    info.exitCode = statusMatch[2] === "null" ? null : Number(statusMatch[2]);
    info.durationMs = Number(statusMatch[3]);
  }
  const summaryMatch = content.match(/^summary: (.+)$/m);
  if (summaryMatch) info.summary = summaryMatch[1];

  const extractBlock = (label: string): string | null => {
    const pattern = new RegExp(`^${label}:$\\n^\\\`\\\`\\\`text$\\n([\\s\\S]*?)^\\\`\\\`\\\`$`, "m");
    const match = content.match(pattern);
    return match ? match[1].replace(/\n$/, "") : null;
  };
  info.stdout = extractBlock("stdout");
  info.stderr = extractBlock("stderr");
  return info;
}

/**
 * 构建单条工具消息的展示模型。
 *
 * @param msg 工具相关消息（role:'tool'，或工具 trace / 记忆检索 / 插件注入系统消息）。
 * @param args 配对到的调用参数（来自 assistant.tool_calls，按 tool_call_id 配对）。
 */
export function buildToolDisplayModel(msg: MessageItem, args?: unknown): ToolDisplayModel {
  const content = textContent(msg);
  const toolName = toolNameOf(msg) || "";

  // 插件注入消息（数据来源：xxx）
  if (content.startsWith("数据来源：")) {
    const source = content.match(/^数据来源：(\S+)/)?.[1] ?? "plugin";
    const detail =
      content.match(/command:\s*(.+)/)?.[1] ||
      content.match(/url:\s*(.+)/)?.[1] ||
      content.match(/title:\s*(.+)/)?.[1] ||
      "";
    return {
      variant: "plugin",
      toolName: toolName || "plugin_injection",
      title: VARIANT_TITLES.plugin,
      summary: detail ? `${source} · ${clamp(detail.trim(), 60)}` : source,
      state: "ok",
      errorSummary: null,
      argsText: null,
      outputText: content,
      terminal: null,
      filePath: null,
      durationMs: null,
    };
  }

  // 记忆检索消息
  if (baseToolName(toolName ?? "") === "recall_memory" || content.startsWith("[记忆检索]")) {
    const count = content.match(/命中 (\d+) 条/)?.[1] ?? "";
    const noHit = content.includes("无相关记忆");
    return {
      variant: "memory",
      toolName: toolName || "recall_memory",
      title: VARIANT_TITLES.memory,
      summary: noHit ? "无命中" : count ? `${count} 条命中` : "记忆检索",
      state: "ok",
      errorSummary: null,
      argsText: argsToText(args),
      outputText: content,
      terminal: null,
      filePath: null,
      durationMs: null,
    };
  }

  // 系统 trace 消息（工具执行 [...]）：按 trace 内的工具名分类，展开为终端卡物料。
  const trace = parseToolTrace(content);
  if (trace && messageKind(msg) === "system") {
    const variant = classifyToolName(trace.toolName);
    const fallbackSummary =
      trace.command ?? trace.summary ?? firstNonEmptyLine(content) ?? trace.toolName;
    return {
      variant,
      toolName: trace.toolName,
      title: VARIANT_TITLES[variant],
      summary: summaryFromArgs(variant, args) ?? clamp(fallbackSummary || trace.toolName, 120),
      state: trace.ok ? "ok" : "error",
      errorSummary: trace.ok ? null : clamp(trace.stderr || trace.summary || "执行失败", 160),
      argsText: argsToText(args),
      outputText: content,
      terminal: {
        command: trace.command,
        stdout: trace.stdout,
        stderr: trace.stderr,
      },
      filePath:
        variant === "file-read" || variant === "file-write"
          ? (trace.command?.match(/^(?:path=)?(\S+)/)?.[1] ?? null)
          : null,
      durationMs: trace.durationMs,
    };
  }

  // 工具结果消息（role:'tool'）或兜底。
  const variant = classifyToolName(toolName);
  const isError = toolIsError(msg);
  const outputText = (messageKind(msg) === "tool" ? stripToolResultHeader(content) : content) || null;
  const argSummary = summaryFromArgs(variant, args);

  let summary: string;
  let filePath: string | null = null;
  if (argSummary) {
    summary = argSummary;
  } else if (toolName && toolName !== variant) {
    summary = variant === "other" ? `${toolName} · ${firstLineOr(outputText, "执行完成")}` : toolName;
  } else {
    summary = clamp(firstNonEmptyLine(outputText) || "执行完成", 80);
  }
  const firstPathArg = Array.isArray(args)
    ? filterCwdArgs(args).find((item) => typeof item === "string")
    : undefined;
  if (typeof firstPathArg === "string") {
    filePath = firstPathArg;
  } else if (args && typeof args === "object" && !Array.isArray(args)) {
    const record = args as Record<string, unknown>;
    const path = record.path ?? record.file_path;
    if (typeof path === "string") filePath = path;
  }

  const errorSummary =
    isError && outputText ? clamp(firstNonEmptyLine(outputText) || "执行失败", 160) : null;

  let terminal: TerminalMaterial | null = null;
  if (variant === "terminal") {
    // 展开卡命令与摘要共用 terminalCommandFromArgs，保证两处显示一致。
    const command = terminalCommandFromArgs(args) ?? argSummary;
    terminal = { command: command ?? null, stdout: outputText, stderr: null };
  }

  return {
    variant,
    toolName,
    title: VARIANT_TITLES[variant],
    summary,
    state: isError ? "error" : "ok",
    errorSummary,
    argsText: argsToText(args),
    outputText,
    terminal,
    filePath,
    durationMs: durationMsOf(msg),
  };
}

/** 运行中工具调用的展示模型（结果未到达，只有名称与参数；展开只显示参数）。 */
export function buildRunningToolModel(name: string, args?: unknown): ToolDisplayModel {
  const variant = classifyToolName(name);
  const argSummary = summaryFromArgs(variant, args);
  const command = variant === "terminal" ? (terminalCommandFromArgs(args) ?? argSummary) : null;
  return {
    variant,
    toolName: name,
    title: VARIANT_TITLES[variant],
    summary: argSummary ?? name,
    state: "running",
    errorSummary: null,
    argsText: argsToText(args),
    outputText: null,
    terminal: command ? { command, stdout: null, stderr: null } : null,
    filePath: null,
    durationMs: null,
  };
}

function firstLineOr(text: string | null, fallback: string): string {
  if (!text) return fallback;
  const line = firstNonEmptyLine(text);
  return clamp(line || fallback, 80);
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
): ToolDisplayModel {
  const model = buildRunningToolModel(name, args);
  return {
    ...model,
    state: finished.ok ? "ok" : "error",
    errorSummary: finished.ok ? null : "执行失败",
    durationMs: finished.durationMs,
  };
}
