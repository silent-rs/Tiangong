import { LongContent } from "./LongContent";
import type { TerminalSendStatus } from "../toolDisplayModel";
import { readableTerminalInput } from "../toolDisplayModel";

/** 终端 send 一轮的展示数据（输入 + 状态 + 本轮新增输出）。 */
export interface TerminalWaitRound {
  /** 本轮输入原文（空输入即续等）。 */
  input: string;
  status: TerminalSendStatus | null;
  /** 去掉状态行后的真实输出（status 为 null 时为原始输出文本）。 */
  body: string;
}

function statusLabel(status: TerminalSendStatus | null): string {
  switch (status?.kind) {
    case "running":
      return status.elapsedSecs != null ? `运行中 · 已 ${status.elapsedSecs} 秒` : "运行中";
    case "completed":
      return `已完成 · 退出码 ${status.exitCode ?? 0}`;
    case "interactive":
      return "交互中 · 等待输入";
    case "updated":
      return "已更新";
    default:
      return "";
  }
}

/**
 * 终端 send 调用的展开卡片（issue #707）：按轮次展示「输入/续等 + 状态 +
 * 本轮新增输出」，运行中无输出给「暂无新输出」占位；写给模型的操作提示
 * （summary）不进入展示。
 */
export function TerminalCard({ command, rounds }: { command: string | null; rounds: TerminalWaitRound[] }) {
  if (rounds.length === 0) return null;
  return (
    <div className="rounded-md border border-border/60 bg-muted/30 overflow-hidden" data-terminal-card>
      {command && (
        <div className="px-3 pt-1.5 pb-1 text-xs font-mono text-foreground/85 break-all" data-terminal-command>
          $ {command}
        </div>
      )}
      <div className="divide-y divide-border/40">
        {rounds.map((round, index) => {
          const input = readableTerminalInput(round.input) ?? (round.input ? round.input : null);
          const label = statusLabel(round.status);
          return (
            <div key={index} className="px-3 py-1.5" data-terminal-round>
              <div className="flex items-center gap-2 text-[11px] text-muted-foreground">
                <span className="font-mono">{input ?? (index === 0 ? "（执行）" : "（续等）")}</span>
                {label && <span className="rounded bg-muted px-1.5 py-0.5">{label}</span>}
              </div>
              {(round.status ? round.body : round.body)?.trim() ? (
                <LongContent
                  content={round.body}
                  className="mt-1 text-xs font-mono whitespace-pre-wrap break-words max-h-60 overflow-y-auto text-foreground/85"
                />
              ) : (
                <div className="mt-1 text-[11px] text-muted-foreground/60 italic">（暂无新输出）</div>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
