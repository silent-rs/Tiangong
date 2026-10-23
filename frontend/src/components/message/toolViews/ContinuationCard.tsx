import { LongContent } from "./LongContent";
import { formatToolDuration } from "../utils";
import type { ParsedContinuation } from "../toolDisplayModel";

/**
 * 延续条目的展开卡片（通用展示机制）：按轮次展示「本轮标签 + 状态 +
 * 本轮输出」。数据全部来自工具结果携带的延续声明，宿主不解释工具语义；
 * 典型如终端命令的「执行 + 空输入续等」序列。
 */
export function ContinuationCard({
  command,
  rounds,
}: {
  /** 逻辑条目标题（如原命令）；空时仅展示轮次。 */
  command: string | null;
  rounds: { continuation: ParsedContinuation; roundMs: number | null }[];
}) {
  if (rounds.length === 0) return null;
  return (
    <div className="rounded-md border border-border/60 bg-muted/30 overflow-hidden" data-continuation-card>
      {command && (
        <div className="px-3 pt-1.5 pb-1 text-xs font-mono text-foreground/85 break-all" data-continuation-title>
          $ {command}
        </div>
      )}
      <div className="divide-y divide-border/40">
        {rounds.map((round, index) => {
          const label = round.continuation.mark.round_label?.trim();
          const status = round.continuation.mark.status_label?.trim();
          return (
            <div key={index} className="px-3 py-1.5" data-continuation-round>
              <div className="flex items-center gap-2 text-[11px] text-muted-foreground">
                <span className="font-mono">{label || (index === 0 ? "（执行）" : "（续等）")}</span>
                {status && <span className="rounded bg-muted px-1.5 py-0.5" title="命令累计运行时长（含轮次间隙）">{status}</span>}
                {round.roundMs != null && round.roundMs > 0 && (
                  <span
                    className="tabular-nums"
                    title="本轮等待/调用时长（不含轮次间隙）"
                  >
                    本轮 {formatToolDuration(round.roundMs)}
                  </span>
                )}
              </div>
              {round.continuation.body.trim() ? (
                <LongContent
                  content={round.continuation.body}
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
