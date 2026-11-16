/**
 * 中枢右上角运行状态（对齐普通模式顶栏状态）：中枢 Core 本轮 runStatus
 * 的紧凑徽章——执行中带脉动点，空闲/出错分色。绝对定位于视图右上角。
 */
import { useStore } from '@/store/useStore';

export function CentralStatusBadge() {
  const runStatus = useStore((s) => s.runStatus);
  const label =
    runStatus === 'executing' ? '执行中' : runStatus === 'error' ? '出错' : '空闲';
  const dotClass =
    runStatus === 'executing'
      ? 'bg-emerald-500 animate-pulse'
      : runStatus === 'error'
        ? 'bg-destructive'
        : 'bg-muted-foreground/50';
  return (
    <div
      data-testid="central-status-badge"
      className="pointer-events-none absolute right-3 top-2 z-20 flex items-center gap-1.5 rounded-md bg-background/80 px-2 py-0.5 text-[11px] text-muted-foreground/80 backdrop-blur-sm"
      aria-label={`中枢状态：${label}`}
    >
      <span className={`inline-block h-1.5 w-1.5 rounded-full ${dotClass}`} aria-hidden="true" />
      中枢 · {label}
    </div>
  );
}
