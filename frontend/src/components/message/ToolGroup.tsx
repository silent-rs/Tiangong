import { useEffect, useState, type ReactNode } from "react";
import { ChevronDown, ChevronRight, Cpu } from "lucide-react";
import type { MessageItem } from "./types";
import { toolCallIdOf, toolNameOf } from "@/api/message";
import { formatToolDuration, summarizeToolGroup } from "./utils";
import type { ExpansionState } from "./useExpansionState";
import {
  buildFinishedToolModel,
  buildRunningToolModel,
  buildToolDisplayModel,
  isTerminalSendCall,
  parseTerminalSendStatus,
  readableTerminalInput,
  type ToolDisplayModel,
  type TerminalSendStatus,
} from "./toolDisplayModel";
import { ToolIcon } from "./ToolIcon";
import { ToolCallCard } from "./toolViews/ToolCallCard";
import { TerminalCard } from "./toolViews/TerminalCard";
import { ToolViewHost, useToolTitle } from "./toolViews/ToolViewHost";
import { useToolArgsPresentation } from "./toolArgsPresentation";

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
  extraTrailing,
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
  /** 行尾附加展示（终端续等行的状态标签与续等次数）。 */
  extraTrailing?: ReactNode;
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
        <span className="font-medium shrink-0" title={model.toolName || undefined}>{model.title}</span>
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
        {extraTrailing}
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

/** 终端 send 一轮（含配对参数与解析后的状态）。 */
interface TerminalRound {
  tool: MessageItem;
  args: Record<string, unknown>;
  status: TerminalSendStatus | null;
  body: string;
}

/** 从参数取终端编号（同编号的连续 send 合并为一次逻辑执行）。 */
function terminalIdOf(args: Record<string, unknown> | null | undefined): string | null {
  return typeof args?.terminal_id === "string" && args.terminal_id ? args.terminal_id : null;
}

function statusBadgeLabel(status: TerminalSendStatus | null): string {
  switch (status?.kind) {
    case "running":
      return "运行中";
    case "completed":
      return `已完成 · 退出码 ${status.exitCode ?? 0}`;
    case "interactive":
      return "交互中";
    case "updated":
      return "已更新";
    default:
      return "";
  }
}

/**
 * 终端执行的合并行（issue #707）：同一终端上「带命令的发送 + 后续空输入
 * 续等」合并为一个逻辑条目。折叠行显示原命令、状态与续等次数，耗时用状态
 * 行携带的命令累计秒数；展开由 TerminalCard 按轮次展示。
 */
function TerminalRunRow({
  rounds,
  expansion,
}: {
  rounds: TerminalRound[];
  expansion: ExpansionState;
}) {
  const first = rounds[0];
  const last = rounds[rounds.length - 1];
  const command =
    rounds
      .map((round) => readableTerminalInput(typeof round.args.input === "string" ? round.args.input : null))
      .find(Boolean) ??
    last.status?.command ??
    terminalIdOf(first.args) ??
    "";
  const waitCount = rounds.filter((round) => !String(round.args.input ?? "")).length;
  const totalMs =
    last.status?.elapsedSecs != null ? last.status.elapsedSecs * 1000 : null;
  const expandKey = first.tool.id;
  const expanded = expansion.isExpanded(expandKey);
  const badge = statusBadgeLabel(last.status);
  return (
    <div>
      <button
        type="button"
        className="w-full flex items-center gap-2 px-2 py-0.5 rounded text-xs transition-colors text-left text-muted-foreground hover:bg-muted/50"
        onClick={() => expansion.toggle(expandKey)}
      >
        {expanded ? <ChevronDown className="w-3 h-3 shrink-0" /> : <ChevronRight className="w-3 h-3 shrink-0" />}
        <ToolIcon toolName={toolNameOf(first.tool) || ""} args={first.args} className="w-3 h-3 shrink-0" />
        <span className="font-medium shrink-0" title="命令执行">命令执行</span>
        {command && <span className="truncate opacity-60">{command}</span>}
        {badge && (
          <span className="ml-auto shrink-0 pl-1 text-[11px] text-muted-foreground/70">{badge}</span>
        )}
        {waitCount > 0 && (
          <span className="shrink-0 pl-1 text-[11px] text-muted-foreground/70" title="空输入续等次数">
            续等 {waitCount} 次
          </span>
        )}
        {totalMs != null && totalMs > 0 && (
          <span
            className={`${badge || waitCount > 0 ? "" : "ml-auto "}shrink-0 pl-1 text-[11px] text-muted-foreground/70 tabular-nums`}
            title="命令累计运行时长"
          >
            {formatToolDuration(totalMs)}
          </span>
        )}
      </button>
      {expanded && (
        <div className="ml-5 mt-0.5 mb-1">
          <TerminalCard
            command={last.status?.command ?? command}
            rounds={rounds.map((round) => ({
              input: typeof round.args.input === "string" ? round.args.input : "",
              status: round.status,
              body: round.status ? round.status.body : round.body,
            }))}
          />
        </div>
      )}
    </div>
  );
}

