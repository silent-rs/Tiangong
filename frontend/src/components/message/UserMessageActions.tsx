import { useState } from "react";
import { Pencil, Copy, Check } from "lucide-react";

export function UserMessageActions({ text, messageId, time, runStatus, canEdit, showEdit = true, onStartEdit }: {
  text: string; messageId: string; time?: string; runStatus: string; canEdit: boolean;
  showEdit?: boolean;
  onStartEdit: (messageId: string, text: string) => void;
}) {
  const [copied, setCopied] = useState(false);
  const idle = runStatus === "idle";
  const handleCopy = async () => {
    try { await navigator.clipboard.writeText(text); setCopied(true); setTimeout(() => setCopied(false), 2000); }
    catch (e) { console.error("复制失败:", e); }
  };
  const btnClass = "p-1 rounded text-muted-foreground/50 hover:text-muted-foreground hover:bg-accent transition-colors";
  return (
    <div className="flex items-center gap-0.5 mt-1">
      {time && <span className="mr-1 text-[11px] text-muted-foreground/60 tabular-nums">{time}</span>}
      <button onClick={handleCopy} className={btnClass} title={copied ? "已复制" : "复制"}>
        {copied ? <Check className="w-3.5 h-3.5" /> : <Copy className="w-3.5 h-3.5" />}
      </button>
      {showEdit && (
        <button onClick={() => onStartEdit(messageId, text)} className={`${btnClass} ${(!idle || !canEdit) ? 'opacity-30 cursor-not-allowed' : ''}`} title={!canEdit ? "已压缩消息无法编辑" : !idle ? "执行中无法编辑" : "编辑并重发"} disabled={!idle || !canEdit}>
          <Pencil className="w-3.5 h-3.5" />
        </button>
      )}
    </div>
  );
}
