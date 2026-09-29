import { useState } from "react";
import { Copy, Check, Clock } from "lucide-react";
import { formatDuration } from "./utils";
import { CallUsageDetails } from "./CallUsageDetails";
import { MessagePluginHost } from "../MessagePluginHost";
import type { MessageItem } from "./types";

export function MessageActions({ messageId, text, durationMs, usageMessages }: { messageId: string; text: string; durationMs?: number | null; usageMessages?: MessageItem[] }) {
  const [copied, setCopied] = useState(false);

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch (e) { console.error("复制失败:", e); }
  };

  const btnClass = "p-1 rounded text-muted-foreground hover:text-foreground hover:bg-accent transition-colors";

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
      {durationMs != null && durationMs > 0 && (
        <span className="inline-flex items-center gap-0.5 ml-1 pl-1 border-l border-border/60 text-[11px] text-muted-foreground/70 tabular-nums" title="本轮执行总时长">
          <Clock className="w-3 h-3" />
          {formatDuration(durationMs)}
        </span>
      )}
      {usageMessages && <CallUsageDetails messages={usageMessages} />}
    </div>
  );
}
