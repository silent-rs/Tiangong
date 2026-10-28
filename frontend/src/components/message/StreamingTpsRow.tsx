import { memo } from "react";
import { Zap } from "lucide-react";
import { useLiveSessionId, useLiveTps } from "@/hooks/useLiveTps";
import { formatTps, round1 } from "@/utils/tps";

/** 流式生成速度行：与完成后的操作行（MessageActions）同构同位——正文
 *  下方 border-t 小字行，复制按钮同宽占位后跟随速度（与操作行的生成
 *  度量合并块同样的左边距与分隔线样式），完成后被操作行原位替换，
 *  速度与 token 统计的位置完全对齐、视觉连续。
 *  只显示速度数字（无「正在生成」文案与秒数）；等待期由 tracker 保留
 *  最近值；尚无样本（TTFT）时行占位、内容为空，避免高度跳动。
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
      className="mt-1 flex items-center gap-0.5 border-t border-border/50 pt-1 text-[11px] text-muted-foreground/70 tabular-nums"
      role="status"
      aria-label={showTps ? `正在生成，速度 ${formatTps(round1(tps))}` : '正在生成'}
    >
      {/* 与操作行复制按钮（p-1 + w-3.5 图标）同宽的占位：速度起点对齐
          完成后生成度量合并块的位置。 */}
      <span className="inline-block h-3.5 w-3.5 p-1" aria-hidden="true" />
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
