import { useState } from "react";
import { MdPreview } from "md-editor-rt";
import { ChevronDown, ChevronRight } from "lucide-react";
import { useResolvedTheme } from "@/hooks/useTheme";
import { resolveMarkdownImages } from "./utils";
import { formatMessageTime } from "./utils";

interface AgentReplyCardProps {
  label: string;
  body: string;
  time?: string;
  /** 初始是否展开（如搜索命中时强制展开）。 */
  defaultExpanded?: boolean;
}

/** 从汇报正文中提取一行摘要（用于折叠态展示）。 */
function extractSummary(body: string): string {
  const firstLine = body.split("\n").find((line) => line.trim().length > 0)?.trim() ?? "";
  // 汇报正文通常以 "[label] 执行完成" 开头，直接用作摘要。
  return firstLine;
}

/**
 * Sub Agent 汇报卡片：默认折叠，只显示标签和一行摘要；
 * 点击展开后渲染完整 Markdown 正文。
 */
export function AgentReplyCard({ label, body, time, defaultExpanded = false }: AgentReplyCardProps) {
  const resolvedTheme = useResolvedTheme();
  const [expanded, setExpanded] = useState(defaultExpanded);
  const summary = extractSummary(body);

  return (
    <div className="text-foreground">
      <button
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-1.5 rounded-md px-1 py-0.5 text-left hover:bg-muted/40 transition-colors"
      >
        {expanded ? (
          <ChevronDown className="w-3 h-3 shrink-0 text-muted-foreground" />
        ) : (
          <ChevronRight className="w-3 h-3 shrink-0 text-muted-foreground" />
        )}
        <span className="inline-flex items-center gap-1.5 rounded-full border border-green-500/30 bg-green-500/10 px-2 py-0.5 text-xs font-medium text-green-700 dark:text-green-300">
          <span className="h-1.5 w-1.5 rounded-full bg-green-500" />
          {label}
        </span>
        {!expanded && summary && (
          <span className="truncate text-xs text-muted-foreground">{summary}</span>
        )}
        {time && (
          <span className="ml-auto shrink-0 pl-1 text-[11px] text-muted-foreground/60 tabular-nums">{formatMessageTime(time)}</span>
        )}
      </button>
      {expanded && body && (
        <div className="mt-1 border-l-2 border-green-500/50 pl-3">
          <MdPreview
            modelValue={resolveMarkdownImages(body)}
            theme={resolvedTheme}
            previewTheme="github"
          />
        </div>
      )}
    </div>
  );
}
