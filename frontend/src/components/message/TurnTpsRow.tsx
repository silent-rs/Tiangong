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
 *  现过就一直显示，新一轮开始沿用上一轮最近值）；轮次已有用量数据时
 *  徽标同行跟随，不跨行。
 *  小字行 + border-t，与完成后的操作行视觉同族，但不再模拟操作行的
 *  复制按钮前缀——速度行独立挂在轮次尾部，不需要与消息内按钮对齐。
 *  只显示速度数字（无文案与秒数）。读屏语义（正在生成）保留在
 *  aria-label。尚未拿到速度样本、也没有用量数据时整体不渲染（轮次
 *  刚开始不出现空分隔线），首个样本到达后才出现。
 *
 *  内部自订阅 store 与采样定时器：memo 化后不随父级 16ms 批量重渲而刷新，
 *  仅当 600ms 采样到的快照变化时重渲（父级已按用量内容签名缓存
 *  usageMessages，引用稳定）。 */
export const TurnTpsRow = memo(function TurnTpsRow({ usageMessages }: {
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
      className="mt-1 flex items-center gap-1.5 border-t border-border/50 pt-1 text-[11px] text-muted-foreground/70 tabular-nums"
      role="status"
      aria-label={showTps ? `正在生成，速度 ${formatTps(round1(tps))}` : '正在生成'}
    >
      {showTps && (
        <span className="inline-flex items-center gap-0.5">
          <Zap className="w-3 h-3" aria-hidden="true" />
          {formatTps(round1(tps))}
        </span>
      )}
      {hasUsage && <CallUsageDetails messages={usageList} />}
    </div>
  );
});