export function ToolGroup({ tools, expansion, argsOf, runningCalls }: ToolGroupProps) {
  const hints = useToolArgsPresentation();
  const titleOf = useToolTitle();
  // 插件声明了标题时替换通用标题「工具调用」，摘要仍为通用参数摘要。
  const withTitle = (model: ToolDisplayModel, args: unknown): ToolDisplayModel => {
    const title = titleOf(model.toolName, args);
    return title ? { ...model, title } : model;
  };
  if (tools.length === 0 && !runningCalls?.length) return null;
  const brief = summarizeToolGroup(tools);

  const renderToolItem = (tool: MessageItem) => {
    const args = argsOf?.(tool);
    return (
      <ToolRunRow
        key={tool.id}
        callId={toolCallIdOf(tool) ?? tool.id}
        model={withTitle(buildToolDisplayModel(tool, args, hints), args)}
        args={args}
        expanded={expansion.isExpanded(tool.id)}
        onToggle={() => expansion.toggle(tool.id)}
      />
    );
  };
  // 终端 send 合并预处理（issue #707）：同一终端上连续的发送/空输入续等
  // 合并为一个逻辑条目；其余工具保持单条（run_command / run_shell 不受影响）。
  type GroupItem =
    | { kind: "tool"; tool: MessageItem }
    | { kind: "terminal-run"; rounds: TerminalRound[] };
  const items: GroupItem[] = [];
  for (const tool of tools) {
    const args = (argsOf?.(tool) ?? null) as Record<string, unknown> | null;
    const isSend = isTerminalSendCall(toolNameOf(tool) || "", args);
    if (isSend) {
      const output = buildToolDisplayModel(tool, args, hints).outputText;
      const status = parseTerminalSendStatus(output);
      const round: TerminalRound = { tool, args: args ?? {}, status, body: output ?? "" };
      const previous = items[items.length - 1];
      if (
        previous?.kind === "terminal-run" &&
        terminalIdOf(args) != null &&
        terminalIdOf(args) === terminalIdOf(previous.rounds[previous.rounds.length - 1].args)
      ) {
        previous.rounds.push(round);
      } else {
        items.push({ kind: "terminal-run", rounds: [round] });
      }
      continue;
    }
    items.push({ kind: "tool", tool });
  }

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
      <div className="ml-4 space-y-0">
        {items.map((item) =>
          item.kind === "tool" ? (
            renderToolItem(item.tool)
          ) : (
            <TerminalRunRow key={item.rounds[0].tool.id} rounds={item.rounds} expansion={expansion} />
          ),
        )}
      </div>
      {runningCalls?.map((call) => (
        <div key={`running-${call.id}`} className="ml-4">
          <ToolRunRow
            callId={call.id}
            model={withTitle(
              call.finished
                ? buildFinishedToolModel(call.name, call.arguments, call.finished, hints)
                : buildRunningToolModel(call.name, call.arguments, hints),
              call.arguments,
            )}
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
