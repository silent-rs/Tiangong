import * as Dialog from "@radix-ui/react-dialog";
import { ArrowUp, ArrowDown, Database, X } from "lucide-react";
import { usageOf } from "@/api/message";
import type { MessageItem } from "./types";
import { cacheHitRate, sumUsage } from "./usage";

const number = new Intl.NumberFormat("zh-CN");
const compactNumber = new Intl.NumberFormat("zh-CN", { notation: "compact", maximumFractionDigits: 1 });
const rateText = (rate: number | null) => rate == null ? "未知" : `${(rate * 100).toFixed(1)}%`;
const statuses = { processing: "进行中", success: "完成", failed: "失败", cancelled: "中断" };

export function CallUsageDetails({ messages }: { messages: MessageItem[] }) {
  const calls = messages.filter((message) => usageOf(message) != null);
  if (calls.length === 0) return null;
  const usage = sumUsage(calls.map((message) => usageOf(message)!));
  return (
    <Dialog.Root>
      <Dialog.Trigger asChild>
        <button type="button" aria-label="查看调用用量详情"
          className="inline-flex shrink-0 items-center gap-2 rounded px-1 py-0.5 text-[11px] text-muted-foreground/70 tabular-nums hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
          <span className="inline-flex items-center gap-0.5" title={`输入 ${number.format(usage.prompt_tokens)} tokens`} aria-label={`输入 ${number.format(usage.prompt_tokens)}`}>
            <ArrowUp className="h-3 w-3" aria-hidden="true" /><span>{compactNumber.format(usage.prompt_tokens)}</span>
          </span>
          <span className="inline-flex items-center gap-0.5" title={`输出 ${number.format(usage.completion_tokens)} tokens`} aria-label={`输出 ${number.format(usage.completion_tokens)}`}>
            <ArrowDown className="h-3 w-3" aria-hidden="true" /><span>{compactNumber.format(usage.completion_tokens)}</span>
          </span>
          <span className="inline-flex items-center gap-0.5" title={`缓存命中${cacheHitRate(usage) == null && usage.prompt_cache_hit_tokens != null ? "（部分统计）" : ""}：${usage.prompt_cache_hit_tokens == null ? "未知" : number.format(usage.prompt_cache_hit_tokens) + " tokens"}`} aria-label={`缓存命中 ${usage.prompt_cache_hit_tokens == null ? "未知" : number.format(usage.prompt_cache_hit_tokens)}`}>
            <Database className="h-3 w-3" aria-hidden="true" /><span>{usage.prompt_cache_hit_tokens == null ? "未知" : compactNumber.format(usage.prompt_cache_hit_tokens)}</span>
          </span>
        </button>
      </Dialog.Trigger>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-[70] bg-black/50" />
        <Dialog.Content className="fixed left-1/2 top-1/2 z-[71] flex max-h-[85vh] w-[calc(100%_-_2rem)] max-w-3xl -translate-x-1/2 -translate-y-1/2 flex-col gap-4 rounded-lg border bg-background p-4 shadow-lg sm:p-6">
          <div className="pr-8">
            <Dialog.Title className="text-base font-semibold">调用用量</Dialog.Title>
            <Dialog.Description className="mt-1 text-xs text-muted-foreground">{calls.length} 次调用 · 合计 {number.format(usage.total_tokens)} tokens</Dialog.Description>
          </div>
          <Dialog.Close className="absolute right-3 top-3 rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring" aria-label="关闭调用用量详情" title="关闭">
            <X className="h-4 w-4" />
          </Dialog.Close>
          <div className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground tabular-nums">
            <span>输入 {number.format(usage.prompt_tokens)}</span>
            <span>输出 {number.format(usage.completion_tokens)}</span>
            <span>缓存命中 {rateText(cacheHitRate(usage))}</span>
          </div>
          <div className="min-h-0 overflow-auto text-xs text-muted-foreground tabular-nums">
            <table className="w-full min-w-[520px] text-left">
              <thead><tr className="border-b border-border/60">
                {["模型 / 调用", "状态", "输入", "输出", "缓存命中", "命中率"].map((label) => <th key={label} className="px-2 py-1 font-medium whitespace-nowrap">{label}</th>)}
              </tr></thead>
              <tbody>{calls.map((message) => {
                const entry = usageOf(message)!;
                return <tr key={message.id} className="border-b border-border/30">
                  <td className="max-w-48 break-all px-2 py-1" title={message.created_at}>{entry.model}<span className="block whitespace-nowrap opacity-70">{entry.source === "context_summary" ? "压缩" : "对话"}</span></td>
                  <td className="whitespace-nowrap px-2 py-1">{statuses[entry.status]}</td>
                  <td className="px-2 py-1">{number.format(entry.prompt_tokens)}</td>
                  <td className="px-2 py-1">{number.format(entry.completion_tokens)}</td>
                  <td className="px-2 py-1">{entry.prompt_cache_hit_tokens == null ? "未知" : number.format(entry.prompt_cache_hit_tokens)}</td>
                  <td className="px-2 py-1">{rateText(cacheHitRate(entry))}</td>
                </tr>;
              })}</tbody>
            </table>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
