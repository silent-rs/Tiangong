import { useCallback, useEffect, useRef, useState } from 'react';
import { Loader2, RefreshCw } from 'lucide-react';
import { api } from '@/api/tauri';
import type { CodexUsage, CodexUsageWindow } from '@/api/tauri';
import { Button } from './ui/button';

/** 窗口时长描述：5 小时 / 7 天 等。 */
export function windowLabel(seconds?: number): string {
  if (!seconds || seconds <= 0) return '额度';
  if (seconds % 86400 === 0) return `${seconds / 86400} 天`;
  if (seconds % 3600 === 0) return `${seconds / 3600} 小时`;
  return `${Math.ceil(seconds / 60)} 分钟`;
}

/** 距重置的相对时间描述。 */
export function resetLabel(resetAt: number | undefined, nowMs: number = Date.now()): string | null {
  if (!resetAt) return null;
  const secs = Math.max(0, Math.round(resetAt - nowMs / 1000));
  const days = Math.floor(secs / 86400);
  const hours = Math.floor((secs % 86400) / 3600);
  const mins = Math.floor((secs % 3600) / 60);
  if (days > 0) return `${days} 天 ${hours} 小时后重置`;
  if (hours > 0) return `${hours} 小时 ${mins} 分钟后重置`;
  return `${Math.max(1, mins)} 分钟后重置`;
}

function barColor(percent: number): string {
  if (percent >= 90) return 'bg-destructive';
  if (percent >= 70) return 'bg-amber-500';
  return 'bg-green-500';
}

function UsageBar({ label, window }: { label: string; window: CodexUsageWindow }) {
  const percent = Math.min(100, Math.max(0, window.used_percent));
  const reset = resetLabel(window.reset_at);
  const resetTitle = window.reset_at ? new Date(window.reset_at * 1000).toLocaleString() : undefined;
  return (
    <div className="space-y-1">
      <div className="flex items-center justify-between text-xs">
        <span>{label}</span>
        <span className="text-muted-foreground" title={resetTitle}>
          已用 {percent.toFixed(percent < 10 && percent % 1 !== 0 ? 1 : 0)}%{reset ? ` · ${reset}` : ''}
        </span>
      </div>
      <div className="h-1.5 w-full overflow-hidden rounded-full bg-muted">
        <div className={`h-full ${barColor(percent)}`} style={{ width: `${percent}%` }} />
      </div>
    </div>
  );
}

/** ChatGPT 账号用量额度：展示各额度窗口已用百分比与重置时间，支持手动刷新。 */
export function CodexUsagePanel() {
  const [usage, setUsage] = useState<CodexUsage | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const seqRef = useRef(0);

  const load = useCallback(async () => {
    const seq = ++seqRef.current;
    setLoading(true);
    setError(null);
    try {
      const next = await api.codexAuthUsage();
      if (seq === seqRef.current) setUsage(next);
    } catch (err) {
      if (seq === seqRef.current) setError(String(err));
    } finally {
      if (seq === seqRef.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
    return () => { seqRef.current += 1; };
  }, [load]);

  return (
    <div className="rounded-md border px-3 py-2 space-y-2">
      <div className="flex items-center justify-between">
        <span className="text-xs font-medium">
          用量额度
          {usage && (usage.limit_reached || !usage.allowed) && (
            <span className="ml-2 text-destructive">已达上限</span>
          )}
        </span>
        <Button variant="ghost" size="sm" className="h-6 px-2 text-xs" onClick={load} disabled={loading}>
          {loading ? <Loader2 className="w-3 h-3 mr-1 animate-spin" /> : <RefreshCw className="w-3 h-3 mr-1" />}
          刷新
        </Button>
      </div>
      {usage === null && loading && (
        <div className="text-xs text-muted-foreground">正在查询...</div>
      )}
      {usage && (
        <>
          {usage.windows.length === 0 && (
            <div className="text-xs text-muted-foreground">服务端未返回额度窗口</div>
          )}
          {usage.windows.map((w, i) => (
            <UsageBar key={`w-${i}`} label={`${windowLabel(w.window_seconds)}额度`} window={w} />
          ))}
          {usage.extra_limits.flatMap((limit) =>
            limit.windows.length > 0
              ? limit.windows.map((w, i) => (
                <UsageBar key={`${limit.name}-${i}`} label={`${limit.name} · ${windowLabel(w.window_seconds)}`} window={w} />
              ))
              : limit.limit_reached
                ? [<div key={limit.name} className="text-xs text-destructive">{limit.name}：已达上限</div>]
                : [],
          )}
          {(usage.credits_unlimited || usage.credits_balance) && (
            <div className="text-xs text-muted-foreground">
              点数：{usage.credits_unlimited ? '不限量' : usage.credits_balance}
            </div>
          )}
        </>
      )}
      {error && <p className="text-xs text-destructive break-all">{error}</p>}
    </div>
  );
}
