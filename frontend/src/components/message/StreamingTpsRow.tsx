import { memo } from "react";
import { Zap } from "lucide-react";
import { useLiveSessionId, useLiveTps } from "@/hooks/useLiveTps";
import { formatTps, round1 } from "@/utils/tps";

/** 流式生成速度行：与完成后的操作行（MessageActions）同构——正文下方
 *  border-t 小字行，流式期间在此先行显示实时速度，完成后被操作行
 *  （速度 + token 用量合并块）原位替换，位置与样式保持连续。
 *  只显示速度数字（无「正在生成」文案与秒数）；等待期由 tracker 保留
 *  最近值；尚无样本（TTFT）时行占位、内容为空，避免出现时高度跳动。
 *  读屏语义（正在生成）保留在 aria-label。
 *
 *  内部自订阅 store 与采样定时器：memo 化后不随父级 16ms 批量重渲而刷新，
 *  仅当 600ms 采样到的快照变化时重渲。 */
export const StreamingTpsRow = memo(function StreamingTpsRow() {
  const sessionId = useLiveSessionId();
  const { tps } = useLiveTps(sessionId, true);
  const showTps = tps > 0;
  return (
    <div
      className="mt-1 flex items-center gap-1 border-t border-border/50 pt-1 text-[11px] text-muted-foreground/70 tabular-nums"
      role="status"
      aria-label={showTps ? `正在生成，速度 ${formatTps(round1(tps))}` : '正在生成'}
    >
      {showTps && (
        <span className="inline-flex items-center gap-0.5">
          <Zap className="w-3 h-3" aria-hidden="true" />
          {formatTps(round1(tps))}
        </span>
      )}
    </div>
  );
});
