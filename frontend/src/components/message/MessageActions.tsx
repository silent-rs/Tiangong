import { useState } from "react";
import { Copy, Check, Clock, Zap } from "lucide-react";
import { formatDuration, formatTpsText } from "./utils";
import { CallUsageDetails } from "./CallUsageDetails";
import { MessagePluginHost } from "../MessagePluginHost";
import type { MessageItem } from "./types";

export function MessageActions({ messageId, text, time, durationMs, tps, tpsDetail, usageMessages }: {
  messageId: string;
  text: string;
  time?: string;
  durationMs?: number | null;
  /** 正文生成速度（tokens/s）：来自后端持久化 `text_tps` 或旧数据折算，null 不显示。 */
  tps?: number | null;
  /** 速度明细（hover 提示）：输出 tokens 与正文耗时。 */
  tpsDetail?: { tokens: number; elapsedMs: number } | null;
  usageMessages?: MessageItem[];
}) {
  const [copied, setCopied] = useState(false);

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch (e) { console.error("复制失败:", e); }
  };

  const btnClass = "p-1 rounded text-muted-foreground hover:text-foreground hover:bg-accent transition-colors";
  const showTps = tps != null && tps > 0;

  return (
    <div className="flex flex-wrap items-center gap-0.5 mt-1">
      <button onClick={handleCopy} className={btnClass} title={copied ? "已复制" : "复制"}>
        {copied ? <Check className="w-3.5 h-3.5" /> : <Copy className="w-3.5 h-3.5" />}
      </button>
      {/* 插件消息操作（如朗读）：由插件经 session.message-action 注入 */}
      <MessagePluginHost
        slot="session.message-action"
        message={{ id: messageId, role: "assistant", text, attachments: [] }}
      />
      {time && (
        <span className="ml-1 pl-1 border-l border-border/60 text-[11px] text-muted-foreground/70 tabular-nums">{time}</span>
      )}
      {durationMs != null && durationMs > 0 && (
        <span className="inline-flex items-center gap-0.5 ml-1 pl-1 border-l border-border/60 text-[11px] text-muted-foreground/70 tabular-nums" title="本轮执行总时长">
          <Clock className="w-3 h-3" />
          {formatDuration(durationMs)}
        </span>
      )}
      {/* 生成度量（速度 + token 用量）合并为一块区域：同类信息聚合，
          共享一个左侧分隔；两者皆无时不渲染。 */}
      {(showTps || usageMessages) && (
        <span className="inline-flex items-center gap-1.5 ml-1 pl-1.5 border-l border-border/60 text-[11px] text-muted-foreground/70 tabular-nums">
          {showTps && (
            <span
              className="inline-flex items-center gap-0.5"
              title={tpsDetail
                ? `正文输出 ${tpsDetail.tokens.toLocaleString()} tokens · 耗时 ${formatDuration(tpsDetail.elapsedMs)}（${formatTpsText(tps)}）`
                : "正文生成速度"}
              aria-label={`正文生成速度 ${formatTpsText(tps)}`}
            >
              <Zap className="w-3 h-3" />
              {formatTpsText(tps)}
            </span>
          )}
          {usageMessages && <CallUsageDetails messages={usageMessages} />}
        </span>
      )}
    </div>
  );
}
