import { useEffect, useState } from "react";
import { ChevronDown, ChevronRight, Cpu } from "lucide-react";
import type { MessageItem } from "./types";
import { toolCallIdOf } from "@/api/message";
import { formatToolDuration, summarizeToolGroup } from "./utils";
import type { ExpansionState } from "./useExpansionState";
import {
  buildFinishedToolModel,
  buildRunningToolModel,
  buildToolDisplayModel,
  type ToolDisplayModel,
} from "./toolDisplayModel";
import { ToolIcon } from "./ToolIcon";
import { ToolCallCard } from "./toolViews/ToolCallCard";
import { ToolViewHost } from "./toolViews/ToolViewHost";

/** 运行中工具调用（结果未到达）。 */
export interface RunningToolCall {
  id: string;
  name: string;
  arguments?: unknown;
  /** 调用发起时刻（Date.now()），运行行据此实时跳秒。 */
  startedAt?: number;
  /** 任务已执行完成、结果等待前序工具按序提交：停止计时，显示真实耗时。 */
  finished?: { ok: boolean; durationMs: number | null };
}

interface ToolGroupProps {
  tools: MessageItem[];
  expansion: ExpansionState;
  /** 按消息取配对的调用参数（assistant.tool_calls 按 tool_call_id 配对）。 */
  argsOf?: (msg: MessageItem) => unknown;
  /** 本组之后仍在执行、尚无结果的工具调用，渲染为组尾运行行。 */
  runningCalls?: RunningToolCall[];
}

/**
 * 展开体：插件声明了工具视图时由插件渲染调用与结果，否则文本默认显示
 * （参数 + 完成后的结果）。
 */
function ToolCardBody({
  model,
  callId,
  args,
}: {
  model: ToolDisplayModel;
  callId: string;
  args?: unknown;
}) {
  return (
    <ToolViewHost
      data={{
        tool_call_id: callId,
        tool_name: model.toolName,
        arguments: args ?? null,
        status: model.state,
        output: model.outputText,
        duration_ms: model.durationMs,
      }}
      fallback={
        <ToolCallCard
          argsText={model.argsText}
          outputText={model.outputText}
          isError={model.state === "error"}
        />
      }
    />
  );
}

/** 该调用是否有内容可展开：执行中有调用参数即可展开查看。 */
function isExpandable(model: ToolDisplayModel): boolean {
  return model.argsText !== null || model.outputText !== null;
}

/**
 * 单条工具行：图标 + 类别 + 摘要，点击行展开/收起详情卡片。
 * 失败行摘要为错误首行（错误色）；运行行带扫光动画，展开只显示调用参数。
 */
function ToolRunRow({
  callId,
  model,
  args,
  expanded = false,
  onToggle,
  startedAtMs,
  pendingCommit = false,
}: {
  /** 工具调用 ID（交给工具视图）；旧会话无 ID 时用消息 ID。 */
  callId: string;
  model: ToolDisplayModel;
  args?: unknown;
  expanded?: boolean;
  onToggle?: () => void;
  /** 运行中调用的发起时刻（毫秒时间戳）：行尾实时跳秒。 */
  startedAtMs?: number;
  /** 已执行完成、结果等待前序工具按序提交。 */
  pendingCommit?: boolean;
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
    <div>
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
          args={args}
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
        {pendingCommit && (
          <span className="shrink-0 pl-1 text-[11px] text-muted-foreground/60" title="已执行完成，等待前序工具结果后按顺序提交">
            等待前序结果
          </span>
        )}
        {model.durationMs != null && model.durationMs > 0 && (
          <span className={`${pendingCommit ? "" : "ml-auto "}shrink-0 pl-1 text-[11px] text-muted-foreground/70 tabular-nums`} title="本次调用耗时">
            {formatToolDuration(model.durationMs)}
          </span>
        )}
      </button>
      {open && (
        <div className="ml-5 mt-0.5 mb-1">
          <ToolCardBody model={model} callId={callId} args={args} />
        </div>
      )}
    </div>
  );
}

export function ToolGroup({ tools, expansion, argsOf, runningCalls }: ToolGroupProps) {
  if (tools.length === 0 && !runningCalls?.length) return null;
  const brief = summarizeToolGroup(tools);

  const renderToolItem = (tool: MessageItem) => {
    const args = argsOf?.(tool);
    return (
      <ToolRunRow
        key={tool.id}
        callId={toolCallIdOf(tool) ?? tool.id}
        model={buildToolDisplayModel(tool, args)}
        args={args}
        expanded={expansion.isExpanded(tool.id)}
        onToggle={() => expansion.toggle(tool.id)}
      />
    );
  };

  return (
    <div>
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
            callId={call.id}
            model={
              call.finished
                ? buildFinishedToolModel(call.name, call.arguments, call.finished)
                : buildRunningToolModel(call.name, call.arguments)
            }
            args={call.arguments}
            expanded={expansion.isExpanded(`running-${call.id}`)}
            onToggle={() => expansion.toggle(`running-${call.id}`)}
            startedAtMs={call.finished ? undefined : call.startedAt}
            pendingCommit={call.finished != null}
          />
        </div>
      ))}
    </div>
  );
}
