import { useEffect, useState } from "react";
import { ChevronDown, ChevronRight, Cpu } from "lucide-react";
import type { MessageItem } from "./types";
import { formatMessageTime, formatToolDuration, summarizeToolGroup } from "./utils";
import type { ExpansionState } from "./useExpansionState";
import {
  buildRunningToolModel,
  buildToolDisplayModel,
  writeContentFromArgs,
  type ToolDisplayModel,
} from "./toolDisplayModel";
import { ToolIcon } from "./ToolIcon";
import { TerminalCard } from "./toolViews/TerminalCard";
import { NumberedFileCard } from "./toolViews/NumberedFileCard";
import { InOutCard, WritePreviewCard } from "./toolViews/InOutCard";

/** 运行中工具调用（结果未到达）。 */
export interface RunningToolCall {
  id: string;
  name: string;
  arguments?: unknown;
  /** 调用发起时刻（Date.now()），运行行据此实时跳秒。 */
  startedAt?: number;
}

interface ToolGroupProps {
  tools: MessageItem[];
  expansion: ExpansionState;
  /** 按消息取配对的调用参数（assistant.tool_calls 按 tool_call_id 配对）。 */
  argsOf?: (msg: MessageItem) => unknown;
  /** 本组之后仍在执行、尚无结果的工具调用，渲染为组尾运行行。 */
  runningCalls?: RunningToolCall[];
}

/** 变体的展开体：terminal → 终端卡；读文件 → 行号内容块；写文件 → 内容预览；其余 → IN/OUT。 */
function ToolCardBody({ model, args }: { model: ToolDisplayModel; args?: unknown }) {
  if (model.variant === "terminal" && model.terminal) {
    return <TerminalCard terminal={model.terminal} />;
  }
  if (model.variant === "file-read" && model.outputText) {
    return <NumberedFileCard content={model.outputText} title={model.filePath} />;
  }
  if (model.variant === "file-write") {
    return (
      <WritePreviewCard
        path={model.filePath}
        content={writeContentFromArgs(args)}
        outputText={model.outputText}
      />
    );
  }
  return (
    <InOutCard
      argsText={model.argsText}
      outputText={model.outputText}
      isError={model.state === "error"}
    />
  );
}

/** 该调用是否有详情卡片可展开；执行中的调用只要有参数就能展开查看。 */
function isExpandable(model: ToolDisplayModel): boolean {
  if (model.state === "running") return !!model.terminal?.command || model.argsText !== null;
  if (model.variant === "terminal") return !!model.terminal?.command || !!model.terminal?.stdout || !!model.terminal?.stderr;
  if (model.variant === "file-read") return !!model.outputText;
  return model.argsText !== null || model.outputText !== null;
}

/**
 * 单条工具行：图标 + 类别 + 摘要，点击行展开/收起详情卡片。
 * 失败行摘要为错误首行（错误色）；运行行带扫光动画，展开只显示调用参数。
 */
function ToolRunRow({
  model,
  args,
  expanded = false,
  onToggle,
  time,
  startedAtMs,
}: {
  model: ToolDisplayModel;
  args?: unknown;
  expanded?: boolean;
  onToggle?: () => void;
  time?: string;
  /** 运行中调用的发起时刻（毫秒时间戳）：行尾实时跳秒。 */
  startedAtMs?: number;
}) {
  const isError = model.state === "error";
  const isRunning = model.state === "running";
  const summaryText = model.errorSummary ?? model.summary;
  const expandable = isExpandable(model);
  const open = expandable && expanded;

  // 运行行本地跳秒：秒数逐秒前进，结果到达后该行整体被完成行替换。
  const [, setRunTick] = useState(0);
  useEffect(() => {
    if (!isRunning || startedAtMs == null) return;
    const timer = setInterval(() => setRunTick((tick) => tick + 1), 1000);
    return () => clearInterval(timer);
  }, [isRunning, startedAtMs]);
  const runningMs = startedAtMs != null ? Date.now() - startedAtMs : null;

  return (
    <div title={time}>
      <button
        type="button"
        className={`w-full flex items-center gap-2 px-2 py-0.5 rounded text-xs transition-colors text-left ${
          isRunning
            ? "tool-run-row text-foreground/90"
            : "text-muted-foreground hover:bg-muted/50"
        }`}
        onClick={expandable ? onToggle : undefined}
      >
        {open ? (
          <ChevronDown className="w-3 h-3 shrink-0" />
        ) : (
          <ChevronRight className={`w-3 h-3 shrink-0 ${expandable ? "" : "invisible"}`} />
        )}
        <ToolIcon
          toolName={model.toolName}
          className={`w-3 h-3 shrink-0 ${isError ? "text-destructive" : ""}`}
        />
        <span className="font-medium shrink-0">{model.title}</span>
        {summaryText && (
          <span
            className={`truncate ${
              isError ? "text-destructive" : "opacity-60"
            }`}
          >
            {summaryText}
          </span>
        )}
        {isRunning && runningMs != null && runningMs >= 1000 && (
          <span className="ml-auto shrink-0 pl-1 text-[11px] text-muted-foreground/70 tabular-nums" title="已运行时长">
            {formatToolDuration(runningMs)}
          </span>
        )}
        {model.durationMs != null && model.durationMs > 0 && (
          <span className="ml-auto shrink-0 pl-1 text-[11px] text-muted-foreground/70 tabular-nums" title="本次调用耗时">
            {formatToolDuration(model.durationMs)}
          </span>
        )}
      </button>
      {open && (
        <div className="ml-5 mt-0.5 mb-1">
          <ToolCardBody model={model} args={args} />
        </div>
      )}
    </div>
  );
}

export function ToolGroup({ tools, expansion, argsOf, runningCalls }: ToolGroupProps) {
  if (tools.length === 0 && !runningCalls?.length) return null;
  const brief = summarizeToolGroup(tools);
  const groupTime = tools.length > 0 ? formatMessageTime(tools[0].created_at) : undefined;

  const renderToolItem = (tool: MessageItem) => {
    const args = argsOf?.(tool);
    return (
      <ToolRunRow
        key={tool.id}
        model={buildToolDisplayModel(tool, args)}
        args={args}
        expanded={expansion.isExpanded(tool.id)}
        onToggle={() => expansion.toggle(tool.id)}
        time={formatMessageTime(tool.created_at)}
      />
    );
  };

  return (
    <div title={groupTime}>
      {/* 组统计为静态摘要行：不再折叠包裹工具行，工具行始终直接列出。
          只有执行中的调用（尚无结果）时不显示「0 次」统计。 */}
      {tools.length > 0 && (
        <div className="flex items-center gap-2 px-2 py-0.5 text-xs text-muted-foreground">
          <Cpu className="w-3 h-3 shrink-0" />
          <span>{brief}</span>
        </div>
      )}
      <div className="ml-4 space-y-0">{tools.map(renderToolItem)}</div>
      {runningCalls?.map((call) => (
        <div key={`running-${call.id}`} className="ml-4">
          <ToolRunRow
            model={buildRunningToolModel(call.name, call.arguments)}
            args={call.arguments}
            expanded={expansion.isExpanded(`running-${call.id}`)}
            onToggle={() => expansion.toggle(`running-${call.id}`)}
            startedAtMs={call.startedAt}
          />
        </div>
      ))}
    </div>
  );
}
