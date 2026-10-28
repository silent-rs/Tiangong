import { memo } from "react";
import { Zap } from "lucide-react";
import { usageOf } from "@/api/message";
import { useLiveSessionId, useLiveTps } from "@/hooks/useLiveTps";
import { formatTps, round1 } from "@/utils/tps";
import { CallUsageDetails } from "./CallUsageDetails";
import type { MessageItem } from "./types";

/** 轮次实时速度行：挂在轮次尾部（isActive 期间常驻——含流式输出、
 *  工具执行、TTFT 等待），轮次结束由父级卸载。速度取 stream 累计计数
 *  的滑动窗口差分（tracker.lastTps 跨等待期保留最近有效值：轮次中出
 *  现过就一直显示）；轮次已有用量数据时徽标同行跟随，不跨行。
 *  与完成后的操作行同构：border-t 小字行 + 复制按钮同宽占位；只显示
 *  速度数字（无文案与秒数）。读屏语义（正在生成）保留在 aria-label。
 *  尚未拿到速度样本、也没有用量数据时整体不渲染（轮次刚开始不出现空
 *  分隔线），首个样本到达后才出现。
 *
 *  内部自订阅 store 与采样定时器：memo 化后不随父级 16ms 批量重渲而刷新，
 *  仅当 600ms 采样到的快照变化时重渲。 */
export const StreamingTpsRow = memo(function StreamingTpsRow({ usageMessages }: {
  usageMessages?: MessageItem[];
}) {
  const sessionId = useLiveSessionId();
  const { tps } = useLiveTps(sessionId, true);
  const showTps = tps > 0;
  // CallUsageDetails 无用量时自身返回 null，这里先过滤以整行隐藏。
  const usageList = (usageMessages ?? []).filter((message) => usageOf(message) != null);
  const hasUsage = usageList.length > 0;
  if (!showTps && !hasUsage) return null;
  return (
    <div
      className="mt-1 flex items-center gap-0.5 border-t border-border/50 pt-1 text-[11px] text-muted-foreground/70 tabular-nums"
      role="status"
      aria-label={showTps ? `正在生成，速度 ${formatTps(round1(tps))}` : '正在生成'}
    >
      {/* 与操作行复制按钮（p-1 + w-3.5 图标）同宽的占位。 */}
      <span className="inline-block h-3.5 w-3.5 p-1" aria-hidden="true" />
      <span className="inline-flex items-center gap-1.5 ml-1 pl-1.5 border-l border-border/60">
        {showTps && (
          <span className="inline-flex items-center gap-0.5">
            <Zap className="w-3 h-3" aria-hidden="true" />
            {formatTps(round1(tps))}
          </span>
        )}
        {hasUsage && <CallUsageDetails messages={usageList} />}
      </span>
    </div>
  );
});
