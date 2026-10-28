import { memo } from "react";
import { Zap } from "lucide-react";
import { useLiveSessionId, useLiveTps } from "@/hooks/useLiveTps";
import { formatTps, round1 } from "@/utils/tps";

/** 流式生成速度行：与完成后的操作行（MessageActions）同构——正文下方
 *  border-t 小字行，渲染与操作行相同的前缀（复制按钮同宽占位 + 消息
 *  时间），速度以与生成度量合并块相同的样式跟随其后。完成后被操作行
 *  原位替换（仅追加时长与用量徽标），速度位置基本不动。
 *  只显示速度数字（无「正在生成」文案与秒数）；等待期由 tracker 保留
 *  最近值；尚无样本（TTFT）时仍渲染前缀占位（与操作行布局一致，避免
 *  出现时跳动）。读屏语义（正在生成）保留在 aria-label。
 *
 *  内部自订阅 store 与采样定时器：memo 化后不随父级 16ms 批量重渲而刷新，
 *  仅当 600ms 采样到的快照变化时重渲；time 字符串稳定不影响 memo。 */
export const StreamingTpsRow = memo(function StreamingTpsRow({ time }: { time?: string }) {
  const sessionId = useLiveSessionId();
  const { tps } = useLiveTps(sessionId, true);
  const showTps = tps > 0;
  return (
    <div
      className="mt-1 flex items-center gap-0.5 border-t border-border/50 pt-1 text-[11px] text-muted-foreground/70 tabular-nums"
      role="status"
      aria-label={showTps ? `正在生成，速度 ${formatTps(round1(tps))}` : '正在生成'}
    >
      {/* 与操作行复制按钮（p-1 + w-3.5 图标）同宽的占位。 */}
      <span className="inline-block h-3.5 w-3.5 p-1" aria-hidden="true" />
      {time && (
        <span className="ml-1 pl-1 border-l border-border/60 text-[11px] text-muted-foreground/70 tabular-nums">
          {time}
        </span>
      )}
      {showTps && (
        <span className="inline-flex items-center gap-1.5 ml-1 pl-1.5 border-l border-border/60">
          <span className="inline-flex items-center gap-0.5">
            <Zap className="w-3 h-3" aria-hidden="true" />
            {formatTps(round1(tps))}
          </span>
        </span>
      )}
    </div>
  );
});
