/**
 * 中枢输入区用量条（token 统计 + 生成速度）：与普通模式 MessageInput 的
 * TokenStatsBlock 同源同语义——实时 tps 优先，缺失回退最近完成消息的持久
 * 速度；token 环形进度 + 总计。中枢固定会话 ID 采样。
 */
import { Zap } from 'lucide-react';
import { useStore } from '@/store/useStore';
import { useLiveTps } from '@/hooks/useLiveTps';
import { formatTps, round1 } from '@/utils/tps';
import { latestTpsOf } from '@/api/message';
import { CENTRAL_SESSION_ID } from '@/features/central/lib/centralSession';

function formatTokenCount(count: number): string {
  if (count >= 1_000_000) return `${round1(count / 1_000_000)}M`;
  if (count >= 1_000) return `${round1(count / 1_000)}k`;
  return String(count);
}

export function CentralUsageBar() {
  const tokenStats = useStore((s) => s.tokenStats);
  const { tps } = useLiveTps(CENTRAL_SESSION_ID, true);
  const historyTps = useStore((s) => latestTpsOf(s.messages));
  const displayTps = tps > 0 ? tps : (historyTps ?? 0);
  const displayTokens = tokenStats?.current_tokens ?? 0;
  const totalTokens = tokenStats?.total_tokens ?? 0;
  const compressionThreshold = tokenStats?.compression_threshold_tokens ?? 0;
  const compressionProgress =
    compressionThreshold > 0 ? Math.min(100, (displayTokens / compressionThreshold) * 100) : 0;
  const showTps = displayTps > 0;
  const showTokens = displayTokens > 0 || totalTokens > 0;
  if (!showTps && !showTokens) return null;
  return (
    <div
      className="flex items-center gap-2 rounded-md bg-muted/40 px-2 py-0.5 text-xs text-muted-foreground/70 tabular-nums"
      title={`当前 ${displayTokens.toLocaleString()} tokens\n压缩阈值 ${compressionThreshold.toLocaleString()} tokens\n总计 ${totalTokens.toLocaleString()} tokens`}
    >
      {showTps && (
        <span
          className="flex shrink-0 items-center gap-1"
          aria-label={`最近生成速度 ${formatTps(round1(displayTps))}`}
          title="最近一次生成的输出速度"
        >
          <Zap className="h-3 w-3 text-foreground/70" aria-hidden="true" />
          {formatTps(round1(displayTps))}
        </span>
      )}
      {showTps && showTokens && <span className="h-3 w-px bg-border/60" aria-hidden="true" />}
      {showTokens && (
        <>
          {compressionThreshold > 0 && (
            <span
              className="relative inline-flex h-3 w-3 items-center justify-center"
              title={`上下文 ${Math.round(compressionProgress)}%`}
              aria-hidden="true"
            >
              <svg viewBox="0 0 36 36" className="h-3 w-3 -rotate-90">
                <circle cx="18" cy="18" r="15" fill="none" strokeWidth="5" className="stroke-border/60" />
                <circle
                  cx="18"
                  cy="18"
                  r="15"
                  fill="none"
                  strokeWidth="5"
                  strokeLinecap="round"
                  className="stroke-foreground/70"
                  strokeDasharray={`${(compressionProgress / 100) * 94.2} 94.2`}
                />
              </svg>
            </span>
          )}
          <span>总计 {formatTokenCount(totalTokens)}</span>
        </>
      )}
    </div>
  );
}
