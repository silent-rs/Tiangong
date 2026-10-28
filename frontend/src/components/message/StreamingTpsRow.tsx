import { memo } from "react";
import { Activity } from "lucide-react";
import { useLiveSessionId, useLiveTps } from "@/hooks/useLiveTps";
import { formatTps, round1 } from "@/utils/tps";

/** 流式生成行：复用 tool-run-row 扫光语言（DeepSeek Harness running sweep），
 *  展示「正在生成 + 实时速度 + 跳秒」；速度为后端逐批真实 token 计数
 *  （StreamEvent.tokens）的滑动窗口值，样本不足时只显示「正在生成」。
 *  挂在正在流式的消息正文下方，轮次结束由父级卸载。
 *
 *  内部自订阅 store 与采样定时器：memo 化后不随父级 16ms 批量重渲而刷新，
 *  仅当 600ms 采样到的快照变化时重渲。 */
export const StreamingTpsRow = memo(function StreamingTpsRow() {
  const sessionId = useLiveSessionId();
  const { tps, elapsedMs } = useLiveTps(sessionId, true);
  const seconds = elapsedMs != null ? Math.floor(elapsedMs / 1000) : null;
  const showTps = tps > 0;
  return (
    <div
      className="tool-run-row mt-1 flex w-full items-center gap-2 rounded px-2 py-1 text-xs text-foreground/80"
      role="status"
      aria-label={
        showTps
          ? `正在生成，速度 ${formatTps(round1(tps))}${seconds != null ? `，已进行 ${seconds} 秒` : ''}`
          : '正在生成'
      }
    >
      <Activity className="h-3 w-3 shrink-0" aria-hidden="true" />
      <span className="shrink-0 font-medium">正在生成</span>
      {(showTps || seconds != null) && (
        <span className="truncate text-muted-foreground tabular-nums">
          {showTps && formatTps(round1(tps))}
          {showTps && seconds != null && ' · '}
          {seconds != null && `${seconds}s`}
        </span>
      )}
    </div>
  );
});
